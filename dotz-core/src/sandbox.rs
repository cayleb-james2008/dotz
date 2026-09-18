//! dotz sandbox — real code-execution run store. Port of src/sandbox.ts + server.ts sandbox
//! routes (lines 509-543).
//!
//! A run spins up a program (node/tsx/python/sh/bash/powershell) in a fresh temp dir, captures
//! stdout+stderr (capped ~50KB), and updates the in-memory run record from running → done/error/
//! killed with exitCode + endedAt. POST returns the created run immediately (status "running"); the
//! UI polls GET /api/sandbox/runs/:id for the final output, mirroring server.ts.
//!
//! When started from a WebSocket session, stdout/stderr are streamed line-by-line as
//! `sandbox_output` events and web-mode listener banners emit `sandbox_port` as soon as the port
//! is reachable, so the UI preview iframe can load before the run terminates.
use axum::{
    extract::Path,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

/// Language list, in the exact order of LANGUAGES in sandbox.ts (Object.keys order).
/// Matches fixtures/sandbox.languages.json exactly.
const SANDBOX_LANGUAGES: [&str; 6] = [
    "javascript",
    "typescript",
    "python",
    "bash",
    "powershell",
    "shell",
];

const DEFAULT_TIMEOUT_MS: i64 = 30_000;
/// Default timeout for `mode:"web"` (preview) runs. A web run hosts a dev server that is meant to
/// stay alive while the user looks at the preview iframe — the 30 s terminal default killed every
/// preview mid-view (the UI never sends `timeoutMs`). 30 minutes keeps an active preview alive for
/// any realistic viewing session while still bounding a forgotten one so it can't leak a dev
/// server forever. An explicit client `timeoutMs` still wins in either mode.
const DEFAULT_WEB_TIMEOUT_MS: i64 = 30 * 60 * 1000;

/// Resolve the effective timeout for a run: a positive client-supplied value wins; otherwise the
/// mode-aware default (web previews get the long preview default, terminal runs the short one).
/// Shared by the REST handler and the WebSocket `sandbox.start` path so both resolve identically.
pub fn resolve_timeout_ms(requested: Option<i64>, mode: &str) -> i64 {
    match requested {
        Some(n) if n > 0 => n,
        _ => {
            if mode == "web" {
                DEFAULT_WEB_TIMEOUT_MS
            } else {
                DEFAULT_TIMEOUT_MS
            }
        }
    }
}
/// Output cap, matching the spirit of the Node streaming buffer — keep memory bounded.
const OUTPUT_CAP: usize = 50 * 1024;
/// Soft cap on retained run entries. Terminal runs (done/error/killed) are evicted oldest-first
/// once the store exceeds this size, so a long-lived dotz-core server doesn't leak every run's
/// captured output (up to `OUTPUT_CAP` each) and its `end_emitted` dedup flag forever. Running
/// runs are never evicted — only finished ones.
const MAX_RETAINED_RUNS: usize = 64;

/// Per-language { file, command } mapping, mirroring LANGUAGES in sandbox.ts.
fn lang_spec(language: &str) -> Option<(&'static str, Vec<&'static str>)> {
    match language {
        "javascript" => Some(("run.mjs", vec!["node", "run.mjs"])),
        "typescript" => Some(("run.ts", vec!["npx", "tsx", "run.ts"])),
        "python" => Some(("run.py", vec!["python", "run.py"])),
        "bash" => Some(("run.sh", vec!["bash", "run.sh"])),
        "powershell" => Some((
            "run.ps1",
            vec!["powershell", "-NoProfile", "-File", "run.ps1"],
        )),
        "shell" => Some(("run.sh", vec!["sh", "run.sh"])),
        _ => None,
    }
}

/// A sandbox run record — mirrors the SandboxRun interface in src/types.ts.
#[derive(Clone, Debug, Serialize)]
pub struct SandboxRun {
    pub id: String,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    pub language: String,
    pub code: String,
    pub status: String,
    pub output: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i64>,
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    #[serde(rename = "endedAt")]
    pub ended_at: Option<i64>,
}

/// Internal entry: the public run record plus runtime handles (pid for kill, mode, detected port).
struct RunEntry {
    run: SandboxRun,
    /// OS pid of the spawned child while it is alive; None once it has exited/been reaped.
    pid: Option<u32>,
    /// Set before a deliberate kill so the exit path reports "killed", not "error".
    killed_by_us: bool,
    mode: String,
    port: Option<u16>,
}

/// In-memory runs store (module singleton, mirrors the Node `sandbox` module-level instance).
fn runs() -> &'static Mutex<HashMap<String, RunEntry>> {
    static RUNS: OnceLock<Mutex<HashMap<String, RunEntry>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the sandbox runs mutex, recovering from a poisoned lock. A panic while holding the runs
/// lock (e.g. inside a spawn callback or an I/O error path) must not permanently brick the sandbox
/// REST endpoints, `/api/health`, or the WebSocket sandbox controls.
fn runs_guard() -> std::sync::MutexGuard<'static, HashMap<String, RunEntry>> {
    runs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Live sandbox run count, for the `/api/health` merge.
pub fn run_count() -> usize {
    runs_guard().len()
}

/// Set of run ids for which `sandbox_end` has already been emitted over the WebSocket. Both the
/// sandbox-start poller and the sandbox-kill handler emit `sandbox_end`, so without dedup a
/// killed run would deliver two end events to the UI. `try_mark_end_emitted` atomically records
/// that an end was emitted and returns `true` only for the first caller, so exactly one end
/// event is delivered per run.
static END_EMITTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
fn end_emitted_set() -> &'static Mutex<HashSet<String>> {
    END_EMITTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Atomically claim the `sandbox_end` emission for a run. Returns `true` if this caller is the
/// first to claim it (and should emit), `false` if another caller already did.
pub fn try_mark_end_emitted(id: &str) -> bool {
    end_emitted_set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.to_string())
}

// ---- platform sandbox backend ----
//
// Cross-platform abstraction over the two Windows-conditional seams in sandbox process
// management: (1) spawn-time flags (CREATE_NO_WINDOW on Windows, process_group on posix), and
// (2) tree-kill (taskkill /T /F on Windows, kill -9 -<pgid> on posix). The Windows impl is
// verbatim from the former inline `#[cfg(windows)]` blocks; the Mac/Linux impls carry the posix
// fallback (process_group + kill -9 -<pgid>) and reserve seatbelt/bwrap fields that are NOT yet
// applied.
//
// ponytail: the seatbelt (macOS `sandbox-exec -p <profile>`) and bwrap (Linux `bwrap --unshare-...`)
// argv are deferred — the stubs set process_group + tree-kill the posix way, which is the safe
// subset that compiles and runs on every host without a macOS/Linux build box. The
// `if self.seatbelt` / `if self.bwrap` branches are empty by design; filling them requires a
// macOS/Linux host to validate and is tracked as the B2 hardening follow-up.

/// Platform abstraction over the two Windows-conditional seams in sandbox process management:
/// (1) spawn-time flags (CREATE_NO_WINDOW on Windows, own process group on posix), and
/// (2) tree-kill (taskkill /T /F on Windows, kill -9 -<pgid> on posix). Best-effort;
/// implementations must reap their own kill subprocess (`.status()`, not `.spawn()`).
pub trait SandboxBackend: Send + Sync + 'static {
    /// Configure a `tokio::process::Command` before spawn (hide window on Windows, own process
    /// group on posix). Called for every sandbox + agent-browser spawn.
    fn prepare_command(&self, command: &mut tokio::process::Command);

    /// Kill a pid and its descendants. Synchronous: the sandbox calls it inline; `browser.rs`
    /// wraps it in `spawn_blocking` when async dispatch is needed. Must reap its own kill
    /// subprocess.
    fn kill_tree(&self, pid: u32);
}

/// Windows backend: `CREATE_NO_WINDOW` on spawn, `taskkill /T /F` for tree-kill. Verbatim from
/// the former inline `#[cfg(windows)]` blocks; reuses `crate::util::no_window_tokio` /
/// `crate::util::no_window` so the `CREATE_NO_WINDOW` constant lives in one place.
struct WindowsSandbox;

impl SandboxBackend for WindowsSandbox {
    fn prepare_command(&self, command: &mut tokio::process::Command) {
        crate::util::no_window_tokio(command);
    }

    fn kill_tree(&self, pid: u32) {
        // Use .status() (not .spawn()) so the taskkill subprocess is reaped. Dropping a spawned
        // std::process::Child without waiting leaves a zombie that accumulates over a long-lived
        // server with many sandbox kills.
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = crate::util::no_window(&mut cmd).status();
    }
}

/// macOS backend stub: posix fallback (process_group + `kill -9 -<pgid>`). The `seatbelt` field
/// reserves the `sandbox-exec -p <profile>` wrap for a future macOS-host follow-up; the
/// `if self.seatbelt` branch is empty by design (ponytail: deferred — requires a macOS host).
#[cfg(target_os = "macos")]
struct MacSandbox {
    seatbelt: bool,
}

#[cfg(target_os = "macos")]
impl MacSandbox {
    /// Best-effort detect: `seatbelt` is true only if `sandbox-exec` is on PATH. No failure if
    /// absent — the posix fallback still runs without it.
    fn detect() -> Self {
        let seatbelt = std::process::Command::new("sandbox-exec")
            .arg("-h")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        Self { seatbelt }
    }
}

#[cfg(target_os = "macos")]
impl SandboxBackend for MacSandbox {
    fn prepare_command(&self, command: &mut tokio::process::Command) {
        command.process_group(0);
        if self.seatbelt {
            // ponytail: TODO — wrap argv in `sandbox-exec -p <profile>` for the B2 macOS
            // hardening pass. Requires a macOS host to validate the seatbelt profile; deferred.
        }
    }

    fn kill_tree(&self, pid: u32) {
        posix_kill_tree(pid);
    }
}

/// Linux backend stub: posix fallback (process_group + `kill -9 -<pgid>`). The `bwrap` field
/// reserves the `bwrap --unshare-... <argv>` wrap for a future Linux-host follow-up; the
/// `if self.bwrap` branch is empty by design (ponytail: deferred — requires a Linux host).
#[cfg(target_os = "linux")]
struct LinuxSandbox {
    bwrap: bool,
}

