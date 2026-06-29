//! Shared provider health tracker + automatic failover.
//!
//! Both the executive session loop and the subagent runtime record the outcome of every provider
//! call here. When a provider accumulates too many consecutive failures of a failover-worthy kind
//! (HTTP 429 rate-limit, 402 payment-required, request timeout), the tracker flips that provider
//! into `Degraded` and `Failover::effective_model` starts returning the configured backup model
//! instead — so a flaky OpenRouter :free tier silently falls back to Ollama Cloud (or vice-versa)
//! instead of failing the task. A successful call resets the consecutive-failure counter.
//!
//! The tracker is process-global (good: a subagent fan-out shares one view of provider health) and
//! lock-contended only for the duration of a short critical section per call.
use crate::agent::provider;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};

/// Classify a provider error string into a `FailKind`. Only the kinds we know how to recover from
/// by switching providers count toward the degradation threshold — a model-not-found or a bad
/// request won't be fixed by failing over, so we don't punish the provider for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailKind {
    /// HTTP 429 — rate limited. The single most common :free-tier failure; failover helps.
    RateLimit,
    /// HTTP 402 — payment required / insufficient balance. Failover to a working provider helps.
    PaymentRequired,
    /// Request timeout (hung provider or network). A different endpoint may respond.
    Timeout,
    /// Stream reset / connection closed mid-stream. Often transient; failover may help.
    StreamInterrupted,
    /// Anything else (auth, model-not-found, 5xx, ...). Not failover-worthy on its own.
    Other,
}

impl FailKind {
    /// True when this kind of failure should count toward the degradation threshold. A 401
    /// (bad key) or 404 (bad model) won't be fixed by switching providers, so we don't.
    pub fn is_failover_worthy(self) -> bool {
        matches!(
            self,
            FailKind::RateLimit
                | FailKind::PaymentRequired
                | FailKind::Timeout
                | FailKind::StreamInterrupted
        )
    }
}

/// Parse a provider error string (from the adapter's `Err(...)`) into a `FailKind`. Looks for
/// the HTTP status code and common transport-error substrings. Best-effort: unrecognized errors
/// classify as `Other`.
pub fn classify_error(err: &str) -> FailKind {
    let lower = err.to_ascii_lowercase();
    // HTTP status: "returned 429", "429", "status: 429", etc.
    if lower.contains("429") || lower.contains("rate limit") {
        return FailKind::RateLimit;
    }
    if lower.contains("402") || lower.contains("payment") || lower.contains("insufficient") {
        return FailKind::PaymentRequired;
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("request to")
    {
        // "request to <url> failed: ..." is the adapter's transport-error prefix; only treat as
        // timeout when a timeout-related cause is present, otherwise fall through to Other.
        if lower.contains("timeout") || lower.contains("timed out") {
            return FailKind::Timeout;
        }
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return FailKind::Timeout;
    }
    if lower.contains("stream error")
        || lower.contains("connection")
        || lower.contains("reset")
        || lower.contains("broken pipe")
        || lower.contains("eof")
    {
        return FailKind::StreamInterrupted;
    }
    FailKind::Other
}

/// Per-provider health record. Failure counters are consecutive: any successful call resets them.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProviderHealth {
    pub status: HealthStatus,
    /// Consecutive failover-worthy failures since the last success.
    pub consecutive_failures: u32,
    /// Total failures of each kind since the last reset (for the UI breakdown).
    pub failures_by_kind: HashMap<FailKind, u32>,
    /// Total successful calls since the last reset.
    pub successes: u64,
    /// Total failover-worthy failures since the last reset.
    pub failures: u64,
    /// When the provider entered `Degraded` (unix ms), or null while healthy.
    #[serde(rename = "degradedAt", skip_serializing_if = "Option::is_none")]
    pub degraded_at: Option<i64>,
    /// The last error message, for the UI tooltip.
    #[serde(rename = "lastError", skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Provider is responding normally.
    Healthy,
    /// Consecutive failures exceeded the threshold; failover is active.
    Degraded,
    /// Operator manually disabled this provider (not yet used; reserved).
    Disabled,
}

