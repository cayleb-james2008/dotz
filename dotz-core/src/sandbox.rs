//! dotz sandbox — real code-execution run store. Port of src/sandbox.ts + server.ts sandbox
//! routes (lines 509-543).
//!
//! A run spins up a program (node/tsx/python/sh/bash/powershell) in a fresh temp dir, captures
//! stdout+stderr (capped ~50KB), and updates the in-memory run record from running → done/error/
//! killed with exitCode + endedAt. POST returns the created run immediately (status "running"); the
//! UI polls GET /api/sandbox/runs/:id for the final output, mirroring server.ts.
//!
//! ponytail: live line-by-line WS streaming (sandbox_output/sandbox_port events) is a follow-up.
//! Phase 4 captures the FINAL output + status + best-effort web-port detection, which is what the
//! REST surface (and the polling UI) needs.
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
use tokio::io::AsyncReadExt;

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

/// Live sandbox run count, for the `/api/health` merge.
pub fn run_count() -> usize {
    runs().lock().unwrap().len()
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
    let store = runs().lock().unwrap();
    let list: Vec<&SandboxRun> = store.values().map(|e| &e.run).collect();
    Json(json!({ "runs": list }))
}

async fn get_run(Path(id): Path<String>) -> Result<Json<SandboxRun>, (StatusCode, Json<Value>)> {
    let store = runs().lock().unwrap();
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
    if !SANDBOX_LANGUAGES.contains(&language.as_str()) {
        return Err(bad(format!(
            "unsupported language: {}. Available: {}",
            language,
            SANDBOX_LANGUAGES.join(", ")
        )));
    }
    // Validate mode — only "terminal"/"web" (or absent → "terminal").
    let mode = match b.get("mode") {
        None | Some(Value::Null) => "terminal".to_string(),
        Some(Value::String(s)) if s == "terminal" || s == "web" => s.clone(),
        _ => return Err(bad("mode must be \"terminal\" or \"web\"")),
    };
    let project_id = b
        .get("projectId")
        .and_then(|v| v.as_str())
        .map(String::from);
    // A non-finite/absent timeout coerces to the default — never silently disable the timeout.
    let timeout_ms = match b.get("timeoutMs").and_then(|v| v.as_i64()) {
        Some(n) if n > 0 => n,
        _ => DEFAULT_TIMEOUT_MS,
    };

    let run = SandboxRun {
        id: uuid::Uuid::new_v4().to_string(),
        project_id,
        language: language.clone(),
        code: code.clone(),
        status: "running".to_string(),
        output: String::new(),
        exit_code: None,
        started_at: now_ms(),
        ended_at: None,
    };
    let id = run.id.clone();
    {
        let mut store = runs().lock().unwrap();
        store.insert(
            id.clone(),
            RunEntry {
                run: run.clone(),
                pid: None,
                killed_by_us: false,
                mode,
                port: None,
            },
        );
    }

    // Spawn the executor task; it updates the store as the child runs and exits.
    tokio::spawn(execute_run(id.clone(), language, code, timeout_ms));

    Ok(Json(run))
}

/// Spawn the child for `language`, capture stdout+stderr, apply the timeout, then update the run.
async fn execute_run(id: String, language: String, code: String, timeout_ms: i64) {
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

    // Record the pid so kill_run can reach the child.
    if let Some(pid) = child.id() {
        if let Some(e) = runs().lock().unwrap().get_mut(&id) {
            e.pid = Some(pid);
        }
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read_out = async {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    };
    let read_err = async {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    };

    let run_fut = async {
        let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
        (out, err, status)
    };

    let result = if timeout_ms > 0 {
        match tokio::time::timeout(Duration::from_millis(timeout_ms as u64), run_fut).await {
            Ok(r) => Some(r),
            Err(_) => None, // timed out
        }
    } else {
        Some(run_fut.await)
    };

    let _ = tokio::fs::remove_dir_all(&temp_dir).await;

    match result {
        Some((out, err, status)) => {
            let mut output = String::new();
            output.push_str(&String::from_utf8_lossy(&out));
            output.push_str(&String::from_utf8_lossy(&err));
            let killed_by_us = runs()
                .lock()
                .unwrap()
                .get(&id)
                .map(|e| e.killed_by_us)
                .unwrap_or(false);
            match status {
                Ok(es) => {
                    let code_n = es.code().map(|c| c as i64);
                    if killed_by_us {
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
        }
        None => {
            // Timeout: kill the child tree, capture whatever output streamed, mark killed.
            mark_killed_by_us(&id);
            kill_pid(child.id());
            let _ = child.start_kill();
            // Drain the captured output now that the pipes are closing.
            let mut out = Vec::new();
            let mut err = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut out).await;
            }
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut err).await;
            }
            let mut output = String::new();
            output.push_str(&String::from_utf8_lossy(&out));
            output.push_str(&String::from_utf8_lossy(&err));
            output.push_str(&format!("\n[timeout] killed after {timeout_ms}ms\n"));
            finish(&id, "killed", None, &cap(&output));
        }
    }
}

/// Truncate output to OUTPUT_CAP bytes (on a char boundary), matching the ~50KB cap.
fn cap(s: &str) -> String {
    if s.len() <= OUTPUT_CAP {
        return s.to_string();
    }
    let mut end = OUTPUT_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]\n", &s[..end])
}

/// Set the terminal state on a run: status, exitCode, output, endedAt; clear the pid.
fn finish(id: &str, status: &str, exit_code: Option<i64>, output: &str) {
    if let Some(e) = runs().lock().unwrap().get_mut(id) {
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
    if let Some(e) = runs().lock().unwrap().get_mut(id) {
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

/// POST /api/sandbox/runs/:id/kill — kill the child, mark killed → { ok }.
async fn kill_run(Path(id): Path<String>) -> Json<Value> {
    let pid = {
        let mut store = runs().lock().unwrap();
        match store.get_mut(&id) {
            Some(e) if e.pid.is_some() && e.run.status == "running" => {
                e.killed_by_us = true;
                e.pid
            }
            _ => None,
        }
    };
    match pid {
        Some(p) => {
            kill_pid(Some(p));
            Json(json!({ "ok": true }))
        }
        None => Json(json!({ "ok": false })),
    }
}

/// GET /api/sandbox/runs/:id/port — best-effort web port detection for mode:"web". 404 if none.
async fn run_port(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (output, mode, cached) = {
        let store = runs().lock().unwrap();
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
            if let Some(e) = runs().lock().unwrap().get_mut(&id) {
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
/// sandbox.ts: a listener keyword near a port-with-prefix on the same line. Avoids bare ":<port>"
/// and the generic word "server" so client-talk lines can't hijack the preview.
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
    let mut out: Vec<u16> = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !KEYWORDS.iter().any(|k| lower.contains(k)) {
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
        let baseline = run_count();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut store = runs().lock().unwrap();
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
        }
        assert_eq!(run_count(), baseline + 1, "run_count should include the inserted entry");
        {
            let mut store = runs().lock().unwrap();
            store.remove(&id);
        }
        assert_eq!(run_count(), baseline, "run_count should return to baseline after removal");
    }
}
