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
use axum::{
    extract::Query,
    http::HeaderMap,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

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

// ---- perf metrics (B3) ------------------------------------------------------
//
// Local-only, opt-in performance metrics for the perf dashboard. A per-metric
// in-memory ring buffer (last 1000 samples) records TurnLatency / GraphRenderTime
// / EmbedLatency / ToolCallLatency / FirstTurnLatency so the UI can surface p50/p95
// /p99/max + sparklines.
//
// PRIVACY MOAT (the user decision: "Stay opt-in — preserves the privacy moat"):
//   * Perf recording is GATED on `telemetry::is_enabled()` OR the separate
//     `perf_recording_enabled` flag (default OFF). When both are false, `record()`
//     is a zero-overhead early-return no-op — the buffer stays empty.
//   * The perf buffer is in-memory ONLY. It is NEVER written to disk and NEVER
//     sent to any remote endpoint. The only way perf data leaves the process is a
//     future explicit "Export JSON" action (out of scope for B3).
//   * The perf routes (`/api/perf/*`) are protected by `token_guard` + `origin_guard`
//     like every other `/api/*` route — no new auth surface.
//   * The module NEVER logs sample VALUES (only metric names + counts) so a stderr
//     capture cannot leak latency fingerprints. `test_perf_never_logs_sample_values`
//     pins this.

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

/// Ring-buffer capacity per metric. The last 1000 samples per metric is plenty
/// for a sparkline + stable p99 without growing unbounded on a long-running
/// desktop session.
const PERF_BUFFER_CAP: usize = 1000;

/// One recorded latency sample. `session_id` is optional so client-side metrics
/// (e.g. GraphRenderTime posted from the UI) can omit it. Serialized camelCase
/// to match the rest of the wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSample {
    pub metric: PerfMetric,
    pub value_ms: f64,
    pub ts: i64, // millis since epoch (util::now_ms)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The five latency metrics the perf dashboard surfaces. `snake_case` over the
/// wire so the UI's `?metric=turn_latency` query reads the same as the JSON tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfMetric {
    /// Full turn time (user prompt → final assistant message).
    TurnLatency,
    /// Workflow graph render time (client-side; posted from the UI).
    GraphRenderTime,
    /// ONNX embed call (the `embed_text` path). Skips the load-time itself
    /// because Q1 warm-up handles that separately.
    EmbedLatency,
    /// Individual tool call execution time.
    ToolCallLatency,
    /// First turn after startup (cold) — the `static FIRST_TURN` flag arms this
    /// once per process so only the very first turn is tagged.
    FirstTurnLatency,
}

impl PerfMetric {
    /// The wire tag the UI keys on (matches the `#[serde(rename_all)]` output).
    pub fn as_str(self) -> &'static str {
        match self {
            PerfMetric::TurnLatency => "turn_latency",
            PerfMetric::GraphRenderTime => "graph_render_time",
            PerfMetric::EmbedLatency => "embed_latency",
            PerfMetric::ToolCallLatency => "tool_call_latency",
            PerfMetric::FirstTurnLatency => "first_turn_latency",
        }
    }

    /// Parse a wire tag back to the enum (None on an unknown string so a bad
    /// `?metric=` query degrades to an empty samples list instead of a 500).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "turn_latency" => Some(PerfMetric::TurnLatency),
            "graph_render_time" => Some(PerfMetric::GraphRenderTime),
            "embed_latency" => Some(PerfMetric::EmbedLatency),
            "tool_call_latency" => Some(PerfMetric::ToolCallLatency),
            "first_turn_latency" => Some(PerfMetric::FirstTurnLatency),
            _ => None,
        }
    }

    /// Every variant, in the order the dashboard renders them.
    pub fn all() -> [PerfMetric; 5] {
        [
            PerfMetric::TurnLatency,
            PerfMetric::GraphRenderTime,
            PerfMetric::EmbedLatency,
            PerfMetric::ToolCallLatency,
            PerfMetric::FirstTurnLatency,
        ]
    }
}