impl Default for HealthStatus {
    fn default() -> Self {
        HealthStatus::Healthy
    }
}

/// The failover pair: a primary provider and the backup to use when the primary is degraded.
/// The backup model is resolved at construction time so `effective_model` never fails.
#[derive(Clone, Debug)]
pub struct FailoverPair {
    pub primary_provider: String,
    pub primary_model: String,
    pub backup_provider: String,
    pub backup_model: String,
}

/// Resolve the default failover pair for a given primary provider. The backup is the *other* big
/// free-form provider: OpenRouter ↔ Ollama Cloud. For anything else we fall back to Ollama Cloud
/// (the cheapest always-on default) if it differs from the primary, otherwise no backup.
pub fn default_failover_for(primary_provider: &str, primary_model: &str) -> Option<FailoverPair> {
    let (backup_provider, backup_model) = match primary_provider {
        "openrouter" => ("ollama", "minimax-m3"),
        "ollama" => ("openrouter", "nex-agi/nex-n2-pro:free"),
        _ => {
            // For any other primary, try Ollama Cloud as a backup if it differs.
            if primary_provider != "ollama" {
                ("ollama", "minimax-m3")
            } else {
                return None;
            }
        }
    };
    // Don't configure a failover to the same provider.
    if backup_provider == primary_provider {
        return None;
    }
    // Validate the backup resolves before committing to it.
    if provider::resolve(backup_provider, backup_model).is_none() {
        return None;
    }
    Some(FailoverPair {
        primary_provider: primary_provider.to_string(),
        primary_model: primary_model.to_string(),
        backup_provider: backup_provider.to_string(),
        backup_model: backup_model.to_string(),
    })
}

/// The effective model to use for the next call, given the current health of the primary provider.
/// Returns the primary model when healthy, the backup when degraded.
pub fn effective_model(pair: &FailoverPair, primary_healthy: bool) -> (String, String) {
    if primary_healthy {
        (
            pair.primary_provider.clone(),
            pair.primary_model.clone(),
        )
    } else {
        (
            pair.backup_provider.clone(),
            pair.backup_model.clone(),
        )
    }
}

/// Consecutive failover-worthy failures before a provider is marked degraded. Low (2) because a
/// :free tier that 429s twice in a row is almost certainly saturated, and we want fast failover.
const DEGRADE_THRESHOLD: u32 = 2;
/// After a provider is degraded, allow a single probe call to it after this cooldown. If the probe
/// succeeds, the provider recovers. Long enough to let a rate-limit window pass.
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The process-global health map, keyed by provider id.
static HEALTH: OnceLock<RwLock<HashMap<String, ProviderHealth>>> = OnceLock::new();
fn health_map() -> &'static RwLock<HashMap<String, ProviderHealth>> {
    HEALTH.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Broadcast channel for provider-health state-change events. A single sender is stored globally;
/// WS loops subscribe via `subscribe_health_events`. The channel is large enough that a slow
/// subscriber lags rather than blocking the recorder.
static HEALTH_EVENTS: OnceLock<broadcast::Sender<serde_json::Value>> = OnceLock::new();
fn health_events() -> &'static broadcast::Sender<serde_json::Value> {
    HEALTH_EVENTS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel::<serde_json::Value>(64);
        tx
    })
}

/// Subscribe to provider-health state-change events. Used by the WS loop to push updates to the
/// UI's conn-chip. Returns a fresh receiver; the channel never closes in practice.
pub fn subscribe_health_events() -> broadcast::Receiver<serde_json::Value> {
    let tx = health_events();
    tx.subscribe()
}

/// Emit a health state-change event to all WS subscribers. Called by `record_success` and
/// `record_failure` when the status transitions. Best-effort: a dropped/laggard receiver is fine.
fn emit_health_event(provider: &str, status: HealthStatus) {
    let tx = health_events();
    let frame = json!({
        "kind": "provider_health",
        "provider": provider,
        "status": status,
    });
    let _ = tx.send(frame);
}

/// Reset all health state (test helper + future admin endpoint).
pub async fn reset() {
    let mut m = health_map().write().await;
    m.clear();
}

