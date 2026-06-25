//! dotz isolated browser controller — Rust port of src/browser.ts (the 471-line oracle).
//!
//! Remote pages never run inside dotz's renderer. Each session drives a SELF-CONTAINED native
//! `agent-browser` binary (no Node) spawned ONE-SHOT per command: build the flags, run, parse the
//! JSON stdout, kill-tree on timeout. We port the CONTROLLER, not the browser engine.
//!
//! Security boundaries kept verbatim from the oracle: a disposable profile dir per session, an
//! explicit http(s) origin allowlist (passed as `--allowed-domains` host list AND re-checked after
//! every navigation, because the flag is host-only and can't enforce the port), `--content-boundaries`,
//! and `--confirm-actions eval,download,upload,clipboard` so raw eval / uploads / downloads / clipboard
//! are NEVER exposed. The closed action set is validated up front so a typo'd action can't no-op into a
//! false-success observation.
//!
//! ponytail: no persistent daemon management beyond what browser.ts does — one spawn per command, and
//! a best-effort `reap_stray_browsers` backstop on app shutdown (the Tauri shell calls `dispose_all`
//! and `reap_stray_browsers` on `RunEvent::Exit` so headless Chrome instances do not outlive the app).
use axum::{
    extract::Query,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
const MAX_OUTPUT: i64 = 50_000;
// 75s (not the oracle's 35s): the COLD first Chrome launch on a fresh profile can take ~40-50s here;
// subsequent commands hit the warm agent-browser daemon and return fast.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(75);

/// The closed set of valid actions. An unknown action string from an untrusted body would otherwise
/// fall through action_args() to None and be silently no-op'd while returning a 200 "ready"
/// observation — act() rejects it up front instead.
const ACTION_NAMES: [&str; 12] = [
    "navigate", "observe", "back", "forward", "reload", "click", "clickAt", "type", "key",
    "select", "scroll", "wait",
];

// ---- ISO-8601 timestamp (UTC, ms) without a chrono dependency ----
fn now_iso() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_ms = dur.as_millis() as i64;
    let secs = total_ms / 1000;
    let ms = (total_ms % 1000) as i64;
    // Civil-from-days (Howard Hinnant's algorithm).
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hh, mm, ss, ms
    )
}

// ---- origin normalization: lowercase scheme://host[:port], http(s) only ----
/// Parse a URL into a normalized origin (`scheme://host[:port]`, lowercased). Rejects non-http(s).
/// Minimal hand-rolled parse — the only fields we need are scheme, host, and port.
fn normalize_origin(value: &str) -> Result<String, String> {
    let value = value.trim();
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| "browser URLs must use http or https".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("browser URLs must use http or https".to_string());
    }
    // authority ends at the first '/', '?', or '#'.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    // strip userinfo if present
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(&authority);
    if authority.is_empty() {
        return Err(format!("invalid browser URL: {value}"));
    }

    let default_port: u16 = if scheme == "http" { 80 } else { 443 };

    // IPv6 literal: [host]:port or [host]. Brackets are part of the host syntax and must be
    // preserved in the normalized origin so the allowlist compares equal to user-supplied URLs.
    if authority.starts_with('[') {
        let Some(close) = authority.find(']') else {
            return Err(format!("invalid browser URL: {value}"));
        };
        let host = &authority[1..close];
        if host.is_empty() {
            return Err(format!("invalid browser URL: {value}"));
        }
        let after = &authority[close + 1..];
        if after.is_empty() {
            return Ok(format!("{scheme}://[{host}]"));
        }
        let Some(port_str) = after.strip_prefix(':') else {
            return Ok(format!("{scheme}://{authority}"));
        };
        let Ok(port) = port_str.parse::<u16>() else {
            return Ok(format!("{scheme}://{authority}"));
        };
        if port == default_port {
            return Ok(format!("{scheme}://[{host}]"));
        }
        return Ok(format!("{scheme}://[{host}]:{port}"));
    }

    // Hostname/IPv4 with optional port.
    if let Some((host, port)) = authority.rsplit_once(':') {
        if let Ok(p) = port.parse::<u16>() {
            if p == default_port {
                return Ok(format!("{scheme}://{host}"));
            }
            return Ok(format!("{scheme}://{host}:{p}"));
        }
    }
    Ok(format!("{scheme}://{authority}"))
}

/// The host portion of a normalized origin (for the `--allowed-domains` flag).
/// IPv6 brackets are stripped so the allowlist receives the bare host.
fn origin_host(origin: &str) -> String {
    let host = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin);
    if host.starts_with('[') {
        let end = host.find(']').unwrap_or(host.len());
        return host[1..end].to_string();
    }
    host.rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .to_string()
}

// ---- BrowserObservation (serde camelCase, contract shape) ----
#[derive(Clone, Serialize)]
pub struct BrowserOwner {
    pub app: String,
    #[serde(rename = "projectId")]
    pub project_id: String,
    #[serde(rename = "workflowId", skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(rename = "stepId", skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct Viewport {
    pub width: i64,
    pub height: i64,
}

#[derive(Clone, Serialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub viewport: Viewport,
}

#[derive(Clone, Serialize)]
pub struct ElementRef {
    pub r#ref: String,
    pub role: String,
    pub name: String,
    #[serde(rename = "observationSeq")]
    pub observation_seq: i64,
}

#[derive(Clone, Serialize)]
pub struct CurrentAction {
    pub name: String,
    #[serde(rename = "targetRef", skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
    pub summary: String,
}

#[derive(Clone, Serialize)]
pub struct Cursor {
    pub x: f64,
    pub y: f64,
    pub kind: String,
}

#[derive(Clone, Serialize)]
pub struct FrameMeta {
    pub seq: i64,
    pub mime: String,
    pub width: i64,
    pub height: i64,
    pub available: bool,
}

#[derive(Clone, Serialize)]
pub struct Counters {
    pub actions: i64,
    #[serde(rename = "consoleErrors")]
    pub console_errors: i64,
    #[serde(rename = "networkErrors")]
    pub network_errors: i64,
}

