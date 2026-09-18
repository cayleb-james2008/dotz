//! Cognee adapter — optional graph-memory backend for dotz.
//!
//! Cognee (github.com/topoteretes/cognee, 28.7k stars, Apache-2.0) is a knowledge-graph +
//! vector + relational memory system with a REST API at `/api/v1/*` (`remember`, `recall`,
//! `improve`, `forget`). It builds entities + relationships via an LLM during `cognify`, giving
//! dotz a graph memory (semantic relationships) instead of just a vector similarity index.
//!
//! This adapter is OPTIONAL. When `DOTZ_COGNEE_URL` is unset, all functions return `None` and
//! `memory.rs` falls back to the built-in rusqlite + ort ONNX store (the on-device default).
//! When `DOTZ_COGNEE_URL` is set (e.g. `http://localhost:8000` for a self-hosted Docker
//! container, or `https://your-tenant.cognee.ai` for Cognee Cloud), recall + capture route to
//! Cognee; the local store stays as a write-through cache for offline operation.
//!
//! The adapter uses `reqwest` (already a dep) — no new heavy deps. The trust boundary is the
//! `DOTZ_COGNEE_URL` + `DOTZ_COGNEE_API_KEY` env vars (operator-configured, like provider keys).
//! The Cognee server runs with its own auth + its own LLM key (separate from dotz's providers).
//!
//! `subagent.rs` remains the sole emitter of `step_*` events; Cognee calls do NOT emit graph
//! events. `skills.rs` remains the single skill-discovery path; Cognee is a memory backend, not
//! a skill loader.

use serde::Deserialize;
use serde_json::{self, json};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Cognee config resolved from env. `None` when `DOTZ_COGNEE_URL` is unset/empty (Cognee disabled).
#[derive(Debug, Clone)]
pub struct CogneeConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub dataset: String,
    pub timeout_ms: u64,
}

impl CogneeConfig {
    /// Resolve from env. Returns `None` when Cognee is not configured (the default — the local
    /// rusqlite + ONNX store is the on-device memory backend).
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("DOTZ_COGNEE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        // Normalize: strip trailing slash so URL joins are clean.
        let base_url = base_url.trim_end_matches('/').to_string();
        // SSRF guard: reject non-https non-loopback URLs (same rule as the gateway provider +
        // MCP HTTP transport). A misconfigured Cognee URL pointing at an internal service
        // would exfiltrate memory contents (tool args + file reads captured into the graph).
        if let Err(e) = validate_url(&base_url) {
            eprintln!("cognee: disabled — invalid DOTZ_COGNEE_URL: {e}");
            return None;
        }
        let api_key = std::env::var("DOTZ_COGNEE_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let dataset = std::env::var("DOTZ_COGNEE_DATASET")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "dotz".to_string());
        let timeout_ms = std::env::var("DOTZ_COGNEE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&ms: &u64| (1000..=120_000).contains(&ms))
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Some(Self {
            base_url,
            api_key,
            dataset,
            timeout_ms,
        })
    }
}

/// Validate a Cognee URL. Accept `https://` anywhere, or `http://localhost`/`http://127.0.0.1`
/// for a self-hosted container. Reject other `http://` (SSRF guard).
fn validate_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1") {
        return Ok(());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("must start with http:// or https://, got: {url}"));
    }
    Err(format!(
        "non-https http:// URLs are rejected (SSRF guard); only http://localhost and http://127.0.0.1 are allowed. Got: {url}"
    ))
}

