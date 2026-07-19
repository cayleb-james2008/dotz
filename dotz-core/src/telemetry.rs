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
use axum::{http::HeaderMap, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

// ---- config -----------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
    /// Shared receiver token for standalone-receiver mode (see `router_with_token`). Sent as the
    /// `x-dotz-telemetry-token` header on every event POST when non-empty. Empty = no token
    /// (the in-app loopback receiver). `serde(default)` keeps pre-token `telemetry.json` files
    /// loading unchanged.
    #[serde(default)]
    pub token: String,
}

impl Default for TelemetryConfig {
    /// Default is OFF with no endpoint — the only safe default for telemetry.
    fn default() -> Self {
        TelemetryConfig {
            enabled: false,
            endpoint: String::new(),
            token: String::new(),
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
            if let Some(t) = v.get("token").and_then(|x| x.as_str()) {
                cfg.token = t.to_string();
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
/// endpoint (and its paired receiver token) when turning OFF so a later
/// re-enable does not silently resume sending to a stale endpoint (operator
/// must re-confirm the endpoint).
pub fn set_enabled(enabled: bool) {
    let mut cfg = load_config();
    cfg.enabled = enabled;
    if !enabled {
        cfg.endpoint = String::new();
        cfg.token = String::new();
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

/// Public: set the shared receiver token sent with every event POST (standalone-receiver mode).
/// Empty clears it. Persisted immediately, mirroring `set_endpoint`.
pub fn set_token(token: impl Into<String>) {
    let mut cfg = load_config();
    cfg.token = token.into();
    if let Err(e) = save_config(&cfg) {
        eprintln!("telemetry: failed to persist token: {e}");
    }
}

/// The app's own local receiver endpoint for the server bound on `port` — the guaranteed-working
/// default sink (`/telemetry/ingest` is merged into the main axum app, see `router`).
pub fn local_ingest_endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}/telemetry/ingest")
}

/// Can `endpoint` answer HTTP at all? ANY response counts as reachable — a GET on the POST-only
/// ingest route yields 405, which still proves a live receiver without polluting the sink — and
/// only connect/DNS/timeout failures count as unreachable. 2 s ceiling so a settings-panel
/// refresh never hangs the UI. Empty endpoints are unreachable by definition.
pub async fn endpoint_reachable(endpoint: &str) -> bool {
    if endpoint.trim().is_empty() {
        return false;
    }
    reqwest::Client::new()
        .get(endpoint)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// Enable telemetry AND guarantee a collectable sink — the enabled-implies-working-endpoint
/// invariant. An empty or unreachable endpoint is replaced with this app's own local receiver
/// (`local_ingest_endpoint`), so one settings click always yields a working send -> receive loop
/// instead of a silently dropped event stream (the pre-fix failure mode: enabled + dead endpoint
/// collected nothing, invisibly). A REACHABLE custom endpoint (e.g. the operator's standalone
/// receiver) is left untouched. Returns the resulting persisted config for the caller's UI.
pub async fn enable_with_working_endpoint(local_port: u16) -> TelemetryConfig {
    set_enabled(true);
    let cfg = load_config();
    if !endpoint_reachable(&cfg.endpoint).await {
        set_endpoint(local_ingest_endpoint(local_port));
    }
    load_config()
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
    let mut req = reqwest::Client::new()
        .post(&cfg.endpoint)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5));
    // Standalone-receiver mode: attach the shared token so a non-loopback receiver
    // (`router_with_token`) accepts the event. Empty = the tokenless in-app receiver.
    if !cfg.token.trim().is_empty() {
        req = req.header(TOKEN_HEADER, cfg.token.trim());
    }
    let _ = req.send().await;
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

// ---- daily-active day gate --------------------------------------------------
//
// The daily-active half of the weekly-active metric must fire at most once per
// calendar day per install. The launcher (src-tauri/main.rs) calls this on every
// app launch; the once-per-day gate lives here so the day-boundary logic is unit
// testable in dotz-core (main.rs is a thin Tauri shell that is not).

/// `{ "lastActiveDay": <int> }` — the last UTC day index a `DailyActive` was emitted
/// for. Sits next to the other telemetry files under `~/.dotz/` (or `DOTZ_CONFIG_DIR`).
fn active_file() -> PathBuf {
    dotz_dir().join("telemetry_active.json")
}

/// UTC day index (days since the Unix epoch) for a millis-epoch timestamp. Pure so the
/// day-boundary logic is testable without the wall clock. `div_euclid` keeps the pre-epoch
/// (negative) case monotonic and non-panicking, matching `util::now_ms`'s never-panic contract.
///
/// A UTC (not local) boundary is a deliberate simplicity choice: the weekly-active metric only
/// needs roughly one heartbeat per install per calendar day, and a local-midnight boundary would
/// require a timezone dependency (ponytail: no heavy deps). Worst case an install that is only
/// ever used within a few hours either side of UTC midnight is counted on the "wrong" day — which
/// does not affect the distinct-active-installs-per-week count the metric actually reports.
fn utc_day_index(now_ms: i64) -> i64 {
    now_ms.div_euclid(86_400_000)
}

/// Read the last recorded day index, or `None` if never recorded / the file is missing or
/// corrupt (which simply means "record today"). Never panics.
fn load_last_active_day() -> Option<i64> {
    let raw = std::fs::read_to_string(active_file()).ok()?;
    let v = serde_json::from_str::<Value>(&raw).ok()?;
    v.get("lastActiveDay").and_then(|x| x.as_i64())
}

/// Persist the last recorded day index, best-effort. A failed write just means the next launch
/// re-records the same day (a duplicate heartbeat) — never a crash.
fn save_last_active_day(day: i64) {
    let _ = std::fs::create_dir_all(dotz_dir());
    if let Ok(json) = serde_json::to_string_pretty(&json!({ "lastActiveDay": day })) {
        let _ = std::fs::write(active_file(), json);
    }
}

/// The pure day gate: returns `true` and advances the persisted marker to `today` iff `today`
/// differs from the last recorded day; returns `false` (no write) if today was already recorded.
/// Deliberately independent of the wall clock (caller injects `today`) so the once-per-day
/// boundary is deterministically testable. The marker is only advanced when this returns `true`,
/// so a disabled → enabled transition that short-circuits before calling this still fires today.
fn claim_day_if_new(today: i64) -> bool {
    if load_last_active_day() == Some(today) {
        return false;
    }
    save_last_active_day(today);
    true
}

/// Record a `DailyActive` event at most once per UTC calendar day. Silent no-op when telemetry is
/// disabled — checked first, before the day marker is touched, so opting in mid-day still produces
/// a heartbeat for that day rather than being swallowed by an already-advanced marker. The launcher
/// spawns this on every app launch alongside `record_app_launch`.
pub async fn record_daily_active_if_new_day() {
    if !is_enabled() {
        return;
    }
    if claim_day_if_new(utc_day_index(util::now_ms())) {
        record_daily_active().await;
    }
}

// ---- receiver ---------------------------------------------------------------
//
// A minimal sink so the send -> receive path is end-to-end: the opt-in emitter POSTs to this
// route and the operator reads the collected events out of a local JSONL file. Two hostings:
//
//   * In-app (loopback): `router()` merged into the main axum app, inheriting the origin/Host
//     allowlist guard (loopback / no-Origin only). Deliberately NOT under `/api/` so the app's
//     own emitter can loop back without carrying the per-process session token.
//   * Standalone (`telemetry receive` bin): `router_with_token(Some(..))` served on an
//     operator-chosen non-loopback addr WITHOUT the origin guard (external installs carry a
//     non-loopback Host), gated instead by a long-lived shared token — this is what makes the
//     external-user half of the weekly-active metric collectable at all.

/// The append-only JSONL sink the receiver writes to (`~/.dotz/telemetry_sink.jsonl`,
/// `DOTZ_CONFIG_DIR` honored). Public so the `telemetry` bin + docs name the same path.
pub fn sink_file() -> PathBuf {
    dotz_dir().join("telemetry_sink.jsonl")
}

/// Append one PII-free event object as a JSON line to the local sink. Best-effort I/O errors are
/// surfaced to the caller (which maps them to `{ ok: false }`) — the receiver never panics.
fn append_to_sink(event: &Value) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dotz_dir())?;
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sink_file())?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// The request header carrying the shared receiver token in standalone-receiver mode. Distinct
/// from the per-process session token header (`x-dotz-token`, `server::guard::TOKEN_HEADER`):
/// this one is a long-lived shared secret the operator hands to each opted-in external install.
pub const TOKEN_HEADER: &str = "x-dotz-telemetry-token";

/// The receiver's router with an optional shared-token gate. `None` = the tokenless in-app
/// loopback receiver. `Some(token)` = standalone mode: a missing/wrong `x-dotz-telemetry-token`
/// header gets `401` and never touches the sink (constant-time compare via `guard::token_ok`,
/// same as the session token). POST `/telemetry/ingest` appends the posted event verbatim to the
/// JSONL sink and returns `{ ok }` — the emitter only ever sends the fixed PII-free schema
/// (`test_no_pii_in_event_payload` pins it), so no field filtering is needed here.
pub fn router_with_token(token: Option<String>) -> Router<()> {
    Router::new().route(
        "/telemetry/ingest",
        post(move |headers: HeaderMap, Json(event): Json<Value>| {
            let token = token.clone();
            async move {
                let provided = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
                if !crate::server::guard::token_ok(token.as_deref(), provided) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "ok": false, "error": "missing or invalid telemetry token" })),
                    );
                }
                match append_to_sink(&event) {
                    Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
                    Err(e) => {
                        eprintln!("telemetry receiver: failed to append event to sink: {e}");
                        (StatusCode::OK, Json(json!({ "ok": false })))
                    }
                }
            }
        }),
    )
}

