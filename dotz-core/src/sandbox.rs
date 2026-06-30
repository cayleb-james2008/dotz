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
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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
/// Output cap, matching the spirit of the Node streaming buffer — keep memory bounded.
const OUTPUT_CAP: usize = 50 * 1024;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

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
    let timeout_ms = match b.get("timeoutMs").and_then(|v| v.as_i64()) {
        Some(n) if n > 0 => n,
        _ => DEFAULT_TIMEOUT_MS,
    };

    match start_run(
        &language,
        &code,
        &mode,
        project_id.as_deref(),
        timeout_ms,
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

    let run = SandboxRun {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project_id.map(String::from),
        language: language.to_string(),
        code: code.to_string(),
        status: "running".to_string(),
        output: String::new(),
        exit_code: None,
        started_at: now_ms(),
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

    tokio::spawn(execute_run(
        id.clone(),
        language.to_string(),
        code.to_string(),
        timeout_ms,
        mode.to_string(),
        tx,
    ));

    Ok(run)
}

/// Look up a run by id. Used by the WebSocket loop to poll for terminal status.
pub fn lookup(id: &str) -> Option<SandboxRun> {
    runs_guard().get(id).map(|e| e.run.clone())
}

#[cfg(test)]
/// Remove a run entry from the in-memory store. Test-only helper so WebSocket sandbox tests can
/// clean up the process-global runs map.
pub fn remove_test_run(id: &str) {
    runs_guard().remove(id);
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
) {
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

    let mut command = tokio::process::Command::new(cmd[0]);
    command
        .args(&cmd[1..])
        .current_dir(&temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW); // inherent on tokio::process::Command (no CommandExt import needed)
    }

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
        if let Some(e) = runs_guard().get_mut(&id) {
            e.pid = Some(pid);
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
    let (output, status) = match tx {
        Some(tx) => {
            let st = tokio::spawn(stream_output(id.clone(), mode.clone(), stdout, stderr, tx));
            let (status, output) = tokio::join!(child.wait(), st);
            (output.unwrap_or_default(), status)
        }
        None => {
            let read_out = async {
                let mut buf = Vec::new();
                if let Some(mut s) = stdout {
                    let _ = s.read_to_end(&mut buf).await;
                }
                buf
            };
            let read_err = async {
                let mut buf = Vec::new();
                if let Some(mut s) = stderr {
                    let _ = s.read_to_end(&mut buf).await;
                }
                buf
            };
            let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
            let output = String::from_utf8_lossy(&out).to_string() + &String::from_utf8_lossy(&err);
            (output, status)
        }
    };

    if let Some(w) = watchdog {
        let _ = w.abort();
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
                output.push_str(&format!("\n[timeout] killed after {timeout_ms}ms\n"));
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
async fn detect_port_in_window(
    id: &str,
    line: &str,
    window: &[String],
    tx: &broadcast::Sender<Value>,
) {
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
    if let Some(e) = runs_guard().get_mut(id) {
        // Don't clobber a run already moved to a terminal state (e.g. kill raced the timeout).
        if e.run.status != "running" {
            return;
        }
        e.run.status = status.to_string();
        e.run.exit_code = exit_code;
        e.run.output = output.to_string();
        e.run.ended_at = Some(now_ms());
        e.pid = None;
    }
}

fn mark_killed_by_us(id: &str) {
    if let Some(e) = runs_guard().get_mut(id) {
        e.killed_by_us = true;
    }
}

/// Kill a pid and its descendants — taskkill /T /F on win32, SIGKILL on posix. Best-effort.
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
        // SIGKILL via libc-free path: the standard `kill` binary.
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
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
                e.run.ended_at = Some(now_ms());
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
                        started_at: now_ms(),
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
                        started_at: now_ms(),
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
        // Windows, but it still validates the cleanup path everywhere.)
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
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
                        started_at: now_ms(),
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
        // via `runs_guard()` in the normal (non-timeout) exit path.
        let (language, code) = if cfg!(windows) {
            ("powershell", "Write-Output ok")
        } else {
            ("bash", "echo ok")
        };
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            5000,
            "terminal".to_string(),
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
                        started_at: now_ms(),
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
                        started_at: now_ms(),
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
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            5000,
            "terminal".to_string(),
            Some(tx),
        )
        .await;

        let mut found = false;
        while let Ok(frame) = rx.try_recv() {
            assert_eq!(
                frame.get("runId").and_then(|r| r.as_str()),
                Some(id.as_str())
            );
            if frame.get("type").and_then(|t| t.as_str()) == Some("sandbox_output") {
                if frame.get("line").and_then(|l| l.as_str()) == Some("dotz-stream-test") {
                    assert_eq!(frame.get("stream").and_then(|s| s.as_str()), Some("stdout"));
                    found = true;
                }
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
                        started_at: now_ms(),
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
        execute_run(
            id.clone(),
            language.to_string(),
            code.to_string(),
            5000,
            "web".to_string(),
            Some(tx),
        )
        .await;

        let mut found = false;
        while let Ok(frame) = rx.try_recv() {
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
                        started_at: now_ms(),
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
}