/// A recalled memory item from Cognee. Mirrors the subset of `RecallResponse` fields dotz uses.
/// Cognee returns a list of `RecallResponse` objects, each tagged with a `source` field
/// ("graph" | "session" | "trace" | "graph_context"). We extract the `text` (or `content` for
/// graph_context) + a score, and normalize to dotz's `MemoryView` shape upstream.
#[derive(Debug, Clone, Deserialize)]
pub struct CogneeRecallItem {
    pub text: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Is Cognee configured? (i.e. `DOTZ_COGNEE_URL` set + valid).
pub fn enabled() -> bool {
    CogneeConfig::from_env().is_some()
}

/// Recall memories for a query via Cognee's `/api/v1/recall` (or `/api/v1/search` alias).
/// Returns `None` when Cognee is disabled (caller falls back to the local store). Best-effort:
/// any HTTP/parse error yields `Some(Vec::new())` (Cognee reachable but returned nothing usable)
/// rather than propagating the error — memory recall is never fatal to a turn.
pub async fn recall(query: &str, cwd: Option<&str>, top_k: u32) -> Option<Vec<CogneeRecallItem>> {
    let cfg = CogneeConfig::from_env()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(cfg.timeout_ms))
        .build()
        .ok()?;
    let scope = scope_for_cwd(cwd);
    let body = json!({
        "query_text": query,
        "query_type": "GRAPH_COMPLETION",
        "datasets": [cfg.dataset],
        "top_k": top_k,
        "scope": scope,
    });
    let mut req = client
        .post(format!("{}/api/v1/recall", cfg.base_url))
        .json(&body)
        .header("Content-Type", "application/json");
    if let Some(key) = &cfg.api_key {
        req = req.header("X-Api-Key", key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        eprintln!(
            "cognee: recall returned {} (query: {:?})",
            resp.status(),
            truncate_str(query, 80)
        );
        return Some(Vec::new());
    }
    let items: Vec<CogneeRecallItem> = resp.json().await.ok()?;
    Some(items)
}

/// Remember (capture) a memory via Cognee's `/api/v1/remember` — runs in the background on the
/// Cognee side (`run_in_background: true`) because `cognify` is multi-LLM-call and not real-time.
/// Returns `None` when Cognee is disabled (caller falls back to the local capture path). The
/// memory text is sent as a string (Cognee accepts `data: str | list[str]` over the JSON body).
///
/// This is fire-and-forget from dotz's perspective — the recall path will see the new memory
/// after Cognee's background `cognify` completes (typically seconds). The local rusqlite store
/// also captures the same text so recall works immediately even before Cognee finishes.
pub async fn remember(text: &str, cwd: Option<&str>) -> Option<()> {
    let cfg = CogneeConfig::from_env()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(cfg.timeout_ms))
        .build()
        .ok()?;
    let scope = scope_for_cwd(cwd);
    let body = json!({
        "data": text,
        "datasetName": cfg.dataset,
        "run_in_background": true,
        "self_improvement": true,
        "session_id": scope,
    });
    let mut req = client
        .post(format!("{}/api/v1/remember", cfg.base_url))
        .json(&body)
        .header("Content-Type", "application/json");
    if let Some(key) = &cfg.api_key {
        req = req.header("X-Api-Key", key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        eprintln!(
            "cognee: remember returned {} (text: {:?})",
            resp.status(),
            truncate_str(text, 80)
        );
        return None;
    }
    Some(())
}

/// Forget all memories for a scope (project or global) via Cognee's `/api/v1/forget`. Used when
/// a project is purged. Returns `None` when Cognee is disabled.
pub async fn forget_scope(cwd: Option<&str>) -> Option<()> {
    let cfg = CogneeConfig::from_env()?;
    let scope = scope_for_cwd(cwd);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(cfg.timeout_ms))
        .build()
        .ok()?;
    let body = json!({
        "dataset": cfg.dataset,
        "session_id": scope,
    });
    let mut req = client
        .delete(format!("{}/api/v1/forget", cfg.base_url))
        .json(&body)
        .header("Content-Type", "application/json");
    if let Some(key) = &cfg.api_key {
        req = req.header("X-Api-Key", key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        eprintln!("cognee: forget returned {} (scope: {scope})", resp.status());
    }
    Some(())
}

/// Ping the Cognee server's `/api/v1/health` endpoint. Used by `/api/health` to surface whether
/// Cognee is reachable. Returns `None` when Cognee is disabled.
pub async fn health() -> Option<bool> {
    let cfg = CogneeConfig::from_env()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(2_000))
        .build()
        .ok()?;
    let mut req = client.get(format!("{}/api/v1/health", cfg.base_url));
    if let Some(key) = &cfg.api_key {
        req = req.header("X-Api-Key", key);
    }
    let resp = req.send().await.ok()?;
    Some(resp.status().is_success())
}