#[cfg(target_os = "linux")]
impl LinuxSandbox {
    /// Best-effort detect: `bwrap` is true only if `bwrap` is on PATH. No failure if absent.
    fn detect() -> Self {
        let bwrap = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        Self { bwrap }
    }
}

#[cfg(target_os = "linux")]
impl SandboxBackend for LinuxSandbox {
    fn prepare_command(&self, command: &mut tokio::process::Command) {
        command.process_group(0);
        if self.bwrap {
            // ponytail: TODO — wrap argv in `bwrap --unshare-...` for the B2 Linux hardening
            // pass. Requires a Linux host to validate the bubblewrap argv; deferred.
        }
    }

    fn kill_tree(&self, pid: u32) {
        posix_kill_tree(pid);
    }
}

/// Posix tree-kill: signal the child's process group (pgid == child pid when spawned with
/// `process_group(0)`), falling back to a direct signal if the group doesn't exist (e.g. a
/// caller that spawned the child without its own process group, as some unit tests do). Uses
/// `.status()` so the kill subprocess is reaped. Verbatim from the former inline
/// `#[cfg(not(windows))]` block in `kill_pid`.
#[cfg(unix)]
fn posix_kill_tree(pid: u32) {
    let group = std::process::Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let group_ok = matches!(group, Ok(s) if s.success());
    if !group_ok {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Select the platform's sandbox backend. The cfg ladder is exhaustive; an unsupported target
/// fails at compile time.
pub fn platform_backend() -> Box<dyn SandboxBackend> {
    #[cfg(windows)]
    {
        Box::new(WindowsSandbox)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(MacSandbox::detect())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(LinuxSandbox::detect())
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        compile_error!("unsupported platform: dotz requires windows, macos, or linux");
    }
}

/// Process-wide sandbox backend. The trait methods are stateless (`WindowsSandbox` has no fields;
/// Mac/Linux carry only the detected seatbelt/bwrap flags), so a single shared instance serves
/// every sandbox + agent-browser spawn/kill. `OnceLock` initializes once on first use. Shared
/// with `browser.rs` so both subsystems dispatch through the same platform seam.
pub(crate) fn backend() -> &'static dyn SandboxBackend {
    static BACKEND: OnceLock<Box<dyn SandboxBackend>> = OnceLock::new();
    BACKEND.get_or_init(platform_backend).as_ref()
}

/// Evict terminal runs oldest-first (by `startedAt`) until the store is at or below
/// `MAX_RETAINED_RUNS`, and drop the matching `end_emitted` flags — once a run is gone from the
/// store its dedup flag is dead weight. Running runs are always retained. Called from
/// `start_run` (insertion) and `finish` (terminal transition) so the store stays bounded as
/// runs accumulate over a long-lived server process. Lock order is `runs` then `end_emitted`,
/// matching `remove_test_run`; no path takes them in reverse, so this cannot deadlock.
fn prune_finished_runs() {
    let evicted: Vec<String> = {
        let mut store = runs_guard();
        if store.len() <= MAX_RETAINED_RUNS {
            return;
        }
        // Oldest terminal runs first; running runs are never candidates.
        let mut terminal: Vec<(String, i64)> = store
            .iter()
            .filter(|(_, e)| e.run.status != "running")
            .map(|(id, e)| (id.clone(), e.run.started_at))
            .collect();
        terminal.sort_by_key(|(_, t)| *t);
        let to_evict = store.len().saturating_sub(MAX_RETAINED_RUNS);
        let mut evicted = Vec::with_capacity(to_evict);
        for (id, _) in terminal.into_iter().take(to_evict) {
            store.remove(&id);
            evicted.push(id);
        }
        evicted
    };
    if evicted.is_empty() {
        return;
    }
    let mut emitted = end_emitted_set().lock().unwrap_or_else(|p| p.into_inner());
    for id in &evicted {
        emitted.remove(id);
    }
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/sandbox/languages", get(languages))
        .route("/api/sandbox/runs", get(list_runs).post(create_run))
        .route("/api/sandbox/runs/{id}", get(get_run))
        .route("/api/sandbox/runs/{id}/port", get(run_port))
        .route("/api/sandbox/runs/{id}/kill", post(kill_run))
}

async fn languages() -> Json<Value> {
    Json(json!({ "languages": SANDBOX_LANGUAGES }))
}

async fn list_runs() -> Json<Value> {
    let store = runs_guard();
    let list: Vec<&SandboxRun> = store.values().map(|e| &e.run).collect();
    Json(json!({ "runs": list }))
}

async fn get_run(Path(id): Path<String>) -> Result<Json<SandboxRun>, (StatusCode, Json<Value>)> {
    let store = runs_guard();
    match store.get(&id) {
        Some(e) => Ok(Json(e.run.clone())),
        None => Err(not_found("no such sandbox run")),
    }
}

/// POST /api/sandbox/runs — create the run, spawn the process async, return the run (status "running").
async fn create_run(
    body: Option<Json<Value>>,
) -> Result<Json<SandboxRun>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));

    let language = match b.get("language").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Err(bad("language and code are required (strings)")),
    };
    let code = match b.get("code").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Err(bad("language and code are required (strings)")),
    };
    let mode = match b.get("mode") {
        None | Some(Value::Null) => "terminal".to_string(),
        Some(Value::String(s)) if s == "terminal" || s == "web" => s.clone(),
        _ => return Err(bad("mode must be \"terminal\" or \"web\"")),
    };
    let project_id = b
        .get("projectId")
        .and_then(|v| v.as_str())
        .map(String::from);
    let timeout_ms = resolve_timeout_ms(b.get("timeoutMs").and_then(|v| v.as_i64()), &mode);

    match start_run(
        &language,
        &code,
        &mode,
        project_id.as_deref(),
        timeout_ms,
        None,
        None,
    )
    .await
    {
        Ok(run) => Ok(Json(run)),
        Err(e) => Err(bad(e)),
    }
}

/// Create and start a sandbox run without the axum JSON wrapper. Shared by the REST handler and
/// the WebSocket control loop so both paths produce the same run record.
///
/// `tx` is an optional WebSocket broadcast sender; when present, stdout/stderr lines and detected
/// web ports are emitted as `sandbox_output`/`sandbox_port` events.
pub async fn start_run(
    language: &str,
    code: &str,
    mode: &str,
    project_id: Option<&str>,
    timeout_ms: i64,
    tx: Option<broadcast::Sender<Value>>,
    cwd: Option<std::path::PathBuf>,
) -> Result<SandboxRun, String> {
    if !SANDBOX_LANGUAGES.contains(&language) {
        return Err(format!(
            "unsupported language: {}. Available: {}",
            language,
            SANDBOX_LANGUAGES.join(", ")
        ));
    }
    if mode != "terminal" && mode != "web" {
        return Err("mode must be \"terminal\" or \"web\"".to_string());
    }

    // Web previews are single-occupancy per project: the UI never kills the prior preview
    // before starting a new one, and with the 30-minute web default an iterate-on-preview
    // loop would otherwise stack live dev-server children (each alive for up to 30 min, and
    // a fixed-port server keeps its port occupied so every replacement fails with
    // EADDRINUSE until the stale run is hunted down). Supersede: kill any still-running
    // web run for the same project before spawning the replacement.
    if mode == "web" {
        supersede_prior_web_runs(project_id);
    }

    let run = SandboxRun {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project_id.map(String::from),
        language: language.to_string(),
        code: code.to_string(),
        status: "running".to_string(),
        output: String::new(),
        exit_code: None,
        started_at: crate::util::now_ms(),
        ended_at: None,
    };
    let id = run.id.clone();
    {
        let mut store = runs_guard();
        store.insert(
            id.clone(),
            RunEntry {
                run: run.clone(),
                pid: None,
                killed_by_us: false,
                mode: mode.to_string(),
                port: None,
            },
        );
    }
    // Bounded-retention sweep: evict oldest terminal runs so the store doesn't leak every
    // run's captured output over a long-lived server process.
    prune_finished_runs();

    tokio::spawn(execute_run(
        id.clone(),
        language.to_string(),
        code.to_string(),
        timeout_ms,
        mode.to_string(),
        tx,
        cwd,
    ));

    Ok(run)
}

/// Look up a run by id. Used by the WebSocket loop to poll for terminal status.
pub fn lookup(id: &str) -> Option<SandboxRun> {
    runs_guard().get(id).map(|e| e.run.clone())
}

#[cfg(test)]
/// Read the recorded child pid for a run. Test-only helper so WebSocket sandbox tests can wait
/// for `execute_run` to actually spawn the child (a powershell cold start can take several
/// seconds under load) instead of sleeping a fixed interval before killing it.
pub fn test_run_pid(id: &str) -> Option<u32> {
    runs_guard().get(id).and_then(|e| e.pid)
}

#[cfg(test)]
/// Remove a run entry from the in-memory store. Test-only helper so WebSocket sandbox tests can
/// clean up the process-global runs map.
pub fn remove_test_run(id: &str) {
    runs_guard().remove(id);
    let _ = end_emitted_set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(id);
}

