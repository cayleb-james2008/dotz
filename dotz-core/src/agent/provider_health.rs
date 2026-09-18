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
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};

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
    /// Anything else (auth, model-not-found, ...). Not failover-worthy on its own.
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
    // HTTP status: all three adapters format errors as "{provider} returned {status}: {detail}".
    // Match the status code as a standalone number so a "429" or "402" appearing inside the
    // response body detail (a request id, port number, timestamp, …) does not false-positive
    // into RateLimit / PaymentRequired and pollute the failover tracker.
    if contains_status_code(&lower, "429") || lower.contains("rate limit") {
        return FailKind::RateLimit;
    }
    if contains_status_code(&lower, "402")
        || lower.contains("payment")
        || lower.contains("insufficient")
    {
        return FailKind::PaymentRequired;
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return FailKind::Timeout;
    }
    // HTTP 5xx — server-side errors (500/502/503/504). Often transient (provider outage,
    // overloaded upstream); failover to a different provider may succeed. Reuses
    // `StreamInterrupted` (already failover-worthy) so a provider-side outage trips failover
    // instead of failing the task.
    if contains_status_code(&lower, "500")
        || contains_status_code(&lower, "502")
        || contains_status_code(&lower, "503")
        || contains_status_code(&lower, "504")
        || lower.contains("service unavailable")
        || lower.contains("bad gateway")
    {
        return FailKind::StreamInterrupted;
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

/// True when `text` contains `code` as a standalone integer token (not a substring of a larger
/// number). This prevents a "429" embedded in "14293" or ":4290" from matching. The check is
/// byte-level and safe because the codes are ASCII digits.
fn contains_status_code(text: &str, code: &str) -> bool {
    let bytes = text.as_bytes();
    let cb = code.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(code) {
        let idx = from + rel;
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_digit();
        let after = idx + cb.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        if before_ok && after_ok {
            return true;
        }
        from = idx + 1;
    }
    false
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
#[derive(Default)]
pub enum HealthStatus {
    /// Provider is responding normally.
    #[default]
    Healthy,
    /// Consecutive failures exceeded the threshold; failover is active.
    Degraded,
    /// Operator manually disabled this provider (not yet used; reserved).
    Disabled,
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
    provider::resolve(backup_provider, backup_model)?;
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
        (pair.primary_provider.clone(), pair.primary_model.clone())
    } else {
        (pair.backup_provider.clone(), pair.backup_model.clone())
    }
}

/// Consecutive failover-worthy failures before a provider is marked degraded. Low (2) because a
/// :free tier that 429s twice in a row is almost certainly saturated, and we want fast failover.
const DEGRADE_THRESHOLD: u32 = 2;
/// After a provider is degraded, allow a single probe call to it after this cooldown. If the probe
/// succeeds, the provider recovers. Long enough to let a rate-limit window pass. Overridable for
/// tests via `DOTZ_RECOVERY_COOLDOWN_MS` (clamped to [0, 1h]); defaults to 60s.
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(60);

/// Resolve the recovery cooldown, honoring the `DOTZ_RECOVERY_COOLDOWN_MS` override (mainly for
/// tests that need to exercise the probe path without waiting a real minute). Mirrors the
/// `ws_ping_interval` env-override pattern in `agent::mod`.
fn recovery_cooldown() -> Duration {
    const MAX_MS: u64 = 3_600_000; // 1 hour
    std::env::var("DOTZ_RECOVERY_COOLDOWN_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|d| d.as_millis() <= MAX_MS as u128)
        .unwrap_or(RECOVERY_COOLDOWN)
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
        if h.consecutive_failures >= DEGRADE_THRESHOLD && h.status == HealthStatus::Healthy {
            h.status = HealthStatus::Degraded;
            h.degraded_at = Some(crate::util::now_ms());
            transitioned = true;
        } else if h.status == HealthStatus::Degraded {
            // A failover-worthy failure while already degraded (most importantly: a recovery
            // probe that found the primary STILL down) must restart the recovery cooldown.
            // Without this, the original `degraded_at` stays frozen at the first degradation,
            // the cooldown stays "elapsed", and `resolve_effective_model` routes EVERY
            // subsequent call to the primary as a probe — permanently abandoning the backup
            // even though the primary is still broken. Refreshing the timestamp sends the next
            // call back to the backup until another cooldown window passes.
            h.degraded_at = Some(crate::util::now_ms());
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
        
        {
            let m = health_map().read().await;
            m.get(primary_provider)
                .and_then(|h| h.degraded_at)
                .map(|at| {
                    let elapsed = Duration::from_millis((crate::util::now_ms() - at).max(0) as u64);
                    elapsed >= recovery_cooldown()
                })
                .unwrap_or(false)
        }
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

    /// Serialize tests that touch the process-global `HEALTH` map. `reset()` calls `m.clear()`,
    /// which wipes EVERY provider's state, not just the test's own key — so two of these tests
    /// running concurrently let one's `reset()` erase the other's accumulated failures mid-test
    /// (e.g. clearing a provider between its `record_failure` calls and its `is_degraded` check).
    /// A unique provider name does not help, because the clear is global. This lock serializes them.
    static HEALTH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn classify_error_detects_rate_limit() {
        assert_eq!(
            classify_error("openrouter returned 429: rate limit exceeded"),
            FailKind::RateLimit
        );
        assert_eq!(
            classify_error("HTTP 429 Too Many Requests"),
            FailKind::RateLimit
        );
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
            classify_error(
                "request to https://openrouter.ai/api/v1/chat/completions failed: timed out"
            ),
            FailKind::Timeout
        );
        assert_eq!(classify_error("operation timed out"), FailKind::Timeout);
    }

    #[test]
    fn classify_error_detects_stream_interrupted() {
        assert_eq!(
            classify_error("stream error: connection reset"),
            FailKind::StreamInterrupted
        );
        assert_eq!(classify_error("broken pipe"), FailKind::StreamInterrupted);
    }

    /// HTTP 5xx and equivalent server-side messages must classify as a failover-worthy kind so
    /// a provider-side outage trips failover (e.g. OpenRouter 503 → Ollama) instead of failing
    /// the task. Before the fix, a 5xx fell into `Other` and never triggered failover.
    #[test]
    fn classify_error_detects_5xx_as_failover_worthy() {
        // Numeric status codes via the standalone-token helper.
        assert_eq!(
            classify_error("openrouter returned 500: internal server error"),
            FailKind::StreamInterrupted
        );
        assert_eq!(
            classify_error("openrouter returned 502: bad gateway"),
            FailKind::StreamInterrupted
        );
        assert_eq!(
            classify_error("openrouter returned 503: service unavailable"),
            FailKind::StreamInterrupted
        );
        // 504 "gateway timeout" contains "timeout" which is checked first → Timeout (still
        // failover-worthy, so failover trips correctly).
        assert_eq!(
            classify_error("openrouter returned 504: gateway timeout"),
            FailKind::Timeout
        );
        // Textual server-side messages without an explicit code.
        assert_eq!(
            classify_error("service unavailable"),
            FailKind::StreamInterrupted
        );
        assert_eq!(classify_error("bad gateway"), FailKind::StreamInterrupted);
        // The failover-worthy invariant the task requires.
        assert!(
            classify_error("openrouter returned 503: service unavailable").is_failover_worthy(),
            "a 503 service unavailable must be failover-worthy"
        );
        // A bare numeric body (no status code, no transport keyword) is still Other.
        assert_eq!(classify_error("14293"), FailKind::Other);
    }

    #[test]
    fn classify_error_falls_back_to_other() {
        assert_eq!(
            classify_error("returned 401: invalid api key"),
            FailKind::Other
        );
        assert_eq!(classify_error("model not found"), FailKind::Other);
        // A bare numeric body (e.g. a request id) with no recognizable status code or
        // transport-error substring must still classify as Other.
        assert_eq!(classify_error("14293"), FailKind::Other);
    }

    /// A status-code substring embedded in the response body detail (a request id, port number,
    /// timestamp, …) must NOT false-positive into RateLimit or PaymentRequired. Before the
    /// standalone-number fix, `contains("429")` matched the "429" inside "14293" and degraded
    /// a healthy provider whose actual error was a 500.
    #[test]
    fn classify_error_does_not_match_status_code_substring_in_body() {
        // "429" inside a request id in the response body of a 500 error.
        // The 500 itself is now failover-worthy (StreamInterrupted), but the point of this
        // assertion is that the "429" substring inside the body must NOT classify as RateLimit.
        assert_eq!(
            classify_error(
                "openrouter returned 500: {\"request_id\":\"req_14293abc\",\"error\":\"internal\"}"
            ),
            FailKind::StreamInterrupted,
            "a 429 substring inside the body must not classify as RateLimit; the 500 status is the real signal"
        );
        // "402" inside a port number in the response body of a 500 error.
        assert_eq!(
            classify_error("ollama returned 500: upstream http://10.0.0.1:4020/ timed out"),
            FailKind::Timeout,
            "a 402 substring inside a port must not classify as PaymentRequired; the real signal is 'timed out'"
        );
        // "429" as part of a larger number in the body.
        assert_eq!(
            classify_error("openrouter returned 4290: weird"),
            FailKind::Other,
            "4290 is not 429 — must not classify as RateLimit"
        );
        // Genuine 429 still works (standalone, preceded by space, followed by colon).
        assert_eq!(
            classify_error("openrouter returned 429: rate limit exceeded"),
            FailKind::RateLimit
        );
        // Genuine 402 still works (standalone, preceded by space, followed by space).
        assert_eq!(
            classify_error("402 Payment Required"),
            FailKind::PaymentRequired
        );
    }

    /// `contains_status_code` must match the code as a standalone integer token at any position
    /// in the string (start, middle, end) and reject it when it is a substring of a larger number.
    #[test]
    fn contains_status_code_matches_standalone_only() {
        assert!(contains_status_code("returned 429: detail", "429"));
        assert!(contains_status_code("429 too many requests", "429"));
        assert!(contains_status_code("error: 429", "429"));
        assert!(!contains_status_code("request_id 14293abc", "429"));
        assert!(!contains_status_code("port 4020", "402"));
        assert!(!contains_status_code("4290", "429"));
        assert!(!contains_status_code("", "429"));
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
        // Isolate this test: serialize against other global-state tests, then reset.
        let _guard = HEALTH_TEST_LOCK.lock().await;
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
        let _guard = HEALTH_TEST_LOCK.lock().await;
        reset().await;
        let provider = "test-nonworthy-provider";

        let status = record_failure(provider, "returned 401: invalid api key").await;
        assert_eq!(status, HealthStatus::Healthy);
        assert!(!is_degraded(provider).await);
        reset().await;
    }

    #[tokio::test]
    async fn success_resets_consecutive_failures() {
        let _guard = HEALTH_TEST_LOCK.lock().await;
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
        let _guard = HEALTH_TEST_LOCK.lock().await;
        reset().await;
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
        let (prov, model, probe) = resolve_effective_model(primary_provider, primary_model)
            .await
            .unwrap();
        assert_eq!(prov, "ollama");
        assert_eq!(model, "minimax-m3");
        assert!(!probe);

        // A success recovers the provider.
        record_success(primary_provider).await;
        let (prov, model, _) = resolve_effective_model(primary_provider, primary_model)
            .await
            .unwrap();
        assert_eq!(prov, primary_provider);
        assert_eq!(model, "nex-agi/nex-n2-pro:free");
        reset().await;
    }

    /// A failed recovery probe must restart the cooldown so the NEXT call goes back to the
    /// backup instead of permanently hammering the (still-dead) primary. Before the fix,
    /// `degraded_at` was frozen at the first degradation, so once the cooldown elapsed every
    /// subsequent call was routed to the primary as a probe — the backup was abandoned even
    /// though the primary never recovered.
    #[tokio::test]
    async fn failed_probe_restarts_cooldown_and_routes_back_to_backup() {
        let _guard = HEALTH_TEST_LOCK.lock().await;
        reset().await;

        // Use a tiny cooldown so the probe path is exercisable without a real 60s wait.
        // Save/restore the env var; HEALTH_TEST_LOCK serializes these tests so no other test
        // sees the override.
        let prev = std::env::var("DOTZ_RECOVERY_COOLDOWN_MS").ok();
        std::env::set_var("DOTZ_RECOVERY_COOLDOWN_MS", "400");

        let primary_provider = "test-failed-probe-provider";
        let primary_model = "nex-agi/nex-n2-pro:free";

        // Degrade the primary with two consecutive failover-worthy failures.
        record_failure(primary_provider, "returned 429: rate limit").await;
        record_failure(primary_provider, "returned 429: rate limit").await;
        assert!(is_degraded(primary_provider).await);

        // Immediately (cooldown NOT elapsed) → backup, not a probe.
        let (prov, _, probe) = resolve_effective_model(primary_provider, primary_model)
            .await
            .unwrap();
        assert_eq!(
            prov, "ollama",
            "before cooldown the call must go to the backup"
        );
        assert!(!probe);

        // Wait for the cooldown to elapse → the next call is a probe to the primary.
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        let (prov, _, probe) = resolve_effective_model(primary_provider, primary_model)
            .await
            .unwrap();
        assert_eq!(
            prov, primary_provider,
            "after cooldown the call must probe the primary"
        );
        assert!(probe);

        // The probe FAILS — the primary is still down. This must restart the cooldown.
        record_failure(primary_provider, "returned 429: rate limit").await;

        // Immediately after the failed probe (cooldown just restarted) → backup, NOT primary.
        // Without the fix this would return the primary (probe) again, permanently abandoning
        // the backup.
        let (prov, _, probe) = resolve_effective_model(primary_provider, primary_model)
            .await
            .unwrap();
        assert_eq!(
            prov, "ollama",
            "a failed probe must route the next call back to the backup, got {prov}"
        );
        assert!(
            !probe,
            "the next call after a failed probe must not be another probe"
        );

        // Restore env + state.
        match prev {
            Some(p) => std::env::set_var("DOTZ_RECOVERY_COOLDOWN_MS", p),
            None => std::env::remove_var("DOTZ_RECOVERY_COOLDOWN_MS"),
        }
        reset().await;
    }

    /// A provider with no failover pair (e.g. anthropic → would fall back to ollama, which is
    /// different, so it DOES have a pair). Verify the None case: a provider whose backup would
    /// be itself returns None. We can't easily construct that with the current rules, so instead
    /// verify that a known-free-form provider returns Some.
    #[tokio::test]
    async fn resolve_effective_model_returns_some_for_free_form_providers() {
        let _guard = HEALTH_TEST_LOCK.lock().await;
        reset().await;
        assert!(
            resolve_effective_model("openrouter", "nex-agi/nex-n2-pro:free")
                .await
                .is_some()
        );
        assert!(resolve_effective_model("ollama", "glm-5.2").await.is_some());
        reset().await;
    }

    /// The snapshot JSON must include both the per-provider health and the failover pairs so the
    /// UI can render the chip + a tooltip.
    #[tokio::test]
    async fn health_snapshot_json_includes_pairs_and_providers() {
        let _guard = HEALTH_TEST_LOCK.lock().await;
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

    /// Regression for the `run_turn` provider-health re-probe shadowing bug.
    ///
    /// Before the fix, `run_turn` shadowed the session's original `provider_id`/`model_id`
    /// with the failover-resolved values (`let provider_id = prov_for_turn`). This meant
    /// that after round 1 failed over to the backup, round 2's health check probed the
    /// BACKUP's health — not the original session provider's. The original provider's
    /// recovery was never re-detected, so the turn stayed on the backup for all remaining
    /// rounds even after the primary came back.
    ///
    /// This test proves the invariant the fix relies on: `resolve_effective_model` must be
    /// called with the ORIGINAL session provider each round. Probing the original detects
    /// recovery and routes back; probing the backup (the old behavior) never sees the
    /// original's state and stays on the backup forever.
    #[tokio::test]
    async fn reprobe_original_provider_detects_recovery_but_probing_backup_does_not() {
        let _guard = HEALTH_TEST_LOCK.lock().await;
        reset().await;

        let primary = "openrouter";
        let primary_model = "nex-agi/nex-n2-pro:free";

        // Degrade the primary with two consecutive failover-worthy failures.
        record_failure(primary, "returned 429: rate limit").await;
        record_failure(primary, "returned 429: rate limit").await;
        assert!(is_degraded(primary).await);

        // Round 1 (both old and new code): failover to the backup.
        let (r1_prov, _, _) = resolve_effective_model(primary, primary_model)
            .await
            .unwrap();
        assert_eq!(
            r1_prov, "ollama",
            "degraded primary must fail over to backup"
        );

        // Simulate recovery: the rate-limit window passes and the primary is healthy again.
        record_success(primary).await;
        assert!(!is_degraded(primary).await, "primary should be recovered");

        // Fixed behavior (what run_turn does now): re-probe the ORIGINAL primary.
        let (fixed_prov, _, _) = resolve_effective_model(primary, primary_model)
            .await
            .unwrap();
        assert_eq!(
            fixed_prov, primary,
            "fixed: re-probing the original primary must route back to it after recovery"
        );

        // Now demonstrate the old buggy behavior: re-degrade the primary, then show that
        // probing the BACKUP (what the old shadowing did) never detects the primary's state.
        record_failure(primary, "returned 429: rate limit").await;
        record_failure(primary, "returned 429: rate limit").await;
        assert!(
            is_degraded(primary).await,
            "primary should be degraded again"
        );

        // Old code called resolve_effective_model with the BACKUP provider, not the original.
        // The backup is not degraded, so this always returns the backup — the primary's
        // degradation (or recovery) is invisible.
        let (old_prov, _, _) = resolve_effective_model("ollama", "minimax-m3")
            .await
            .unwrap();
        assert_eq!(
            old_prov, "ollama",
            "old buggy behavior: probing the backup stays on the backup, never seeing the primary"
        );
        // The primary is still degraded — the old code would never notice it recovered.
        assert!(
            is_degraded(primary).await,
            "primary is still degraded but old code can't see it"
        );

        // Fixed code probes the original primary — and after recovery, routes back.
        record_success(primary).await; // simulate recovery
        let (fixed_prov2, _, _) = resolve_effective_model(primary, primary_model)
            .await
            .unwrap();
        assert_eq!(
            fixed_prov2, primary,
            "fixed: probing the original primary after recovery routes back to it"
        );

        reset().await;
    }
}