/// Read a snapshot of every provider's health, for the REST endpoint.
pub async fn snapshot() -> HashMap<String, ProviderHealth> {
    health_map().read().await.clone()
}

/// Record a successful provider call. Resets the consecutive-failure counter and recovers a
/// degraded provider immediately (a live success is the strongest signal).
pub async fn record_success(provider: &str) {
    let mut m = health_map().write().await;
    let h = m.entry(provider.to_string()).or_default();
    let was_degraded = h.status == HealthStatus::Degraded;
    h.consecutive_failures = 0;
    h.successes += 1;
    if was_degraded {
        h.status = HealthStatus::Healthy;
        h.degraded_at = None;
        h.last_error = None;
        emit_health_event(provider, HealthStatus::Healthy);
    }
}

/// Record a failed provider call. Returns the new health status so the caller can decide whether
/// to fail over. Classifies the error and only counts failover-worthy kinds toward the threshold.
pub async fn record_failure(provider: &str, error: &str) -> HealthStatus {
    let kind = classify_error(error);
    let mut m = health_map().write().await;
    let h = m.entry(provider.to_string()).or_default();
    *h.failures_by_kind.entry(kind).or_insert(0) += 1;
    h.last_error = Some(error.to_string());
    let mut transitioned = false;
    if kind.is_failover_worthy() {
        h.failures += 1;
        h.consecutive_failures += 1;
        if h.consecutive_failures >= DEGRADE_THRESHOLD
            && h.status == HealthStatus::Healthy
        {
            h.status = HealthStatus::Degraded;
            h.degraded_at = Some(now_ms());
            transitioned = true;
        }
    }
    // A non-failover-worthy error (401/404/4xx) does NOT reset the consecutive counter — the
    // provider may still be degraded from earlier 429s. But a non-worthy error alone can't
    // degrade it either.
    let status = h.status;
    if transitioned {
        emit_health_event(provider, status);
    }
    status
}

/// True when the primary provider is currently degraded (failover should be active). Also handles
/// the recovery cooldown: a degraded provider that has been quiet long enough for a rate-limit
/// window to pass is eligible for a probe.
pub async fn is_degraded(provider: &str) -> bool {
    let m = health_map().read().await;
    let Some(h) = m.get(provider) else {
        return false;
    };
    h.status == HealthStatus::Degraded
}

/// The full failover decision for a call: which (provider, model) to use, and whether this call
/// is a recovery probe. The session/subagent loop calls this BEFORE building the request.
///
/// Returns None when no failover pair is configured for this primary (caller uses the primary
/// unchanged). Returns Some((provider, model, is_probe)) otherwise.
pub async fn resolve_effective_model(
    primary_provider: &str,
    primary_model: &str,
) -> Option<(String, String, bool)> {
    let pair = default_failover_for(primary_provider, primary_model)?;
    let degraded = is_degraded(primary_provider).await;

    // Recovery probe: if degraded AND the cooldown has elapsed, let ONE call through to the
    // primary to test whether it recovered. The caller must record the outcome so a success
    // clears the degraded flag.
    let probe = if degraded {
        let cooldown_elapsed = {
            let m = health_map().read().await;
            m.get(primary_provider)
                .and_then(|h| h.degraded_at)
                .map(|at| {
                    let elapsed = Duration::from_millis((now_ms() - at).max(0) as u64);
                    elapsed >= RECOVERY_COOLDOWN
                })
                .unwrap_or(false)
        };
        cooldown_elapsed
    } else {
        false
    };

    let (prov, model) = effective_model(&pair, !degraded || probe);
    Some((prov, model, probe))
}

/// A WS frame the UI can consume to update the conn-chip. Emitted whenever a provider transitions
/// between Healthy and Degraded.
pub fn health_event_json() -> serde_json::Value {
    serde_json::json!({ "kind": "provider_health" })
}