/// Spawn the child for `language`, capture stdout+stderr, apply the timeout, then update the run.
/// When `tx` is provided, output is streamed line-by-line as `sandbox_output` events and web-mode
/// listener banners emit `sandbox_port` as soon as the port is reachable.
async fn execute_run(
    id: String,
    language: String,
    code: String,
    timeout_ms: i64,
    mode: String,
    tx: Option<broadcast::Sender<Value>>,
    cwd: Option<std::path::PathBuf>,
) {
    // A kill/supersede that landed BEFORE this task spawned anything (kill_run_by_id is a no-op
    // while pid is None, so supersede_prior_web_runs marks the pid-less entry killed_by_us +
    // terminal instead) means the child must not be started at all. Spawning and then
    // tree-killing is inherently racy on Windows: `taskkill /T` enumerates the process tree
    // once, so a grandchild forked mid-kill (bash -> sleep) escapes the kill while holding the
    // inherited stdout/stderr pipes, wedging this future until the orphan exits — the run then
    // appears to live out its full sleep despite the "immediate" kill. The superseder already
    // recorded the terminal "killed" state and "[killed]" output marker, so simply never spawn.
    let killed_before_spawn = runs_guard()
        .get(&id)
        .map(|e| e.killed_by_us && e.pid.is_none())
        .unwrap_or(false);
    if killed_before_spawn {
        return;
    }

    let (file, cmd) = match lang_spec(&language) {
        Some(v) => v,
        None => {
            finish(&id, "error", None, "[spawn error] unsupported language\n");
            return;
        }
    };

    // Fresh temp dir per run, mirroring fs.mkdtemp(os.tmpdir(), "dotz-sandbox-").
    let temp_dir = std::env::temp_dir().join(format!("dotz-sandbox-{}", id));
    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        finish(&id, "error", None, &format!("[spawn error] {e}\n"));
        return;
    }
    if let Err(e) = tokio::fs::write(temp_dir.join(file), &code).await {
        finish(&id, "error", None, &format!("[spawn error] {e}\n"));
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        return;
    }

    // A caller-supplied cwd (verify checks run in the project dir, not the throwaway temp
    // dir — a check like `[ -f Cargo.toml ] && cargo check` is vacuous in an empty dir)
    // requires the script to be invoked by ABSOLUTE path, since the relative filename in
    // lang_spec only resolves from temp_dir. Default callers keep today's exact argv.
    let work_dir = cwd.filter(|p| p.is_dir());
    let args: Vec<String> = if work_dir.is_some() {
        let script = temp_dir.join(file).to_string_lossy().replace('\\', "/");
        cmd[1..]
            .iter()
            .map(|a| {
                if *a == file {
                    script.clone()
                } else {
                    a.to_string()
                }
            })
            .collect()
    } else {
        cmd[1..].iter().map(|a| a.to_string()).collect()
    };
    let mut command = tokio::process::Command::new(cmd[0]);
    command
        .args(&args)
        .current_dir(work_dir.as_deref().unwrap_or(&temp_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Cross-platform spawn flags via the shared backend: CREATE_NO_WINDOW on Windows, own
    // process group on posix (so `kill_tree` can signal the whole group). See `SandboxBackend`.
    backend().prepare_command(&mut command);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            finish(&id, "error", None, &format!("[spawn error] {e}\n"));
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            return;
        }
    };

    // Record the pid so kill_run and the timeout watchdog can reach the child.
    if let Some(pid) = child.id() {
        let killed_before_spawn = {
            let mut guard = runs_guard();
            match guard.get_mut(&id) {
                Some(e) => {
                    e.pid = Some(pid);
                    e.killed_by_us
                }
                None => false,
            }
        };
        // A kill/supersede that ran before the pid landed (kill_run_by_id and the web
        // supersession path are no-ops while pid is None) could not signal the child;
        // deliver it now so a superseded preview can't live out its full timeout.
        if killed_before_spawn {
            kill_pid(Some(pid));
        }
    }

    // Watchdog: kill the child tree after timeout_ms. The collection future then finishes naturally
    // when the pipes close, so a single code path handles both normal exit and timeout.
    let watchdog = if timeout_ms > 0 {
        let id2 = id.clone();
        Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms as u64)).await;
            mark_killed_by_us(&id2);
            let pid = runs_guard().get(&id2).and_then(|e| e.pid);
            kill_pid(pid);
        }))
    } else {
        None
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Always take the streaming path. A run started without a WS sender (REST
    // `POST /api/sandbox/runs`, verify checks) previously fell into a read-to-end branch that
    // buffered ALL output until process exit and never ran `detect_port_in_window` — so a
    // `mode:"web"` run started over REST could NEVER publish its port while alive, and
    // `GET /api/sandbox/runs/:id/port` (the documented polling mirror of `sandbox_port`)
    // 404'd for its entire lifetime. A receiver-less broadcast channel keeps the streaming
    // machinery intact: line events are dropped harmlessly (`send` errors are ignored) while
    // web-port detection still caches the port on the run record.
    let tx = tx.unwrap_or_else(|| broadcast::channel(16).0);
    let (output, status) = {
        let st = tokio::spawn(stream_output(id.clone(), mode.clone(), stdout, stderr, tx));
        let (status, output) = tokio::join!(child.wait(), st);
        (output.unwrap_or_default(), status)
    };

    if let Some(w) = watchdog {
        w.abort();
    }

    // Reap the child so its current directory is released before we remove the temp dir
    // (primarily a Windows concern).
    let _ = child.wait().await;

    let mut output = output;
    // Recover from a poisoned runs mutex: a panic in another task must not crash the exit path.
    let killed_by_us = runs_guard()
        .get(&id)
        .map(|e| e.killed_by_us)
        .unwrap_or(false);
    match status {
        Ok(es) => {
            let code_n = es.code().map(|c| c as i64);
            if killed_by_us {
                // Distinguish a manual kill (kill_run_by_id already set status to
                // "killed" for immediate UI feedback) from a timeout kill (the watchdog
                // only set killed_by_us; the status is still "running").  Without this
                // check a manual kill would be mislabelled as a timeout.
                let already_killed = runs_guard()
                    .get(&id)
                    .map(|e| e.run.status == "killed")
                    .unwrap_or(false);
                if already_killed {
                    output.push_str("\n[killed]\n");
                } else {
                    output.push_str(&format!("\n[timeout] killed after {timeout_ms}ms\n"));
                }
                finish(&id, "killed", None, &cap(&output));
            } else if es.success() {
                finish(&id, "done", code_n, &cap(&output));
            } else {
                finish(&id, "error", code_n, &cap(&output));
            }
        }
        Err(e) => {
            output.push_str(&format!("\n[spawn error] {e}\n"));
            finish(&id, "error", None, &cap(&output));
        }
    }
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
}

/// Stream stdout/stderr lines from a running child, emitting `sandbox_output` events and (in web
/// mode) `sandbox_port` events as soon as a detected listener port is reachable. Returns the
/// captured combined output.
async fn stream_output(
    id: String,
    mode: String,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    tx: broadcast::Sender<Value>,
) -> String {
    use std::collections::VecDeque;
    let mut out = String::new();
    let mut err = String::new();
    let mut recent_out: VecDeque<String> = VecDeque::with_capacity(3);
    let mut recent_err: VecDeque<String> = VecDeque::with_capacity(3);

    let out_fut = drain_stream(
        id.clone(),
        mode.clone(),
        "stdout",
        stdout,
        &tx,
        &mut out,
        &mut recent_out,
    );
    let err_fut = drain_stream(
        id.clone(),
        mode,
        "stderr",
        stderr,
        &tx,
        &mut err,
        &mut recent_err,
    );
    tokio::join!(out_fut, err_fut);

    format!("{out}{err}")
}

async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(
    id: String,
    mode: String,
    stream: &str,
    reader: Option<R>,
    tx: &broadcast::Sender<Value>,
    out: &mut String,
    recent: &mut std::collections::VecDeque<String>,
) {
    let Some(reader) = reader else { return };
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = tx.send(json!({
            "type": "sandbox_output",
            "runId": id,
            "line": &line,
            "stream": stream,
        }));
        if mode == "web" {
            let window: Vec<String> = recent.iter().cloned().collect();
            let tx2 = tx.clone();
            let id2 = id.clone();
            let line2 = line.clone();
            tokio::spawn(async move {
                detect_port_in_window(&id2, &line2, &window, &tx2).await;
            });
        }
        out.push_str(&line);
        out.push('\n');
        cap_in_place(out);
        recent.push_back(line);
        if recent.len() > 3 {
            recent.pop_front();
        }
    }
}

/// Scan the rolling context window plus the newest line for a listener port; if one is reachable,
/// cache it on the run and emit a `sandbox_port` event at most once.
///
/// Short-circuits when a port is already cached or the run was evicted: without this guard every
/// subsequent output line in web mode would spawn a task that probes each candidate port (400ms
/// TCP timeout each) and re-locks the runs mutex — wasted work that accumulates fast on a chatty
/// dev server long after the preview port is known.
async fn detect_port_in_window(
    id: &str,
    line: &str,
    window: &[String],
    tx: &broadcast::Sender<Value>,
) {
    // Early-return: if a port is already cached or the run was evicted from the store, skip the
    // scan + TCP probes entirely. The lock is held only for the brief read, then dropped before
    // any await so there is no contention with the output streaming path.
    {
        let store = runs_guard();
        match store.get(id) {
            None => return,
            Some(e) if e.port.is_some() => return,
            _ => {}
        }
    }
    let mut context = window.join("\n");
    if !context.is_empty() {
        context.push('\n');
    }
    context.push_str(line);
    for port in scan_ports(&context) {
        if is_port_open(port).await {
            let mut store = runs_guard();
            if let Some(e) = store.get_mut(id) {
                if e.port.is_none() {
                    e.port = Some(port);
                    drop(store);
                    let _ = tx.send(json!({
                        "type": "sandbox_port",
                        "runId": id,
                        "port": port,
                    }));
                    return;
                }
            }
        }
    }
}

/// Truncate `s` to OUTPUT_CAP bytes (on a char boundary), appending a marker once.
fn cap_in_place(s: &mut String) {
    if s.len() <= OUTPUT_CAP {
        return;
    }
    let mut end = OUTPUT_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n[output truncated]\n");
}

/// Truncate output to OUTPUT_CAP bytes (on a char boundary), matching the ~50KB cap.
fn cap(s: &str) -> String {
    let mut out = s.to_string();
    cap_in_place(&mut out);
    out
}

/// Set the terminal state on a run: status, exitCode, output, endedAt; clear the pid.
fn finish(id: &str, status: &str, exit_code: Option<i64>, output: &str) {
    // All mutations happen inside this block so the runs mutex guard is dropped before
    // prune_finished_runs() re-locks it (std::sync::Mutex is not reentrant).
    {
        let mut guard = runs_guard();
        if let Some(e) = guard.get_mut(id) {
            // Don't clobber a run already moved to a terminal state (e.g. kill raced the
            // timeout).  A manual kill (kill_run_by_id) already set the terminal state and
            // a "[killed]" marker for immediate UI feedback — but the exit path's collected
            // stdout/stderr would be silently lost without writing it here.
            if e.run.status != "running" {
                if !output.is_empty() {
                    e.run.output = output.to_string();
                }
            } else {
                e.run.status = status.to_string();
                e.run.exit_code = exit_code;
                e.run.output = output.to_string();
                e.run.ended_at = Some(crate::util::now_ms());
                e.pid = None;
            }
        }
    }
    // A run just became terminal — sweep oldest finished runs so the store stays bounded.
    prune_finished_runs();
}