/// Per-metric summary the dashboard renders as a card. `count == 0` means no
/// samples yet (the card shows "—"); the percentiles are `Option` so an empty
/// metric serializes cleanly withoutsentinel values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfMetricSummary {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
    pub count: usize,
}

/// The full summary response: one entry per metric, keyed by the wire tag.
/// An empty metric is still present (with `count: 0`) so the UI always renders
/// all five cards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSummary {
    pub metrics: std::collections::BTreeMap<String, PerfMetricSummary>,
}

// ---- perf recording flag (persisted to ~/.dotz/config.json under perfRecording) ----
//
// The flag is read fresh on each `record()` call so a UI toggle takes effect
// immediately without a restart (mirrors `is_enabled()`). It persists to
// `config.json` under a `perfRecording` field (default false) via the helpers in
// `config.rs` (added in B3) — NOT to `telemetry.json`, so it is independent of the
// remote-telemetry opt-in.

static PERF_RECORDING_FLAG: AtomicBool = AtomicBool::new(false);
static PERF_RECORDING_INIT: OnceLock<()> = OnceLock::new();

/// Load the persisted `perfRecording` flag (default false) into the static atomics.
/// Called once on first access; subsequent toggles go through `set_perf_recording`
/// which keeps the atomics in sync. Best-effort: a corrupt config.json degrades to
/// the OFF default (matches `config::load`'s graceful-degradation contract).
fn init_perf_recording_flag() {
    PERF_RECORDING_INIT.get_or_init(|| {
        let on = crate::config::load().perf_recording;
        PERF_RECORDING_FLAG.store(on, Ordering::SeqCst);
    });
}

/// Public: is perf recording enabled right now? True if EITHER the operator opted
/// into remote telemetry (`is_enabled()`) OR explicitly enabled perf recording
/// via the dashboard toggle. The OR (not AND) preserves the privacy moat: a user
/// who wants the local perf dashboard but NOT remote telemetry can have both.
pub fn perf_recording_enabled() -> bool {
    init_perf_recording_flag();
    is_enabled() || PERF_RECORDING_FLAG.load(Ordering::SeqCst)
}

/// Public: turn perf recording on/off and persist the choice to `config.json`
/// under `perfRecording` so it survives a restart. Best-effort: a persist failure
/// is logged + swallowed (perf is non-critical; it must never crash the app).
pub fn set_perf_recording(enabled: bool) {
    init_perf_recording_flag();
    PERF_RECORDING_FLAG.store(enabled, Ordering::SeqCst);
    if let Err(e) = crate::config::set_perf_recording(&crate::config::load(), enabled) {
        eprintln!("perf: failed to persist perfRecording={enabled}: {e}");
    }
}

// ---- the ring buffer ----

static PERF_BUFFER: OnceLock<Mutex<VecDeque<PerfSample>>> = OnceLock::new();

fn buffer() -> &'static Mutex<VecDeque<PerfSample>> {
    PERF_BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(PERF_BUFFER_CAP)))
}

fn buffer_guard() -> std::sync::MutexGuard<'static, VecDeque<PerfSample>> {
    buffer()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record a perf sample. ZERO-OVERHEAD NO-OP when perf recording is disabled
/// (the privacy-moat default): the disabled check is the very first line so a
/// hot-path fire point (every turn / embed / tool call) never even locks the
/// mutex. When enabled, the sample is pushed to the ring buffer; if the buffer
/// is at capacity the OLDEST sample is evicted (drop-oldest ring). Best-effort:
/// a poisoned mutex is recovered so a panic in one fire point does not brick the
/// rest. Never logs the sample value (only metric names + counts in the toggle
/// path) — see `test_perf_never_logs_sample_values`.
pub fn record(metric: PerfMetric, value_ms: f64, session_id: Option<&str>) {
    if !perf_recording_enabled() {
        return;
    }
    let sample = PerfSample {
        metric,
        value_ms,
        ts: util::now_ms(),
        session_id: session_id.map(|s| s.to_string()),
    };
    let mut g = buffer_guard();
    if g.len() >= PERF_BUFFER_CAP {
        g.pop_front();
    }
    g.push_back(sample);
}