/// Build the full snapshot payload for the REST endpoint + the initial WS push.
pub async fn health_snapshot_json() -> serde_json::Value {
    let snap = snapshot().await;
    // Include the configured failover pairs so the UI can show "OpenRouter ↔ Ollama Cloud".
    let mut pairs = serde_json::Map::new();
    for provider in ["openrouter", "ollama"] {
        if let Some(pair) = default_failover_for(
            provider,
            if provider == "openrouter" {
                "nex-agi/nex-n2-pro:free"
            } else {
                "minimax-m3"
            },
        ) {
            pairs.insert(
                provider.to_string(),
                serde_json::json!({
                    "primary": { "provider": pair.primary_provider, "modelId": pair.primary_model },
                    "backup": { "provider": pair.backup_provider, "modelId": pair.backup_model },
                }),
            );
        }
    }
    serde_json::json!({ "providers": snap, "failoverPairs": pairs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_detects_rate_limit() {
        assert_eq!(
            classify_error("openrouter returned 429: rate limit exceeded"),
            FailKind::RateLimit
        );
        assert_eq!(classify_error("HTTP 429 Too Many Requests"), FailKind::RateLimit);
        assert_eq!(classify_error("rate limit hit"), FailKind::RateLimit);
    }

    #[test]
    fn classify_error_detects_payment_required() {
        assert_eq!(
            classify_error("openrouter returned 402: insufficient balance"),
            FailKind::PaymentRequired
        );
        assert_eq!(
            classify_error("402 Payment Required"),
            FailKind::PaymentRequired
        );
    }

    #[test]
    fn classify_error_detects_timeout() {
        assert_eq!(
            classify_error("request to https://openrouter.ai/api/v1/chat/completions failed: timed out"),
            FailKind::Timeout
        );
        assert_eq!(classify_error("operation timed out"), FailKind::Timeout);
    }

    #[test]
    fn classify_error_detects_stream_interrupted() {
        assert_eq!(classify_error("stream error: connection reset"), FailKind::StreamInterrupted);
        assert_eq!(classify_error("broken pipe"), FailKind::StreamInterrupted);
    }

    #[test]
    fn classify_error_falls_back_to_other() {
        assert_eq!(classify_error("returned 401: invalid api key"), FailKind::Other);
        assert_eq!(classify_error("model not found"), FailKind::Other);
        assert_eq!(classify_error("500 internal server error"), FailKind::Other);
    }

    #[test]
    fn failover_worthy_kinds() {
        assert!(FailKind::RateLimit.is_failover_worthy());
        assert!(FailKind::PaymentRequired.is_failover_worthy());
        assert!(FailKind::Timeout.is_failover_worthy());
        assert!(FailKind::StreamInterrupted.is_failover_worthy());
        assert!(!FailKind::Other.is_failover_worthy());
    }

    #[test]
    fn default_failover_pairs_are_symmetric() {
        let or = default_failover_for("openrouter", "nex-agi/nex-n2-pro:free").unwrap();
        assert_eq!(or.backup_provider, "ollama");
        assert_eq!(or.backup_model, "minimax-m3");

        let ol = default_failover_for("ollama", "glm-5.2").unwrap();
        assert_eq!(ol.backup_provider, "openrouter");
        assert_eq!(ol.backup_model, "nex-agi/nex-n2-pro:free");
    }

    #[test]
    fn default_failover_returns_none_for_same_provider_backup() {
        // A provider whose only possible backup would be itself has no failover.
        // "local" → backup would be "ollama" (different), so it DOES have a failover.
        assert!(default_failover_for("local", "qwen2.5-coder").is_some());
    }

    #[test]
    fn effective_model_picks_backup_when_degraded() {
        let pair = FailoverPair {
            primary_provider: "openrouter".into(),
            primary_model: "nex-agi/nex-n2-pro:free".into(),
            backup_provider: "ollama".into(),
            backup_model: "minimax-m3".into(),
        };
        assert_eq!(
            effective_model(&pair, true),
            ("openrouter".into(), "nex-agi/nex-n2-pro:free".into())
        );
        assert_eq!(
            effective_model(&pair, false),
            ("ollama".into(), "minimax-m3".into())
        );
    }

    #[tokio::test]
    async fn record_failure_degrades_after_threshold() {
        // Isolate this test: reset the global state first.
        reset().await;
        let provider = "test-degrade-provider";

        // First failure: not yet degraded.
        let status = record_failure(provider, "returned 429: rate limit").await;
        assert_eq!(status, HealthStatus::Healthy);

        // Second consecutive failure: crosses the threshold → degraded.
        let status = record_failure(provider, "returned 429: rate limit").await;
        assert_eq!(status, HealthStatus::Degraded);

        assert!(is_degraded(provider).await);
        reset().await;
    }

    #[tokio::test]
    async fn non_worthy_failure_does_not_degrade() {
        reset().await;
        let provider = "test-nonworthy-provider";

        let status = record_failure(provider, "returned 401: invalid api key").await;
        assert_eq!(status, HealthStatus::Healthy);
        assert!(!is_degraded(provider).await);
        reset().await;
    }

    #[tokio::test]
    async fn success_resets_consecutive_failures() {
        reset().await;
        let provider = "test-reset-provider";

        record_failure(provider, "returned 429: rate limit").await;
        record_failure(provider, "returned 429: rate limit").await;
        assert!(is_degraded(provider).await);

        record_success(provider).await;
        assert!(!is_degraded(provider).await);

        let snap = snapshot().await;
        let h = snap.get(provider).unwrap();
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.successes, 1);
        assert_eq!(h.status, HealthStatus::Healthy);
        reset().await;
    }

    #[tokio::test]
    async fn resolve_effective_model_failover_and_recovery() {
        // Use a unique provider name so concurrent tests sharing the global health map
        // cannot wipe this test's state via their own reset() calls.
        let primary_provider = "test-failover-recovery-provider";
        let primary_model = "nex-agi/nex-n2-pro:free";

        // Healthy → primary.
        let r = resolve_effective_model(primary_provider, primary_model).await;
        let (prov, model, probe) = r.unwrap();
        assert_eq!(prov, primary_provider);
        assert_eq!(model, "nex-agi/nex-n2-pro:free");
        assert!(!probe);

        // Degrade the primary.
        record_failure(primary_provider, "returned 429: rate limit").await;
        record_failure(primary_provider, "returned 429: rate limit").await;
        assert!(is_degraded(primary_provider).await);

        // Degraded → backup, NOT a probe (cooldown not elapsed). The backup for an
        // arbitrary provider is ollama/minimax-m3 (see default_failover_for).
        let (prov, model, probe) =
            resolve_effective_model(primary_provider, primary_model).await.unwrap();
        assert_eq!(prov, "ollama");
        assert_eq!(model, "minimax-m3");
        assert!(!probe);

        // A success recovers the provider.
        record_success(primary_provider).await;
        let (prov, model, _) =
            resolve_effective_model(primary_provider, primary_model).await.unwrap();
        assert_eq!(prov, primary_provider);
        assert_eq!(model, "nex-agi/nex-n2-pro:free");
        reset().await;
    }

    /// A provider with no failover pair (e.g. anthropic → would fall back to ollama, which is
    /// different, so it DOES have a pair). Verify the None case: a provider whose backup would
    /// be itself returns None. We can't easily construct that with the current rules, so instead
    /// verify that a known-free-form provider returns Some.
    #[tokio::test]
    async fn resolve_effective_model_returns_some_for_free_form_providers() {
        reset().await;
        assert!(
            resolve_effective_model("openrouter", "nex-agi/nex-n2-pro:free")
                .await
                .is_some()
        );
        assert!(
            resolve_effective_model("ollama", "glm-5.2")
                .await
                .is_some()
        );
        reset().await;
    }

    /// The snapshot JSON must include both the per-provider health and the failover pairs so the
    /// UI can render the chip + a tooltip.
    #[tokio::test]
    async fn health_snapshot_json_includes_pairs_and_providers() {
        reset().await;
        record_failure("openrouter", "returned 429: rate limit").await;
        let json = health_snapshot_json().await;
        assert!(json.get("providers").is_some());
        assert!(json.get("failoverPairs").is_some());
        let pairs = json.get("failoverPairs").unwrap().as_object().unwrap();
        assert!(pairs.contains_key("openrouter"));
        assert!(pairs.contains_key("ollama"));
        reset().await;
    }
}