#[derive(Clone, Serialize)]
pub struct ObsError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Serialize)]
pub struct BrowserObservation {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub seq: i64,
    pub status: String,
    pub owner: BrowserOwner,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub page: Page,
    #[serde(rename = "allowedOrigins")]
    pub allowed_origins: Vec<String>,
    pub refs: Vec<String>,
    pub elements: Vec<ElementRef>,
    pub snapshot: String,
    #[serde(rename = "currentAction", skip_serializing_if = "Option::is_none")]
    pub current_action: Option<CurrentAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<FrameMeta>,
    pub counters: Counters,
    #[serde(rename = "consoleErrors")]
    pub console_errors: Vec<String>,
    #[serde(rename = "networkErrors")]
    pub network_errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ObsError>,
}

// ---- session store ----
struct SessionRecord {
    profile_dir: PathBuf,
    observation: BrowserObservation,
    frame_data: Option<Vec<u8>>,
    /// Set the instant stop() begins so in-flight run() calls abort before the profile dir is removed.
    disposed: bool,
}

fn sessions() -> &'static Mutex<HashMap<String, SessionRecord>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionRecord>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the browser session store, recovering from a poisoned mutex. A panic while holding the
/// sessions lock (e.g. inside a JSON parse or agent-browser callback) must not permanently brick
/// the browser controller.
fn sessions_guard() -> std::sync::MutexGuard<'static, HashMap<String, SessionRecord>> {
    sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- executable resolution (mirror executableCandidates) ----
/// The agent-browser binary name for this target. win32-x64 is the shipped target.
fn binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "agent-browser-win32-x64.exe"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "agent-browser-darwin-arm64"
        } else {
            "agent-browser-darwin-x64"
        }
    } else if cfg!(target_arch = "aarch64") {
        "agent-browser-linux-arm64"
    } else {
        "agent-browser-linux-x64"
    }
}

/// Resolve the agent-browser executable. DOTZ_BROWSER_BIN wins; else
/// `<DOTZ_RESOURCES or cwd>/node_modules/agent-browser/bin/<name>`; else the PATH fallback name.
fn resolve_executable() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("DOTZ_BROWSER_BIN") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let name = binary_name();
    let base = std::env::var("DOTZ_RESOURCES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let bundled = base
        .join("node_modules")
        .join("agent-browser")
        .join("bin")
        .join(name);
    if bundled.exists() {
        return Ok(bundled);
    }
    // PATH fallback (the oracle's final "agent-browser" candidate).
    let bare = if cfg!(target_os = "windows") {
        "agent-browser.exe"
    } else {
        "agent-browser"
    };
    Ok(PathBuf::from(bare))
}

// ---- agent-browser JSON-output parsing (mirror parseJsonOutput/dataValue/stringValue) ----
/// agent-browser prints one JSON object per command. Parse the LAST JSON line (newest), falling back
/// to the whole-trimmed parse, then to the raw string.
fn parse_json_output(output: &str) -> Value {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Value::Null;
    }
    for line in trimmed.lines().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            return v;
        }
    }
    serde_json::from_str::<Value>(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string()))
}

/// Unwrap the `.data` / `.result` envelope agent-browser wraps results in.
fn data_value(v: &Value) -> Value {
    if let Some(obj) = v.as_object() {
        if let Some(d) = obj.get("data") {
            return d.clone();
        }
        if let Some(r) = obj.get("result") {
            return r.clone();
        }
    }
    v.clone()
}