/// Map a dotz cwd to a Cognee session_id (the scope partition). Mirrors `memory::scope_user`:
/// global / no-cwd → `__global__`; project → `proj:<norm cwd>`.
fn scope_for_cwd(cwd: Option<&str>) -> String {
    match cwd {
        None => "__global__".to_string(),
        Some(c) => {
            let norm = c.replace('\\', "/");
            let norm = norm.trim_end_matches('/').to_string();
            let norm = if cfg!(windows) {
                norm.to_lowercase()
            } else {
                norm
            };
            format!("proj:{norm}")
        }
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate `DOTZ_COGNEE_*` env vars so they don't race each other under
    /// `--test-threads=2`. Each test holds this lock for its whole body (save → mutate → assert
    /// → restore). A module-local lock is correct here because no other module mutates these
    /// env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// When `DOTZ_COGNEE_URL` is unset, `enabled()` is false and `from_env` returns None — the
    /// local rusqlite + ONNX store is the default, and Cognee is an opt-in backend.
    #[test]
    fn disabled_by_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_URL") };
        assert!(
            !enabled(),
            "Cognee must be disabled when DOTZ_COGNEE_URL is unset"
        );
        assert!(CogneeConfig::from_env().is_none());
        if let Some(p) = prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) };
        }
    }

    /// A valid https URL is accepted by the SSRF guard.
    #[test]
    fn validate_accepts_https() {
        assert!(validate_url("https://your-tenant.cognee.ai").is_ok());
        assert!(validate_url("https://api.example.com").is_ok());
    }

    /// http://localhost and http://127.0.0.1 are accepted (self-hosted Docker).
    #[test]
    fn validate_accepts_loopback_http() {
        assert!(validate_url("http://localhost:8000").is_ok());
        assert!(validate_url("http://127.0.0.1:8000").is_ok());
    }

    /// A non-loopback http:// URL is rejected (SSRF guard — prevents exfiltrating memory
    /// contents to an internal service via a misconfigured URL).
    #[test]
    fn validate_rejects_non_loopback_http() {
        assert!(validate_url("http://internal.svc:8000").is_err());
        assert!(validate_url("http://169.254.169.254").is_err()); // metadata endpoint
    }

    /// A non-http(s) URL is rejected.
    #[test]
    fn validate_rejects_non_http() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("not a url").is_err());
    }

    /// The trailing slash is stripped from the base URL so URL joins are clean.
    #[test]
    fn from_env_strips_trailing_slash() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_URL", "http://localhost:8000/") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(cfg.base_url, "http://localhost:8000");
        if let Some(p) = prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) };
        }
    }

    /// The default dataset is "dotz"; `DOTZ_COGNEE_DATASET` overrides it.
    #[test]
    fn from_env_default_dataset_is_dotz() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_url = std::env::var("DOTZ_COGNEE_URL").ok();
        let prev_ds = std::env::var("DOTZ_COGNEE_DATASET").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_URL", "http://localhost:8000") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_DATASET") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(cfg.dataset, "dotz");
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_DATASET", "my-project") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(cfg.dataset, "my-project");
        match prev_url {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_URL") },
        }
        match prev_ds {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_DATASET", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_DATASET") },
        }
    }

    /// The scope partition maps a cwd to a Cognee session_id. Global/None → `__global__`;
    /// project → `proj:<norm cwd>` (backslashes → forward slashes, lowercased on Windows,
    /// trailing slash stripped — mirrors `memory::scope_user`).
    #[test]
    fn scope_for_cwd_partitions_correctly() {
        assert_eq!(scope_for_cwd(None), "__global__");
        assert_eq!(scope_for_cwd(Some("/foo/bar")), "proj:/foo/bar");
        assert_eq!(scope_for_cwd(Some("/foo/bar/")), "proj:/foo/bar");
        if cfg!(windows) {
            assert_eq!(scope_for_cwd(Some("C:\\foo\\bar")), "proj:c:/foo/bar");
        }
    }

    /// The recall function returns None when Cognee is disabled (the caller falls back to the
    /// local store). This is the no-op-when-disabled contract.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recall_returns_none_when_disabled() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_URL") };
        assert!(recall("test", None, 5).await.is_none());
        if let Some(p) = prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) };
        }
    }

    /// The remember function returns None when Cognee is disabled.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn remember_returns_none_when_disabled() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_URL") };
        assert!(remember("test fact", None).await.is_none());
        if let Some(p) = prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) };
        }
    }

    /// The health function returns None when Cognee is disabled.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn health_returns_none_when_disabled() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_URL") };
        assert!(health().await.is_none());
        if let Some(p) = prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) };
        }
    }

    /// A `CogneeRecallItem` with `text` parses correctly (the common graph/session case).
    #[test]
    fn recall_item_parses_text() {
        let raw = serde_json::json!({
            "text": "dotz uses Rust + Tauri for native perf",
            "score": 0.92,
            "source": "graph",
            "kind": "Entity"
        });
        let item: CogneeRecallItem = serde_json::from_value(raw).unwrap();
        assert_eq!(
            item.text.as_deref(),
            Some("dotz uses Rust + Tauri for native perf")
        );
        assert!((item.score.unwrap() - 0.92).abs() < 1e-6);
        assert_eq!(item.source.as_deref(), Some("graph"));
    }

    /// A `CogneeRecallItem` with `content` (the graph_context case) parses correctly.
    #[test]
    fn recall_item_parses_content() {
        let raw = serde_json::json!({
            "content": "dotz → uses → Rust + Tauri",
            "source": "graph_context"
        });
        let item: CogneeRecallItem = serde_json::from_value(raw).unwrap();
        assert_eq!(item.text, None);
        assert_eq!(item.content.as_deref(), Some("dotz → uses → Rust + Tauri"));
        assert_eq!(item.source.as_deref(), Some("graph_context"));
    }

    /// A `CogneeRecallItem` with missing fields parses (defaults to None).
    #[test]
    fn recall_item_parses_missing_fields() {
        let raw = serde_json::json!({});
        let item: CogneeRecallItem = serde_json::from_value(raw).unwrap();
        assert!(item.text.is_none());
        assert!(item.content.is_none());
        assert!(item.score.is_none());
        assert!(item.source.is_none());
        assert!(item.kind.is_none());
    }

    /// The API key is read from `DOTZ_COGNEE_API_KEY` (optional — self-hosted Cognee may not
    /// require auth; Cognee Cloud does).
    #[test]
    fn from_env_reads_api_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_url = std::env::var("DOTZ_COGNEE_URL").ok();
        let prev_key = std::env::var("DOTZ_COGNEE_API_KEY").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_URL", "http://localhost:8000") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_COGNEE_API_KEY") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert!(
            cfg.api_key.is_none(),
            "api_key should be None when env unset"
        );
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_API_KEY", "ck_test_key_123") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(cfg.api_key.as_deref(), Some("ck_test_key_123"));
        match prev_url {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_URL") },
        }
        match prev_key {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_API_KEY", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_API_KEY") },
        }
    }

    /// An invalid URL (non-http non-https) disables Cognee (from_env returns None) rather than
    /// constructing a client that would fail on every call.
    #[test]
    fn from_env_disables_on_invalid_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("DOTZ_COGNEE_URL").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_URL", "ftp://bad") };
        assert!(
            CogneeConfig::from_env().is_none(),
            "invalid URL should disable Cognee"
        );
        assert!(!enabled());
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_URL") },
        }
    }

    /// The timeout is clamped to [1000, 120000] ms; out-of-range values fall back to the default.
    #[test]
    fn from_env_clamps_timeout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_url = std::env::var("DOTZ_COGNEE_URL").ok();
        let prev_to = std::env::var("DOTZ_COGNEE_TIMEOUT_MS").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_URL", "http://localhost:8000") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_TIMEOUT_MS", "100") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(
            cfg.timeout_ms, DEFAULT_TIMEOUT_MS,
            "too-small timeout should fall back"
        );
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_TIMEOUT_MS", "999999") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(
            cfg.timeout_ms, DEFAULT_TIMEOUT_MS,
            "too-large timeout should fall back"
        );
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_COGNEE_TIMEOUT_MS", "5000") };
        let cfg = CogneeConfig::from_env().expect("should parse");
        assert_eq!(cfg.timeout_ms, 5000, "in-range timeout should be honored");
        match prev_url {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_URL", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_URL") },
        }
        match prev_to {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_COGNEE_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_COGNEE_TIMEOUT_MS") },
        }
    }

    /// The truncate helper caps at the max length with an ellipsis.
    #[test]
    fn truncate_str_caps_with_ellipsis() {
        assert_eq!(truncate_str("short", 10), "short");
        assert_eq!(truncate_str("exactly 10", 10), "exactly 10");
        assert_eq!(truncate_str("this is too long", 10), "this is to…");
    }
}
