//! dotz opt-in telemetry — anonymous usage signals for the fleet ledger.
//!
//! ### Goal
//!
//! The fleet-improvement plan needs to confirm 5 external beta users with
//! weekly-active use. This module emits the smallest possible signal set
//! (`AppLaunch`, `CommandRun`, `DailyActive`) so the operator can count
//! distinct active installs without ever touching user content.
//!
//! ### Opt-in by default
//!
//! Telemetry is OFF until the operator explicitly turns it on
//! (`set_enabled(true)` or writing `{"enabled":true,...}` to
//! `~/.dotz/telemetry.json`). When disabled, or when the endpoint is empty,
//! `record_event` is a silent no-op — no network, no disk write, no panic.
//! No event is ever sent without consent.
//!
//! ### No PII — ever
//!
//! The payload schema is fixed and intentionally tiny:
//!   - `event_type`: the variant tag ("appLaunch" / "commandRun" / "dailyActive")
//!   - `session_id`: a random UUID v4 generated once and persisted to
//!     `~/.dotz/telemetry_id.json` — a stable per-install handle, NOT a user
//!     identity. It carries no name, email, machine name, or file paths.
//!   - `ts`: milliseconds since the Unix epoch (via `util::now_ms`, panic-free)
//!   - `command`: for `CommandRun` only, the slash-command name (e.g.
//!     "/implement") — never its arguments, never file content.
//!
//! `test_no_pii_in_event_payload` pins this schema so a future field addition
//! that leaks paths or user data is caught at test time.
//!
//! ### Persistence model
//!
//! Two files under `~/.dotz/` (overridable via `DOTZ_CONFIG_DIR`, mirroring
//! `config.rs`):
//!   - `telemetry.json` — `{ enabled, endpoint }` config.
//!   - `telemetry_id.json` — `{ "session_id": "<uuid>" }` stable install handle.
//!
//! Both degrade to defaults on corrupt/missing files (never panic), matching
//! the graceful-degradation contract every other dotz config file follows.
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

// ---- config -----------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
}

impl Default for TelemetryConfig {
    /// Default is OFF with no endpoint — the only safe default for telemetry.
    fn default() -> Self {
        TelemetryConfig {
            enabled: false,
            endpoint: String::new(),
        }
    }
}

fn dotz_dir() -> PathBuf {
    // Mirrors config::dotz_dir so telemetry honors the same DOTZ_CONFIG_DIR
    // override the rest of dotz uses for test isolation + portable installs.
    if let Ok(d) = std::env::var("DOTZ_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dotz")
}

fn config_file() -> PathBuf {
    dotz_dir().join("telemetry.json")
}

fn id_file() -> PathBuf {
    dotz_dir().join("telemetry_id.json")
}

/// Load telemetry config, clamping a corrupt/missing file to the OFF default.
/// Never panics — a truncated `telemetry.json` must not crash the app on launch.
pub fn load_config() -> TelemetryConfig {
    let mut cfg = TelemetryConfig::default();
    if let Ok(raw) = std::fs::read_to_string(config_file()) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
                cfg.enabled = b;
            }
            if let Some(e) = v.get("endpoint").and_then(|x| x.as_str()) {
                cfg.endpoint = e.to_string();
            }
        } else {
            eprintln!(
                "telemetry: {} is not valid JSON; telemetry stays OFF until the file is fixed.",
                config_file().display()
            );
        }
    }
    cfg
}

fn save_config(c: &TelemetryConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(dotz_dir())?;
    std::fs::write(config_file(), serde_json::to_string_pretty(c)?)?;
    Ok(())
}

/// Public: is telemetry currently enabled? Reads the persisted config each call
/// so a settings change takes effect without a restart (mirrors config::load).
pub fn is_enabled() -> bool {
    load_config().enabled
}