fn mark_killed_by_us(id: &str) {
    if let Some(e) = runs_guard().get_mut(id) {
        e.killed_by_us = true;
    }
}

/// Kill a pid and its descendants — `taskkill /T /F` on win32, `kill -9 -<pgid>` on posix.
/// Dispatched via the shared `SandboxBackend` so the kill logic lives in one place and
/// `browser.rs` reuses it for the agent-browser process tree. Best-effort; the kill subprocess
/// is reaped (`.status()`, not `.spawn()`) by each platform impl.
fn kill_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    backend().kill_tree(pid);
}

/// Kill a running sandbox run by id, returning true if a live child was signalled. Shared by the
/// REST kill endpoint and the WebSocket kill message so both paths behave identically.
///
/// The run is marked `killed` (and `endedAt` set) immediately so the UI reflects the operator
/// action without waiting for the asynchronous exit path to reap the process. If the process
/// has already exited, `finish` is a no-op; otherwise the cleanup path removes the temp dir as
/// usual once the child reaps.
pub fn kill_run_by_id(id: &str) -> bool {
    let pid = {
        let mut store = runs_guard();
        match store.get_mut(id) {
            Some(e) if e.pid.is_some() && e.run.status == "running" => {
                e.killed_by_us = true;
                e.run.status = "killed".to_string();
                e.run.ended_at = Some(crate::util::now_ms());
                e.run.output.push_str("\n[killed]\n");
                e.pid.take()
            }
            _ => None,
        }
    };
    match pid {
        Some(p) => {
            kill_pid(Some(p));
            true
        }
        None => false,
    }
}