/// Read a string from a command result: bare string, or `data[key]`.
fn string_value(v: &Value, key: &str) -> String {
    let data = data_value(v);
    if let Some(s) = data.as_str() {
        return s.to_string();
    }
    if let Some(s) = data.get(key).and_then(|x| x.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Extract console/error entries: the wrapper object's first array field, a bare array, or text
/// lines. Empty results must yield zero lines (not one phantom "{...}").
fn result_lines(v: &Value) -> Vec<String> {
    let data = data_value(v);
    let arr = if data.is_array() {
        data.as_array().cloned()
    } else if let Some(obj) = data.as_object() {
        obj.values()
            .find(|x| x.is_array())
            .and_then(|x| x.as_array().cloned())
    } else {
        None
    };
    if let Some(arr) = arr {
        return arr
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| x.to_string())
            })
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(s) = data.as_str() {
        return s
            .lines()
            .map(String::from)
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

/// Parse the accessibility snapshot into element refs (mirror snapshotElements).
fn snapshot_elements(snapshot: &str, observation_seq: i64) -> Vec<ElementRef> {
    let mut out = Vec::new();
    for line in snapshot.lines() {
        let Some(reff) = first_ref(line) else {
            continue;
        };
        // descriptor: strip a leading "- "/"* " bullet, then drop the "[ref=eN] …" tail.
        let mut descriptor = line.trim_start();
        descriptor = descriptor.trim_start_matches(['-', '*', ' ']);
        let descriptor = strip_ref_tail(descriptor);
        let (role, name) = split_role_name(descriptor.trim());
        out.push(ElementRef {
            r#ref: reff,
            role: if role.is_empty() {
                "element".into()
            } else {
                role
            },
            name,
            observation_seq,
        });
    }
    out
}

/// Find the first `@eN` or `ref=eN` ref token on a line, skipping non-ref `@` text
/// (e.g. an email address or literal @ symbol) that would otherwise hide a later valid ref.
fn first_ref(line: &str) -> Option<String> {
    all_refs(line).into_iter().next()
}

/// All unique refs on a line/snapshot, in order.
fn all_refs(snapshot: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut rest = snapshot;
    loop {
        let at = rest.find('@');
        let req = rest.find("ref=");
        let pos = match (at, req) {
            (Some(a), Some(r)) => Some((a.min(r), if a <= r { "@" } else { "ref=" })),
            (Some(a), None) => Some((a, "@")),
            (None, Some(r)) => Some((r, "ref=")),
            (None, None) => None,
        };
        let Some((pos, tok)) = pos else { break };
        let after = &rest[pos + tok.len()..];
        if let Some(stripped) = after.strip_prefix('e') {
            let digits: String = stripped
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                let reff = format!("e{digits}");
                if !seen.contains(&reff) {
                    seen.push(reff);
                }
            }
        }
        rest = &rest[pos + tok.len()..];
    }
    seen
}

/// Drop the "[ref=eN] …" / "[eN] …" trailing portion of a snapshot descriptor.
fn strip_ref_tail(s: &str) -> &str {
    if let Some(idx) = s.find('[') {
        // only treat as a ref tail if a ref token follows shortly
        let tail = &s[idx..];
        if tail.contains("e") && (tail.contains("ref=") || tail.starts_with("[e")) {
            return s[..idx].trim_end();
        }
    }
    s
}

/// Split a descriptor `role "name"` (or `role 'name'`) into (role, name).
fn split_role_name(descriptor: &str) -> (String, String) {
    for q in ['"', '\''] {
        if let Some(start) = descriptor.find(q) {
            if let Some(end_rel) = descriptor[start + 1..].find(q) {
                let role = descriptor[..start].trim().to_string();
                let name = descriptor[start + 1..start + 1 + end_rel].to_string();
                return (role, name);
            }
        }
    }
    (descriptor.trim().to_string(), String::new())
}

// ---- the one-shot command runner ----
/// Spawn agent-browser ONE-SHOT with the exact security flags + AGENT_BROWSER_HEADED=false, a 35s
/// timeout, capture stdout/stderr, kill-tree on timeout, parse stdout as JSON. `command` is the
/// trailing `--json <command...>` portion.
async fn run(
    session_id: &str,
    profile_dir: &PathBuf,
    allowed_origins: &[String],
    command: &[&str],
) -> Result<Value, String> {
    // Abort if a concurrent stop() tore the session down — the teardown's own "close" is exempt.
    {
        let disposed = sessions_guard()
            .get(session_id)
            .map(|r| r.disposed)
            .unwrap_or(false);
        if disposed && command.first() != Some(&"close") {
            return Err("browser session stopped".into());
        }
    }
    let executable = resolve_executable()?;
    let domains = allowed_origins
        .iter()
        .map(|o| origin_host(o))
        .collect::<Vec<_>>()
        .join(",");
    let profile_str = profile_dir.to_string_lossy().to_string();
    let max_output = MAX_OUTPUT.to_string();

    let mut args: Vec<String> = vec![
        "--session".into(),
        session_id.to_string(),
        "--profile".into(),
        profile_str,
        "--allowed-domains".into(),
        domains,
        "--content-boundaries".into(),
        "--max-output".into(),
        max_output,
        "--confirm-actions".into(),
        "eval,download,upload,clipboard".into(),
        "--json".into(),
    ];
    for c in command {
        args.push((*c).to_string());
    }

    // Redirect stdout/stderr to FILES (not pipes): agent-browser leaves a persistent Chrome daemon
    // that inherits the pipes and never closes them, so a pipe-EOF read (read_to_end) would hang
    // forever even after the command process exits. Files let us wait on process exit, then read.
    let nonce = uuid::Uuid::new_v4().to_string();
    let out_path = profile_dir.join(format!("command-{nonce}.out"));
    let err_path = profile_dir.join(format!("command-{nonce}.err"));
    let out_file =
        std::fs::File::create(&out_path).map_err(|e| format!("browser stdout file: {e}"))?;
    let err_file =
        std::fs::File::create(&err_path).map_err(|e| format!("browser stderr file: {e}"))?;

    let mut cmd = tokio::process::Command::new(&executable);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .env("AGENT_BROWSER_HEADED", "false");
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // inherent on tokio::process::Command (no CommandExt import needed)
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("agent-browser spawn failed: {e}"))?;
    let pid = child.id();

    let status = match tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await {
        Err(_) => {
            // Timed out: kill-tree (the child + its headless Chrome grandchild) and fail.
            kill_pid(pid);
            let _ = child.start_kill();
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return Err("agent-browser command timed out".into());
        }
        Ok(s) => s,
    };
    // Let any final buffered write flush, then read the captured files.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stdout_s = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr_s = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    match status {
        Ok(es) if es.success() => Ok(parse_json_output(&stdout_s)),
        Ok(es) => {
            let msg = if !stderr_s.trim().is_empty() {
                stderr_s.trim().to_string()
            } else if !stdout_s.trim().is_empty() {
                stdout_s.trim().to_string()
            } else {
                format!("agent-browser exited {}", es.code().unwrap_or(-1))
            };
            Err(msg.chars().take(1000).collect())
        }
        Err(e) => Err(format!("agent-browser wait failed: {e}")),
    }
}

/// Kill a pid and its descendant tree — taskkill /T /F on win32, kill -9 on posix. Best-effort.
fn kill_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

// ---- clamping helpers ----
fn clamp_i64(v: i64, lo: i64, hi: i64) -> i64 {
    v.max(lo).min(hi)
}

// ---- observation building ----
/// Re-observe the page after an action: validate the post-nav origin FIRST, then snapshot/title/
/// console/errors/screenshot, bump seq, and rebuild the observation. Mutates the stored record.
async fn observe(
    session_id: &str,
    action: Option<CurrentAction>,
) -> Result<BrowserObservation, String> {
    let (profile_dir, allowed_origins) = {
        let store = sessions_guard();
        let r = store.get(session_id).ok_or("no such browser session")?;
        (r.profile_dir.clone(), r.observation.allowed_origins.clone())
    };

    // Origin check FIRST — reject a cross-port redirect off the allowlist before rendering.
    let url_result = run(session_id, &profile_dir, &allowed_origins, &["get", "url"]).await?;
    let new_url = string_value(&url_result, "url");
    let effective_url = if new_url.is_empty() {
        sessions_guard()
            .get(session_id)
            .map(|r| r.observation.page.url.clone())
            .unwrap_or_default()
    } else {
        new_url.clone()
    };
    let post_origin = normalize_origin(&effective_url)?;
    if !allowed_origins.contains(&post_origin) {
        return Err(format!(
            "browser navigated outside the allowlist: {effective_url}"
        ));
    }

    let title_result = run(
        session_id,
        &profile_dir,
        &allowed_origins,
        &["get", "title"],
    )
    .await?;
    let snapshot_result = run(
        session_id,
        &profile_dir,
        &allowed_origins,
        &["snapshot", "-i", "-c"],
    )
    .await?;
    let console_result = run(session_id, &profile_dir, &allowed_origins, &["console"]).await?;
    let errors_result = run(session_id, &profile_dir, &allowed_origins, &["errors"]).await?;

    let snapshot = {
        let s = string_value(&snapshot_result, "snapshot");
        if !s.is_empty() {
            s
        } else {
            let d = data_value(&snapshot_result);
            d.as_str()
                .map(String::from)
                .unwrap_or_else(|| d.to_string())
        }
    };
    let console_lines: Vec<String> = result_lines(&console_result)
        .into_iter()
        .filter(|l| {
            let lc = l.to_ascii_lowercase();
            lc.contains("error") || lc.contains("exception") || lc.contains("failed")
        })
        .collect();
    let console_lines: Vec<String> = console_lines.iter().rev().take(50).rev().cloned().collect();
    let network_lines: Vec<String> = result_lines(&errors_result);
    let network_lines: Vec<String> = network_lines.iter().rev().take(50).rev().cloned().collect();

    // Screenshot to a JPEG in the profile dir, read it back.
    let frame_path = profile_dir.join("frame.jpg");
    let frame_path_str = frame_path.to_string_lossy().to_string();
    run(
        session_id,
        &profile_dir,
        &allowed_origins,
        &[
            "screenshot",
            &frame_path_str,
            "--screenshot-format",
            "jpeg",
            "--screenshot-quality",
            "72",
        ],
    )
    .await?;
    let frame_bytes = tokio::fs::read(&frame_path).await.ok();

    let title = string_value(&title_result, "title");
    let refs = all_refs(&snapshot);

    let mut store = sessions_guard();
    let r = store.get_mut(session_id).ok_or("no such browser session")?;
    r.observation.seq += 1;
    let seq = r.observation.seq;
    r.observation.status = "ready".into();
    r.observation.updated_at = now_iso();
    r.observation.page.url = effective_url;
    r.observation.page.title = title;
    r.observation.snapshot = snapshot.clone();
    r.observation.elements = snapshot_elements(&snapshot, seq);
    r.observation.refs = refs;
    r.observation.current_action = action;
    r.observation.console_errors = console_lines.clone();
    r.observation.network_errors = network_lines.clone();
    r.observation.counters.console_errors = console_lines.len() as i64;
    r.observation.counters.network_errors = network_lines.len() as i64;
    r.observation.error = None;
    if let Some(bytes) = frame_bytes {
        let (w, h) = (
            r.observation.page.viewport.width,
            r.observation.page.viewport.height,
        );
        r.frame_data = Some(bytes);
        r.observation.frame = Some(FrameMeta {
            seq,
            mime: "image/jpeg".into(),
            width: w,
            height: h,
            available: true,
        });
    }
    Ok(r.observation.clone())
}