/// The tokenless in-app receiver, merged into the main axum app (see `server::app_with_token`).
/// Stateless `Router<()>`, matching the other cold modules' `router()` convention.
pub fn router() -> Router<()> {
    router_with_token(None)
}

// ---- weekly-active aggregation ----------------------------------------------
//
// The fleet metric is "distinct opted-in installs active per UTC ISO week" (target: 5 external
// weekly-active users). The sink stores one JSON line per event ({eventType, sessionId, ts, ..});
// aggregation is pure over that text so the `telemetry weekly` subcommand and tests share it.
// No chrono: two textbook civil-date helpers (Howard Hinnant's algorithms) are all ISO-8601
// week numbering needs (ponytail: no calendar dep for one label format).

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian (Hinnant `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) -> days since 1970-01-01 (Hinnant `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO-8601 week label (e.g. "2026-W29") for a UTC millis-epoch timestamp. ISO weeks run
/// Mon–Sun and belong to the year of their Thursday, so computing the containing Thursday's
/// calendar date and its week-of-year index gives the label directly. Pinned against Python
/// `date.isocalendar()` anchors in `test_iso_week_matches_isocalendar_anchors`.
pub fn iso_week(ts_ms: i64) -> String {
    let day = ts_ms.div_euclid(86_400_000);
    let dow = (day + 3).rem_euclid(7); // Monday=0 … Sunday=6 (1970-01-01 was a Thursday)
    let thursday = day - dow + 3;
    let (y, _, _) = civil_from_days(thursday);
    let week = (thursday - days_from_civil(y, 1, 1)) / 7 + 1;
    format!("{y}-W{week:02}")
}

/// Aggregate sink JSONL into per-ISO-week rows `(week, distinct installs, events)`, week-sorted.
/// "Distinct installs" = distinct `sessionId` (the stable per-install telemetry id). Lines that
/// are not JSON or lack `sessionId`/`ts` are skipped — a truncated tail line from a killed
/// receiver must not sink the whole report.
pub fn weekly_active(jsonl: &str) -> Vec<(String, usize, usize)> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut weeks: BTreeMap<String, (BTreeSet<String>, usize)> = BTreeMap::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let (Some(id), Some(ts)) = (
            v.get("sessionId").and_then(|x| x.as_str()),
            v.get("ts").and_then(|x| x.as_i64()),
        ) else {
            continue;
        };
        let entry = weeks.entry(iso_week(ts)).or_default();
        entry.0.insert(id.to_string());
        entry.1 += 1;
    }
    weeks
        .into_iter()
        .map(|(w, (ids, n))| (w, ids.len(), n))
        .collect()
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

    // Plain #[test] driving a multi-thread runtime inside `with_tmp_dir` rather than
    // #[tokio::test] + `with_tmp_dir(|_| async {..}).await`: the latter builds the future,
    // then `with_tmp_dir` restores DOTZ_CONFIG_DIR and drops its LOCK *before* the body is
    // awaited, so the enabled-path config writes race the process-global env var against every
    // other test in the binary (observed as an intermittent "sink received Null"). Blocking on
    // the runtime inside the closure keeps the LOCK + env override held for the whole body.
    #[test]
    fn test_record_event_when_enabled_posts() {
        use axum::{routing::post, Router};

        with_tmp_dir(|_| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
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
            });
        });
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

    /// The UTC day index is pure and monotonic: the epoch and everything in its first 24h maps to
    /// day 0, the first millisecond of the next UTC day rolls to day 1, and a pre-epoch clock stays
    /// non-panicking (floored, not truncated-toward-zero) so `now_ms()==0` degrades cleanly.
    #[test]
    fn test_utc_day_index_rolls_on_day_boundary() {
        assert_eq!(utc_day_index(0), 0, "epoch is day 0");
        assert_eq!(
            utc_day_index(86_400_000 - 1),
            0,
            "last ms of day 0 is still day 0"
        );
        assert_eq!(utc_day_index(86_400_000), 1, "first ms of day 1 rolls over");
        assert_eq!(utc_day_index(86_400_000 + 1), 1);
        assert_eq!(utc_day_index(2 * 86_400_000), 2);
        // div_euclid keeps a pre-epoch (negative) millis floored, not truncated toward zero.
        assert_eq!(
            utc_day_index(-1),
            -1,
            "one ms before epoch is day -1, not 0"
        );
    }

    /// The once-per-day gate: `claim_day_if_new` fires exactly once per distinct day and advances
    /// only when it fires. Same day twice -> one claim; a new day -> a fresh claim. This is the
    /// deterministic proof that `DailyActive` is recorded once per day boundary.
    #[test]
    fn test_daily_active_claimed_once_per_day_boundary() {
        with_tmp_dir(|dir| {
            // No marker yet -> the first claim for a day succeeds and persists the marker.
            assert!(!dir.join("telemetry_active.json").exists());
            assert!(claim_day_if_new(20_000), "first claim of a day must fire");
            assert!(
                dir.join("telemetry_active.json").exists(),
                "a successful claim must persist the day marker"
            );
            assert_eq!(load_last_active_day(), Some(20_000));

            // Same day again -> no second claim (the daily heartbeat is idempotent per day).
            assert!(!claim_day_if_new(20_000), "same day must not fire twice");
            assert_eq!(load_last_active_day(), Some(20_000), "marker unchanged");

            // A new day boundary -> a fresh claim, and the marker advances.
            assert!(claim_day_if_new(20_001), "a new day must fire");
            assert_eq!(load_last_active_day(), Some(20_001));
            assert!(
                !claim_day_if_new(20_001),
                "the new day is now also idempotent"
            );
        });
    }

    /// `record_daily_active_if_new_day` must be a total no-op when telemetry is disabled: no event
    /// sent, no sink written, no session id minted, AND the day marker must NOT advance — so that a
    /// later opt-in on the same day still produces that day's heartbeat instead of being swallowed.
    #[test]
    fn test_daily_active_is_noop_when_disabled() {
        with_tmp_dir(|dir| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(record_daily_active_if_new_day());

            assert!(!is_enabled(), "default must be OFF");
            assert!(
                !dir.join("telemetry_active.json").exists(),
                "disabled telemetry must not advance the day marker"
            );
            assert!(
                !dir.join("telemetry_id.json").exists(),
                "disabled telemetry must not mint a session id"
            );
            assert!(
                !dir.join("telemetry_sink.jsonl").exists(),
                "disabled telemetry must not write the sink"
            );
        });
    }

    /// The local receiver route must accept a POSTed telemetry event and append it to the JSONL
    /// sink — proving the send -> receive path end-to-end. We drive it through the *real* emitter
    /// (`record_daily_active`) pointed at the receiver served from `telemetry::router()`, so this
    /// also proves an opted-in `DailyActive` flows all the way to disk.
    #[test]
    fn test_receiver_appends_posted_event() {
        with_tmp_dir(|dir| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // Stand up the actual receiver router on a loopback ephemeral port.
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, router()).await.unwrap();
                });

                // Opt in and point the emitter at the local receiver.
                set_endpoint(format!("http://127.0.0.1:{}/telemetry/ingest", addr.port()));
                set_enabled(true);
                assert!(is_enabled());

                // Send a real DailyActive event through record_event's send path.
                record_daily_active().await;

                // Wait for the append to land, then read the sink back.
                let sink = dir.join("telemetry_sink.jsonl");
                let mut lines: Vec<String> = Vec::new();
                for _ in 0..40 {
                    if let Ok(raw) = std::fs::read_to_string(&sink) {
                        lines = raw.lines().map(str::to_string).collect();
                        if !lines.is_empty() {
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                server.abort();

                assert_eq!(
                    lines.len(),
                    1,
                    "receiver must append exactly one JSONL line"
                );
                let got: Value = serde_json::from_str(&lines[0]).expect("sink line must be JSON");
                assert_eq!(
                    got["eventType"],
                    json!("dailyActive"),
                    "event type persisted"
                );
                assert_eq!(
                    got["sessionId"].as_str().unwrap_or("").len(),
                    36,
                    "sessionId must be a persisted uuid: {got}"
                );
                assert!(
                    got["ts"].as_i64().unwrap_or(0) > 1_577_836_800_000,
                    "ts must be a plausible recent millis epoch: {got}"
                );
                // PII-free invariant on the wire: only the fixed key set, no command/path leak.
                let keys: Vec<&str> = got
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                for k in &keys {
                    assert!(
                        matches!(*k, "eventType" | "sessionId" | "ts"),
                        "unexpected key in received dailyActive event: {k}"
                    );
                }
            });
        });
    }

    /// set_enabled(false) must also clear the endpoint (and its paired receiver token) so a
    /// later re-enable does not silently resume sending to a stale address.
    #[test]
    fn test_set_enabled_false_clears_endpoint() {
        with_tmp_dir(|_| {
            set_endpoint("http://example.invalid/fleet");
            set_token("shared-secret");
            assert!(load_config().endpoint.contains("example.invalid"));
            assert_eq!(load_config().token, "shared-secret");

            set_enabled(false);
            let cfg = load_config();
            assert!(!cfg.enabled, "must be off");
            assert!(
                cfg.endpoint.is_empty(),
                "turning off must clear the endpoint, got {:?}",
                cfg.endpoint
            );
            assert!(
                cfg.token.is_empty(),
                "turning off must clear the receiver token, got {:?}",
                cfg.token
            );
        });
    }

    /// The enabled-implies-working-endpoint invariant: enabling with an endpoint that is empty or
    /// unreachable must fall back to the app's own local receiver, and the resulting endpoint must
    /// actually collect — a real event sent through `record_event` lands in the sink. This is the
    /// regression pin for the audit's "enabled but pointed at a dead endpoint, silent failure".
    #[test]
    fn test_enable_with_unreachable_endpoint_defaults_to_local_receiver() {
        with_tmp_dir(|dir| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // The app's "own" receiver: the real router on a loopback ephemeral port.
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let local_port = listener.local_addr().unwrap().port();
                let server = tokio::spawn(async move {
                    axum::serve(listener, router()).await.unwrap();
                });

                // A guaranteed-dead endpoint: bind an ephemeral port, then drop the listener so
                // connecting to it is refused (no other process can have grabbed it mid-test
                // reliably enough to matter for loopback).
                let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let dead_port = dead.local_addr().unwrap().port();
                drop(dead);
                set_endpoint(format!("http://127.0.0.1:{dead_port}/telemetry/ingest"));
                assert!(
                    !endpoint_reachable(&load_config().endpoint).await,
                    "the dropped port must probe unreachable"
                );

                // Enabling must repair the endpoint to the local receiver…
                let cfg = enable_with_working_endpoint(local_port).await;
                assert!(cfg.enabled, "must be enabled");
                assert_eq!(
                    cfg.endpoint,
                    local_ingest_endpoint(local_port),
                    "unreachable endpoint must be replaced with the local receiver"
                );
                assert!(
                    endpoint_reachable(&cfg.endpoint).await,
                    "enabled implies a REACHABLE endpoint"
                );

                // …and the repaired endpoint must actually collect end-to-end.
                record_event(TelemetryEvent::AppLaunch).await;
                let sink = dir.join("telemetry_sink.jsonl");
                let mut got = String::new();
                for _ in 0..40 {
                    if let Ok(raw) = std::fs::read_to_string(&sink) {
                        if !raw.trim().is_empty() {
                            got = raw;
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                server.abort();
                assert!(
                    got.contains("appLaunch"),
                    "the repaired endpoint must collect the event: sink = {got:?}"
                );
            });
        });
    }

    /// Enabling with a REACHABLE custom endpoint must keep it — the invariant repairs dead sinks,
    /// it does not stomp an operator-configured standalone receiver.
    #[test]
    fn test_enable_keeps_reachable_custom_endpoint() {
        with_tmp_dir(|_| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // A live "custom" receiver on its own port…
                let custom = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let custom_port = custom.local_addr().unwrap().port();
                let server = tokio::spawn(async move {
                    axum::serve(custom, router()).await.unwrap();
                });
                let custom_ep = format!("http://127.0.0.1:{custom_port}/telemetry/ingest");
                set_endpoint(&custom_ep);

                // …must survive enable_with_working_endpoint aimed at a DIFFERENT local port.
                let cfg = enable_with_working_endpoint(custom_port.wrapping_add(1)).await;
                server.abort();
                assert_eq!(
                    cfg.endpoint, custom_ep,
                    "a reachable custom endpoint must not be replaced"
                );
            });
        });
    }

    /// Standalone-receiver token gate: with a token configured, a POST without (or with a wrong)
    /// `x-dotz-telemetry-token` header gets 401 and never touches the sink; the real emitter with
    /// the matching token in its config gets through and the event lands.
    #[test]
    fn test_receiver_token_rejects_missing_and_accepts_matching() {
        with_tmp_dir(|dir| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, router_with_token(Some("s3cret".into())))
                        .await
                        .unwrap();
                });
                let url = format!("http://127.0.0.1:{}/telemetry/ingest", addr.port());
                let sink = dir.join("telemetry_sink.jsonl");
                let client = reqwest::Client::new();

                // No token -> 401, nothing written.
                let r = client
                    .post(&url)
                    .json(&json!({"probe": 1}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 401, "missing token must be rejected");
                // Wrong token -> 401 too.
                let r = client
                    .post(&url)
                    .header(TOKEN_HEADER, "wrong")
                    .json(&json!({"probe": 2}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 401, "wrong token must be rejected");
                assert!(!sink.exists(), "rejected posts must never touch the sink");

                // The real emitter, configured with the matching shared token -> lands.
                set_endpoint(&url);
                set_token("s3cret");
                set_enabled(true);
                record_daily_active().await;
                let mut got = String::new();
                for _ in 0..40 {
                    if let Ok(raw) = std::fs::read_to_string(&sink) {
                        if !raw.trim().is_empty() {
                            got = raw;
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                server.abort();
                let lines: Vec<&str> = got.lines().collect();
                assert_eq!(
                    lines.len(),
                    1,
                    "exactly the authorized event lands: {got:?}"
                );
                assert!(
                    got.contains("dailyActive"),
                    "authorized event persisted: {got:?}"
                );
            });
        });
    }

    /// Pin the ISO-week labels against `datetime.date.isocalendar()` anchors (verified with
    /// CPython 3.x): year-boundary weeks are where hand-rolled week math dies, so every anchor
    /// here is a boundary case except the mid-year sanity row.
    #[test]
    fn test_iso_week_matches_isocalendar_anchors() {
        // (y, m, d, expected label) — ts is the UTC midnight of that date.
        let anchors = [
            (1970, 1, 1, "1970-W01"),   // epoch, a Thursday
            (2016, 1, 1, "2015-W53"),   // Friday belonging to the PREVIOUS iso year
            (2021, 1, 1, "2020-W53"),   // same shape, leap-adjacent
            (2024, 12, 30, "2025-W01"), // Monday belonging to the NEXT iso year
            (2025, 12, 29, "2026-W01"), // Monday starting 2026-W01
            (2026, 1, 1, "2026-W01"),   // Thursday anchor day itself
            (2026, 7, 17, "2026-W29"),  // mid-year sanity (today, at authoring time)
            (2026, 12, 28, "2026-W53"), // Monday of a 53-week iso year
        ];
        for (y, m, d, want) in anchors {
            let ts = days_from_civil(y, m, d) * 86_400_000;
            assert_eq!(iso_week(ts), want, "{y}-{m:02}-{d:02}");
            // Last millisecond of the same UTC day must stay in the same week.
            assert_eq!(
                iso_week(ts + 86_399_999),
                want,
                "{y}-{m:02}-{d:02} 23:59:59.999"
            );
        }
        // The civil-date helpers must be inverses around the anchors.
        for (y, m, d, _) in anchors {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    /// The aggregator counts DISTINCT session ids per ISO week (the weekly-active metric),
    /// tolerates garbage lines, and keys strictly off `ts`+`sessionId`.
    #[test]
    fn test_weekly_active_counts_distinct_ids_per_week() {
        let wk1 = days_from_civil(2026, 7, 13) * 86_400_000; // Monday of 2026-W29
        let wk2 = days_from_civil(2026, 7, 20) * 86_400_000; // Monday of 2026-W30
        let line = |id: &str, ts: i64| {
            format!(r#"{{"eventType":"dailyActive","sessionId":"{id}","ts":{ts}}}"#)
        };
        let jsonl = [
            line("aaa", wk1),
            line("aaa", wk1 + 86_400_000), // same install again in wk1 -> still 1 distinct
            line("bbb", wk1 + 2 * 86_400_000),
            line("bbb", wk2), // same install active NEXT week counts there too
            line("ccc", wk2),
            "not json at all".to_string(),        // must be skipped
            r#"{"eventType":"x","ts":1}"#.into(), // no sessionId -> skipped
            r#"{"sessionId":"zzz"}"#.into(),      // no ts -> skipped
        ]
        .join("\n");

        let rows = weekly_active(&jsonl);
        assert_eq!(
            rows,
            vec![
                ("2026-W29".to_string(), 2, 3), // aaa+bbb distinct, 3 events
                ("2026-W30".to_string(), 2, 2), // bbb+ccc distinct, 2 events
            ]
        );
    }
}