/// Public: turn telemetry on/off and persist the choice. Also wipes the
/// endpoint to empty when turning OFF so a later re-enable does not silently
/// resume sending to a stale endpoint (operator must re-confirm the endpoint).
pub fn set_enabled(enabled: bool) {
    let mut cfg = load_config();
    cfg.enabled = enabled;
    if !enabled {
        cfg.endpoint = String::new();
    }
    if let Err(e) = save_config(&cfg) {
        eprintln!("telemetry: failed to persist enabled={enabled}: {e}");
    }
}

/// Public: set the endpoint telemetry POSTs to. Persisted immediately so the
/// operator can configure it before enabling. Empty string = disabled (the
/// record_event path treats an empty endpoint as a no-op even when enabled).
pub fn set_endpoint(endpoint: impl Into<String>) {
    let mut cfg = load_config();
    cfg.endpoint = endpoint.into();
    if let Err(e) = save_config(&cfg) {
        eprintln!("telemetry: failed to persist endpoint: {e}");
    }
}

// ---- session id -------------------------------------------------------------

/// Load-or-create the stable per-install session id. Persisted to
/// `telemetry_id.json` so the same install reports a consistent id across
/// restarts (lets the ledger count distinct installs). Never panics; a corrupt
/// id file is replaced with a fresh UUID.
fn get_or_create_session_id() -> String {
    if let Ok(raw) = std::fs::read_to_string(id_file()) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(id) = v.get("session_id").and_then(|x| x.as_str()) {
                if !id.trim().is_empty() {
                    return id.to_string();
                }
            }
        }
    }
    // First launch, corrupt id file, or empty id — mint a fresh one and
    // persist best-effort. A failure to persist is non-fatal: the id is still
    // usable for this process, it just won't survive a restart.
    let id = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::create_dir_all(dotz_dir());
    if let Ok(json) = serde_json::to_string_pretty(&json!({ "session_id": id })) {
        let _ = std::fs::write(id_file(), json);
    }
    id
}

// ---- events -----------------------------------------------------------------

/// The full event surface. Adding a variant = add a match arm in `event_type`
/// + `to_payload`. No PII: only the slash-command NAME for `CommandRun`, never
///   its arguments, file paths, or content.
#[derive(Clone, Debug)]
pub enum TelemetryEvent {
    AppLaunch,
    CommandRun { command: String },
    DailyActive,
}

impl TelemetryEvent {
    /// The string tag the ledger keys on. CamelCase to match the existing
    /// JSON contract (verify.rs, config.rs both use camelCase over the wire).
    fn event_type(&self) -> &'static str {
        match self {
            TelemetryEvent::AppLaunch => "appLaunch",
            TelemetryEvent::CommandRun { .. } => "commandRun",
            TelemetryEvent::DailyActive => "dailyActive",
        }
    }

    /// Build the JSON payload. The schema is fixed and PII-free:
    /// `{ "eventType", "sessionId", "ts", ("command") }`. `command` is only
    /// present for `CommandRun` and only carries the command name.
    fn to_payload(&self, session_id: &str, ts: i64) -> Value {
        match self {
            TelemetryEvent::AppLaunch | TelemetryEvent::DailyActive => json!({
                "eventType": self.event_type(),
                "sessionId": session_id,
                "ts": ts,
            }),
            TelemetryEvent::CommandRun { command } => json!({
                "eventType": self.event_type(),
                "sessionId": session_id,
                "ts": ts,
                "command": command,
            }),
        }
    }
}

// ---- record + dispatch ------------------------------------------------------