/// Mark the record as failed (status "error", bump seq, attach error) unless already disposed.
fn fail(session_id: &str, message: &str) {
    let mut store = sessions_guard();
    if let Some(r) = store.get_mut(session_id) {
        if r.disposed {
            return;
        }
        r.observation.status = "error".into();
        r.observation.seq += 1;
        r.observation.updated_at = now_iso();
        r.observation.error = Some(ObsError {
            code: "BROWSER_ACTION_FAILED".into(),
            message: message.to_string(),
            retryable: true,
        });
    }
}

// ---- public controller API ----

/// start(): create the record + disposable profile dir, set viewport, open the URL, build the first
/// observation (seq=1). Tears the session down on any failure.
pub async fn start(
    project_id: &str,
    url: &str,
    allowed_origins: Option<Vec<String>>,
    viewport: Option<(i64, i64)>,
    workflow_id: Option<String>,
    step_id: Option<String>,
) -> Result<BrowserObservation, String> {
    if project_id.trim().is_empty() {
        return Err("projectId is required".into());
    }
    let initial_origin = normalize_origin(url)?;
    let raw = allowed_origins
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![initial_origin.clone()]);
    let mut allowed: Vec<String> = Vec::new();
    for o in &raw {
        let n = normalize_origin(o)?;
        if !allowed.contains(&n) {
            allowed.push(n);
        }
    }
    if !allowed.contains(&initial_origin) {
        return Err(format!(
            "initial URL origin {initial_origin} is not in the allowlist"
        ));
    }

    let session_id = format!("dotz-{}", uuid::Uuid::new_v4());
    let (vw, vh) = viewport.unwrap_or((1280, 800));
    let viewport = Viewport {
        width: clamp_i64(vw, 320, 2560),
        height: clamp_i64(vh, 240, 1600),
    };

    // Disposable profile dir under temp_dir()/dotz-browser-<id>.
    let profile_dir = fresh_profile_dir(&session_id).await?;

    let now = now_iso();
    let observation = BrowserObservation {
        schema_version: VERSION,
        session_id: session_id.clone(),
        seq: 0,
        status: "starting".into(),
        owner: BrowserOwner {
            app: "dotz".into(),
            project_id: project_id.to_string(),
            workflow_id,
            step_id,
        },
        started_at: now.clone(),
        updated_at: now,
        page: Page {
            url: url.to_string(),
            title: String::new(),
            viewport: Viewport {
                width: viewport.width,
                height: viewport.height,
            },
        },
        allowed_origins: allowed.clone(),
        refs: Vec::new(),
        elements: Vec::new(),
        snapshot: String::new(),
        current_action: None,
        cursor: None,
        frame: None,
        counters: Counters {
            actions: 0,
            console_errors: 0,
            network_errors: 0,
        },
        console_errors: Vec::new(),
        network_errors: Vec::new(),
        error: None,
    };
    let vw = viewport.width;
    let vh = viewport.height;
    sessions_guard().insert(
        session_id.clone(),
        SessionRecord {
            profile_dir: profile_dir.clone(),
            observation,
            frame_data: None,
            disposed: false,
        },
    );

    let outcome: Result<BrowserObservation, String> = async {
        run(
            &session_id,
            &profile_dir,
            &allowed,
            &["set", "viewport", &vw.to_string(), &vh.to_string()],
        )
        .await?;
        run(&session_id, &profile_dir, &allowed, &["open", url]).await?;
        observe(
            &session_id,
            Some(CurrentAction {
                name: "navigate".into(),
                target_ref: None,
                summary: format!("opened {url}"),
            }),
        )
        .await
    }
    .await;

    match outcome {
        Ok(obs) => Ok(obs),
        Err(e) => {
            fail(&session_id, &e);
            let _ = run(&session_id, &profile_dir, &allowed, &["close"]).await;
            let _ = tokio::fs::remove_dir_all(&profile_dir).await;
            sessions_guard().remove(&session_id);
            Err(e)
        }
    }
}