/// Kill every still-running `mode:"web"` run for the same project (`None` matches `None`,
/// `Some` matches the equal `Some`), so web previews stay single-occupancy per project.
/// Called by `start_run` before a new web run is inserted; returns the superseded ids.
///
/// A prior run whose child has not spawned yet (pid still `None` — `kill_run_by_id` is a
/// no-op for those) is marked `killed_by_us` + terminal here; `execute_run` delivers the
/// actual kill the moment the pid lands, so even a start that races the previous spawn
/// cannot leak a 30-minute dev server.
fn supersede_prior_web_runs(project_id: Option<&str>) -> Vec<String> {
    let stale: Vec<String> = {
        let store = runs_guard();
        store
            .iter()
            .filter(|(_, e)| {
                e.mode == "web"
                    && e.run.status == "running"
                    && e.run.project_id.as_deref() == project_id
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    for id in &stale {
        if !kill_run_by_id(id) {
            // No pid yet: mark it terminal + killed_by_us so the spawn path kills the
            // child as soon as it exists (mirrors kill_run_by_id's terminal bookkeeping).
            let mut store = runs_guard();
            if let Some(e) = store.get_mut(id) {
                if e.run.status == "running" {
                    e.killed_by_us = true;
                    e.run.status = "killed".to_string();
                    e.run.ended_at = Some(crate::util::now_ms());
                    e.run.output.push_str("\n[killed]\n");
                }
            }
        }
    }
    stale
}

/// POST /api/sandbox/runs/:id/kill — kill the child, mark killed → { ok }.
async fn kill_run(Path(id): Path<String>) -> Json<Value> {
    Json(json!({ "ok": kill_run_by_id(&id) }))
}

/// GET /api/sandbox/runs/:id/port — best-effort web port detection for mode:"web". 404 if none.
async fn run_port(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (output, mode, cached) = {
        let store = runs_guard();
        match store.get(&id) {
            Some(e) => (e.run.output.clone(), e.mode.clone(), e.port),
            None => return Err(not_found("no such sandbox run")),
        }
    };
    if let Some(p) = cached {
        return Ok(Json(json!({ "port": p })));
    }
    if mode != "web" {
        return Err(not_found(
            "no web port detected (terminal run or not yet listening)",
        ));
    }
    // Scan the captured output for a listener banner, then TCP-probe the candidate.
    for cand in scan_ports(&output) {
        if is_port_open(cand).await {
            if let Some(e) = runs_guard().get_mut(&id) {
                e.port = Some(cand);
            }
            return Ok(Json(json!({ "port": cand })));
        }
    }
    Err(not_found(
        "no web port detected (terminal run or not yet listening)",
    ))
}

/// Extract candidate ports from server-listener banners, mirroring the PortDetector regex intent in
/// sandbox.ts. A port-with-prefix is a candidate only when a listener keyword ("ready", "local",
/// etc.) appears on the same line or within the previous two lines — dev servers like Vite and
/// Next.js print the keyword on one line and the `localhost:` URL on the next. Avoids bare
/// ":<port>" and the generic word "server" so client-talk lines can't hijack the preview.
fn scan_ports(text: &str) -> Vec<u16> {
    const KEYWORDS: [&str; 6] = [
        "listening",
        "serving",
        "running",
        "started",
        "ready",
        "local",
    ];
    const PREFIXES: [&str; 4] = ["port ", "localhost:", "127.0.0.1:", "0.0.0.0:"];
    const CONTEXT_LINES: usize = 2;

    let lines: Vec<String> = text.lines().map(|l| l.to_ascii_lowercase()).collect();
    let mut out: Vec<u16> = Vec::new();
    for (i, lower) in lines.iter().enumerate() {
        // A listener keyword on the current line or within the previous two lines puts this line
        // in context for port detection.
        let keyword_nearby = lines
            .iter()
            .skip(i.saturating_sub(CONTEXT_LINES))
            .take(i.min(CONTEXT_LINES) + 1)
            .any(|l| KEYWORDS.iter().any(|k| l.contains(k)));
        if !keyword_nearby {
            continue;
        }
        for pfx in PREFIXES {
            let mut search = lower.as_str();
            while let Some(pos) = search.find(pfx) {
                let after = &search[pos + pfx.len()..];
                let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse::<u32>() {
                    if p > 1024 && p < 65536 {
                        let p = p as u16;
                        if !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
                search = &search[pos + pfx.len()..];
            }
        }
    }
    out
}

/// TCP-probe 127.0.0.1:port with a short timeout, mirroring isPortOpen in sandbox.ts.
async fn is_port_open(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(400),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn not_found(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: web-mode previews were killed by the 30 s terminal default because the UI
    /// never sends `timeoutMs`. The mode-aware resolver must give `mode:"web"` runs the long
    /// preview default while terminal runs keep the short default, and an explicit positive
    /// client value must win in either mode.
    #[test]
    fn resolve_timeout_is_mode_aware() {
        // No client value: terminal keeps 30 s, web gets the preview default.
        assert_eq!(resolve_timeout_ms(None, "terminal"), DEFAULT_TIMEOUT_MS);
        assert_eq!(resolve_timeout_ms(None, "web"), DEFAULT_WEB_TIMEOUT_MS);
        // Const block (clippy::assertions_on_constants, rust 1.95): the invariant is
        // const-evaluable, so let it fail at compile time instead of at test time.
        const {
            assert!(
                DEFAULT_WEB_TIMEOUT_MS > DEFAULT_TIMEOUT_MS,
                "preview default must exceed the terminal default or the fix is vacuous"
            );
        }
        // Explicit positive value wins in both modes.
        assert_eq!(resolve_timeout_ms(Some(5_000), "terminal"), 5_000);
        assert_eq!(resolve_timeout_ms(Some(5_000), "web"), 5_000);
        // Zero/negative are rejected exactly like the old `.filter(|n| *n > 0)` guards, so a
        // client cannot request an unbounded run by sending 0 or -1.
        assert_eq!(resolve_timeout_ms(Some(0), "web"), DEFAULT_WEB_TIMEOUT_MS);
        assert_eq!(resolve_timeout_ms(Some(-1), "terminal"), DEFAULT_TIMEOUT_MS);
    }

    /// Regression: a `mode:"web"` run started WITHOUT a WS broadcast sender (the REST
    /// `POST /api/sandbox/runs` path, tx = None) must still detect its web port while the run
    /// is alive. Before the always-stream fix, the tx=None branch buffered all output until
    /// process exit, `detect_port_in_window` never ran, and `GET .../port` 404'd forever.
    ///
    /// The test binds its own listener (so the probed port is genuinely open), has the
    /// sandboxed script print a listener banner and stay alive, and asserts the port lands in
    /// the run-record cache mid-run.
    #[tokio::test]
    async fn web_run_without_ws_sender_still_detects_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().unwrap().port();

        // The child stays alive well beyond the poll window so the detached
        // `detect_port_in_window` task has time to be scheduled + complete its
        // TCP probe even under full-suite runtime contention (the original
        // 10s/20s budget raced the scheduler when 500+ tests saturated tokio).
        let code = format!("echo \"listening on 127.0.0.1:{port}\"\nsleep 45\n");
        let run = start_run("bash", &code, "web", None, 60_000, None, None)
            .await
            .expect("start_run should succeed");

        // Poll the run-record port cache (what GET /api/sandbox/runs/:id/port serves first).
        // Generous budget: the detection task is fire-and-forget so its scheduling latency
        // scales with runtime load; 30s accommodates a saturated multi-test worker without
        // weakening the "port is cached mid-run" assertion.
        let mut detected = None;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            detected = runs_guard().get(&run.id).and_then(|e| e.port);
            if detected.is_some() {
                break;
            }
        }

        // Clean up the child before asserting so a failure doesn't leak a 20s sleeper.
        let pid = runs_guard().get(&run.id).and_then(|e| e.pid);
        mark_killed_by_us(&run.id);
        kill_pid(pid);
        drop(listener);

        assert_eq!(
            detected,
            Some(port),
            "web run started with tx=None must cache its detected port mid-run"
        );
    }

    /// Regression (audit w3): iterating on a preview must not stack live dev servers. The UI
    /// never kills the prior preview before starting a new one, so with the 30-minute web
    /// default each iteration used to leak a live child (and its port) for up to 30 minutes.
    /// Starting a new web run for the same project must kill the prior still-running one.
    #[tokio::test]
    async fn web_start_supersedes_prior_running_web_run_for_same_project() {
        let project = format!("proj-{}", uuid::Uuid::new_v4());
        let first = start_run(
            "bash",
            "sleep 45\n",
            "web",
            Some(&project),
            60_000,
            None,
            None,
        )
        .await
        .expect("first web run should start");
        // Wait for the child to actually spawn so the supersession exercises the live-pid
        // kill path (the pre-spawn path has its own test below).
        let mut pid = None;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            pid = test_run_pid(&first.id);
            if pid.is_some() {
                break;
            }
        }
        assert!(pid.is_some(), "first web run's child should spawn");

        let second = start_run(
            "bash",
            "sleep 45\n",
            "web",
            Some(&project),
            60_000,
            None,
            None,
        )
        .await
        .expect("second web run should start");

        // Supersession happens synchronously inside start_run, before the new run is
        // inserted: by the time the second start returns, the first must be terminal.
        let first_status = lookup(&first.id).map(|r| r.status);
        assert_eq!(
            first_status.as_deref(),
            Some("killed"),
            "prior running web run for the same project must be killed by the new start"
        );
        let second_status = lookup(&second.id).map(|r| r.status);
        assert_eq!(
            second_status.as_deref(),
            Some("running"),
            "the replacement web run must not kill itself"
        );

        // Clean up: kill the second child and drop both entries from the global store.
        let mut pid2 = None;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            pid2 = test_run_pid(&second.id);
            if pid2.is_some() {
                break;
            }
        }
        mark_killed_by_us(&second.id);
        kill_pid(pid2);
        remove_test_run(&first.id);
        remove_test_run(&second.id);
    }

    /// The web supersession must be scoped: same-project web runs only. Other projects'
    /// web runs, terminal-mode runs, and already-finished runs are untouched — and a
    /// pid-less running web run (child not spawned yet) is still marked killed so the
    /// spawn path can deliver the kill.
    #[test]
    fn web_supersession_is_scoped_to_same_project_and_web_mode() {
        let proj_a = format!("proj-a-{}", uuid::Uuid::new_v4());
        let proj_b = format!("proj-b-{}", uuid::Uuid::new_v4());
        let mk = |project: &str, mode: &str, status: &str| RunEntry {
            run: SandboxRun {
                id: uuid::Uuid::new_v4().to_string(),
                project_id: Some(project.to_string()),
                language: "shell".to_string(),
                code: String::new(),
                status: status.to_string(),
                output: String::new(),
                exit_code: None,
                started_at: crate::util::now_ms(),
                ended_at: None,
            },
            pid: None,
            killed_by_us: false,
            mode: mode.to_string(),
            port: None,
        };
        let web_a = mk(&proj_a, "web", "running");
        let web_b = mk(&proj_b, "web", "running");
        let term_a = mk(&proj_a, "terminal", "running");
        let done_a = mk(&proj_a, "web", "done");
        let ids: Vec<String> = [&web_a, &web_b, &term_a, &done_a]
            .iter()
            .map(|e| e.run.id.clone())
            .collect();
        {
            let mut store = runs_guard();
            for e in [web_a, web_b, term_a, done_a] {
                store.insert(e.run.id.clone(), e);
            }
        }

        let superseded = supersede_prior_web_runs(Some(&proj_a));

        assert_eq!(
            superseded,
            vec![ids[0].clone()],
            "only the running web run of the SAME project may be superseded"
        );
        {
            let store = runs_guard();
            let e = store.get(&ids[0]).expect("superseded entry still stored");
            assert_eq!(e.run.status, "killed");
            assert!(
                e.killed_by_us,
                "pid-less superseded run must be marked killed_by_us for the spawn path"
            );
            assert!(e.run.ended_at.is_some());
            assert_eq!(store.get(&ids[1]).unwrap().run.status, "running");
            assert_eq!(store.get(&ids[2]).unwrap().run.status, "running");
            assert_eq!(store.get(&ids[3]).unwrap().run.status, "done");
        }

        // Clean up the process-global store.
        let mut store = runs_guard();
        for id in &ids {
            store.remove(id);
        }
    }

    /// A supersede/kill that lands BEFORE the child pid is recorded (kill_run_by_id is a
    /// no-op while pid is None) must make execute_run return promptly WITHOUT the run living
    /// out its sleep: execute_run sees the pre-spawn kill and never starts the child at all
    /// (spawn-then-tree-kill was racy on Windows — a grandchild forked mid-`taskkill /T`
    /// escaped the kill holding the stdio pipes, so the run waited out its full timeout).
    #[tokio::test]
    async fn execute_run_kills_child_immediately_when_superseded_before_spawn() {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        // Exactly the state supersede_prior_web_runs leaves a pid-less run in.
                        status: "killed".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: Some(crate::util::now_ms()),
                    },
                    pid: None,
                    killed_by_us: true,
                    mode: "web".to_string(),
                    port: None,
                },
            );
        }

        // The would-be child sleeps far beyond the assertion budget; only the pre-spawn
        // early return (or, for the residual mid-spawn race, the immediate post-spawn kill)
        // can make execute_run return in time. The budget is generous (process latency
        // scales badly when the full suite saturates the machine) but stays well under the
        // sleep, so it still discriminates the fast path from waiting out the child.
        let done = tokio::time::timeout(
            Duration::from_secs(120),
            execute_run(
                id.clone(),
                "bash".to_string(),
                "sleep 300\n".to_string(),
                600_000,
                "web".to_string(),
                None,
                None,
            ),
        )
        .await;
        assert!(
            done.is_ok(),
            "execute_run must kill a pre-superseded child right after spawn, not wait out the sleep"
        );

        let status = lookup(&id).map(|r| r.status);
        assert_eq!(status.as_deref(), Some("killed"));
        remove_test_run(&id);
    }

    /// `try_mark_end_emitted` must return true only for the first caller so the sandbox-start
    /// poller and the sandbox-kill handler don't both emit `sandbox_end` for the same run.
    #[test]
    fn try_mark_end_emitted_is_one_shot() {
        let id = format!("test-end-dedup-{}", uuid::Uuid::new_v4());
        assert!(
            try_mark_end_emitted(&id),
            "first call for a fresh id must claim the emission"
        );
        assert!(
            !try_mark_end_emitted(&id),
            "second call for the same id must not re-claim the emission"
        );
        // Clean up the process-global set.
        end_emitted_set()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }

    #[test]
    fn run_count_reflects_store_entries() {
        // Perform insert + remove while holding one lock so the count sequence is atomic
        // with respect to other concurrently-running sandbox tests that mutate the store.
        let id = uuid::Uuid::new_v4().to_string();
        let (before, after_insert, after_remove) = {
            let mut store = runs_guard();
            let before = store.len();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "shell".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
            let after_insert = store.len();
            store.remove(&id);
            let after_remove = store.len();
            (before, after_insert, after_remove)
        };
        assert_eq!(
            after_insert,
            before + 1,
            "run_count should include the inserted entry"
        );
        assert_eq!(
            after_remove, before,
            "run_count should return to baseline after removal"
        );
    }

    /// `prune_finished_runs` must keep the runs store bounded by evicting the oldest *terminal*
    /// runs (never running ones) once the store exceeds `MAX_RETAINED_RUNS`, and must drop the
    /// matching `end_emitted` dedup flags so neither the store nor the dedup set leaks every
    /// run's captured output / flag over a long-lived server.
    ///
    /// This test drives the prune path directly with runs whose `started_at` values are far
    /// below any real `crate::util::now_ms()`, so they are always the oldest entries in the shared store and
    /// are evicted before any other test's runs — making the assertions deterministic without a
    /// global test serialization lock. Running runs use `started_at` 1..3 and terminal runs use
    /// 100..(100+N); only terminal runs are eviction candidates, so the running runs' lower
    /// `started_at` does not affect eviction order.
    #[test]
    fn prune_finished_runs_evicts_oldest_terminal_and_cleans_end_emitted() {
        let my_running: Vec<String> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let my_terminal: Vec<String> = (0..(MAX_RETAINED_RUNS + 20))
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();

        // Insert with started_at far below crate::util::now_ms() so these are always the oldest entries in
        // the shared store and are evicted first, never touching other tests' runs.
        {
            let mut store = runs_guard();
            for (i, id) in my_running.iter().enumerate() {
                store.insert(
                    id.clone(),
                    RunEntry {
                        run: SandboxRun {
                            id: id.clone(),
                            project_id: None,
                            language: "shell".to_string(),
                            code: String::new(),
                            status: "running".to_string(),
                            output: String::new(),
                            exit_code: None,
                            started_at: 1 + i as i64,
                            ended_at: None,
                        },
                        pid: None,
                        killed_by_us: false,
                        mode: "terminal".to_string(),
                        port: None,
                    },
                );
            }
            for (i, id) in my_terminal.iter().enumerate() {
                store.insert(
                    id.clone(),
                    RunEntry {
                        run: SandboxRun {
                            id: id.clone(),
                            project_id: None,
                            language: "shell".to_string(),
                            code: String::new(),
                            status: "done".to_string(),
                            output: String::new(),
                            exit_code: Some(0),
                            started_at: 100 + i as i64,
                            ended_at: Some(100 + i as i64),
                        },
                        pid: None,
                        killed_by_us: false,
                        mode: "terminal".to_string(),
                        port: None,
                    },
                );
            }
        }

        // Mark every terminal run as end-emitted so we can verify the dedup flags are cleaned.
        {
            let mut emitted = end_emitted_set().lock().unwrap_or_else(|p| p.into_inner());
            for id in &my_terminal {
                emitted.insert(id.clone());
            }
        }

        prune_finished_runs();

        // Running runs are never evicted.
        {
            let store = runs_guard();
            for id in &my_running {
                assert!(
                    store.contains_key(id),
                    "running run {id} must never be evicted by prune_finished_runs"
                );
            }
        }

        // The store must be at or below the cap plus a small slack for other tests' running
        // runs (running runs are never evicted, so a concurrent test could hold a few extra).
        {
            let store = runs_guard();
            assert!(
                store.len() <= MAX_RETAINED_RUNS + 8,
                "store must be bounded after prune, got {} entries",
                store.len()
            );
        }

        // At least one terminal run must have been evicted (we inserted cap+20, well over cap).
        let evicted: Vec<String> = {
            let store = runs_guard();
            my_terminal
                .iter()
                .filter(|id| !store.contains_key(*id))
                .cloned()
                .collect()
        };
        assert!(
            !evicted.is_empty(),
            "at least one terminal run must be evicted when the store exceeds the cap"
        );

        // Evicted runs must have their end_emitted flag cleaned; retained terminal runs must
        // keep theirs — proving the dedup set does not leak evicted runs' flags.
        {
            let emitted = end_emitted_set().lock().unwrap_or_else(|p| p.into_inner());
            for id in &evicted {
                assert!(
                    !emitted.contains(id),
                    "evicted run {id} must have its end_emitted flag cleaned by prune"
                );
            }
            let store = runs_guard();
            for id in &my_terminal {
                if store.contains_key(id) {
                    assert!(
                        emitted.contains(id),
                        "retained terminal run {id} must keep its end_emitted flag"
                    );
                }
            }
        }

        // Clean up: remove my surviving entries so the global store is left pristine.
        {
            let mut store = runs_guard();
            for id in my_running.iter().chain(my_terminal.iter()) {
                store.remove(id);
            }
        }
        {
            let mut emitted = end_emitted_set().lock().unwrap_or_else(|p| p.into_inner());
            for id in &my_terminal {
                emitted.remove(id);
            }
        }
    }

    /// Dev servers (Vite, Next.js, etc.) often print the listener keyword on one line and the
    /// `localhost:` URL on the next. scan_ports must look back a couple of lines so the port
    /// isn't missed.
    #[test]
    fn scan_ports_finds_port_on_line_after_keyword() {
        let output = "VITE v5.0.0  ready in 300 ms\n\n  ->  Local:   http://localhost:5173/\n";
        let ports = scan_ports(output);
        assert_eq!(ports, vec![5173]);
    }

    #[test]
    fn scan_ports_finds_port_on_same_line_as_keyword() {
        let output = "Server listening at http://127.0.0.1:3000\n";
        let ports = scan_ports(output);
        assert_eq!(ports, vec![3000]);
    }

    #[test]
    fn scan_ports_ignores_port_without_listener_keyword() {
        // A URL-like port without a listener keyword nearby must not be detected.
        let output = "random line\nanother line\nhttp://127.0.0.1:8080\n";
        let ports = scan_ports(output);
        assert!(
            ports.is_empty(),
            "ports without a listener keyword should be ignored"
        );
    }

    #[test]
    fn scan_ports_ignores_bare_colon_port_even_with_keyword() {
        // A bare `:9000` (no recognized prefix) must not hijack the preview even when a keyword
        // puts the line in context.
        let output = "ready\nsome client said :9000\n";
        let ports = scan_ports(output);
        assert!(
            ports.is_empty(),
            "bare :port without a recognized prefix should be ignored"
        );
    }

    #[test]
    fn scan_ports_ignores_out_of_range_ports() {
        let output = "ready\nlocal: http://localhost:80\nlocal: http://localhost:70000\n";
        let ports = scan_ports(output);
        assert!(
            ports.is_empty(),
            "ports outside 1025-65535 should be ignored"
        );
    }

    #[test]
    fn scan_ports_dedupes_duplicate_ports() {
        let output = "ready\nLocal: http://localhost:3000\nAlso at 127.0.0.1:3000\n";
        let ports = scan_ports(output);
        assert_eq!(ports, vec![3000]);
    }

    #[test]
    fn scan_ports_finds_multiple_distinct_ports() {
        let output = "ready\nLocal: http://localhost:3000\nAdmin: http://127.0.0.1:4000\n";
        let ports = scan_ports(output);
        assert_eq!(ports, vec![3000, 4000]);
    }

    /// `detect_port_in_window` must short-circuit when a port is already cached on the run, so a
    /// chatty dev server's subsequent output lines don't each spawn a task that does multiple
    /// 400ms TCP probes and mutex locks for no benefit. Without the early-return the function
    /// would probe every candidate port even though the cached port is already known.
    #[tokio::test]
    async fn detect_port_in_window_short_circuits_when_port_already_cached() {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "shell".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "web".to_string(),
                    // Port already detected — the function must not re-probe.
                    port: Some(3000),
                },
            );
        }

        let (tx, _rx) = broadcast::channel::<Value>(4);
        // A line with a valid port pattern that scan_ports would find. If the early-return is
        // missing, is_port_open would TCP-probe 5173 (nothing listening => 400ms timeout), making
        // the call noticeably slow. The short-circuit must return in well under that.
        let window: Vec<String> = vec!["ready in 300 ms".into()];
        let start = tokio::time::Instant::now();
        detect_port_in_window(&id, "  ->  Local:   http://localhost:5173/", &window, &tx).await;
        let elapsed = start.elapsed();

        // The cached port must be unchanged (the function must not overwrite it).
        let cached = runs_guard().get(&id).and_then(|e| e.port);
        assert_eq!(cached, Some(3000), "cached port must not be overwritten");

        // Must return near-instantly — far below the 400ms TCP-probe timeout that would fire
        // without the short-circuit.
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "detect_port_in_window should short-circuit when a port is cached, took {elapsed:?}"
        );

        // Clean up.
        runs_guard().remove(&id);
    }

    /// `detect_port_in_window` must short-circuit when the run was evicted from the store, so a
    /// spurious late output line doesn't probe ports for a run that no longer exists.
    #[tokio::test]
    async fn detect_port_in_window_short_circuits_when_run_evicted() {
        let id = uuid::Uuid::new_v4().to_string();
        // Intentionally do NOT insert the run into the store.
        let (tx, _rx) = broadcast::channel::<Value>(4);
        let window: Vec<String> = vec!["ready in 300 ms".into()];
        let start = tokio::time::Instant::now();
        detect_port_in_window(&id, "  ->  Local:   http://localhost:5173/", &window, &tx).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "detect_port_in_window should short-circuit when the run is evicted, took {elapsed:?}"
        );
    }

    /// A timed-out sandbox run must reap its child and remove its temp dir. Before the fix the
    /// timeout path killed the child but did not wait for it to exit, so on Windows the process
    /// still held its current directory and `remove_dir_all` silently failed, leaking stale
    /// `dotz-sandbox-*` directories.
    #[tokio::test]
    async fn timeout_run_reaps_child_and_removes_temp_dir() {
        let id = uuid::Uuid::new_v4().to_string();
        let temp_dir = std::env::temp_dir().join(format!("dotz-sandbox-{id}"));
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
        }

        // Use a command that outlives the 500ms timeout so we exercise the timeout cleanup path.
        let (language, code) = if cfg!(windows) {
            ("powershell", "Start-Sleep -Seconds 3")
        } else {
            ("bash", "sleep 2")
        };
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            500,
            "terminal".to_string(),
            None,
            None,
        )
        .await;

        let status = {
            let store = runs_guard();
            let entry = store.get(&id).expect("run entry should exist");
            entry.run.status.clone()
        };
        assert_eq!(status, "killed");

        // Give Windows a moment to finish taskkill and release handles, then assert the temp
        // dir was cleaned up. (Linux allows removing an in-use dir, so this primarily guards
        // Windows, but it still validates the cleanup path everywhere.) The deadline is
        // generous because taskkill + handle release can take many seconds under heavy load;
        // the tight poll keeps the happy path fast.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut gone = false;
        while tokio::time::Instant::now() < deadline {
            if !temp_dir.exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(gone, "timed-out sandbox run should remove its temp dir");

        // Be a good citizen: remove the terminal run entry and any leftover temp dir.
        {
            let mut store = runs_guard();
            store.remove(&id);
        }
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    /// The normal exit path of `execute_run` reads the run entry to decide between
    /// `done`/`error`/`killed`. It must use `runs_guard()` so a poisoned mutex (from an unrelated
    /// panic elsewhere) does not crash the run's finish path.
    #[tokio::test]
    async fn execute_run_normal_exit_recovers_from_poisoned_mutex() {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
        }

        // Intentionally poison the runs mutex while holding the lock.
        let m = runs();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("intentional sandbox runs mutex poison");
        }));
        assert!(poisoned.is_err(), "mutex should be poisoned");

        // Run a quick command that exits cleanly, forcing `execute_run` to read `killed_by_us`
        // via `runs_guard()` in the normal (non-timeout) exit path. The timeout is generous:
        // it only bounds a hang, and a powershell cold start can take well over 5 s when the
        // machine is under heavy load — a short timeout turns this into a "killed" run and a
        // spurious failure.
        let (language, code) = if cfg!(windows) {
            ("powershell", "Write-Output ok")
        } else {
            ("bash", "echo ok")
        };
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            60_000,
            "terminal".to_string(),
            None,
            None,
        )
        .await;

        {
            let store = runs_guard();
            let entry = store.get(&id).expect("run entry should exist");
            assert_eq!(entry.run.status, "done");
            assert!(entry.run.output.contains("ok"));
        }

        {
            let mut store = runs_guard();
            store.remove(&id);
        }
    }

    /// A panic while holding the sandbox runs mutex (e.g. inside a spawn callback or an I/O
    /// error path) must not permanently brick the sandbox REST endpoints, `/api/health`, or the
    /// WebSocket sandbox controls. `runs_guard()` recovers from a poisoned lock so the store
    /// remains usable.
    #[test]
    fn runs_guard_recovers_from_poisoned_mutex() {
        // Ensure the singleton is initialized (use the same guard we are testing, in case a
        // parallel test has already poisoned the mutex).
        drop(runs_guard());

        let m = runs();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("intentional sandbox runs mutex poison");
        }));
        assert!(poisoned.is_err(), "mutex should be poisoned");

        // Poison the mutex, then verify recovery by performing an atomic insert+remove
        // sequence under one recovered lock so concurrent tests can't mutate the count.
        let id = uuid::Uuid::new_v4().to_string();
        let (before, after_insert, after_remove) = {
            let mut store = runs_guard();
            let before = store.len();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
            let after_insert = store.len();
            store.remove(&id);
            let after_remove = store.len();
            (before, after_insert, after_remove)
        };
        assert_eq!(
            after_insert,
            before + 1,
            "runs_guard must recover and allow store mutations after poison"
        );
        assert_eq!(
            after_remove, before,
            "runs_guard must recover and allow store removals after poison"
        );
    }

    /// A sandbox run started with a broadcast sender must stream stdout/stderr lines as
    /// `sandbox_output` events before the run terminates. This is the core capability that lets
    /// the UI append terminal output live instead of waiting for the run to finish.
    #[tokio::test]
    async fn execute_run_streams_output_events_when_tx_present() {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
        }

        let (tx, mut rx) = broadcast::channel::<Value>(16);
        let (language, code) = if cfg!(windows) {
            ("powershell", "Write-Output dotz-stream-test")
        } else {
            ("bash", "echo dotz-stream-test")
        };
        // Generous timeout: it only bounds a hang. A powershell cold start can exceed 5 s under
        // heavy load, and a timeout kill here would drop the output line the test asserts on.
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            60_000,
            "terminal".to_string(),
            Some(tx),
            None,
        )
        .await;

        let mut found = false;
        while let Ok(frame) = rx.try_recv() {
            assert_eq!(
                frame.get("runId").and_then(|r| r.as_str()),
                Some(id.as_str())
            );
            if frame.get("type").and_then(|t| t.as_str()) == Some("sandbox_output")
                && frame.get("line").and_then(|l| l.as_str()) == Some("dotz-stream-test")
            {
                assert_eq!(frame.get("stream").and_then(|s| s.as_str()), Some("stdout"));
                found = true;
            }
        }
        assert!(
            found,
            "sandbox_output event should carry the command output line"
        );

        {
            let store = runs_guard();
            let entry = store.get(&id).expect("run entry should exist");
            assert!(
                entry.run.output.contains("dotz-stream-test"),
                "stored output should contain the streamed line"
            );
        }

        {
            let mut store = runs_guard();
            store.remove(&id);
        }
    }

    /// A web-mode sandbox run must emit a `sandbox_port` event as soon as a listener banner with
    /// a reachable port appears in the output. The test binds a local port so `is_port_open`
    /// succeeds without requiring an external dev server.
    #[tokio::test]
    async fn execute_run_emits_sandbox_port_event_in_web_mode() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "web".to_string(),
                    port: None,
                },
            );
        }

        let (tx, mut rx) = broadcast::channel::<Value>(16);
        let (language, code) = if cfg!(windows) {
            (
                "powershell",
                &format!("Write-Output 'ready'; Write-Output 'http://localhost:{port}/'; Start-Sleep -Seconds 1"),
            )
        } else {
            (
                "bash",
                &format!("echo 'ready'; echo 'http://localhost:{port}/'; sleep 1"),
            )
        };
        // Generous timeout: it only bounds a hang, and a powershell cold start under heavy load
        // can exceed 5 s — a timeout kill would swallow the banner line before port detection.
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            60_000,
            "web".to_string(),
            Some(tx),
            None,
        )
        .await;

        // Port detection runs on a detached task (see drain_stream), so the `sandbox_port` event
        // can legitimately arrive after `execute_run` returns. Wait for it with a bounded
        // deadline instead of draining only what is already buffered — under load the detached
        // probe can lose that race by seconds. `Closed` means every sender (including the probe
        // task's clone) is gone, so no further event can arrive and we can stop early.
        let mut found = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while !found && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv()).await {
                Ok(Ok(frame)) => {
                    if frame.get("type").and_then(|t| t.as_str()) == Some("sandbox_port") {
                        assert_eq!(
                            frame.get("runId").and_then(|r| r.as_str()),
                            Some(id.as_str())
                        );
                        assert_eq!(
                            frame.get("port").and_then(|p| p.as_u64()),
                            Some(port as u64)
                        );
                        found = true;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Err(_) => continue,
            }
        }
        assert!(
            found,
            "sandbox_port event should be emitted for a reachable listener port"
        );

        {
            let store = runs_guard();
            let entry = store.get(&id).expect("run entry should exist");
            assert_eq!(
                entry.port,
                Some(port),
                "run entry should cache the detected port"
            );
        }

        drop(listener);
        {
            let mut store = runs_guard();
            store.remove(&id);
        }
    }

    /// Killing a sandbox run must mark it `killed` immediately and set `endedAt`, instead of
    /// leaving the entry as `running` until the asynchronous cleanup path finishes reaping the
    /// child. The UI polls the run record and needs the terminal state to appear promptly.
    #[tokio::test]
    async fn kill_run_by_id_marks_run_killed_immediately_and_reaps_child() {
        let id = uuid::Uuid::new_v4().to_string();

        // Spawn a long-lived child so we have a real, killable pid to exercise the path.
        let mut child = if cfg!(windows) {
            tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
                .spawn()
                .expect("powershell should be available")
        } else {
            tokio::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep should be available")
        };
        let pid = child.id().expect("child should have a pid");

        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: Some(pid),
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
        }

        assert!(
            kill_run_by_id(&id),
            "kill_run_by_id should signal a live run"
        );

        {
            let store = runs_guard();
            let entry = store.get(&id).expect("run entry should exist");
            assert_eq!(
                entry.run.status, "killed",
                "run status must become 'killed' immediately"
            );
            assert!(
                entry.run.ended_at.is_some(),
                "endedAt must be set immediately"
            );
            assert!(
                entry.pid.is_none(),
                "pid must be cleared so the cleanup path does not double-kill"
            );
            assert!(
                entry.run.output.contains("[killed]"),
                "output should contain a killed marker"
            );
        }

        // The killed child must actually exit (be reaped), not leak as a zombie/handle. The budget
        // is generous on purpose: on win32 the kill shells out to `taskkill.exe`, whose process
        // launch + tree-walk can take 10s+ on machines with aggressive AV real-time scanning (the
        // termination itself is instant once taskkill runs). `child.wait()` returns the moment the
        // process dies, so this cap costs nothing on fast machines and only guards against that
        // worst case — what we assert is "the child is reaped", not a specific speed.
        let reaped = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
        assert!(
            reaped.is_ok(),
            "killed child process should be reaped (exit), not leak"
        );

        {
            let mut store = runs_guard();
            store.remove(&id);
        }
    }

    /// `kill_pid` must deliver a signal to the target process AND reap its own
    /// `kill`/`taskkill` subprocess. The old code used `.spawn()` and immediately dropped the
    /// `Child` handle — in Rust's std, dropping a `Child` does NOT call `waitpid`, so the
    /// `kill`/`taskkill` subprocess became a zombie that persisted until the dotz-core process
    /// exited. Over a long-lived server with many sandbox kills (timeouts + manual kills),
    /// zombies accumulated. The fix uses `.status()` which runs the signal-delivery command to
    /// completion and reaps it. This test verifies `kill_pid` still kills the target and returns
    /// only after the signal-delivery subprocess has been reaped (i.e., `.status()` completed).
    #[test]
    fn kill_pid_kills_target_and_reaps_kill_subprocess() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
                .spawn()
                .expect("powershell should be available")
        } else {
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep should be available")
        };
        // `std::process::Child::id()` returns `u32` on every platform (`Option<u32>` is the
        // *tokio* Child API — not used here). The old cfg branches had it backwards and did not
        // compile on non-Windows.
        let pid = child.id();

        // kill_pid must kill the target. With .status() it also blocks until the kill/taskkill
        // subprocess exits and is reaped, so by the time kill_pid returns no zombie lingers.
        kill_pid(Some(pid));

        // The target process must have been killed. Poll with try_wait — the signal was already
        // delivered, so this resolves quickly. A generous deadline guards against slow taskkill
        // on AV-heavy Windows machines.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break, // child exited — killed and reaped
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        panic!("kill_pid did not kill the target process within 15s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
    }

    /// When a sandbox run is manually killed mid-execution, the child's captured stdout/stderr
    /// must survive in the final run record.  Before the fix, `kill_run_by_id` set the terminal
    /// state (status="killed", output="\n[killed]\n") for immediate UI feedback, and the later
    /// `execute_run` exit path's `finish()` was a no-op (status already terminal), so the actual
    /// process output was silently lost — the run record showed only "\n[killed]\n".
    ///
    /// This test starts a real `execute_run` that prints a marker line then sleeps, kills it
    /// after the marker has been produced, waits for `execute_run` to finish, and asserts the
    /// final record contains BOTH the marker AND the "[killed]" marker — not just the latter.
    #[tokio::test]
    async fn manual_kill_preserves_child_output_in_final_record() {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs_guard();
            store.insert(
                id.clone(),
                RunEntry {
                    run: SandboxRun {
                        id: id.clone(),
                        project_id: None,
                        language: "bash".to_string(),
                        code: String::new(),
                        status: "running".to_string(),
                        output: String::new(),
                        exit_code: None,
                        started_at: crate::util::now_ms(),
                        ended_at: None,
                    },
                    pid: None,
                    killed_by_us: false,
                    mode: "terminal".to_string(),
                    port: None,
                },
            );
        }

        // Print a unique marker immediately, then sleep long enough for the kill to arrive
        // mid-execution.  The marker is what we assert survives in the final output.
        let marker = "dotz-kill-output-survives";
        let (language, code) = if cfg!(windows) {
            (
                "powershell",
                format!("Write-Output '{marker}'; Start-Sleep -Seconds 30"),
            )
        } else {
            ("bash", format!("echo '{marker}'; sleep 30"))
        };

        // Drive execute_run in a spawned task with a broadcast sender so we can observe the
        // child's stdout line-by-line as it is produced, rather than guessing with a fixed
        // sleep.  The previous version waited a hard-coded 300ms after the pid appeared and
        // then killed — which raced PowerShell's slow stdout flush under parallel test load
        // (the marker had not yet reached the pipe when taskkill /F struck, so the final
        // record showed only "\n[killed]\n" and the test failed intermittently).  Waiting for
        // the actual `sandbox_output` event carrying the marker makes the kill deterministic.
        let (tx, mut rx) = broadcast::channel::<Value>(16);
        let id_for_task = id.clone();
        let run_task = tokio::spawn(async move {
            execute_run(
                id_for_task,
                language.to_string(),
                code,
                60_000, // long timeout so the watchdog doesn't fire first
                "terminal".to_string(),
                Some(tx),
                None,
            )
            .await;
        });

        // Wait until the child has actually produced the marker line on its stdout pipe —
        // observed via the streamed `sandbox_output` event — before issuing the kill.  This
        // replaces the racy fixed-delay sleep and makes the test deterministic.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut saw_marker = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(frame)) => {
                    if frame.get("type").and_then(|t| t.as_str()) == Some("sandbox_output")
                        && frame
                            .get("line")
                            .and_then(|l| l.as_str())
                            .is_some_and(|l| l.contains(marker))
                    {
                        saw_marker = true;
                        break;
                    }
                }
                // Sender dropped (run finished) or channel closed — fall through to the
                // assertion below, which will then fail with a clear message.
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_marker,
            "child did not stream the marker within 15s; kill would race stdout flush"
        );

        // Kill the run mid-execution, now that we know the marker is already in the pipe.
        assert!(
            kill_run_by_id(&id),
            "kill_run_by_id should signal a live run"
        );

        // Wait for execute_run to finish reaping the child and writing the final record.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), run_task).await;

        // The final run record must contain BOTH the child's actual output AND the
        // "[killed]" marker — not just the latter.
        let final_output = {
            let store = runs_guard();
            store
                .get(&id)
                .map(|e| e.run.output.clone())
                .unwrap_or_default()
        };
        assert!(
            final_output.contains(marker),
            "final output must preserve the child's stdout marker, got: {final_output}"
        );
        assert!(
            final_output.contains("[killed]"),
            "final output must contain the [killed] marker, got: {final_output}"
        );
        // The timeout message must NOT appear — this was a manual kill, not a timeout.
        assert!(
            !final_output.contains("[timeout]"),
            "manual kill must not be mislabelled as a timeout, got: {final_output}"
        );

        // Clean up the process-global store.
        remove_test_run(&id);
    }

    // ---- SandboxBackend trait tests (B1 cross-platform safe subset) ----

    /// `WindowsSandbox::prepare_command` must set `CREATE_NO_WINDOW` so the packaged
    /// `windows_subsystem = "windows"` app does not flash a conhost window on every sandbox
    /// spawn. Verified indirectly via `util::no_window_tokio`, which is the same helper the
    /// Windows impl calls — so we exercise the helper's contract (CREATE_NO_WINDOW on Windows,
    /// no-op off Windows) rather than inspect tokio's opaque `Command` internals. The spawn must
    /// succeed on every platform, proving the backend's `prepare_command` leaves a spawnable
    /// `Command` in both branches.
    #[tokio::test]
    async fn sandbox_backend_windows_impl_sets_creation_flags() {
        let mut cmd = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "echo" });
        backend().prepare_command(&mut cmd);
        if cfg!(windows) {
            cmd.arg("/c").arg("exit 0");
        } else {
            cmd.arg("ok");
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // A spawn that completes cleanly proves prepare_command left the Command in a valid
        // state — on Windows the CREATE_NO_WINDOW flag is applied by no_window_tokio, which is
        // itself guarded by dotz-core/tests/windowless_guard.rs.
        let status = cmd.status().await;
        assert!(
            status.is_ok(),
            "WindowsSandbox::prepare_command must leave a spawnable Command: {:?}",
            status.err()
        );
    }

    /// `MacSandbox`/`LinuxSandbox::prepare_command` must set `process_group(0)` so the posix
    /// tree-kill (`kill -9 -<pgid>`) can signal the whole group. We can't easily inspect the
    /// process-group flag on a `tokio::process::Command`, so we assert the observable
    /// consequence: a child spawned through the backend ends up in its OWN process group
    /// (pgid == child pid), distinct from this test's process group. posix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn sandbox_backend_posix_impl_sets_process_group() {
        use std::ffi::c_void;
        // libc::getpgid via std::process — call the backend's prepare_command on a real spawn,
        // then read the child's pgid and assert it equals the child's pid (its own group), not
        // this test process's pgid. We can't use `tokio::process::Child::id` + a pgid read
        // without a raw libc call; instead use std::process::Command with a pipe that lets us
        // read the child pid, then check getpgid via a small nix-free helper.
        let backend = crate::sandbox::platform_backend();
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("echo $$")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        backend.prepare_command(&mut cmd);
        let child = cmd.spawn().expect("posix spawn should succeed");
        let pid = child.id().expect("child has a pid");
        let output = child.wait_with_output().await.expect("child reaps");
        let printed_pid: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("child should print its pid");

        // The child printed its own pid; that must equal the Child::id we observed.
        assert_eq!(printed_pid, pid, "child should print its own pid");

        // Read the child's pgid via libc::getpgid. If process_group(0) was applied, pgid == pid
        // (own group). We use a tiny extern-C shim to avoid adding the `nix` crate (ponytail:
        // stdlib + libc FFI, no new heavy deps).
        extern "C" {
            fn getpgid(pid: i32) -> i32;
        }
        // SAFETY: getpgid is a thread-safe POSIX syscall that reads a fixed process attribute;
        // no aliasing, no mutation. pid is a positive integer from a reaped child, so there's a
        // brief window where the pid may already be recycled, but a -1 return (ESRCH) only
        // makes the assertion loose, not unsound.
        let pgid = unsafe { getpgid(pid as i32) };
        assert_eq!(
            pgid, pid as i32,
            "child must be in its own process group (pgid == pid) after prepare_command; \
             got pgid {pgid} for pid {pid}"
        );
        // Suppress an unused-variable warning on the c_void import path if the compiler
        // didn't already absorb it.
        let _: *const c_void = std::ptr::null();
    }

    /// `kill_tree` must dispatch to the platform-correct kill path: taskkill /T /F on Windows,
    /// `kill -9 -<pgid>` on posix. We verify the dispatch end-to-end by spawning a long-lived
    /// child, calling `backend().kill_tree(pid)`, and asserting the child actually dies (and is
    /// reaped, so no zombie lingers). This is the trait-level mirror of the existing
    /// `kill_pid_kills_target_and_reaps_kill_subprocess` test, but routed through the trait so
    /// a future platform impl can't silently diverge from the sandbox's `kill_pid`.
    #[test]
    fn sandbox_backend_kill_tree_dispatches_to_platform() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
                .spawn()
                .expect("powershell should be available")
        } else {
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep should be available")
        };
        // `std::process::Child::id()` returns `u32` on every platform (see note above).
        let pid = child.id();

        // Dispatch through the trait. Each impl reaps its own kill subprocess.
        backend().kill_tree(pid);

        // The target must be killed and reaped. Poll try_wait — the signal was already
        // delivered, so this resolves quickly. Generous deadline for slow taskkill on AV-heavy
        // Windows machines.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        panic!("kill_tree did not kill the target within 15s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
    }

    /// `backend()` must return the SAME `&'static dyn SandboxBackend` across calls — a
    /// single process-wide instance initialized once via `OnceLock`. A new `Box` per call would
    /// defeat the OnceLock and let a future stateful backend (e.g. one that caches a detected
    /// sandbox binary path) re-detect on every spawn.
    #[test]
    fn sandbox_backend_once_lock_returns_same_instance() {
        let a: &'static dyn SandboxBackend = backend();
        let b: &'static dyn SandboxBackend = backend();
        // Same fat pointer (data ptr + vtable) => same OnceLock'd Box => initialized once.
        // std::ptr::eq supports `?Sized` trait objects, comparing both the data pointer and
        // the vtable pointer, so it's the precise "same trait object" check.
        assert!(
            std::ptr::eq(a, b),
            "backend() must return the same &dyn SandboxBackend across calls"
        );
    }

    /// On posix, `kill_pid` must tree-kill the child's entire process group, not just the direct
    /// child. A sandboxed script that backgrounds a long-lived grandchild (e.g. `sleep 30 &`)
    /// would otherwise leak that grandchild as an orphan when the sandbox run is killed or times
    /// out. This test starts a bash run that backgrounds `sleep 30` and prints the grandchild's
    /// pid, kills the run, and asserts the grandchild is also dead — proving the process-group
    /// signal reaches descendants.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_pid_tree_kills_grandchild_on_posix() {
        let (tx, mut rx) = broadcast::channel::<Value>(64);
        let run = start_run(
            "bash",
            "sleep 30 & echo \"GRANDCHILD_PID=$!\"; sleep 30",
            "terminal",
            None,
            60_000,
            Some(tx.clone()),
            None,
        )
        .await
        .expect("start_run should succeed");
        let id = run.id.clone();

        // Collect streamed output until we see the grandchild pid line.
        let mut grandchild_pid: Option<u32> = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while grandchild_pid.is_none() && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(frame)) => {
                    if frame.get("type").and_then(|t| t.as_str()) == Some("sandbox_output") {
                        if let Some(line) = frame.get("line").and_then(|l| l.as_str()) {
                            if let Some(rest) = line.strip_prefix("GRANDCHILD_PID=") {
                                if let Ok(pid) = rest.trim().parse::<u32>() {
                                    grandchild_pid = Some(pid);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let grandchild_pid = grandchild_pid
            .expect("should have received the grandchild pid from sandbox output within 10s");

        // Sanity: the grandchild should be alive right now (it is sleeping for 30s).
        let alive = std::process::Command::new("kill")
            .args(["-0", &grandchild_pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(alive, "grandchild should be alive before the sandbox kill");

        // Kill the sandbox run — this must tree-kill the process group, including the grandchild.
        assert!(
            kill_run_by_id(&id),
            "kill_run_by_id should signal a live run"
        );

        // Wait for execute_run to reach a terminal state.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if lookup(&id).map(|r| r.status != "running").unwrap_or(true) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("sandbox run did not reach terminal state within 15s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // The grandchild must now be dead — the process-group signal reached it. Poll briefly
        // since the OS may take a moment to reap after SIGKILL.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &grandchild_pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("grandchild was not killed by the process-group signal within 10s");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        remove_test_run(&id);
    }
}
