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
//! Linux sessions run commands inside one persistent, per-session PID namespace. Its PID-1
//! supervisor survives successful one-shot commands; stopping the pinned supervisor tears down
//! reparented browser processes without an image-name sweep. Windows/macOS retain weaker native
//! process-tree/group cleanup and do not claim containment of daemonized descendants.
use axum::{
    Json, Router,
    extract::Query,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(target_os = "linux")]
use tokio::io::AsyncWriteExt;

const VERSION: u32 = 1;
const MAX_OUTPUT: i64 = 50_000;
// 75s (not the oracle's 35s): the COLD first Chrome launch on a fresh profile can take ~40-50s here;
// subsequent commands hit the warm agent-browser daemon and return fast.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(75);
/// Timeout for `agent-browser close`. Linux then tears down the persistent namespace even when
/// close fails; weaker platforms retain the retry record.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const SCOPE_START_TIMEOUT: Duration = Duration::from_secs(5);
const SCOPE_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Configurable wall-clock timeout for each agent-browser command. A hung command (e.g. a
/// crashed Chrome that never returns) otherwise blocks the controller for 75s. Defaults to 75s;
/// override with `DOTZ_BROWSER_TIMEOUT_MS` (clamped to [1s, 5m]).
fn command_timeout() -> Duration {
    const MIN_MS: u64 = 1_000; // 1 second — zero would time out before any command starts.
    const MAX_MS: u64 = 300_000; // 5 minutes — anything larger defeats the purpose of the cap.
    std::env::var("DOTZ_BROWSER_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT)
}

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
    let ms = total_ms % 1000;
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
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
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
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(p) = port.parse::<u16>()
    {
        if p == default_port {
            return Ok(format!("{scheme}://{host}"));
        }
        return Ok(format!("{scheme}://{host}:{p}"));
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
#[cfg(target_os = "linux")]
struct LinuxBrowserScope {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    process: crate::sandbox::OwnedProcess,
    profile_dir: PathBuf,
    ready: bool,
    namespace: Option<String>,
}

struct SessionRecord {
    profile_dir: PathBuf,
    observation: BrowserObservation,
    frame_data: Option<Vec<u8>>,
    /// Serializes whole start/act/stop operations, not merely individual CLI invocations.
    operation: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Set when stop() begins; commands other than its own close are rejected afterward.
    disposed: bool,
    /// True only after the owned Linux namespace has exited (or no command ever started it).
    scope_terminated: bool,
    #[cfg(target_os = "linux")]
    scope_started: bool,
    #[cfg(target_os = "linux")]
    scope_poisoned: bool,
    #[cfg(target_os = "linux")]
    scope_namespace: Option<String>,
    #[cfg(target_os = "linux")]
    scope: std::sync::Arc<tokio::sync::Mutex<Option<LinuxBrowserScope>>>,
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

fn session_operation(session_id: &str) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>, String> {
    sessions_guard()
        .get(session_id)
        .map(|record| record.operation.clone())
        .ok_or_else(|| "no such browser session".into())
}

#[cfg(target_os = "linux")]
fn mark_scope_terminated(session_id: &str, message: &str) {
    let mut store = sessions_guard();
    if let Some(record) = store.get_mut(session_id) {
        record.scope_terminated = true;
        record.scope_poisoned = true;
        record.disposed = true;
        record.observation.status = "error".into();
        record.observation.seq += 1;
        record.observation.updated_at = now_iso();
        record.observation.error = Some(ObsError {
            code: "BROWSER_SCOPE_TERMINATED".into(),
            message: message.to_string(),
            retryable: true,
        });
    }
}

// ---- executable resolution (mirror executableCandidates) ----
/// The agent-browser binary name for this target. win32-x64 is the shipped target, but the
/// stubs now resolve the right per-platform name so a future macOS/Linux build can find the
/// matching binary in the bundled `agent-browser/bin/` directory.
pub fn binary_name() -> &'static str {
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
    if let Ok(p) = std::env::var("DOTZ_BROWSER_BIN")
        && !p.trim().is_empty()
    {
        return Ok(PathBuf::from(p));
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

/// True when the resolved `agent-browser` binary is actually present on disk. Used by the
/// first-run wizard (`GET /api/first-run/state`) so the UI can tell the operator whether the
/// browser panel will work out of the box or whether they need to run
/// `npm run install:deps` first.
/// Mirrors [`resolve_executable`] exactly: `DOTZ_BROWSER_BIN` wins (must exist), then the
/// bundled `node_modules/agent-browser/bin/<name>` path, then a PATH lookup for the bare
/// `agent-browser[.exe]` name (so a system install is recognized).
pub fn binary_present() -> bool {
    match resolve_executable() {
        // An explicit DOTZ_BROWSER_BIN points at a specific file; honor it only if it exists.
        Ok(path)
            if std::env::var("DOTZ_BROWSER_BIN")
                .map(|v| !v.is_empty())
                .unwrap_or(false) =>
        {
            path.exists()
        }
        // The bundled path is already existence-checked inside resolve_executable; the PATH
        // fallback returns a bare name that we must still resolve against $PATH.
        Ok(path) => {
            if path.is_absolute() || path.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
                path.exists()
            } else {
                lookup_in_path(&path)
            }
        }
        Err(_) => false,
    }
}

/// Best-effort `$PATH` lookup for a bare executable name (mirrors `which`/`where`). Returns true
/// if the name resolves to an existing file in any `PATH` entry. On Windows, `.exe` is tried
/// both as-given and with an explicit `.exe` suffix (the bare fallback is already `.exe`-suffixed,
/// so this is mostly defensive for a future Unix/Windows cross-call).
fn lookup_in_path(name: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.exists() {
            return true;
        }
        #[cfg(windows)]
        {
            if name.extension().is_none() {
                let with_exe = candidate.with_extension("exe");
                if with_exe.exists() {
                    return true;
                }
            }
        }
    }
    false
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
        if let Some(start) = descriptor.find(q)
            && let Some(end_rel) = descriptor[start + 1..].find(q)
        {
            let role = descriptor[..start].trim().to_string();
            let name = descriptor[start + 1..start + 1 + end_rel].to_string();
            return (role, name);
        }
    }
    (descriptor.trim().to_string(), String::new())
}

#[cfg(target_os = "linux")]
impl LinuxBrowserScope {
    const SUPERVISOR: &'static str = r#"
umask 077
dir=$1
: > "$dir/scope.ready"
while IFS= read -r request; do
    [ "$request" = stop ] && exit 0
    case "$request" in
        *[!0-9a-f-]*|'') exit 64 ;;
    esac
    base="$dir/request-$request"
    xargs -0 -a "$base.argv" sh -c 'exec "$@"' dotz-browser-scope > "$base.out" 2> "$base.err"
    result=$?
    printf '%s\n' "$result" > "$base.status.tmp" || exit 65
    mv "$base.status.tmp" "$base.status" || exit 66
done
"#;

    async fn spawn(profile_dir: &Path) -> Result<Self, String> {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            Self::SUPERVISOR.to_string(),
            "dotz-browser-scope".to_string(),
            profile_dir.to_string_lossy().into_owned(),
        ];
        let wrapped = crate::sandbox::wrap_sandbox_argv(argv)?;
        let mut command = tokio::process::Command::new(&wrapped[0]);
        command
            .args(&wrapped[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(profile_dir.join("scope.err"))
                    .map_err(|e| format!("browser scope log: {e}"))?,
            ))
            .env("AGENT_BROWSER_HEADED", "false");
        crate::sandbox::backend().prepare_command(&mut command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("browser scope spawn failed: {e}"))?;
        let process = match crate::sandbox::OwnedProcess::pin(child.id()) {
            Ok(process) => process,
            Err(reason) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(format!("browser scope identity tracking failed: {reason}"));
            }
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("browser scope stdin unavailable".to_string());
        };
        Ok(Self {
            child,
            stdin,
            process,
            profile_dir: profile_dir.to_path_buf(),
            ready: false,
            namespace: None,
        })
    }

    #[cfg(target_os = "linux")]
    fn child_pid_namespace(pid: u32) -> Result<String, String> {
        let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .map_err(|e| format!("read browser scope children: {e}"))?;
        for child in children.split_whitespace() {
            let status = std::fs::read_to_string(format!("/proc/{child}/status"))
                .map_err(|e| format!("read browser namespace init status: {e}"))?;
            let nspid = status
                .lines()
                .find(|line| line.starts_with("NSpid:"))
                .unwrap_or_default()
                .split_whitespace()
                .last();
            if nspid != Some("1") {
                continue;
            }
            let namespace = std::fs::read_link(format!("/proc/{child}/ns/pid"))
                .map_err(|e| format!("read browser PID namespace identity: {e}"))?;
            return Ok(namespace.to_string_lossy().into_owned());
        }
        Err("browser PID namespace init was not observable".into())
    }

    async fn wait_ready(&mut self) -> Result<(), String> {
        if self.ready {
            return Ok(());
        }
        let ready_path = self.profile_dir.join("scope.ready");
        let deadline = tokio::time::Instant::now() + SCOPE_START_TIMEOUT;
        loop {
            if tokio::fs::metadata(&ready_path).await.is_ok() {
                self.namespace = Some(Self::child_pid_namespace(self.process.pid())?);
                self.ready = true;
                return Ok(());
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("browser scope status failed: {e}"))?
            {
                let details = tokio::fs::read_to_string(self.profile_dir.join("scope.err"))
                    .await
                    .unwrap_or_default();
                return Err(format!(
                    "browser scope exited before ready ({status}): {}",
                    details.trim()
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("browser scope startup timed out".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn execute(&mut self, argv: &[String]) -> Result<Value, String> {
        self.wait_ready().await?;
        let request = uuid::Uuid::new_v4().simple().to_string();
        let base = self.profile_dir.join(format!("request-{request}"));
        let argv_path = base.with_extension("argv");
        let output_path = base.with_extension("out");
        let error_path = base.with_extension("err");
        let status_path = base.with_extension("status");
        let mut encoded = Vec::new();
        for arg in argv {
            if arg.as_bytes().contains(&0) {
                return Err("browser command contains a NUL byte".into());
            }
            encoded.extend_from_slice(arg.as_bytes());
            encoded.push(0);
        }
        tokio::fs::write(&argv_path, encoded)
            .await
            .map_err(|e| format!("browser command arguments: {e}"))?;
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|e| format!("browser scope request: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("browser scope request: {e}"))?;

        loop {
            if let Ok(status) = tokio::fs::read_to_string(&status_path).await {
                let code = status
                    .trim()
                    .parse::<i32>()
                    .map_err(|e| format!("invalid browser scope status: {e}"))?;
                let stdout = tokio::fs::read_to_string(&output_path)
                    .await
                    .unwrap_or_default();
                let stderr = tokio::fs::read_to_string(&error_path)
                    .await
                    .unwrap_or_default();
                if code != 0 {
                    let message = if !stderr.trim().is_empty() {
                        stderr.trim()
                    } else if !stdout.trim().is_empty() {
                        stdout.trim()
                    } else {
                        "agent-browser command failed"
                    };
                    for path in [&argv_path, &output_path, &error_path, &status_path] {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    return Err(message.chars().take(1000).collect());
                }
                for path in [&argv_path, &output_path, &error_path, &status_path] {
                    let _ = tokio::fs::remove_file(path).await;
                }
                return Ok(parse_json_output(&stdout));
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("browser scope status failed: {e}"))?
            {
                return Err(format!(
                    "browser session scope exited during command ({status})"
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn terminate(&mut self) -> Result<(), String> {
        let _ = self.stdin.shutdown().await;
        let process = self.process.clone();
        let kill =
            tokio::task::spawn_blocking(move || crate::sandbox::backend().kill_tree(&process))
                .await
                .unwrap_or(Err("browser scope cleanup worker failed"));
        if kill.is_err() {
            // Child remains unreaped, so its handle still names this exact process. This fallback
            // asks the pinned namespace wrapper to exit; --kill-child=KILL tears down PID 1.
            let _ = self.child.start_kill();
        }
        match tokio::time::timeout(SCOPE_STOP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("browser scope wait failed: {e}")),
            Err(_) => Err(format!(
                "browser scope teardown unverified (pinned cleanup: {})",
                kill.err().unwrap_or("wait timed out")
            )),
        }
    }
}

// ---- the one-shot command runner ----
/// Spawn agent-browser ONE-SHOT with the exact security flags + AGENT_BROWSER_HEADED=false, a
/// bounded timeout, capture stdout/stderr, cancel the owned process tree on timeout, and parse
/// stdout as JSON. On Linux cleanup reaches live descendants still in the parent tree, including
/// `setsid` children; it does not sweep daemonized/reparented browser processes. `command` is the
/// trailing `--json <command...>` portion.
async fn run(
    session_id: &str,
    profile_dir: &Path,
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

    #[cfg(target_os = "linux")]
    {
        let scope = sessions_guard()
            .get(session_id)
            .map(|record| record.scope.clone())
            .ok_or("no such browser session")?;
        let mut scope = scope.lock().await;
        if scope.is_none() {
            *scope = Some(LinuxBrowserScope::spawn(profile_dir).await?);
            if let Some(record) = sessions_guard().get_mut(session_id) {
                record.scope_started = true;
            }
        }
        if let Err(error) = scope
            .as_mut()
            .expect("scope was initialized")
            .wait_ready()
            .await
        {
            if let Some(record) = sessions_guard().get_mut(session_id) {
                record.scope_poisoned = true;
            }
            return Err(error);
        }
        let namespace = scope
            .as_ref()
            .and_then(|active| active.namespace.clone())
            .ok_or("browser PID namespace identity unavailable")?;
        if let Some(record) = sessions_guard().get_mut(session_id) {
            record.scope_namespace = Some(namespace);
        }
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(executable.to_string_lossy().into_owned());
        argv.extend(args);
        let outcome = tokio::time::timeout(
            command_timeout(),
            scope
                .as_mut()
                .expect("scope was initialized")
                .execute(&argv),
        )
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => {
                let message = match scope
                    .as_mut()
                    .expect("scope was initialized")
                    .terminate()
                    .await
                {
                    Ok(()) => {
                        scope.take();
                        mark_scope_terminated(
                            session_id,
                            "browser command timed out; session scope was terminated",
                        );
                        "agent-browser command timed out; session scope terminated".to_string()
                    }
                    Err(reason) => {
                        let mut store = sessions_guard();
                        if let Some(record) = store.get_mut(session_id) {
                            record.disposed = true;
                            record.scope_poisoned = true;
                            record.observation.status = "error".into();
                            record.observation.seq += 1;
                            record.observation.updated_at = now_iso();
                            record.observation.error = Some(ObsError {
                                code: "BROWSER_CLEANUP_INCOMPLETE".into(),
                                message: reason.clone(),
                                retryable: true,
                            });
                        }
                        format!("agent-browser command timed out; cleanup incomplete: {reason}")
                    }
                };
                Err(message)
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
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
        // Windows uses CREATE_NO_WINDOW; macOS uses a process group and does not claim daemon containment.
        crate::sandbox::backend().prepare_command(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("agent-browser spawn failed: {e}"))?;
        let process = match crate::sandbox::OwnedProcess::pin(child.id()) {
            Ok(process) => process,
            Err(reason) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return Err(format!("agent-browser process tracking failed: {reason}"));
            }
        };
        let mut process_tree_guard = ProcessTreeGuard::new(process.clone());

        let status = match tokio::time::timeout(command_timeout(), child.wait()).await {
            Err(_) => {
                let cleanup = match kill_process(Some(process.clone())) {
                    Some(kill_task) => kill_task.await.unwrap_or(Err("cleanup worker failed")),
                    None => Err("spawned process identity unavailable"),
                };
                let _ = child.start_kill();
                let _ = child.wait().await;
                process_tree_guard.disarm();
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return Err(match cleanup {
                    Ok(()) => "agent-browser command timed out".into(),
                    Err(reason) => {
                        format!("agent-browser command timed out; cleanup incomplete: {reason}")
                    }
                });
            }
            Ok(status) => {
                if status.is_ok() {
                    process_tree_guard.disarm();
                }
                status
            }
        };
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
}

/// Kill a pid and its owned descendants via the platform backend. The blocking backend runs off
/// the Tokio worker. Explicit command timeouts await its completion; the guard below detaches it
/// on future cancellation (for example, stop() timing out run()).
#[cfg(not(target_os = "linux"))]
fn kill_process(
    process: Option<crate::sandbox::OwnedProcess>,
) -> Option<tokio::task::JoinHandle<Result<(), &'static str>>> {
    let process = process?;
    Some(tokio::task::spawn_blocking(move || {
        let result = crate::sandbox::backend().kill_tree(&process);
        if let Err(reason) = result {
            tracing::warn!(
                pid = process.pid(),
                reason,
                "browser process cleanup incomplete"
            );
        }
        result
    }))
}

/// A dropped `run()` future must not orphan the spawned process tree. `Child` itself does not
/// kill descendants on drop, so schedule owned-tree cleanup unless normal completion or the
/// explicit timeout path disarms this guard.
#[cfg(not(target_os = "linux"))]
struct ProcessTreeGuard {
    process: Option<crate::sandbox::OwnedProcess>,
}

#[cfg(not(target_os = "linux"))]
impl ProcessTreeGuard {
    fn new(process: crate::sandbox::OwnedProcess) -> Self {
        Self {
            process: Some(process),
        }
    }

    fn disarm(&mut self) {
        self.process = None;
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        let _ = kill_process(self.process.take());
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
    let operation = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    sessions_guard().insert(
        session_id.clone(),
        SessionRecord {
            profile_dir: profile_dir.clone(),
            observation,
            frame_data: None,
            operation: operation.clone(),
            disposed: false,
            scope_terminated: false,
            #[cfg(target_os = "linux")]
            scope_started: false,
            #[cfg(target_os = "linux")]
            scope_poisoned: false,
            #[cfg(target_os = "linux")]
            scope_namespace: None,
            #[cfg(target_os = "linux")]
            scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        },
    );
    let _operation_guard = operation.lock().await;

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
            match stop_locked(&session_id).await {
                Ok(_) => Err(e),
                Err(cleanup) => Err(format!("{e}; cleanup incomplete: {cleanup}")),
            }
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
    let operation = session_operation(&session_id)?;
    let _operation_guard = operation.lock().await;

    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("action is required")?
        .to_string();

    let (profile_dir, allowed_origins, cur_seq, refs, viewport, status, disposed) = {
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
            r.disposed,
        )
    };
    if disposed || status == "stopped" {
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
            if let (Some(bx), Some(by), Some(bw), Some(bh)) = (bx, by, bw, bh)
                && [bx, by, bw, bh].iter().all(|f| f.is_finite())
            {
                let mut store = sessions_guard();
                if let Some(r) = store.get_mut(&session_id) {
                    r.observation.cursor = Some(Cursor {
                        x: bx + bw / 2.0,
                        y: by + bh / 2.0,
                        kind: action.clone(),
                    });
                }
            }
        } else if action == "clickAt"
            && let (Some(x), Some(y)) = (x, y)
            && x.is_finite()
            && y.is_finite()
        {
            let mut store = sessions_guard();
            if let Some(r) = store.get_mut(&session_id) {
                r.observation.cursor = Some(Cursor {
                    x,
                    y,
                    kind: action.clone(),
                });
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

/// Mark a failed stop while preserving the record/profile so a later stop can retry cleanup.
fn mark_cleanup_error(session_id: &str, message: &str) {
    let mut store = sessions_guard();
    if let Some(record) = store.get_mut(session_id) {
        record.disposed = true;
        record.observation.status = "error".into();
        record.observation.seq += 1;
        record.observation.updated_at = now_iso();
        record.observation.error = Some(ObsError {
            code: "BROWSER_CLEANUP_INCOMPLETE".into(),
            message: message.to_string(),
            retryable: true,
        });
    }
}

/// Stop while the per-session operation mutex is held. Linux only removes the profile after the
/// pinned namespace supervisor has exited; other platforms retain the session if close fails.
async fn stop_locked(session_id: &str) -> Result<BrowserObservation, String> {
    let (profile_dir, allowed_origins, already_terminated, scope_started, scope_poisoned) = {
        let mut store = sessions_guard();
        let record = store.get_mut(session_id).ok_or("no such browser session")?;
        record.disposed = true;
        (
            record.profile_dir.clone(),
            record.observation.allowed_origins.clone(),
            record.scope_terminated,
            #[cfg(target_os = "linux")]
            record.scope_started,
            #[cfg(target_os = "linux")]
            record.scope_poisoned,
        )
    };

    #[cfg(target_os = "linux")]
    let close_error = if scope_started && !already_terminated && !scope_poisoned {
        match tokio::time::timeout(
            CLOSE_TIMEOUT,
            run(session_id, &profile_dir, &allowed_origins, &["close"]),
        )
        .await
        {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => Some(format!("agent-browser close failed: {error}")),
            Err(_) => Some("agent-browser close timed out".to_string()),
        }
    } else {
        None
    };

    #[cfg(target_os = "linux")]
    {
        let scope_arc = sessions_guard()
            .get(session_id)
            .map(|record| record.scope.clone())
            .ok_or("no such browser session")?;
        if scope_started && !already_terminated {
            let mut scope = scope_arc.lock().await;
            let Some(active_scope) = scope.as_mut() else {
                let message = "browser scope handle missing before teardown was verified";
                mark_cleanup_error(session_id, message);
                return Err(message.to_string());
            };
            if let Err(error) = active_scope.terminate().await {
                mark_cleanup_error(session_id, &error);
                return Err(format!("browser scope cleanup incomplete: {error}"));
            }
            scope.take();
        }
        if let Some(record) = sessions_guard().get_mut(session_id) {
            record.scope_terminated = true;
        }
    }

    #[cfg(not(target_os = "linux"))]
    let close_error = match tokio::time::timeout(
        CLOSE_TIMEOUT,
        run(session_id, &profile_dir, &allowed_origins, &["close"]),
    )
    .await
    {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(format!("agent-browser close failed: {error}")),
        Err(_) => Some("agent-browser close timed out; daemon cleanup is unverified".to_string()),
    };

    #[cfg(not(target_os = "linux"))]
    if let Some(error) = close_error.as_deref() {
        mark_cleanup_error(session_id, error);
        return Err(error.to_string());
    }

    if let Err(error) = tokio::fs::remove_dir_all(&profile_dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        let message = format!("browser profile cleanup incomplete: {error}");
        mark_cleanup_error(session_id, &message);
        return Err(message);
    }

    let mut stopped = {
        let mut store = sessions_guard();
        let record = store.get_mut(session_id).ok_or("no such browser session")?;
        record.observation.status = "stopped".into();
        record.observation.seq += 1;
        record.observation.updated_at = now_iso();
        record.observation.current_action = None;
        record.observation.frame = None;
        record.frame_data = None;
        record.observation.clone()
    };
    #[cfg(target_os = "linux")]
    if let Some(error) = close_error {
        stopped.error = Some(ObsError {
            code: "BROWSER_CLOSE_FAILED_SCOPE_TERMINATED".into(),
            message: format!("{error}; Linux browser scope was terminated"),
            retryable: false,
        });
        if let Some(record) = sessions_guard().get_mut(session_id) {
            record.observation.error = stopped.error.clone();
        }
    }
    sessions_guard().remove(session_id);
    Ok(stopped)
}

/// Stop a session and serialize teardown behind any in-flight command. Marking it disposed before
/// waiting on the mutex rejects newly arriving commands; the current operation is allowed to drain.
pub async fn stop(session_id: &str) -> Result<BrowserObservation, String> {
    let operation = session_operation(session_id)?;
    {
        let mut store = sessions_guard();
        let record = store.get_mut(session_id).ok_or("no such browser session")?;
        record.disposed = true;
    }
    let _operation_guard = operation.lock().await;
    stop_locked(session_id).await
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

/// Number of active browser sessions. Surfaced in `/api/health`.
pub fn session_count() -> usize {
    sessions_guard().len()
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

/// Dispose every browser session on application exit. Each `stop()` has bounded close and scope
/// teardown phases; do not wrap it in a shorter outer timeout that could cancel verified cleanup.
pub async fn dispose_all() {
    let ids: Vec<String> = sessions_guard().keys().cloned().collect();
    for id in ids {
        if let Err(error) = stop(&id).await {
            tracing::error!(session_id = %id, %error, "browser session cleanup incomplete during exit");
        }
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
        // Serialize with other tests that mutate the process-global browser env / sessions store
        // (e.g. the timeout tests that insert sessions and point DOTZ_BROWSER_BIN at a fake
        // script). Without this lock a concurrent test can insert a session after we collect
        // ids, leaving the store non-empty and failing the assertion.
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.lock().await;

        let base = std::env::temp_dir().join(format!(
            "dotz-browser-dispose-test-{}",
            uuid::Uuid::new_v4()
        ));
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
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            );
            store.insert(
                sid2.to_string(),
                SessionRecord {
                    profile_dir: p2.clone(),
                    observation: fake_observation(sid2),
                    frame_data: None,
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
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

    /// `stop()` must dispose the session and profile directory even when the `agent-browser close`
    /// command hangs. Before the fix, stop() waited for the full `command_timeout()` which let
    /// `dispose_all()`'s outer timeout drop the future and leave the session record behind.
    #[tokio::test]
    async fn stop_disposes_session_even_when_close_command_hangs() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.lock().await;

        let dir = std::env::temp_dir().join(format!(
            "dotz-browser-stop-hang-test-{}",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        #[cfg(unix)]
        let pidfile = dir.join("pid");
        #[cfg(unix)]
        let sleep_pidfile = dir.join("sleep-pid");
        #[cfg(target_os = "linux")]
        let unrelated_pidfile = dir.join("unrelated-pid");
        #[cfg(unix)]
        let _cleanup = TimeoutTestProcessCleanup {
            pid_files: vec![
                pidfile.clone(),
                sleep_pidfile.clone(),
                #[cfg(target_os = "linux")]
                unrelated_pidfile.clone(),
            ],
        };
        #[cfg(target_os = "linux")]
        let mut unrelated = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn unrelated control process");
        #[cfg(target_os = "linux")]
        std::fs::write(&unrelated_pidfile, unrelated.id().to_string()).unwrap();

        // Fake agent-browser binary: a shell launches a separate sleeper to verify tree cleanup.
        #[cfg(windows)]
        let script_path = {
            let bat = dir.join("fake-browser.bat");
            tokio::fs::write(&bat, "@echo off\nping -n 30 127.0.0.1 >nul\n")
                .await
                .unwrap();
            bat
        };
        #[cfg(unix)]
        let script_path = {
            let sh = dir.join("fake-browser.sh");
            tokio::fs::write(&sh, fake_browser_waiting_script(&pidfile, &sleep_pidfile))
                .await
                .unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&sh).await.unwrap().permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&sh, perms).await.unwrap();
            sh
        };

        let sid = format!("dotz-browser-stop-hang-{}", uuid::Uuid::new_v4());
        let profile_dir = dir.join("profile");
        tokio::fs::create_dir_all(&profile_dir).await.unwrap();

        {
            let mut store = sessions_guard();
            store.insert(
                sid.clone(),
                SessionRecord {
                    profile_dir: profile_dir.clone(),
                    observation: fake_observation(&sid),
                    frame_data: None,
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            );
        }

        #[cfg(target_os = "linux")]
        let scope_namespace = {
            let mut scope = LinuxBrowserScope::spawn(&profile_dir).await.unwrap();
            scope.wait_ready().await.unwrap();
            let namespace = scope.namespace.clone().expect("scope namespace is ready");
            let scope_handle = sessions_guard().get(&sid).unwrap().scope.clone();
            *scope_handle.lock().await = Some(scope);
            let mut record = sessions_guard();
            let record = record.get_mut(&sid).unwrap();
            record.scope_started = true;
            record.scope_namespace = Some(namespace.clone());
            namespace
        };
        let prev_bin = std::env::var("DOTZ_BROWSER_BIN").ok();
        let prev_timeout = std::env::var("DOTZ_BROWSER_TIMEOUT_MS").ok();
        // Use a huge command timeout so the only thing ending the close call is stop()'s own
        // CLOSE_TIMEOUT — if stop() relied on command_timeout() this test would take 75s.
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe {
            std::env::set_var(
                "DOTZ_BROWSER_BIN",
                script_path.to_string_lossy().to_string(),
            )
        };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", "300000") };

        let start = std::time::Instant::now();
        let stop_sid = sid.clone();
        let stop_task = tokio::spawn(async move { stop(&stop_sid).await });
        #[cfg(target_os = "linux")]
        {
            let (root, escaped) =
                wait_for_linux_process_pair(&pidfile, &sleep_pidfile, &scope_namespace)
                    .await
                    .unwrap();
            assert_ne!(
                escaped, root,
                "the stop-test descendant must have escaped the browser's process group and session"
            );
        }
        let result = stop_task.await.expect("browser stop task should not panic");
        let elapsed = start.elapsed();

        match prev_bin {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_BIN", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_BIN") },
        }
        match prev_timeout {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_TIMEOUT_MS") },
        }

        // stop() must complete well before the 5m command_timeout, bounded by CLOSE_TIMEOUT.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "stop() should not wait for the full command_timeout, elapsed: {elapsed:?}"
        );
        assert!(
            result.is_ok(),
            "stop() should succeed even when close hangs: {}",
            result.err().unwrap_or_default()
        );
        let obs = result.unwrap();
        assert_eq!(obs.status, "stopped");
        assert_eq!(obs.session_id, sid);

        assert!(
            sessions_guard().get(&sid).is_none(),
            "stop() should remove the session even when close hangs"
        );
        assert!(
            !profile_dir.exists(),
            "stop() should remove the profile dir even when close hangs"
        );
        #[cfg(target_os = "linux")]
        {
            let child_stopped = wait_for_process_exit(&sleep_pidfile, &scope_namespace).await;
            assert!(
                child_stopped.is_ok(),
                "stop() must terminate the fake browser's setsid-escaped descendant: {child_stopped:?}"
            );
            let unrelated_running = unrelated.try_wait().unwrap().is_none();
            let _ = std::fs::remove_file(&unrelated_pidfile);
            if unrelated_running {
                unrelated.kill().unwrap();
            }
            let _ = unrelated.wait();
            assert!(
                unrelated_running,
                "session stop must not signal an unrelated process"
            );
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Two simultaneous stop requests must serialize without duplicate teardown, and stopping
    /// one session must not remove the independent session record or its profile.
    #[tokio::test]
    async fn concurrent_stop_is_serialized_and_isolated_to_its_session() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!(
            "dotz-browser-concurrent-stop-test-{}",
            uuid::Uuid::new_v4()
        ));
        let profile_a = dir.join("profile-a");
        let profile_b = dir.join("profile-b");
        tokio::fs::create_dir_all(&profile_a).await.unwrap();
        tokio::fs::create_dir_all(&profile_b).await.unwrap();
        let sid_a = format!("dotz-browser-concurrent-a-{}", uuid::Uuid::new_v4());
        let sid_b = format!("dotz-browser-concurrent-b-{}", uuid::Uuid::new_v4());
        {
            let mut store = sessions_guard();
            for (sid, profile_dir) in [(&sid_a, &profile_a), (&sid_b, &profile_b)] {
                store.insert(
                    sid.clone(),
                    SessionRecord {
                        profile_dir: profile_dir.clone(),
                        observation: fake_observation(sid),
                        frame_data: None,
                        operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                        disposed: false,
                        scope_terminated: false,
                        #[cfg(target_os = "linux")]
                        scope_started: false,
                        #[cfg(target_os = "linux")]
                        scope_poisoned: false,
                        #[cfg(target_os = "linux")]
                        scope_namespace: None,
                        #[cfg(target_os = "linux")]
                        scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    },
                );
            }
        }

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let first = {
            let barrier = barrier.clone();
            let sid = sid_a.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                stop(&sid).await
            })
        };
        let second = {
            let barrier = barrier.clone();
            let sid = sid_a.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                stop(&sid).await
            })
        };
        barrier.wait().await;
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "exactly one concurrent stop owns teardown"
        );
        let stopped = first.or(second).unwrap();
        assert_eq!(stopped.status, "stopped");
        assert!(sessions_guard().get(&sid_a).is_none());
        assert!(!profile_a.exists());

        assert!(sessions_guard().contains_key(&sid_b));
        assert!(
            profile_b.exists(),
            "stopping session A must retain session B's profile"
        );
        assert_eq!(stop(&sid_b).await.unwrap().status, "stopped");
        assert!(sessions_guard().get(&sid_b).is_none());
        assert!(!profile_b.exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// SIGKILL of the browser-session owner must close the supervisor's control pipe and tear
    /// down a browser daemon that was already reparented to the namespace init.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn scope_owner_death_reaps_reparented_browser_process() {
        const OWNER_TEST_DIR: &str = "DOTZ_BROWSER_SCOPE_OWNER_TEST_DIR";
        let test_dir = if let Some(path) = std::env::var_os(OWNER_TEST_DIR) {
            let dir = std::path::PathBuf::from(path);
            let namespace_file = dir.join("namespace");
            let daemon_pid_file = dir.join("daemon.pid");
            let mut scope = LinuxBrowserScope::spawn(&dir).await.unwrap();
            scope.wait_ready().await.unwrap();
            std::fs::write(
                &namespace_file,
                scope.namespace.as_deref().expect("namespace captured"),
            )
            .unwrap();
            let script = format!(
                "sleep 60 >/dev/null 2>&1 & echo $! > {}",
                daemon_pid_file.display()
            );
            scope
                .execute(&["/bin/sh".into(), "-c".into(), script])
                .await
                .unwrap();
            std::mem::forget(scope);
            // Bypass destructors: the parent process really exits while the scope is live.
            std::process::exit(0);
        } else {
            std::env::temp_dir().join(format!(
                "dotz-browser-owner-death-test-{}",
                uuid::Uuid::new_v4()
            ))
        };

        tokio::fs::create_dir_all(&test_dir).await.unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser::tests::scope_owner_death_reaps_reparented_browser_process",
                "--nocapture",
            ])
            .env(OWNER_TEST_DIR, &test_dir)
            .output()
            .expect("spawn isolated owner-death helper");
        assert!(
            child.status.success(),
            "owner-death helper failed: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let namespace = std::fs::read_to_string(test_dir.join("namespace")).unwrap();
        let daemon_stopped = wait_for_process_exit(&test_dir.join("daemon.pid"), &namespace).await;
        assert!(
            daemon_stopped.is_ok(),
            "owner exit must kill the reparented browser process: {daemon_stopped:?}"
        );
        assert!(!test_dir.join("profile").exists());
        let _ = tokio::fs::remove_dir_all(&test_dir).await;
    }

    /// `session_count` must reflect the number of active browser sessions so the `/api/health`
    /// endpoint can surface live browser activity to the operator.
    ///
    /// Serialized under `BROWSER_TIMEOUT_TEST_LOCK`: the session store is process-global, and
    /// every other test that mutates it holds this lock. Without it, a parallel test's
    /// insert/remove lands between our baseline read and the `baseline + 2` assert (observed
    /// flake: left 3 / right 2), and our inserted sessions break their `is_empty` asserts.
    #[test]
    fn session_count_reflects_active_sessions() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.blocking_lock();
        let baseline = session_count();
        let sid1 = format!("dotz-count-1-{}", uuid::Uuid::new_v4());
        let sid2 = format!("dotz-count-2-{}", uuid::Uuid::new_v4());
        let dir =
            std::env::temp_dir().join(format!("dotz-browser-count-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        {
            let mut store = sessions_guard();
            store.insert(
                sid1.clone(),
                SessionRecord {
                    profile_dir: dir.join("p1"),
                    observation: fake_observation(&sid1),
                    frame_data: None,
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            );
            store.insert(
                sid2.clone(),
                SessionRecord {
                    profile_dir: dir.join("p2"),
                    observation: fake_observation(&sid2),
                    frame_data: None,
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            );
        }

        assert_eq!(
            session_count(),
            baseline + 2,
            "session_count should include the inserted browser sessions"
        );

        {
            let mut store = sessions_guard();
            store.remove(&sid1);
            store.remove(&sid2);
        }
        assert_eq!(
            session_count(),
            baseline,
            "session_count should return to baseline after removal"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

    /// Serialize tests that mutate the process-global `DOTZ_BROWSER_TIMEOUT_MS` and
    /// `DOTZ_BROWSER_BIN` env vars so concurrent browser timeout tests do not race.
    static BROWSER_TIMEOUT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(unix)]
    struct TimeoutTestProcessCleanup {
        pid_files: Vec<std::path::PathBuf>,
    }

    #[cfg(unix)]
    impl Drop for TimeoutTestProcessCleanup {
        fn drop(&mut self) {
            for path in &self.pid_files {
                let Ok(contents) = std::fs::read_to_string(path) else {
                    continue;
                };
                let Ok(pid) = contents.trim().parse::<u32>() else {
                    continue;
                };
                let pid = pid.to_string();
                let _ = std::process::Command::new("kill")
                    .args(["-9", pid.as_str()])
                    .status();
            }
        }
    }

    #[cfg(unix)]
    fn fake_browser_waiting_script(
        pid_file: &std::path::Path,
        escaped_child_pid_file: &std::path::Path,
    ) -> String {
        #[cfg(target_os = "linux")]
        let child_command = format!(
            "setsid sh -c 'sleep 30 >/dev/null 2>&1 & echo $! > \"{}\"' & child=$!\necho \"$child\" > \"{}.launcher\"\nwait \"$child\"\nsleep 30\n",
            escaped_child_pid_file.to_string_lossy(),
            escaped_child_pid_file.to_string_lossy(),
        );
        #[cfg(not(target_os = "linux"))]
        let child_command = format!(
            "sleep 30 & child=$!\necho \"$child\" > \"{}\"\nwait \"$child\"\n",
            escaped_child_pid_file.to_string_lossy(),
        );
        format!(
            "#!/bin/sh\necho $$ > \"{}\"\n{}",
            pid_file.to_string_lossy(),
            child_command,
        )
    }

    #[cfg(target_os = "linux")]
    fn linux_host_pid(namespace: &str, namespace_pid: u32) -> Result<Option<u32>, String> {
        let processes = std::fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))?;
        for entry in processes.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let ns_link = format!("/proc/{pid}/ns/pid");
            if std::fs::read_link(ns_link)
                .map(|link| link.to_string_lossy() == namespace)
                .unwrap_or(false)
                && std::fs::read_to_string(format!("/proc/{pid}/status"))
                    .ok()
                    .and_then(|status| {
                        status.lines().find_map(|line| {
                            let values = line.strip_prefix("NSpid:")?;
                            values
                                .split_whitespace()
                                .last()
                                .and_then(|value| value.parse::<u32>().ok())
                        })
                    })
                    == Some(namespace_pid)
            {
                return Ok(Some(pid));
            }
        }
        Ok(None)
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_session_namespace(session_id: &str) -> Result<String, String> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(namespace) = sessions_guard()
                    .get(session_id)
                    .and_then(|record| record.scope_namespace.clone())
                {
                    return Ok(namespace);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "browser session PID namespace did not become ready".to_owned())?
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_process_exit(
        pid_file: &std::path::Path,
        namespace: &str,
    ) -> Result<(), String> {
        let contents = std::fs::read_to_string(pid_file)
            .map_err(|e| format!("could not read timeout-test pid file: {e}"))?;
        let pid = contents
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid timeout-test pid {contents:?}: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match linux_host_pid(namespace, pid)? {
                Some(host_pid) => {
                    let stat_path = format!("/proc/{host_pid}/stat");
                    match std::fs::read_to_string(&stat_path) {
                        Ok(stat) => {
                            let state = stat
                                .rsplit_once(')')
                                .and_then(|(_, rest)| rest.split_whitespace().next());
                            if matches!(state, Some("Z" | "X")) {
                                return Ok(());
                            }
                        }
                        Err(_) => return Ok(()),
                    }
                }
                None => return Ok(()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("timed-out child process {pid} remained live"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_session_group(
        pid_file: &std::path::Path,
        namespace: &str,
    ) -> Result<(u32, u32), String> {
        let contents = std::fs::read_to_string(pid_file)
            .map_err(|e| format!("could not read browser-test pid file: {e}"))?;
        let pid = contents
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid browser-test pid {contents:?}: {e}"))?;
        let pid = linux_host_pid(namespace, pid)?
            .ok_or_else(|| format!("browser-test pid {pid} is not observable in {namespace}"))?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|e| format!("could not read process {pid} stat: {e}"))?;
        let (_, fields) = stat
            .rsplit_once(") ")
            .ok_or_else(|| format!("malformed process {pid} stat"))?;
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let group = fields
            .get(2)
            .ok_or_else(|| format!("missing process group for pid {pid}"))?
            .parse::<u32>()
            .map_err(|e| format!("invalid process group for pid {pid}: {e}"))?;
        let session = fields
            .get(3)
            .ok_or_else(|| format!("missing session for pid {pid}"))?
            .parse::<u32>()
            .map_err(|e| format!("invalid session for pid {pid}: {e}"))?;
        Ok((group, session))
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_linux_process_pair(
        root_file: &std::path::Path,
        descendant_file: &std::path::Path,
        namespace: &str,
    ) -> Result<((u32, u32), (u32, u32)), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let (Ok(root), Ok(descendant)) = (
                linux_session_group(root_file, namespace),
                linux_session_group(descendant_file, namespace),
            ) {
                return Ok((root, descendant));
            }
            if std::time::Instant::now() >= deadline {
                return Err("browser-test processes did not become observable".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A timed-out agent-browser command must cancel its live owned descendants, including a
    /// Linux child that calls `setsid` and escapes the command's process group. An unrelated
    /// sleeper acts as a negative control and must remain alive after cleanup.
    #[tokio::test]
    async fn run_reaps_child_after_timeout() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.lock().await;

        let dir = std::env::temp_dir().join(format!(
            "dotz-browser-timeout-test-{}",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        #[cfg(unix)]
        let pidfile = dir.join("pid");
        #[cfg(unix)]
        let sleep_pidfile = dir.join("sleep-pid");
        #[cfg(unix)]
        let unrelated_pidfile = dir.join("unrelated-pid");
        #[cfg(unix)]
        let _cleanup = TimeoutTestProcessCleanup {
            pid_files: vec![
                pidfile.clone(),
                sleep_pidfile.clone(),
                unrelated_pidfile.clone(),
            ],
        };
        #[cfg(target_os = "linux")]
        let mut unrelated = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn unrelated control process");
        #[cfg(target_os = "linux")]
        std::fs::write(&unrelated_pidfile, unrelated.id().to_string()).unwrap();

        // Fake agent-browser binary: a shell launches a separate sleeper to verify tree cleanup.
        #[cfg(windows)]
        let script_path = {
            let bat = dir.join("fake-browser.bat");
            tokio::fs::write(&bat, "@echo off\nping -n 30 127.0.0.1 >nul\n")
                .await
                .unwrap();
            bat
        };
        #[cfg(unix)]
        let script_path = {
            let sh = dir.join("fake-browser.sh");
            tokio::fs::write(&sh, fake_browser_waiting_script(&pidfile, &sleep_pidfile))
                .await
                .unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&sh).await.unwrap().permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&sh, perms).await.unwrap();
            sh
        };

        let sid = format!("dotz-browser-timeout-{}", uuid::Uuid::new_v4());
        let profile_dir = dir.join("profile");
        tokio::fs::create_dir_all(&profile_dir).await.unwrap();
        let allowed = vec!["https://example.com".into()];

        {
            let mut store = sessions_guard();
            store.insert(
                sid.clone(),
                SessionRecord {
                    profile_dir: profile_dir.clone(),
                    observation: fake_observation(&sid),
                    frame_data: None,
                    operation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    disposed: false,
                    scope_terminated: false,
                    #[cfg(target_os = "linux")]
                    scope_started: false,
                    #[cfg(target_os = "linux")]
                    scope_poisoned: false,
                    #[cfg(target_os = "linux")]
                    scope_namespace: None,
                    #[cfg(target_os = "linux")]
                    scope: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            );
        }

        let prev_bin = std::env::var("DOTZ_BROWSER_BIN").ok();
        let prev_timeout = std::env::var("DOTZ_BROWSER_TIMEOUT_MS").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe {
            std::env::set_var(
                "DOTZ_BROWSER_BIN",
                script_path.to_string_lossy().to_string(),
            )
        };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", "500") };

        let start = std::time::Instant::now();
        let task_sid = sid.clone();
        let task_profile_dir = profile_dir.clone();
        let task_allowed = allowed.clone();
        let run_task = tokio::spawn(async move {
            run(&task_sid, &task_profile_dir, &task_allowed, &["get", "url"]).await
        });
        #[cfg(target_os = "linux")]
        let scope_namespace = wait_for_session_namespace(&sid).await.unwrap();
        #[cfg(target_os = "linux")]
        {
            let (root, escaped) =
                wait_for_linux_process_pair(&pidfile, &sleep_pidfile, &scope_namespace)
                    .await
                    .unwrap();
            assert_ne!(
                escaped, root,
                "the timeout-test descendant must have escaped the browser's process group and session"
            );
        }
        let result = run_task
            .await
            .expect("browser timeout task should not panic");
        let elapsed = start.elapsed();

        match prev_bin {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_BIN", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_BIN") },
        }
        match prev_timeout {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_TIMEOUT_MS") },
        }

        assert!(
            result.is_err(),
            "timed-out browser command must return an error: {result:?}"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("timed out"),
            "error should mention timeout, got: {err}"
        );
        // The configured timeout (clamped up to the 1s floor) must fire instead of the 75s default;
        // any bound far below 75s proves that. The margin above ~1s absorbs the Windows kill path
        // (taskkill launch, dispatched off-thread) and scheduling delay under a saturated suite,
        // which made a tight 3s bound flaky without indicating a regression.
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "browser timeout should fire on the configured short timeout, not the 75s default, elapsed: {elapsed:?}"
        );

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let pid_text = std::fs::read_to_string(&pidfile).unwrap_or_default();
            let pid: i32 = pid_text.trim().parse().unwrap_or(-1);
            assert!(pid > 0, "test should have captured a valid child pid");
            let gone = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true);
            assert!(
                gone,
                "timed-out browser child (pid {pid}) should have been killed and reaped, not still running"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let root_stopped = wait_for_process_exit(&pidfile, &scope_namespace).await;
            assert!(
                root_stopped.is_ok(),
                "timed-out browser root process must be gone or zombie-reaped: {root_stopped:?}"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let child_stopped = wait_for_process_exit(&sleep_pidfile, &scope_namespace).await;
            assert!(
                child_stopped.is_ok(),
                "timed-out browser command must terminate its setsid-escaped descendant: {child_stopped:?}"
            );
            let unrelated_running = unrelated.try_wait().unwrap().is_none();
            let _ = std::fs::remove_file(&unrelated_pidfile);
            assert!(
                unrelated_running,
                "browser cancellation must not signal an unrelated process"
            );
            unrelated.kill().unwrap();
            let _ = unrelated.wait();
        }

        sessions_guard().remove(&sid);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The configurable browser-command timeout must clamp to sane bounds. A zero or extremely
    /// small value would time out before any command starts; an enormous value defeats the cap.
    #[tokio::test]
    async fn command_timeout_clamps_invalid_values() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_BROWSER_TIMEOUT_MS").ok();

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_BROWSER_TIMEOUT_MS") };
        assert_eq!(
            command_timeout().as_secs(),
            75,
            "default browser command timeout is 75 seconds"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", "2000") };
        assert_eq!(
            command_timeout().as_millis(),
            2000,
            "valid override is preserved"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", "50") };
        assert_eq!(
            command_timeout().as_millis(),
            1000,
            "below-minimum value clamps to 1 second"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", "100000000") };
        assert_eq!(
            command_timeout().as_millis(),
            300_000,
            "above-maximum value clamps to 5 minutes"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_TIMEOUT_MS") },
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

    /// `binary_present` must reflect whether the resolved `agent-browser` binary is on disk.
    /// With `DOTZ_BROWSER_BIN` pointed at a temp file that exists, it returns true; pointed at a
    /// missing path, it returns false. With the env var cleared, it falls back to the bundled /
    /// PATH lookup, which on a dev host without `npm run install:deps` returns false (and on a
    /// packaged install with the bundled binary returns true). We assert only the
    /// explicit-DOTZ_BROWSER_BIN branch here to keep the test host-independent.
    #[test]
    fn binary_present_matches_disk_state_for_explicit_bin() {
        let _guard = BROWSER_TIMEOUT_TEST_LOCK.blocking_lock();
        let dir =
            std::env::temp_dir().join(format!("dotz-browser-bin-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin_path = if cfg!(windows) {
            dir.join("fake-agent-browser.bat")
        } else {
            dir.join("fake-agent-browser.sh")
        };
        std::fs::write(&bin_path, "exit 0\n").unwrap();

        let prev = std::env::var("DOTZ_BROWSER_BIN").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_BROWSER_BIN", bin_path.to_string_lossy().to_string()) };
        assert!(
            binary_present(),
            "binary_present must be true when DOTZ_BROWSER_BIN points at an existing file"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe {
            std::env::set_var(
                "DOTZ_BROWSER_BIN",
                dir.join("does-not-exist").to_string_lossy().to_string(),
            )
        };
        assert!(
            !binary_present(),
            "binary_present must be false when DOTZ_BROWSER_BIN points at a missing file"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_BROWSER_BIN", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_BROWSER_BIN") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `binary_name()` is `pub` so `src-tauri/src/main.rs` can resolve the bundled agent-browser
    /// path without hardcoding the Windows `.exe`. On Windows it must return the shipped
    /// `agent-browser-win32-x64.exe`; on macOS/Linux it returns the per-arch name for the
    /// future cross-platform build. This guards the pub-visibility + the Windows name against a
    /// regression that re-hardcodes the `.exe` in main.rs.
    #[test]
    fn browser_binary_name_is_pub_and_returns_correct_name() {
        // pub-visibility: the call compiles only because binary_name is `pub fn`.
        let name = binary_name();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "agent-browser-win32-x64.exe");
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                assert_eq!(name, "agent-browser-darwin-arm64");
            } else {
                assert_eq!(name, "agent-browser-darwin-x64");
            }
        } else if cfg!(target_arch = "aarch64") {
            assert_eq!(name, "agent-browser-linux-arm64");
        } else {
            assert_eq!(name, "agent-browser-linux-x64");
        }
        // Never empty — main.rs builds a path with it.
        assert!(!name.is_empty(), "binary_name must never be empty");
    }
}