/// act(): validate the action + sequence/ref binding (409 cases surface as the "stale"/"unknown ref"
/// error text), set cursor, run the mapped command, bump the actions counter, re-observe.
pub async fn act(input: &Value) -> Result<BrowserObservation, String> {
    let session_id = input
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("sessionId is required")?
        .to_string();
    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("action is required")?
        .to_string();

    let (profile_dir, allowed_origins, cur_seq, refs, viewport, status) = {
        let store = sessions_guard();
        let r = store.get(&session_id).ok_or("no such browser session")?;
        (
            r.profile_dir.clone(),
            r.observation.allowed_origins.clone(),
            r.observation.seq,
            r.observation.refs.clone(),
            (
                r.observation.page.viewport.width,
                r.observation.page.viewport.height,
            ),
            r.observation.status.clone(),
        )
    };
    if status == "stopped" {
        return Err("browser session is stopped".into());
    }
    if !ACTION_NAMES.contains(&action.as_str()) {
        return Err(format!("unknown browser action: {action}"));
    }

    let target_ref = input
        .get("targetRef")
        .and_then(|v| v.as_str())
        .map(String::from);
    let text = input.get("text").and_then(|v| v.as_str()).map(String::from);
    let url = input.get("url").and_then(|v| v.as_str()).map(String::from);
    let key = input.get("key").and_then(|v| v.as_str()).map(String::from);
    let values: Vec<String> = input
        .get("values")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let direction = input
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("down")
        .to_string();
    let pixels = input.get("pixels").and_then(|v| v.as_f64());
    let milliseconds = input.get("milliseconds").and_then(|v| v.as_f64());
    let x = input.get("x").and_then(|v| v.as_f64());
    let y = input.get("y").and_then(|v| v.as_f64());
    let expected_seq = input.get("expectedSeq").and_then(|v| v.as_i64());

    // Sequence-bound actions require a matching expectedSeq (these surface as 409 in the route).
    let sequence_bound =
        target_ref.is_some() || action == "clickAt" || (action == "type" && target_ref.is_none());
    if sequence_bound && expected_seq != Some(cur_seq) {
        return Err(format!(
            "stale browser action: expected observation seq {cur_seq}"
        ));
    }
    if let Some(tr) = &target_ref {
        let bare = tr.strip_prefix('@').unwrap_or(tr);
        if !refs.iter().any(|r| r == bare) {
            return Err(format!("unknown browser ref: {tr}"));
        }
    }

    // Move to "acting".
    {
        let mut store = sessions_guard();
        if let Some(r) = store.get_mut(&session_id) {
            r.observation.status = "acting".into();
            r.observation.current_action = Some(CurrentAction {
                name: action.clone(),
                target_ref: target_ref.clone(),
                summary: action_summary(&action, &url, &target_ref, x, y),
            });
            r.observation.updated_at = now_iso();
        }
    }

    let run_action = async {
        // Cursor: ref → box center; clickAt → the given coords.
        if let Some(tr) = &target_ref {
            let reff = if tr.starts_with('@') {
                tr.clone()
            } else {
                format!("@{tr}")
            };
            let box_result = run(
                &session_id,
                &profile_dir,
                &allowed_origins,
                &["get", "box", &reff],
            )
            .await?;
            let bv = data_value(&box_result);
            let bx = bv.get("x").and_then(|v| v.as_f64());
            let by = bv.get("y").and_then(|v| v.as_f64());
            let bw = bv.get("width").and_then(|v| v.as_f64());
            let bh = bv.get("height").and_then(|v| v.as_f64());
            if let (Some(bx), Some(by), Some(bw), Some(bh)) = (bx, by, bw, bh) {
                if [bx, by, bw, bh].iter().all(|f| f.is_finite()) {
                    let mut store = sessions_guard();
                    if let Some(r) = store.get_mut(&session_id) {
                        r.observation.cursor = Some(Cursor {
                            x: bx + bw / 2.0,
                            y: by + bh / 2.0,
                            kind: action.clone(),
                        });
                    }
                }
            }
        } else if action == "clickAt" {
            if let (Some(x), Some(y)) = (x, y) {
                if x.is_finite() && y.is_finite() {
                    let mut store = sessions_guard();
                    if let Some(r) = store.get_mut(&session_id) {
                        r.observation.cursor = Some(Cursor {
                            x,
                            y,
                            kind: action.clone(),
                        });
                    }
                }
            }
        }

        if action == "clickAt" {
            let (xf, yf) = (x.unwrap_or(f64::NAN), y.unwrap_or(f64::NAN));
            if !xf.is_finite() || !yf.is_finite() {
                return Err("clickAt requires finite x and y coordinates".to_string());
            }
            let (xi, yi) = (xf.round() as i64, yf.round() as i64);
            let (vw, vh) = viewport;
            if xi < 0 || yi < 0 || xi >= vw || yi >= vh {
                return Err(format!("clickAt coordinates must be inside {vw}x{vh}"));
            }
            run(
                &session_id,
                &profile_dir,
                &allowed_origins,
                &["mouse", "move", &xi.to_string(), &yi.to_string()],
            )
            .await?;
            run(
                &session_id,
                &profile_dir,
                &allowed_origins,
                &["mouse", "down", "left"],
            )
            .await?;
            run(
                &session_id,
                &profile_dir,
                &allowed_origins,
                &["mouse", "up", "left"],
            )
            .await?;
        } else if let Some(args) = action_args(
            &action,
            &allowed_origins,
            &url,
            &target_ref,
            &text,
            &key,
            &values,
            &direction,
            pixels,
            milliseconds,
        )? {
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            run(&session_id, &profile_dir, &allowed_origins, &arg_refs).await?;
        }

        {
            let mut store = sessions_guard();
            if let Some(r) = store.get_mut(&session_id) {
                r.observation.counters.actions += 1;
            }
        }
        let action_record = sessions_guard()
            .get(&session_id)
            .and_then(|r| r.observation.current_action.clone());
        observe(&session_id, action_record).await
    };

    match run_action.await {
        Ok(obs) => Ok(obs),
        Err(e) => {
            fail(&session_id, &e);
            Err(e)
        }
    }
}