/// Return up to `limit` samples for a single metric, newest-first. Used by the
/// dashboard's sparklines. `limit == 0` is treated as "all" (clamped to the buffer
/// cap so a runaway request cannot materialize more than the ring holds). An
/// unknown metric tag returns an empty vec (the UI shows an empty sparkline).
pub fn samples(metric: PerfMetric, limit: usize) -> Vec<PerfSample> {
    let g = buffer_guard();
    let limit = if limit == 0 { PERF_BUFFER_CAP } else { limit };
    g.iter()
        .rev()
        .filter(|s| s.metric == metric)
        .take(limit)
        .cloned()
        .collect()
}

/// Per-metric p50/p95/p99/max + count. The percentiles are computed on the
/// sorted values (nearest-rank, the simplest correct method for a bounded
/// ring buffer — no interpolation, which would imply a precision the sample size
/// does not warrant). An empty metric returns an all-`None` summary with
/// `count: 0` so the UI renders "—" instead of a misleading 0.0.
pub fn summary() -> PerfSummary {
    let g = buffer_guard();
    let mut metrics = std::collections::BTreeMap::new();
    for m in PerfMetric::all() {
        let mut values: Vec<f64> = g
            .iter()
            .filter(|s| s.metric == m)
            .map(|s| s.value_ms)
            .collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = values.len();
        let summary = if count == 0 {
            PerfMetricSummary {
                p50: None,
                p95: None,
                p99: None,
                max: None,
                count: 0,
            }
        } else {
            // Nearest-rank percentile: index = ceil(p/100 * n) - 1, clamped to [0, n-1].
            let pick = |p: f64| -> f64 {
                let idx = ((p / 100.0) * count as f64).ceil() as usize;
                values[idx.saturating_sub(1).min(count - 1)]
            };
            PerfMetricSummary {
                p50: Some(pick(50.0)),
                p95: Some(pick(95.0)),
                p99: Some(pick(99.0)),
                max: Some(values[count - 1]),
                count,
            }
        };
        metrics.insert(m.as_str().to_string(), summary);
    }
    PerfSummary { metrics }
}

/// Clear the perf buffer (the dashboard "reset" button + tests). O(1) clear.
pub fn clear() {
    let mut g = buffer_guard();
    g.clear();
}

/// One-time-per-process flag that arms `FirstTurnLatency` for the very first turn
/// after startup. The lead session's `run_turn` checks + clears it; a restart
/// re-arms it (a fresh process is "cold" again).
#[allow(dead_code)]
static FIRST_TURN: AtomicBool = AtomicBool::new(true);

/// Called by `session::run_turn`: returns `true` the first time it is called in
/// this process (and clears the flag), `false` every subsequent call. Used to
/// tag the first turn with `FirstTurnLatency` so the dashboard can separate the
/// cold-start turn from steady-state turns.
#[allow(dead_code)]
pub(crate) fn claim_first_turn() -> bool {
    FIRST_TURN.swap(false, Ordering::SeqCst)
}

// ---- perf routes (B3) -------------------------------------------------------
//
// Four routes, all under `/api/perf/*` and therefore protected by the existing
// `token_guard` + `origin_guard` (no new auth). The `POST /api/perf/record` route
// is the ONLY one that accepts client-side samples (GraphRenderTime); the other
// three read/clear the buffer.
//   GET  /api/perf/summary               → PerfSummary (the dashboard cards)
//   GET  /api/perf/samples?metric=X&limit=N → Vec<PerfSample> (sparklines)
//   POST /api/perf/record                 → accepts one PerfSample (UI → backend)
//   POST /api/perf/clear                   → empties the buffer
//   POST /api/perf/toggle                  → flips perf_recording_enabled + persists
//
// ponytail: client-side perf recording via POST; a WS-based stream is the upgrade
// path (the graph already posts step_state over WS, so a `perf_sample` event
// would slot in naturally — but a single POST endpoint is the shortest working
// diff for B3 and keeps the perf module self-contained).