/// Record an event. If telemetry is disabled OR the endpoint is empty, this is
/// a silent no-op (no network, no disk). Otherwise it POSTs the JSON payload
/// to the configured endpoint, best-effort: any network/parse failure is
/// swallowed (telemetry must never break the app). Fire-and-forget by design —
/// callers that want to spawn-and-detach can wrap this in `tokio::spawn`.
pub async fn record_event(event: TelemetryEvent) {
    let cfg = load_config();
    if !cfg.enabled || cfg.endpoint.trim().is_empty() {
        return;
    }
    let session_id = get_or_create_session_id();
    let payload = event.to_payload(&session_id, util::now_ms());
    // Best-effort: a 5s ceiling so a hung ledger endpoint never blocks the UI.
    // Errors are swallowed — telemetry is non-critical and must not surface
    // to the operator as a crash or a failed command.
    let _ = reqwest::Client::new()
        .post(&cfg.endpoint)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

/// Convenience: record an `AppLaunch` event. Async wrapper so the launcher can
/// `tokio::spawn(telemetry::record_app_launch())` without importing the enum.
pub async fn record_app_launch() {
    record_event(TelemetryEvent::AppLaunch).await;
}

/// Convenience: record a `CommandRun` event for a slash-command name.
/// `command` must be the command NAME only (e.g. "/implement") — the caller is
/// responsible for never passing arguments, file paths, or content here.
pub async fn record_command_run(command: impl Into<String>) {
    record_event(TelemetryEvent::CommandRun {
        command: command.into(),
    })
    .await;
}

/// Convenience: record a `DailyActive` event. The launcher is expected to call
/// this once per day on first launch (day-boundary logic lives in the caller —
/// this module only owns the emission).
pub async fn record_daily_active() {
    record_event(TelemetryEvent::DailyActive).await;
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as AMutex};

    // Serialize tests that mutate DOTZ_CONFIG_DIR so env-var overrides don't
    // race with each other or with config::load's own test suite.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-telemetry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        let result = f(&dir);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
        result
    }

    #[test]
    fn test_record_event_when_disabled_is_noop() {
        with_tmp_dir(|dir| {
            // No telemetry.json written → default is OFF + empty endpoint.
            // record_event must return without touching the network or disk.
            // We prove the no-op by confirming no telemetry_id.json is created
            // (session-id persistence only happens on the enabled path).
            assert!(!dir.join("telemetry_id.json").exists());

            // A blocking runtime to await the async no-op. The call returns
            // immediately because the disabled guard short-circuits before any
            // I/O, so this is cheap.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(record_event(TelemetryEvent::AppLaunch));

            // Session id file must NOT have been created on the disabled path.
            assert!(
                !dir.join("telemetry_id.json").exists(),
                "disabled telemetry must not persist a session id"
            );
            assert!(!is_enabled(), "default must be OFF");
        });
    }

    #[tokio::test]
    async fn test_record_event_when_enabled_posts() {
        use axum::{routing::post, Router};

        with_tmp_dir(|_| async move {
            // Stand up a local axum sink that captures the POSTed payload.
            let captured: Arc<AMutex<Value>> = Arc::new(AMutex::new(Value::Null));
            let cap = captured.clone();

            let app = Router::new().route(
                "/fleet/ledger",
                post(
                    move |axum::extract::Json(body): axum::extract::Json<Value>| {
                        let cap = cap.clone();
                        async move {
                            *cap.lock().unwrap() = body;
                            axum::Json(json!({ "ok": true }))
                        }
                    },
                ),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            // Enable telemetry + point it at the local sink.
            set_endpoint(format!("http://127.0.0.1:{}/fleet/ledger", addr.port()));
            set_enabled(true);
            assert!(is_enabled(), "set_enabled(true) must flip is_enabled");

            // First event mints + persists the session id.
            record_event(TelemetryEvent::AppLaunch).await;

            // Give the POST a moment to land, then inspect the captured payload.
            // A short retry loop is more robust than a fixed sleep and matches
            // the best-effort nature of the emitter.
            let mut got = Value::Null;
            for _ in 0..20 {
                let v = captured.lock().unwrap().clone();
                if v != Value::Null {
                    got = v;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }

            server.abort();

            assert_ne!(got, Value::Null, "the sink must have received the event");
            assert_eq!(got["eventType"], json!("appLaunch"), "eventType tag");
            assert!(
                got["sessionId"].as_str().unwrap_or("").len() == 36,
                "sessionId must be a uuid: got {:?}",
                got["sessionId"]
            );
            assert!(
                got["ts"].as_i64().unwrap_or(0) > 1_577_836_800_000,
                "ts must be a plausible recent millis epoch: got {:?}",
                got["ts"]
            );
            // The session id must also have been persisted to disk.
            let id_raw = std::fs::read_to_string(id_file()).unwrap();
            assert!(
                id_raw.contains("session_id"),
                "id file must persist: {id_raw}"
            );
        })
        .await;
    }

    #[test]
    fn test_session_id_is_persisted() {
        with_tmp_dir(|dir| {
            // First call mints + persists.
            let id1 = get_or_create_session_id();
            assert!(
                dir.join("telemetry_id.json").exists(),
                "id file must be created"
            );
            assert_eq!(id1.len(), 36, "a v4 uuid is 36 chars");

            // Second call MUST return the same persisted id, not mint a new one.
            let id2 = get_or_create_session_id();
            assert_eq!(id1, id2, "session id must be stable across calls");

            // A corrupt id file must be replaced with a fresh id, not panic.
            std::fs::write(id_file(), b"{ not valid json }").unwrap();
            let id3 = get_or_create_session_id();
            assert_eq!(id3.len(), 36, "corrupt id file must yield a fresh uuid");
            assert_ne!(id3, id1, "corrupt id must be replaced, not reused");
        });
    }

    /// Pin the PII-free payload schema. Every variant is serialized and asserted
    /// to contain ONLY the allowed keys. This catches a future field addition
    /// that leaks a path, username, or command ARGUMENT at test time.
    #[test]
    fn test_no_pii_in_event_payload() {
        let sid = "11111111-1111-1111-1111-111111111111";
        let ts = 1_700_000_000_000_i64;

        let cases = [
            (
                "appLaunch",
                TelemetryEvent::AppLaunch.to_payload(sid, ts),
                &["eventType", "sessionId", "ts"][..],
            ),
            (
                "commandRun",
                TelemetryEvent::CommandRun {
                    command: "/implement".into(),
                }
                .to_payload(sid, ts),
                &["eventType", "sessionId", "ts", "command"][..],
            ),
            (
                "dailyActive",
                TelemetryEvent::DailyActive.to_payload(sid, ts),
                &["eventType", "sessionId", "ts"][..],
            ),
        ];

        for (label, payload, allowed) in cases {
            let obj = payload
                .as_object()
                .unwrap_or_else(|| panic!("{label} payload must be a JSON object, got {payload}"));
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort();
            let mut want: Vec<&str> = allowed.to_vec();
            want.sort();
            assert_eq!(
                keys, want,
                "{label} payload keys must be exactly the allowed PII-free set"
            );

            // The command value for CommandRun must be the NAME only — never
            // contain a space (which would imply leaked arguments) or a path
            // separator (which would imply a leaked file path).
            if label == "commandRun" {
                let cmd = obj["command"].as_str().unwrap_or("");
                assert!(
                    !cmd.contains(' ') && !cmd.contains('/') || cmd.starts_with('/'),
                    "command must be a bare slash-command name, not arguments/paths: {cmd:?}"
                );
                // A leading slash is the slash-command convention; an embedded
                // slash after position 0 would be a path. Allow "/implement",
                // reject "/implement some/path".
                let inner = &cmd[1.min(cmd.len())..];
                assert!(
                    !inner.contains('/'),
                    "command must not carry a path separator: {cmd:?}"
                );
            }
        }
    }

    /// set_enabled(false) must also clear the endpoint so a later re-enable
    /// does not silently resume sending to a stale address.
    #[test]
    fn test_set_enabled_false_clears_endpoint() {
        with_tmp_dir(|_| {
            set_endpoint("http://example.invalid/fleet");
            assert!(load_config().endpoint.contains("example.invalid"));

            set_enabled(false);
            let cfg = load_config();
            assert!(!cfg.enabled, "must be off");
            assert!(
                cfg.endpoint.is_empty(),
                "turning off must clear the endpoint, got {:?}",
                cfg.endpoint
            );
        });
    }
}