/// Map a (validated) action + its params to agent-browser command args. None = "observe only".
#[allow(clippy::too_many_arguments)]
fn action_args(
    action: &str,
    allowed_origins: &[String],
    url: &Option<String>,
    target_ref: &Option<String>,
    text: &Option<String>,
    key: &Option<String>,
    values: &[String],
    direction: &str,
    pixels: Option<f64>,
    milliseconds: Option<f64>,
) -> Result<Option<Vec<String>>, String> {
    let reff = target_ref.as_ref().map(|t| {
        if t.starts_with('@') {
            t.clone()
        } else {
            format!("@{t}")
        }
    });
    match action {
        "observe" => Ok(None),
        "back" => Ok(Some(vec!["back".into()])),
        "forward" => Ok(Some(vec!["forward".into()])),
        "reload" => Ok(Some(vec!["reload".into()])),
        "navigate" => {
            let url = url.as_ref().ok_or("navigate requires url")?;
            let origin = normalize_origin(url)?;
            if !allowed_origins.contains(&origin) {
                return Err(format!(
                    "navigation origin {origin} is not in the allowlist"
                ));
            }
            Ok(Some(vec!["open".into(), url.clone()]))
        }
        "click" => {
            let r = reff.ok_or("click requires targetRef")?;
            Ok(Some(vec!["click".into(), r]))
        }
        "clickAt" => Ok(None), // handled as explicit mouse commands in act()
        "type" => {
            let text = text.as_ref().ok_or("type requires text")?;
            Ok(Some(match reff {
                Some(r) => vec!["fill".into(), r, text.clone()],
                None => vec!["keyboard".into(), "inserttext".into(), text.clone()],
            }))
        }
        "key" => {
            let k = key.as_ref().ok_or("key requires key")?;
            Ok(Some(vec!["press".into(), k.clone()]))
        }
        "select" => {
            let r = reff.ok_or("select requires targetRef and values")?;
            if values.is_empty() {
                return Err("select requires targetRef and values".into());
            }
            let mut v = vec!["select".into(), r];
            v.extend(values.iter().cloned());
            Ok(Some(v))
        }
        "scroll" => {
            let px = pixels.map(|p| p as i64).unwrap_or(500).max(1);
            Ok(Some(vec![
                "scroll".into(),
                direction.to_string(),
                px.to_string(),
            ]))
        }
        "wait" => {
            let ms = milliseconds.map(|m| m as i64).unwrap_or(500);
            Ok(Some(vec![
                "wait".into(),
                clamp_i64(ms, 0, 30_000).to_string(),
            ]))
        }
        _ => Ok(None),
    }
}

fn action_summary(
    action: &str,
    url: &Option<String>,
    target_ref: &Option<String>,
    x: Option<f64>,
    y: Option<f64>,
) -> String {
    match action {
        "navigate" => format!("navigate {}", url.clone().unwrap_or_default()),
        "clickAt" => format!(
            "click ({}, {})",
            x.unwrap_or(0.0).round() as i64,
            y.unwrap_or(0.0).round() as i64
        ),
        "type" if target_ref.is_none() => "type into focused element".into(),
        _ => match target_ref {
            Some(t) => format!("{action} {t}"),
            None => action.to_string(),
        },
    }
}

/// Create a fresh, disposable browser profile directory for `session_id` under the OS temp dir.
/// If a stale directory exists from a previous crash, it is removed first so the new session
/// never inherits another session's cookies, storage, or state.
async fn fresh_profile_dir(session_id: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("dotz-browser-{session_id}"));
    match tokio::fs::create_dir(&dir).await {
        Ok(()) => Ok(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            tokio::fs::create_dir(&dir)
                .await
                .map_err(|e| format!("create profile dir: {e}"))?;
            Ok(dir)
        }
        Err(e) => Err(format!("create profile dir: {e}")),
    }
}

/// stop(): mark disposed, close the process, set status "stopped", bump seq, dispose the profile dir.
pub async fn stop(session_id: &str) -> Result<BrowserObservation, String> {
    let (profile_dir, allowed_origins) = {
        let mut store = sessions_guard();
        let r = store.get_mut(session_id).ok_or("no such browser session")?;
        r.disposed = true; // abort any in-flight run() before we remove the profile dir
        (r.profile_dir.clone(), r.observation.allowed_origins.clone())
    };
    let _ = run(session_id, &profile_dir, &allowed_origins, &["close"]).await;

    let stopped = {
        let mut store = sessions_guard();
        let r = store.get_mut(session_id).ok_or("no such browser session")?;
        r.observation.status = "stopped".into();
        r.observation.seq += 1;
        r.observation.updated_at = now_iso();
        r.observation.current_action = None;
        r.observation.frame = None;
        r.frame_data = None;
        r.observation.clone()
    };
    let _ = tokio::fs::remove_dir_all(&profile_dir).await;
    sessions_guard().remove(session_id);
    Ok(stopped)
}

/// state(sessionId?): the newest (or named) observation. None when no such session.
fn state(session_id: Option<&str>) -> Option<BrowserObservation> {
    let store = sessions_guard();
    match session_id {
        Some(id) => store.get(id).map(|r| r.observation.clone()),
        // newest by startedAt — HashMap has no order, so pick max updatedAt.
        None => store
            .values()
            .max_by(|a, b| a.observation.updated_at.cmp(&b.observation.updated_at))
            .map(|r| r.observation.clone()),
    }
}

/// list(): every session's observation.
fn list() -> Vec<BrowserObservation> {
    sessions_guard()
        .values()
        .map(|r| r.observation.clone())
        .collect()
}

/// frame(sessionId, afterSeq): the latest JPEG bytes + seq, or None when not newer than afterSeq.
fn frame(session_id: &str, after_seq: i64) -> Option<(i64, Vec<u8>)> {
    let store = sessions_guard();
    let r = store.get(session_id)?;
    let seq = r.observation.frame.as_ref().map(|f| f.seq)?;
    let data = r.frame_data.as_ref()?;
    if seq <= after_seq {
        return None;
    }
    Some((seq, data.clone()))
}

/// Dispose every browser session. Called by the Tauri shell on `RunEvent::Exit` so the app does
/// not leave headless Chrome profiles/processes behind. Each session stop is bounded so a hung
/// `agent-browser close` command cannot block shutdown indefinitely.
pub async fn dispose_all() {
    let ids: Vec<String> = sessions_guard().keys().cloned().collect();
    for id in ids {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stop(&id)).await;
    }
}