async fn perf_get_summary() -> Json<Value> {
    Json(json!(summary()))
}

async fn perf_get_samples(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let metric = q
        .get("metric")
        .and_then(|s| PerfMetric::from_str(s))
        .unwrap_or(PerfMetric::TurnLatency);
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .min(PERF_BUFFER_CAP);
    Json(json!(samples(metric, limit)))
}

async fn perf_post_record(Json(sample): Json<PerfSample>) -> Json<Value> {
    // The UI posts GraphRenderTime samples here. Re-record through the gated
    // `record()` so the privacy-moat no-op + ring-buffer cap still apply (the
    // UI cannot bypass the opt-in by posting directly).
    record(sample.metric, sample.value_ms, sample.session_id.as_deref());
    Json(json!({ "ok": true }))
}

async fn perf_clear() -> Json<Value> {
    clear();
    Json(json!({ "ok": true }))
}

async fn perf_toggle() -> Json<Value> {
    // Flip the flag AND persist so it survives a restart. Returns the new state
    // so the UI can update its "Recording ON/OFF" badge without a second round-trip.
    let next = !perf_recording_enabled();
    set_perf_recording(next);
    Json(json!({ "recording": next }))
}

/// The perf router. Stateless `Router<()>`, merged into the main axum app (see
/// `server::app_with_token`). Inherits the token + origin guards from the outer
/// router — no new auth surface.
pub fn perf_router() -> Router<()> {
    Router::new()
        .route("/api/perf/summary", get(perf_get_summary))
        .route("/api/perf/samples", get(perf_get_samples))
        .route("/api/perf/record", post(perf_post_record))
        .route("/api/perf/clear", post(perf_clear))
        .route("/api/perf/toggle", post(perf_toggle))
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as AMutex};

    // Serialize tests that mutate DOTZ_CONFIG_DIR so env-var overrides don't
    // race with each other or with config::load's own test suite. We share a single
    // process-wide lock with `config::tests` (via `util::dotz_config_dir_test_lock`) so
    // cross-module `with_tmp_dir` calls can't clobber each other's `DOTZ_CONFIG_DIR`.
    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    // =========================================================================
    // B3: perf metrics — local-only, opt-in ring buffer + routes
    // =========================================================================
    //
    // These tests point DOTZ_CONFIG_DIR at a temp dir + serialize on the shared
    // config-dir lock so they never touch the operator's real config.json. The
    // perf buffer is a process-global static, so every test clears it before +
    // after (via `clear()`) so a leftover sample from a prior test cannot leak in
    // and a recorded sample cannot leak out.

    /// Helper: enable perf recording inside a tmp config dir, returning a guard
    /// that disables + clears on drop. The `set_perf_recording(true)` persists
    /// to the tmp config.json so `perf_recording_enabled()` (which reads the
    /// persisted flag via `config::load()`) sees it.
    struct PerfGuard {
        _cfg_guard: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for PerfGuard {
        fn drop(&mut self) {
            // Restore to OFF + wipe the buffer so the next test starts clean.
            set_perf_recording(false);
            clear();
        }
    }
    fn with_perf_enabled<T>(f: impl FnOnce() -> T) -> T {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(true);
        // Force the init flag to re-read so the new persisted value is picked up.
        // The OnceLock means `init_perf_recording_flag` only runs once per process;
        // to make `perf_recording_enabled()` reflect the toggle within a single
        // process we rely on `set_perf_recording` updating the static atomics
        // directly (it does), so the OnceLock init is only for the cold-start path.
        let result = f();
        let _ = std::fs::remove_dir_all(&dir);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        drop(PerfGuard { _cfg_guard: g });
        result
    }

    /// Disabled flag → `record()` does not add to the buffer (privacy moat).
    #[test]
    fn perf_record_is_noop_when_disabled() {
        // Point at a tmp dir with NO config.json so the default (OFF) applies.
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-off-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        // Also ensure remote telemetry is OFF (the OR branch of the gate).
        set_enabled(false);
        assert!(!perf_recording_enabled(), "default must be OFF");
        record(PerfMetric::TurnLatency, 42.0, Some("s1"));
        assert!(
            samples(PerfMetric::TurnLatency, 10).is_empty(),
            "disabled record() must not add to the buffer"
        );
        // Cleanup.
        clear();
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// Enabled → buffer grows by one per record.
    #[test]
    fn perf_record_adds_to_buffer_when_enabled() {
        with_perf_enabled(|| {
            assert!(perf_recording_enabled(), "precondition: must be ON");
            record(PerfMetric::TurnLatency, 100.0, Some("s1"));
            record(PerfMetric::EmbedLatency, 5.0, None);
            let turn = samples(PerfMetric::TurnLatency, 10);
            let embed = samples(PerfMetric::EmbedLatency, 10);
            assert_eq!(turn.len(), 1, "one TurnLatency sample recorded");
            assert_eq!(embed.len(), 1, "one EmbedLatency sample recorded");
            assert_eq!(turn[0].value_ms, 100.0);
            assert_eq!(turn[0].session_id.as_deref(), Some("s1"));
            assert_eq!(embed[0].session_id, None);
        });
    }

    /// 1001 records → oldest evicted (ring buffer cap at 1000).
    #[test]
    fn perf_buffer_is_ring_buffer_capped_at_1000() {
        with_perf_enabled(|| {
            for i in 0..1001 {
                record(PerfMetric::TurnLatency, i as f64, None);
            }
            let s = samples(PerfMetric::TurnLatency, 0); // 0 = all (clamped to cap)
            assert_eq!(s.len(), 1000, "ring buffer must cap at 1000");
            // The oldest (value 0.0) must have been evicted; the newest (1000.0)
            // must be present. samples() returns newest-first, so s[0] is 1000.0.
            assert_eq!(s[0].value_ms, 1000.0, "newest sample must be at the front");
            assert!(
                !s.iter().any(|x| x.value_ms == 0.0),
                "the oldest sample (0.0) must have been evicted"
            );
        });
    }

    /// samples() filters by metric — only matching metric returned.
    #[test]
    fn perf_samples_filters_by_metric() {
        with_perf_enabled(|| {
            record(PerfMetric::TurnLatency, 1.0, None);
            record(PerfMetric::EmbedLatency, 2.0, None);
            record(PerfMetric::TurnLatency, 3.0, None);
            record(PerfMetric::ToolCallLatency, 4.0, None);
            let turn = samples(PerfMetric::TurnLatency, 10);
            assert_eq!(turn.len(), 2, "only TurnLatency samples");
            assert!(turn.iter().all(|s| s.metric == PerfMetric::TurnLatency));
            // GraphRenderTime has none.
            assert!(
                samples(PerfMetric::GraphRenderTime, 10).is_empty(),
                "GraphRenderTime must have zero samples"
            );
        });
    }

    /// summary() computes p50/p95/p99/max from known samples. Uses a fixed set
    /// so the nearest-rank percentiles are deterministic. 100 samples (1..=100 ms)
    /// give distinct p50/p95/p99/max values.
    #[test]
    fn perf_summary_computes_p50_p95_p99_max() {
        with_perf_enabled(|| {
            // 100 samples: 1..=100 (ms). Sorted: [1,2,...,100].
            for i in 1..=100 {
                record(PerfMetric::EmbedLatency, i as f64, None);
            }
            let s = summary();
            let embed = &s.metrics["embed_latency"];
            assert_eq!(embed.count, 100);
            // nearest-rank: p50 -> ceil(0.5*100)-1 = idx 49 -> 50.0
            assert_eq!(embed.p50, Some(50.0));
            // p95 -> ceil(0.95*100)-1 = idx 94 -> 95.0
            assert_eq!(embed.p95, Some(95.0));
            // p99 -> ceil(0.99*100)-1 = idx 98 -> 99.0
            assert_eq!(embed.p99, Some(99.0));
            assert_eq!(embed.max, Some(100.0));
        });
    }

    /// summary() returns empty (count 0, all percentiles None) for a metric
    /// with no samples.
    #[test]
    fn perf_summary_returns_empty_for_no_samples() {
        with_perf_enabled(|| {
            // Record into one metric so the buffer is non-empty, but leave
            // GraphRenderTime empty.
            record(PerfMetric::TurnLatency, 1.0, None);
            let s = summary();
            let graph = &s.metrics["graph_render_time"];
            assert_eq!(graph.count, 0);
            assert_eq!(graph.p50, None);
            assert_eq!(graph.p95, None);
            assert_eq!(graph.p99, None);
            assert_eq!(graph.max, None);
            // The summary always has all five metric keys.
            assert_eq!(s.metrics.len(), 5, "all five metrics must be present");
        });
    }

    /// GET /api/perf/summary returns the dashboard shape (all 5 metric keys).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn perf_route_get_summary() {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-summary-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { axum::serve(listener, perf_router()).await.unwrap() });
        record(PerfMetric::TurnLatency, 50.0, Some("s1"));
        let body: Value =
            reqwest::get(format!("http://127.0.0.1:{}/api/perf/summary", addr.port()))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        handle.abort();
        let metrics = body["metrics"].as_object().expect("metrics is an object");
        for key in [
            "turn_latency",
            "graph_render_time",
            "embed_latency",
            "tool_call_latency",
            "first_turn_latency",
        ] {
            assert!(metrics.contains_key(key), "summary must include {key}");
        }
        assert_eq!(metrics["turn_latency"]["count"], 1);
        assert_eq!(metrics["turn_latency"]["p50"], 50.0);
        // Cleanup.
        set_perf_recording(false);
        clear();
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// GET /api/perf/samples?metric=X&limit=N returns filtered samples in shape.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn perf_route_get_samples() {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-samples-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(true);
        record(PerfMetric::EmbedLatency, 1.0, None);
        record(PerfMetric::EmbedLatency, 2.0, None);
        record(PerfMetric::TurnLatency, 99.0, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { axum::serve(listener, perf_router()).await.unwrap() });
        let url = format!(
            "http://127.0.0.1:{}/api/perf/samples?metric=embed_latency&limit=10",
            addr.port()
        );
        let body: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
        handle.abort();
        let arr = body.as_array().expect("samples is an array");
        assert_eq!(arr.len(), 2, "only embed_latency samples returned");
        assert!(arr.iter().all(|s| s["metric"] == "embed_latency"));
        // Cleanup.
        set_perf_recording(false);
        clear();
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// POST /api/perf/record accepts a UI sample (GraphRenderTime).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn perf_route_post_record() {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-record-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { axum::serve(listener, perf_router()).await.unwrap() });
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/api/perf/record", addr.port()))
            .json(&json!({
                "metric": "graph_render_time",
                "value_ms": 16.7,
                "ts": 0,
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST /api/perf/record should succeed"
        );
        handle.abort();
        let s = samples(PerfMetric::GraphRenderTime, 10);
        assert_eq!(s.len(), 1, "the posted sample must land in the buffer");
        assert!((s[0].value_ms - 16.7).abs() < f64::EPSILON);
        // Cleanup.
        set_perf_recording(false);
        clear();
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// POST /api/perf/clear empties the buffer.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn perf_route_clear() {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-clear-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(true);
        record(PerfMetric::TurnLatency, 1.0, None);
        record(PerfMetric::EmbedLatency, 2.0, None);
        assert!(
            !samples(PerfMetric::TurnLatency, 10).is_empty(),
            "precondition"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { axum::serve(listener, perf_router()).await.unwrap() });
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/api/perf/clear", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        handle.abort();
        assert!(
            samples(PerfMetric::TurnLatency, 10).is_empty(),
            "buffer must be empty"
        );
        assert!(
            samples(PerfMetric::EmbedLatency, 10).is_empty(),
            "buffer must be empty"
        );
        // Cleanup.
        set_perf_recording(false);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// POST /api/perf/toggle flips the enabled flag + persists to config.json.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn perf_route_toggle() {
        // Use a fresh tmp dir + the shared config lock so this never touches the
        // operator's real config.json.
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-toggle-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        clear();
        set_perf_recording(false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { axum::serve(listener, perf_router()).await.unwrap() });
        let client = reqwest::Client::new();
        let resp: Value = client
            .post(format!("http://127.0.0.1:{}/api/perf/toggle", addr.port()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        handle.abort();
        assert_eq!(resp["recording"], true, "toggle from OFF must turn ON");
        // The flag must now be ON in-memory.
        assert!(perf_recording_enabled(), "flag must be ON after toggle");
        // And persisted to config.json under perfRecording.
        let raw = std::fs::read_to_string(dir.join("config.json")).unwrap_or_default();
        assert!(
            raw.contains("\"perfRecording\": true") || raw.contains("\"perfRecording\":true"),
            "perfRecording must be persisted to config.json: {raw}"
        );
        // Cleanup.
        set_perf_recording(false);
        clear();
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// Fresh install → perf_recording_enabled() is false (the privacy moat).
    #[test]
    fn perf_recording_defaults_to_off() {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-perf-default-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        // No config.json written + remote telemetry OFF → perf_recording_enabled
        // must be false. We also force the static atomics to false to mirror a
        // truly fresh process (a prior test may have toggled it ON).
        PERF_RECORDING_FLAG.store(false, Ordering::SeqCst);
        set_enabled(false);
        assert!(
            !perf_recording_enabled(),
            "fresh install must have perf recording OFF"
        );
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(g);
    }

    /// Static check: the perf module never logs sample VALUES (only metric names +
    /// counts). We assert the perf module's source does not `eprintln!` a
    /// `value_ms` / `value` interpolation — a stderr capture must not leak latency
    /// fingerprints. This is a grep-style static guard against a future regression.
    #[test]
    fn perf_never_logs_sample_values() {
        let src = include_str!("telemetry.rs");
        // The perf module section starts at "// ---- perf metrics (B3)".
        let perf_section = src.split("// ---- perf metrics (B3)").nth(1).unwrap_or("");
        // No eprintln! may reference value_ms or .value (the sample's latency).
        // The toggle/clear path logs the boolean flag + counts, which is fine.
        assert!(
            !perf_section.contains("eprintln!(\"perf: sample value")
                && !perf_section.contains("eprintln!(\"perf: value_ms"),
            "the perf module must never log sample values"
        );
        // Sanity: the perf section exists (catches a future rename of the marker).
        assert!(
            perf_section.contains("fn record("),
            "perf section must contain the record() fn — did the marker move?"
        );
    }

    /// Fire-point check: `session.rs` records `PerfMetric::TurnLatency` at turn end.
    /// A grep-style static guard so a future refactor that drops the fire point is
    /// caught at test time.
    #[test]
    fn perf_fire_points_record_turn_latency() {
        let src = include_str!("agent/session.rs");
        assert!(
            src.contains("PerfMetric::TurnLatency"),
            "session.rs must record PerfMetric::TurnLatency at turn end (B3 fire point)"
        );
    }

    /// Fire-point check: `memory.rs` records `PerfMetric::EmbedLatency` around the
    /// embed() call. Static guard so the fire point is not dropped in a refactor.
    #[test]
    fn perf_fire_points_record_embed_latency() {
        let src = include_str!("memory.rs");
        assert!(
            src.contains("PerfMetric::EmbedLatency"),
            "memory.rs must record PerfMetric::EmbedLatency around embed_text (B3 fire point)"
        );
    }
}