/// Force-kill any lingering agent-browser process tree by image name. Best-effort backstop —
/// `agent-browser close` does not reliably reap the headless-Chrome grandchild. Exposed publicly
/// so the Tauri shell can call it after `dispose_all` on shutdown.
pub fn reap_stray_browsers() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "agent-browser-win32-x64.exe", "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("pkill")
            .args(["-f", "agent-browser"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

// ---- HTTP routes ----

/// Map a controller error to the right status code. Staleness cases are 409 (retryable conflict);
/// missing session on stop is 404; everything else is 400.
fn act_status(message: &str) -> StatusCode {
    let lc = message.to_ascii_lowercase();
    if lc.contains("stale browser action") || lc.contains("unknown browser ref") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    }
}

async fn get_state(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let sid = q.get("sessionId").map(|s| s.as_str());
    let observation = state(sid);
    Json(json!({
        "available": true,
        "observation": observation,
        "sessions": list(),
    }))
}

async fn get_frame(Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(session_id) = q.get("sessionId") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sessionId required" })),
        )
            .into_response();
    };
    let after_seq = q
        .get("afterSeq")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1);
    match frame(session_id, after_seq) {
        Some((seq, data)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (
                    header::HeaderName::from_static("x-dotz-frame-seq"),
                    seq.to_string(),
                ),
            ],
            axum::body::Bytes::from(data),
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn post_start(body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let project_id = b.get("projectId").and_then(|v| v.as_str()).unwrap_or("");
    let url = b.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let allowed_origins = b.get("allowedOrigins").and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect::<Vec<_>>()
    });
    let viewport = b.get("viewport").and_then(|v| {
        let w = v.get("width").and_then(|x| x.as_i64())?;
        let h = v.get("height").and_then(|x| x.as_i64())?;
        Some((w, h))
    });
    let workflow_id = b
        .get("workflowId")
        .and_then(|v| v.as_str())
        .map(String::from);
    let step_id = b.get("stepId").and_then(|v| v.as_str()).map(String::from);

    match start(
        project_id,
        url,
        allowed_origins,
        viewport,
        workflow_id,
        step_id,
    )
    .await
    {
        Ok(obs) => (StatusCode::OK, Json(serde_json::to_value(obs).unwrap())).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn post_act(body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    match act(&b).await {
        Ok(obs) => (StatusCode::OK, Json(serde_json::to_value(obs).unwrap())).into_response(),
        Err(e) => (act_status(&e), Json(json!({ "error": e }))).into_response(),
    }
}

async fn post_stop(body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let Some(session_id) = b.get("sessionId").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sessionId required" })),
        )
            .into_response();
    };
    match stop(session_id).await {
        Ok(obs) => (StatusCode::OK, Json(serde_json::to_value(obs).unwrap())).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
    }
}

/// Stateless router for the isolated-browser endpoints (merged after with_state).
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/browser/state", get(get_state))
        .route("/api/browser/frame", get(get_frame))
        .route("/api/browser/start", post(post_start))
        .route("/api/browser/act", post(post_act))
        .route("/api/browser/stop", post(post_stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_origin_lowercases_and_strips_path() {
        assert_eq!(
            normalize_origin("https://Example.COM/path?x=1#frag").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_origin_strips_userinfo() {
        assert_eq!(
            normalize_origin("http://user:pass@example.com/page").unwrap(),
            "http://example.com"
        );
    }

    #[test]
    fn normalize_origin_rejects_non_http() {
        assert!(normalize_origin("ftp://example.com").is_err());
        assert!(normalize_origin("not-a-url").is_err());
    }

    #[test]
    fn normalize_origin_treats_default_ports_as_equal() {
        assert_eq!(
            normalize_origin("http://example.com").unwrap(),
            normalize_origin("http://example.com:80").unwrap()
        );
        assert_eq!(
            normalize_origin("https://example.com").unwrap(),
            normalize_origin("https://example.com:443").unwrap()
        );
    }

    #[test]
    fn normalize_origin_preserves_non_default_ports() {
        assert_eq!(
            normalize_origin("http://example.com:8080").unwrap(),
            "http://example.com:8080"
        );
        assert_eq!(
            normalize_origin("https://example.com:8443/foo").unwrap(),
            "https://example.com:8443"
        );
    }

    #[test]
    fn origin_host_strips_scheme_and_port() {
        assert_eq!(origin_host("https://example.com:8080"), "example.com");
        assert_eq!(origin_host("http://example.com"), "example.com");
    }

    /// IPv6 hosts are bracketed in origins; `origin_host` must return the bare host so the
    /// agent-browser `--allowed-domains` flag receives a conventional unbracketed host value.
    #[test]
    fn origin_host_strips_ipv6_brackets_and_port() {
        assert_eq!(origin_host("http://[::1]:8080"), "::1");
        assert_eq!(origin_host("https://[::1]"), "::1");
        assert_eq!(origin_host("http://[2001:db8::1]:443"), "2001:db8::1");
    }

    /// IPv6 origins must normalize while preserving brackets and stripping default ports,
    /// so the same origin expressed with or without an explicit port compares equal.
    #[test]
    fn normalize_origin_handles_ipv6_default_ports() {
        assert_eq!(
            normalize_origin("http://[::1]").unwrap(),
            normalize_origin("http://[::1]:80").unwrap()
        );
        assert_eq!(
            normalize_origin("https://[::1]").unwrap(),
            normalize_origin("https://[::1]:443").unwrap()
        );
        assert_eq!(
            normalize_origin("http://[::1]:8080").unwrap(),
            "http://[::1]:8080"
        );
    }

    /// IPv6 URLs with userinfo, paths, and non-default ports must normalize correctly.
    #[test]
    fn normalize_origin_handles_ipv6_urls() {
        assert_eq!(
            normalize_origin("http://user:pass@[::1]:8080/path").unwrap(),
            "http://[::1]:8080"
        );
        assert_eq!(
            normalize_origin("https://[2001:db8::1]/foo").unwrap(),
            "https://[2001:db8::1]"
        );
    }

    /// A malformed IPv6 literal (missing closing bracket) must be rejected rather than
    /// producing a truncated origin that could slip into the allowlist.
    #[test]
    fn normalize_origin_rejects_ipv6_without_closing_bracket() {
        assert!(normalize_origin("http://[::1").is_err());
    }

    /// A snapshot line may contain an `@` that is not a valid element ref (e.g. an email
    /// address or literal text). `first_ref` must skip that false positive and still find the
    /// real `ref=eN` or `@eN` token that appears later on the same line. Before the fix it only
    /// checked the first occurrence of each token type, so a bogus `@foo` masked `ref=e5`.
    #[test]
    fn first_ref_skips_invalid_at_token_and_finds_later_valid_ref() {
        assert_eq!(first_ref("button Email @foo ref=e5"), Some("e5".into()));
        assert_eq!(first_ref("label @bad @e12 text"), Some("e12".into()));
        assert_eq!(first_ref("plain text @notref and more"), None);
    }

    #[test]
    fn act_status_maps_stale_and_unknown_ref_to_conflict() {
        assert_eq!(
            act_status("stale browser action: expected observation seq 3"),
            StatusCode::CONFLICT
        );
        assert_eq!(
            act_status("unknown browser ref: @e99"),
            StatusCode::CONFLICT
        );
        assert_eq!(act_status("some other error"), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn action_args_navigate_requires_allowed_origin() {
        let allowed = vec!["https://example.com".into()];
        let args = action_args(
            "navigate",
            &allowed,
            &Some("https://example.com/page".into()),
            &None,
            &None,
            &None,
            &[],
            "down",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            args,
            Some(vec!["open".into(), "https://example.com/page".into()])
        );

        let err = action_args(
            "navigate",
            &allowed,
            &Some("https://evil.com".into()),
            &None,
            &None,
            &None,
            &[],
            "down",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("not in the allowlist"));
    }

    #[test]
    fn action_args_type_requires_text() {
        let err = action_args(
            "type",
            &[],
            &None,
            &None,
            &None,
            &None,
            &[],
            "down",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("type requires text"));
    }

    #[test]
    fn action_args_scroll_clamps_pixels_and_wait_clamps_ms() {
        let allowed = vec![];
        let scroll = action_args(
            "scroll",
            &allowed,
            &None,
            &None,
            &None,
            &None,
            &[],
            "up",
            Some(-100.0),
            None,
        )
        .unwrap();
        assert_eq!(scroll, Some(vec!["scroll".into(), "up".into(), "1".into()]));

        let wait = action_args(
            "wait",
            &allowed,
            &None,
            &None,
            &None,
            &None,
            &[],
            "down",
            None,
            Some(100_000.0),
        )
        .unwrap();
        assert_eq!(wait, Some(vec!["wait".into(), "30000".into()]));
    }

    /// `fresh_profile_dir` must create a clean directory under the OS temp dir. If a stale
    /// directory already exists (e.g. from a previous crash), it must be removed and a fresh,
    /// empty directory created in its place so browser sessions never reuse another session's
    /// profile state.
    #[tokio::test]
    async fn fresh_profile_dir_creates_clean_dir_and_removes_stale() {
        let sid = format!("test-{}", uuid::Uuid::new_v4());
        let dir = fresh_profile_dir(&sid)
            .await
            .expect("fresh_profile_dir should succeed");
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "profile dir should live under the OS temp dir"
        );
        let meta = tokio::fs::metadata(&dir)
            .await
            .expect("profile dir should exist");
        assert!(meta.is_dir(), "profile path should be a directory");

        // Simulate a stale profile from a previous crash.
        tokio::fs::write(dir.join("stale-cookie.txt"), "old")
            .await
            .expect("writing stale file should succeed");

        // Re-creating for the same session id must wipe the stale dir and return a fresh one.
        let dir2 = fresh_profile_dir(&sid)
            .await
            .expect("fresh_profile_dir should clean stale dir");
        assert_eq!(
            dir, dir2,
            "profile dir path should be stable per session id"
        );
        assert!(
            !dir2.join("stale-cookie.txt").exists(),
            "stale profile data must be removed"
        );

        // Cleanup.
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `dispose_all` stops every tracked browser session and removes its disposable profile
    /// directory, even when the external `agent-browser` binary is not present (the `stop()` path
    /// deletes the record and temp dir regardless of whether `agent-browser close` succeeded).
    #[tokio::test]
    async fn dispose_all_closes_every_session_and_removes_profile_dirs() {
        let base =
            std::env::temp_dir().join(format!("dotz-browser-dispose-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();

        let sid1 = "dotz-dispose-1";
        let sid2 = "dotz-dispose-2";
        let p1 = base.join("p1");
        let p2 = base.join("p2");
        tokio::fs::create_dir_all(&p1).await.unwrap();
        tokio::fs::create_dir_all(&p2).await.unwrap();

        {
            let mut store = sessions_guard();
            store.insert(
                sid1.to_string(),
                SessionRecord {
                    profile_dir: p1.clone(),
                    observation: fake_observation(sid1),
                    frame_data: None,
                    disposed: false,
                },
            );
            store.insert(
                sid2.to_string(),
                SessionRecord {
                    profile_dir: p2.clone(),
                    observation: fake_observation(sid2),
                    frame_data: None,
                    disposed: false,
                },
            );
        }

        dispose_all().await;

        assert!(
            sessions_guard().is_empty(),
            "dispose_all should remove every browser session from the store"
        );
        assert!(
            !p1.exists(),
            "dispose_all should remove the first profile directory"
        );
        assert!(
            !p2.exists(),
            "dispose_all should remove the second profile directory"
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    fn fake_observation(session_id: &str) -> BrowserObservation {
        BrowserObservation {
            schema_version: VERSION,
            session_id: session_id.to_string(),
            seq: 0,
            status: "ready".into(),
            owner: BrowserOwner {
                app: "dotz".into(),
                project_id: "test-project".into(),
                workflow_id: None,
                step_id: None,
            },
            started_at: now_iso(),
            updated_at: now_iso(),
            page: Page {
                url: "https://example.com".into(),
                title: "Example".into(),
                viewport: Viewport {
                    width: 1280,
                    height: 800,
                },
            },
            allowed_origins: vec!["https://example.com".into()],
            refs: vec![],
            elements: vec![],
            snapshot: "".into(),
            current_action: None,
            cursor: None,
            frame: None,
            counters: Counters {
                actions: 0,
                console_errors: 0,
                network_errors: 0,
            },
            console_errors: vec![],
            network_errors: vec![],
            error: None,
        }
    }

    /// A panic while holding the browser sessions mutex must not permanently brick the browser
    /// controller. With poison recovery, read-only queries (`state`, `list`) and lookups (`frame`)
    /// keep working after a previous lock owner panicked.
    #[test]
    fn browser_sessions_recover_from_poisoned_mutex() {
        // Poison the global sessions mutex by panicking while holding the lock.
        let poison_thread = std::thread::spawn(|| {
            let _guard = sessions().lock().unwrap();
            panic!("intentional browser sessions poison");
        });
        assert!(
            poison_thread.join().is_err(),
            "panic must leave the sessions mutex poisoned"
        );

        // These calls used to panic on `lock().unwrap()`; now they recover and return gracefully.
        let _ = state(None);
        let _ = list();
        assert!(frame("no-such", -1).is_none());
    }
}
