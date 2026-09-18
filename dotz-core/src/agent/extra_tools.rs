//! The remaining pi tools, ported from the dotz-tools extension: agents_md (doctrine), create_agent,
//! create_skill, rsi_baseline/rsi_compare (the gate metrics), and human_gate (WS approval round-trip).
use super::tools::{Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

// (RESOURCE_NAME kept as documentation of the regex valid_name enforces; the `const _` alias
// below did not satisfy rustc's dead-code analysis, so the attribute is explicit.)
#[allow(dead_code)]
const RESOURCE_NAME: &str = r"^[a-z][a-z0-9-]{1,63}$";
fn valid_name(name: &str) -> bool {
    let n = name.trim();
    let bytes = n.as_bytes();
    if n.len() < 2 || n.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    n.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
// ---- agents_md: read/write the project AGENTS.md doctrine ----
struct AgentsMdTool;
#[async_trait]
impl Tool for AgentsMdTool {
    fn name(&self) -> &'static str {
        "agents_md"
    }
    fn description(&self) -> &'static str {
        "Read or write the project's AGENTS.md doctrine. Args: {action:\"read\"|\"write\", content?}. read returns the file; write replaces it."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "action": { "type": "string", "enum": ["read", "write"] }, "content": { "type": "string" } }, "required": ["action"] })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let file = ctx.cwd.join("AGENTS.md");
        match args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("read")
        {
            "read" => Ok(tokio::fs::read_to_string(&file).await.unwrap_or_default()),
            "write" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or("content is required for write")?;
                tokio::fs::write(&file, content)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(format!(
                    "wrote {} ({} bytes)",
                    file.display(),
                    content.len()
                ))
            }
            other => Err(format!("unknown action: {other}")),
        }
    }
}

// ---- create_agent: persist a subagent (.pi user agents dir; subagent discovery reads it) ----
struct CreateAgentTool;
#[async_trait]
impl Tool for CreateAgentTool {
    fn name(&self) -> &'static str {
        "create_agent"
    }
    fn description(&self) -> &'static str {
        "Create a persistent subagent. Args: {name (lowercase-hyphen), description, systemPrompt}. Written to ~/.pi/agent/agents/<name>.md; never overwrites an existing agent."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "name": { "type": "string" }, "description": { "type": "string" }, "systemPrompt": { "type": "string" } }, "required": ["name", "systemPrompt"] })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if !valid_name(name) {
            return Err(
                "name must be lowercase letters/digits/hyphens, 2-64 chars, starting with a letter"
                    .into(),
            );
        }
        let prompt = args
            .get("systemPrompt")
            .and_then(|v| v.as_str())
            .ok_or("systemPrompt is required")?;
        let desc = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let dir = dirs::home_dir()
            .ok_or("no home dir")?
            .join(".pi")
            .join("agent")
            .join("agents");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let file = dir.join(format!("{name}.md"));
        if tokio::fs::try_exists(&file)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err(format!("agent \"{name}\" already exists"));
        }
        // Discovery (subagent.rs load_agents_from_dir) requires `---` frontmatter with BOTH
        // name and description keys and silently skips files without them — plain markdown
        // here means the created agent could never be invoked. Mirror the bundled
        // .pi/agents/*.md shape; single-line values so a crafted name/desc can't break out
        // of the frontmatter block.
        let fm_name = name.replace(['\r', '\n'], " ");
        let fm_desc = if desc.is_empty() {
            fm_name.clone()
        } else {
            desc.replace(['\r', '\n'], " ")
        };
        let body = format!("---\nname: {fm_name}\ndescription: {fm_desc}\n---\n\n{prompt}\n");
        tokio::fs::write(&file, body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("created agent \"{name}\" at {}", file.display()))
    }
}

// ---- create_skill: persist a user skill in the dotz skills root (skills::scan_roots reads it) ----
struct CreateSkillTool;
#[async_trait]
impl Tool for CreateSkillTool {
    fn name(&self) -> &'static str {
        "create_skill"
    }
    fn description(&self) -> &'static str {
        "Create a persistent skill. Args: {name (lowercase-hyphen), description, body}. Written to ~/.dotz/ai-agents/skills/<name>/SKILL.md; never overwrites an existing skill."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "name": { "type": "string" }, "description": { "type": "string" }, "body": { "type": "string" } }, "required": ["name", "description", "body"] })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if !valid_name(name) {
            return Err(
                "name must be lowercase letters/digits/hyphens, 2-64 chars, starting with a letter"
                    .into(),
            );
        }
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.replace(['\r', '\n'], " ").trim().to_string())
            .unwrap_or_default();
        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if description.is_empty() {
            return Err("description is required".into());
        }
        if body.is_empty() {
            return Err("body is required".into());
        }
        let dir = crate::config::dotz_dir()
            .join("ai-agents")
            .join("skills")
            .join(name);
        let file = dir.join("SKILL.md");
        if tokio::fs::try_exists(&file)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err(format!("skill \"{name}\" already exists"));
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        // Frontmatter with JSON-quoted values (mirrors createUserSkill in skills.ts).
        let content = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            serde_json::to_string(name).unwrap_or_default(),
            serde_json::to_string(&description).unwrap_or_default(),
            body
        );
        tokio::fs::write(&file, content)
            .await
            .map_err(|e| e.to_string())?;
        // Rebuild the skill index so the new skill is immediately visible to list_skills,
        // get_skill, and the `skill` tool — without this the cache would stay stale until restart.
        // The scan walks the filesystem; run it on the blocking pool so it does not stall the
        // async runtime during an agent turn.
        tokio::task::spawn_blocking(crate::skills::reload_index)
            .await
            .map_err(|e| format!("reload index: {e}"))?;
        Ok(format!(
            "created skill \"{name}\" at {} (available immediately)",
            file.display()
        ))
    }
}

// ---- rsi_baseline / rsi_compare: run the project gate, capture/compare pass/fail counts ----
static BASELINE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
fn baselines() -> &'static Mutex<HashMap<String, Value>> {
    BASELINE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the RSI baselines mutex, recovering from a poisoned lock. A panic while holding the
/// baselines lock (e.g. inside a serde callback or a baseline comparison) must not permanently
/// brick the `rsi_baseline` / `rsi_compare` tools.
fn baselines_guard() -> std::sync::MutexGuard<'static, HashMap<String, Value>> {
    baselines()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run the gate command in cwd (default: `npm test`), capture output, parse pass/fail counts.
/// Pick the default gate command from the project manifest: Rust crates run
/// `cargo test -p <name>`; the dotz workspace root is detected and pinned to
/// `cargo test -p dotz-core` (the actual project gate). Node projects run `npm test`.
fn default_gate_command(cwd: &Path) -> String {
    if cwd.join("Cargo.toml").exists() {
        if let Ok(raw) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
            if let Some(name) = cargo_package_name(&raw) {
                return format!("cargo test -p {name}");
            }
            if workspace_has_dotz_core(&raw) {
                return "cargo test -p dotz-core".into();
            }
        }
        return "cargo test".into();
    }
    if cwd.join("package.json").exists() {
        return "npm test".into();
    }
    "cargo test".into()
}

/// Extract `name` from a `[package]` table (no TOML dep — frontmatter-style line scan).
fn cargo_package_name(cargo_toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "name" {
                    let mut v = value.trim();
                    // Strip a trailing TOML comment so `name = "foo" # comment` does not leak
                    // the comment into the gate command.
                    if let Some(idx) = v.find('#') {
                        v = &v[..idx];
                    }
                    let v = v.trim().trim_matches(|c| c == '\"' || c == '\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// True when the root `Cargo.toml` is a workspace that lists `dotz-core` as a member.
fn workspace_has_dotz_core(cargo_toml: &str) -> bool {
    let mut in_workspace = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_workspace = trimmed == "[workspace]";
            continue;
        }
        if in_workspace && (trimmed.contains("\"dotz-core\"") || trimmed.contains("'dotz-core'")) {
            return true;
        }
    }
    false
}

/// Configurable wall-clock timeout for `rsi_baseline` / `rsi_compare` gate runs. A hung test
/// suite (waiting for network, an interactive prompt, or a deadlock) otherwise blocks the RSI
/// loop forever. Defaults to 10 minutes; override with `DOTZ_GATE_TIMEOUT_MS` (clamped to [1s, 1h]).
fn gate_timeout() -> Duration {
    const DEFAULT_MS: u64 = 600_000; // 10 minutes
    const MIN_MS: u64 = 1_000; // 1 second — zero would time out before any gate starts
    const MAX_MS: u64 = 3_600_000; // 1 hour
    std::env::var("DOTZ_GATE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_MS))
}

async fn run_gate(cwd: &Path, command: Option<&str>) -> Value {
    let cwd = cwd.to_path_buf();
    let command = command
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_gate_command(&cwd));
    let timeout = gate_timeout();

    // Build the command with the child placed in its OWN process group (POSIX) so a timeout
    // can tree-kill the shell AND every descendant (cargo/npm/sleep …) with `kill -9 -<pgrp>`.
    // Without this, `start_kill` only terminates the shell and leaves the real workload running
    // as an orphan that keeps consuming CPU — the exact bug already fixed in the `bash` tool.
    // tokio's Command doesn't expose process_group, so build a std Command and convert.
    #[cfg(windows)]
    let mut child = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match crate::util::no_window_tokio(&mut c).spawn() {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "error": format!("gate run failed: {e}"),
                    "ok": false,
                    "passed": 0,
                    "failed": 0,
                });
            }
        }
    };
    #[cfg(not(windows))]
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut sc = std::process::Command::new("sh");
        sc.arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        match tokio::process::Command::from(sc).spawn() {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "error": format!("gate run failed: {e}"),
                    "ok": false,
                    "passed": 0,
                    "failed": 0,
                });
            }
        }
    };

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let run_fut = async {
        let (status, _, _) = tokio::join!(
            child.wait(),
            async {
                if let Some(s) = stdout.as_mut() {
                    let _ = s.read_to_end(&mut stdout_buf).await;
                }
            },
            async {
                if let Some(s) = stderr.as_mut() {
                    let _ = s.read_to_end(&mut stderr_buf).await;
                }
            },
        );
        status
    };

    match tokio::time::timeout(timeout, run_fut).await {
        Ok(Ok(status)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&stdout_buf),
                String::from_utf8_lossy(&stderr_buf)
            );
            let (passed, failed) = parse_counts(&text);
            // Failure-catalog countermeasure #1 (value-blind objective): a gate that exits 0
            // while producing ZERO parseable test evidence (passed:0, failed:0) is
            // UNOBSERVABLE — it must read RED, never green. An empty/missing suite counting
            // green is exactly how the RSI loop promoted on zero information.
            let observable = passed > 0 || failed > 0;
            let ok = status.success() && observable;
            let tail: String = text
                .chars()
                .rev()
                .take(800)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if status.success() && !observable {
                json!({
                    "exitCode": status.code(),
                    "ok": false,
                    "unobservable": true,
                    "error": "gate unobservable: command exited 0 but produced no test counts (0 passed / 0 failed) — an empty suite does not count green",
                    "passed": passed,
                    "failed": failed,
                    "tail": tail,
                })
            } else {
                json!({
                    "exitCode": status.code(),
                    "ok": ok,
                    "passed": passed,
                    "failed": failed,
                    "tail": tail,
                })
            }
        }
        Ok(Err(e)) => json!({
            "error": format!("gate run failed: {e}"),
            "ok": false,
            "passed": 0,
            "failed": 0,
        }),
        Err(_) => {
            // Tree-kill the child AND its descendants so a timed-out `cargo test` / `npm run
            // build` does not leak orphaned processes that keep consuming CPU. `start_kill`
            // only terminates the direct child (the shell), leaving the spawned workload alive.
            //
            // Windows: `taskkill /PID <pid> /T /F` kills the whole process tree.  POSIX: the
            // child was placed in its own process group at spawn, so `kill -9 -<pgrp>` reaps
            // the entire group — shell + every descendant.
            // Use .status() (not .spawn()) so the kill/taskkill subprocess is reaped instead of
            // leaking a zombie/handle — same fix the bash tool's timeout path already carries.
            #[cfg(windows)]
            {
                if let Some(pid) = child.id() {
                    let mut kc = std::process::Command::new("taskkill");
                    kc.args(["/PID", &pid.to_string(), "/T", "/F"])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    let _ = crate::util::no_window(&mut kc).status();
                }
            }
            #[cfg(not(windows))]
            {
                if let Some(pid) = child.id() {
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &format!("-{pid}")])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
            // Reap the killed child so it does not become a zombie (Unix) or leak handles
            // (Windows) after the timeout path returns.
            let _ = child.wait().await;
            json!({
                "error": format!("[timeout] killed after {}ms", timeout.as_millis()),
                "ok": false,
                "passed": 0,
                "failed": 0,
            })
        }
    }
}

/// Parse common test-runner output for pass/fail counts (node:test, vitest, jest, pytest).
///
/// Only counts the keyword when it appears as a standalone token (not as a substring of a
/// longer word like "passphrase" or "failure") so unrelated output cannot inflate the counts.
fn parse_counts(text: &str) -> (i64, i64) {
    let num_around = |kw: &str| -> i64 {
        for line in text.lines() {
            let l = line.to_lowercase();
            let mut search_from = 0usize;
            while let Some(rel) = l[search_from..].find(kw) {
                let idx = search_from + rel;
                let after_idx = idx + kw.len();
                let boundary_ok = {
                    let before = idx.checked_sub(1).map(|i| l.as_bytes()[i]);
                    let after = l.as_bytes().get(after_idx).copied();
                    !before.map(|b| b.is_ascii_alphanumeric()).unwrap_or(false)
                        && !after.map(|b| b.is_ascii_alphanumeric()).unwrap_or(false)
                };
                if boundary_ok {
                    // Number immediately before the keyword (only whitespace/punctuation in between).
                    let mut i = idx;
                    while i > 0 && l.as_bytes()[i - 1].is_ascii_whitespace() {
                        i -= 1;
                    }
                    let mut j = i;
                    while j > 0 && l.as_bytes()[j - 1].is_ascii_digit() {
                        j -= 1;
                    }
                    if j < i {
                        if let Ok(v) = l[j..i].parse::<i64>() {
                            return v;
                        }
                    }
                    // Number immediately after the keyword (node:test "# pass N" / "# fail N").
                    let mut k = after_idx;
                    while k < l.len() && l.as_bytes()[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let mut m = k;
                    while m < l.len() && l.as_bytes()[m].is_ascii_digit() {
                        m += 1;
                    }
                    if m > k {
                        if let Ok(v) = l[k..m].parse::<i64>() {
                            return v;
                        }
                    }
                }
                search_from = idx + 1;
            }
        }
        0
    };
    // node:test "# pass N / # fail N"; vitest/jest "N passed / N failed"; pytest "N passed / N failed".
    let passed = num_around("pass").max(num_around("passed"));
    let failed = num_around("fail").max(num_around("failed"));
    (passed, failed)
}

struct RsiBaselineTool;
#[async_trait]
impl Tool for RsiBaselineTool {
    fn name(&self) -> &'static str {
        "rsi_baseline"
    }
    fn description(&self) -> &'static str {
        "Capture the project's test-gate baseline (run the gate, record pass/fail). Args: {command?}. Stores the baseline for a later rsi_compare."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "command": { "type": "string" } } })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let cmd = args.get("command").and_then(|v| v.as_str());
        let result = run_gate(&ctx.cwd, cmd).await;
        let key = ctx.cwd.to_string_lossy().to_string();
        baselines_guard().insert(key, result.clone());
        Ok(format!("baseline captured: {result}"))
    }
}

struct RsiCompareTool;
#[async_trait]
impl Tool for RsiCompareTool {
    fn name(&self) -> &'static str {
        "rsi_compare"
    }
    fn description(&self) -> &'static str {
        "Re-run the gate and compare against the rsi_baseline (regression/improvement). Args: {command?}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "command": { "type": "string" } } })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let key = ctx.cwd.to_string_lossy().to_string();
        let base = baselines_guard().get(&key).cloned();
        let Some(base) = base else {
            return Err("no baseline — call rsi_baseline first".into());
        };
        let cmd = args.get("command").and_then(|v| v.as_str());
        let now = run_gate(&ctx.cwd, cmd).await;
        let bp = base.get("passed").and_then(|v| v.as_i64()).unwrap_or(0);
        let bf = base.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
        let np = now.get("passed").and_then(|v| v.as_i64()).unwrap_or(0);
        let nf = now.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
        // Failure-catalog #1: an unobservable current gate (exit 0, zero parsed counts) can
        // never be read as NO CHANGE / IMPROVEMENT — surface it as its own hard verdict.
        let now_unobservable = now
            .get("unobservable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let verdict = if now_unobservable {
            "UNOBSERVABLE"
        } else if nf > bf || np < bp {
            "REGRESSION"
        } else if np > bp || nf < bf {
            "IMPROVEMENT"
        } else {
            "NO CHANGE"
        };
        Ok(json!({ "verdict": verdict, "baseline": { "passed": bp, "failed": bf }, "current": { "passed": np, "failed": nf }, "ok": now.get("ok") }).to_string())
    }
}

// ---- human_gate: emit a {kind:"gate"} frame over the session WS, block until approve/reject ----
/// Pending human-gate resolutions keyed by gate id.
type GateMap = HashMap<String, oneshot::Sender<(bool, Option<String>)>>;
static GATES: OnceLock<Mutex<GateMap>> = OnceLock::new();
fn gates() -> &'static Mutex<GateMap> {
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the human-gate mutex, recovering from a poisoned lock. A panic while holding the gates
/// lock (e.g. inside a gate resolution callback) must not permanently brick the `human_gate`
/// tool or the /ws `gate.approve` / `gate.reject` handler.
fn gates_guard() -> std::sync::MutexGuard<'static, GateMap> {
    gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Called by the /ws handler on a {kind:"gate.approve"|"gate.reject"} client message.
pub fn resolve_gate(gate_id: &str, approved: bool, feedback: Option<String>) {
    if let Some(tx) = gates_guard().remove(gate_id) {
        let _ = tx.send((approved, feedback));
    }
}

struct HumanGateTool;
#[async_trait]
impl Tool for HumanGateTool {
    fn name(&self) -> &'static str {
        "human_gate"
    }
    fn description(&self) -> &'static str {
        "Request human approval before proceeding. Args: {plan}. Shows the plan to the operator and BLOCKS until they approve or reject (5-min timeout)."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "plan": { "type": "string" } }, "required": ["plan"] })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let plan = args
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("(no plan provided)");
        let Some(ws_tx) = ctx.tx.as_ref() else {
            return Err(
                "human_gate is only available in an interactive session (not a subagent)".into(),
            );
        };
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        gates_guard().insert(id.clone(), tx);
        // Emit the gate request over the session WS (app.js renders the approval card).
        let _ = ws_tx.send(json!({ "kind": "gate", "gateId": id, "plan": plan }));
        let result = tokio::time::timeout(Duration::from_secs(300), rx).await;
        gates_guard().remove(&id);
        match result {
            Ok(Ok((true, feedback))) => Ok(format!(
                "APPROVED{}",
                feedback.map(|f| format!(": {f}")).unwrap_or_default()
            )),
            Ok(Ok((false, feedback))) => Err(format!(
                "REJECTED{}",
                feedback.map(|f| format!(": {f}")).unwrap_or_default()
            )),
            _ => Err("human gate timed out (no operator response in 5 minutes)".into()),
        }
    }
}

// ---- OpenSpec-native tools ----
struct OpenSpecStatusTool;
#[async_trait]
impl Tool for OpenSpecStatusTool {
    fn name(&self) -> &'static str {
        "openspec_status"
    }
    fn description(&self) -> &'static str {
        "Inspect native OpenSpec-compatible spec state for this project. Args: {}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        Ok(crate::specs::status_for_cwd(&ctx.cwd).to_string())
    }
}

struct OpenSpecExploreTool;
#[async_trait]
impl Tool for OpenSpecExploreTool {
    fn name(&self) -> &'static str {
        "openspec_explore"
    }
    fn description(&self) -> &'static str {
        "Explore active/archived OpenSpec changes, artifacts, readiness, and optional CLI import availability. Args: {}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        Ok(crate::specs::status_for_cwd(&ctx.cwd).to_string())
    }
}

struct OpenSpecProposeTool;
#[async_trait]
impl Tool for OpenSpecProposeTool {
    fn name(&self) -> &'static str {
        "openspec_propose"
    }
    fn description(&self) -> &'static str {
        "Create an OpenSpec-compatible change at openspec/changes/<slug>/ with proposal.md, design.md, tasks.md, specs/, and readiness.md. Args: {title, description?, slug?}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "description": { "type": "string" },
                "slug": { "type": "string" }
            },
            "required": ["title"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let req = crate::specs::CreateSpecChange {
            project_id: None,
            title: args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            slug: args
                .get("slug")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };
        let change = crate::specs::create_change(&ctx.cwd, req)?;
        Ok(serde_json::to_string(&json!({ "change": change })).unwrap_or_default())
    }
}

macro_rules! openspec_action_tool {
    ($name:ident, $tool_name:literal, $desc:literal, $func:path) => {
        struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &'static str {
                $tool_name
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn parameters(&self) -> Value {
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Spec change id/slug" }
                    },
                    "required": ["id"]
                })
            }
            async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or("id is required")?;
                let result = $func(&ctx.cwd, id)?;
                Ok(result.to_string())
            }
        }
    };
}

openspec_action_tool!(
    OpenSpecApplyTool,
    "openspec_apply",
    "Mark a spec change as applying and return its task list. Args: {id}.",
    crate::specs::apply_change
);
openspec_action_tool!(
    OpenSpecVerifyTool,
    "openspec_verify",
    "Verify a spec change has required artifacts, completed tasks, and completed readiness gates. Args: {id}.",
    crate::specs::verify_change
);
openspec_action_tool!(
    OpenSpecSyncTool,
    "openspec_sync",
    "Sync change-local specs into openspec/specs/ and mark the change ready. Args: {id}.",
    crate::specs::sync_change
);
openspec_action_tool!(
    OpenSpecArchiveTool,
    "openspec_archive",
    "Archive a completed spec change under openspec/changes/archive/. Args: {id}.",
    crate::specs::archive_change
);

// ---- living-doc tools ----
struct LivingDocsReadTool;
#[async_trait]
impl Tool for LivingDocsReadTool {
    fn name(&self) -> &'static str {
        "living_docs_read"
    }
    fn description(&self) -> &'static str {
        "Read project or global living docs. Args: {scope?:\"project\"|\"global\"}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["project", "global"] }
            }
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("project");
        let docs = crate::living_docs::list_docs(
            scope,
            if scope == "project" {
                Some(ctx.cwd.as_path())
            } else {
                None
            },
        );
        Ok(serde_json::to_string(&json!({ "docs": docs })).unwrap_or_default())
    }
}

struct LivingDocsUpdateTool;
#[async_trait]
impl Tool for LivingDocsUpdateTool {
    fn name(&self) -> &'static str {
        "living_docs_update"
    }
    fn description(&self) -> &'static str {
        "Replace one living-doc file. Args: {scope?:\"project\"|\"global\", kind, content}; kind is anti_patterns, non_inferables, context_scope, or living_docs."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["project", "global"] },
                "kind": { "type": "string", "enum": ["anti_patterns", "non_inferables", "context_scope", "living_docs"] },
                "content": { "type": "string" }
            },
            "required": ["kind", "content"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("project");
        let kind_value = args.get("kind").cloned().ok_or("kind is required")?;
        let kind: crate::types::LivingDocKind =
            serde_json::from_value(kind_value).map_err(|e| format!("invalid kind: {e}"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("content is required")?;
        let doc = crate::living_docs::write_doc(
            scope,
            if scope == "project" {
                Some(ctx.cwd.as_path())
            } else {
                None
            },
            &kind,
            content,
        )?;
        Ok(serde_json::to_string(&json!({ "doc": doc })).unwrap_or_default())
    }
}

struct LivingDocsSuggestTool;
#[async_trait]
impl Tool for LivingDocsSuggestTool {
    fn name(&self) -> &'static str {
        "living_docs_suggest"
    }
    fn description(&self) -> &'static str {
        "Queue an ambiguous living-doc suggestion for operator review. Args: {scope?, kind, text, confidence?}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": { "type": "string", "enum": ["project", "global"] },
                "kind": { "type": "string", "enum": ["anti_patterns", "non_inferables", "context_scope", "living_docs"] },
                "text": { "type": "string" },
                "confidence": { "type": "number" }
            },
            "required": ["kind", "text"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("project");
        let kind_value = args.get("kind").cloned().ok_or("kind is required")?;
        let kind: crate::types::LivingDocKind =
            serde_json::from_value(kind_value).map_err(|e| format!("invalid kind: {e}"))?;
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or("text is required")?
            .to_string();
        let confidence = args
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5)
            .clamp(0.0, 1.0) as f32;
        let suggestion = crate::living_docs::add_suggestion(
            scope,
            if scope == "project" {
                Some(ctx.cwd.as_path())
            } else {
                None
            },
            kind,
            text,
            confidence,
            "agent_tool".to_string(),
        )?;
        Ok(serde_json::to_string(&json!({ "suggestion": suggestion })).unwrap_or_default())
    }
}

// ---- VCS tools ----
struct VcsStatusTool;
#[async_trait]
impl Tool for VcsStatusTool {
    fn name(&self) -> &'static str {
        "vcs_status"
    }
    fn description(&self) -> &'static str {
        "Inspect git status, branch, upstream, dirty state, and GitHub CLI readiness for this project. Args: {}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        Ok(
            serde_json::to_string(&json!({ "status": crate::vcs::status_for_cwd(&ctx.cwd) }))
                .unwrap_or_default(),
        )
    }
}

struct VcsBranchTool;
#[async_trait]
impl Tool for VcsBranchTool {
    fn name(&self) -> &'static str {
        "vcs_branch"
    }
    fn description(&self) -> &'static str {
        "Create or reuse a dotz/<slug> branch for a spec change. Args: {slug|name}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string" },
                "name": { "type": "string" }
            }
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let branch = args
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("slug").and_then(|v| v.as_str()));
        Ok(crate::vcs::create_or_checkout_branch(&ctx.cwd, branch)?.to_string())
    }
}

struct VcsAtomicCommitTool;
#[async_trait]
impl Tool for VcsAtomicCommitTool {
    fn name(&self) -> &'static str {
        "vcs_atomic_commit"
    }
    fn description(&self) -> &'static str {
        "Stage and commit one logical verified task. Args: {message, body?, files?}. Omitting files stages all changes."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" },
                "body": { "type": "string" },
                "files": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["message"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let mut req: crate::types::AtomicCommitRequest =
            serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
        req.project_id = None;
        Ok(crate::vcs::atomic_commit(&ctx.cwd, req)?.to_string())
    }
}

struct VcsPrTool;
#[async_trait]
impl Tool for VcsPrTool {
    fn name(&self) -> &'static str {
        "vcs_pr"
    }
    fn description(&self) -> &'static str {
        "Create a GitHub PR with gh when installed and logged in. Args: {title?, body?, base?, draft?}. Fails closed if gh auth is unavailable."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" },
                "base": { "type": "string" },
                "draft": { "type": "boolean" }
            }
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let mut req: crate::vcs::PrRequest =
            serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
        req.project_id = None;
        Ok(crate::vcs::create_pr(&ctx.cwd, req)?.to_string())
    }
}

struct VcsRollbackTool;
#[async_trait]
impl Tool for VcsRollbackTool {
    fn name(&self) -> &'static str {
        "vcs_rollback"
    }
    fn description(&self) -> &'static str {
        "Rollback by checkpoint, git revert, or confirmed git reset. Args: {target, mode:\"checkpoint\"|\"revert\"|\"reset\", confirm?}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": { "type": "string" },
                "mode": { "type": "string", "enum": ["checkpoint", "revert", "reset"] },
                "confirm": { "type": "boolean" }
            },
            "required": ["target"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let mut target: crate::types::RollbackTarget =
            serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
        target.project_id = None;
        Ok(crate::vcs::rollback(&ctx.cwd, target)?.to_string())
    }
}

// ---- Design tools (thin wrappers over crate::design; light up the DESIGN panel node) ----
struct DesignListTool;
#[async_trait]
impl Tool for DesignListTool {
    fn name(&self) -> &'static str {
        "design_list"
    }
    fn description(&self) -> &'static str {
        "List the bundled Open Design systems (id, name, category, description) to pick one to build on-brand. Args: {}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        Ok(serde_json::to_string(&crate::design::list_systems_value().await).unwrap_or_default())
    }
}

struct DesignUseTool;
#[async_trait]
impl Tool for DesignUseTool {
    fn name(&self) -> &'static str {
        "design_use"
    }
    fn description(&self) -> &'static str {
        "Load one Open Design system's reference components.html so you can paste its :root tokens and build on-brand. Args: {id}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("id is required")?;
        crate::design::system_components_html(id).await
    }
}

// ---- Sandbox tool (thin wrapper over crate::sandbox::start_run; lights up the SANDBOX panel node) ----
struct SandboxRunTool;
#[async_trait]
impl Tool for SandboxRunTool {
    fn name(&self) -> &'static str {
        "sandbox_run"
    }
    fn description(&self) -> &'static str {
        "Run code in a disposable sandbox and return the created run (poll status via the panel). \
         terminal mode streams stdout/stderr; web mode serves the app on a local port (its url/port \
         is on the returned run) for E2E. Args: {language, code, mode?:\"terminal\"|\"web\", timeoutMs?}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": { "type": "string" },
                "code": { "type": "string" },
                "mode": { "type": "string", "enum": ["terminal", "web"] },
                "timeoutMs": { "type": "number" }
            },
            "required": ["language", "code"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let language = args
            .get("language")
            .and_then(|v| v.as_str())
            .ok_or("language is required")?;
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or("code is required")?;
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("terminal");
        // Run in the session cwd so a real `cargo test` / `npm run build` isn't vacuous in an empty
        // temp dir. Default 30s; callers can extend for slow builds.
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
            .unwrap_or(30_000);
        let run = crate::sandbox::start_run(
            language,
            code,
            mode,
            None,
            timeout_ms,
            ctx.tx.clone(),
            Some(ctx.cwd.clone()),
        )
        .await?;
        Ok(serde_json::to_string(&run).unwrap_or_default())
    }
}

// ---- open-connector bridge tools (optional, config-gated; see crate::connectors) ----
/// List the configured open-connector gateway connectors, or one connector's action catalog.
struct ConnectorListTool;
#[async_trait]
impl Tool for ConnectorListTool {
    fn name(&self) -> &'static str {
        "connector_list"
    }
    fn description(&self) -> &'static str {
        "List configured open-connector gateway connectors, or fetch one connector's action \
         catalog. Args: {connector?}. No connector => the configured connectors (id, host, \
         enabled); with a connector id => that gateway's OpenAPI catalog of invokable actions. \
         Returns an empty list when no gateway is configured."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "connector": { "type": "string" } } })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        match args.get("connector").and_then(|v| v.as_str()) {
            None | Some("") => {
                Ok(serde_json::to_string(&crate::connectors::list_summary()).unwrap_or_default())
            }
            Some(id) => {
                let c = crate::connectors::enabled_by_id(id)
                    .ok_or_else(|| format!("no enabled connector '{id}' configured"))?;
                let client = reqwest::Client::new();
                let catalog = crate::connectors::fetch_catalog(&client, &c).await?;
                Ok(serde_json::to_string(&catalog).unwrap_or_default())
            }
        }
    }
}

/// Invoke a gateway action, proxying to a running open-connector gateway.
struct ConnectorActionTool;
#[async_trait]
impl Tool for ConnectorActionTool {
    fn name(&self) -> &'static str {
        "connector_action"
    }
    fn description(&self) -> &'static str {
        "Invoke an action on a configured open-connector gateway (credentials stay behind the \
         gateway). Args: {connector, action, input?}. `connector` is a configured connector id, \
         `action` is the '<provider>.<action>' identifier (e.g. 'github.create_issue'), `input` \
         is the action's argument object. Use connector_list to discover connectors and actions."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "connector": { "type": "string" },
                "action": { "type": "string" },
                "input": { "type": "object" }
            },
            "required": ["connector", "action"]
        })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let connector = args
            .get("connector")
            .and_then(|v| v.as_str())
            .ok_or("connector is required")?;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or("action is required")?;
        let input = args.get("input").cloned().unwrap_or_else(|| json!({}));
        let c = crate::connectors::enabled_by_id(connector)
            .ok_or_else(|| format!("no enabled connector '{connector}' configured"))?;
        let client = reqwest::Client::new();
        let result = crate::connectors::invoke_action(&client, &c, action, input).await?;
        Ok(serde_json::to_string(&result).unwrap_or_default())
    }
}

// ---- MCP tool (C3: route a call to a connected MCP server's tool) ----
// `mcp_call` is the agent's entry point to MCP tools. It takes a server name, a tool name,
// and an args object, and routes the call to `mcp::registry::call`. The agent discovers the
// available (server, tool) pairs via `mcp::registry::list_all_tools()` (surfaced by a future
// system-prompt injection; for now the agent must know the server+tool names from config).
//
// MCP tool calls do NOT emit `step_*` events — they dispatch through the normal tool path
// (this `Tool::execute`), and `agent::subagent` remains the sole emitter of `step_tool` /
// `step_thinking`. This keeps the workflow graph the single source of truth.
struct McpCallTool;
#[async_trait]
impl Tool for McpCallTool {
    fn name(&self) -> &'static str {
        "mcp_call"
    }
    fn description(&self) -> &'static str {
        "Call a tool on a connected MCP (Model Context Protocol) server. Args: \
         {server: \"<server-name>\", tool: \"<tool-name>\", args?: {<tool args>}}. The available \
         (server, tool) pairs come from `.dotz/mcp.json`. Returns the tool's content blocks \
         flattened to text."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "MCP server name (from mcp.json)" },
                "tool": { "type": "string", "description": "Tool name exposed by the server" },
                "args": { "type": "object", "description": "Arguments object for the tool" }
            },
            "required": ["server", "tool"]
        })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let server = args
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or("server is required")?;
        let tool = args
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or("tool is required")?;
        let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
        let result = crate::mcp::registry::call(server, tool, &call_args)
            .await
            .map_err(|e| e.to_string())?;
        // Flatten the `content` array (MCP tools return `{content: [{type, text}, ...], isError}`)
        // into a single string so the agent sees a normal text result. Non-text content blocks
        // are surfaced as `[<type>]` placeholders so the agent knows something was returned.
        let mut out = String::new();
        if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                    out.push('\n');
                } else if let Some(t) = block.get("type").and_then(|v| v.as_str()) {
                    out.push_str(&format!("[{t}]\n"));
                }
            }
        } else {
            // No `content` array — surface the raw result so the agent can see what came back.
            out.push_str(&result.to_string());
        }
        // Surface the server's `isError` flag as an error result when set.
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_error {
            Err(format!("mcp tool {server}/{tool} returned an error: {out}"))
        } else {
            Ok(out)
        }
    }
}

/// Register all the extra tools into a registry's add-closure.
pub fn register(add: &mut dyn FnMut(Box<dyn Tool>)) {
    add(Box::new(AgentsMdTool));
    add(Box::new(CreateAgentTool));
    add(Box::new(CreateSkillTool));
    add(Box::new(RsiBaselineTool));
    add(Box::new(RsiCompareTool));
    add(Box::new(HumanGateTool));
    add(Box::new(OpenSpecStatusTool));
    add(Box::new(OpenSpecExploreTool));
    add(Box::new(OpenSpecProposeTool));
    add(Box::new(OpenSpecApplyTool));
    add(Box::new(OpenSpecVerifyTool));
    add(Box::new(OpenSpecSyncTool));
    add(Box::new(OpenSpecArchiveTool));
    add(Box::new(LivingDocsReadTool));
    add(Box::new(LivingDocsUpdateTool));
    add(Box::new(LivingDocsSuggestTool));
    add(Box::new(VcsStatusTool));
    add(Box::new(VcsBranchTool));
    add(Box::new(VcsAtomicCommitTool));
    add(Box::new(VcsPrTool));
    add(Box::new(VcsRollbackTool));
    add(Box::new(DesignListTool));
    add(Box::new(DesignUseTool));
    add(Box::new(SandboxRunTool));
    add(Box::new(ConnectorListTool));
    add(Box::new(ConnectorActionTool));
    add(Box::new(McpCallTool));
}

// ---- C4: PluginTool — a tool registered from a plugin manifest, dispatched via one of the 5
// handler types (command / http / mcp_tool / prompt / agent). ----
//
// A `PluginTool` holds the plugin's `ToolDef` (parsed from `plugin.toml`) + the registration name
// (`<plugin>:<tool>`). Its `execute()` method dispatches to the right handler based on
// `ToolDef.handler`. Plugin tools dispatch through the normal tool path (`ToolRegistry::run`) and
// do NOT emit `step_*` events — `subagent.rs` remains the sole emitter of those.

/// A tool registered from a plugin manifest. Dispatches to one of 5 handlers on `execute()`.
pub struct PluginTool {
    reg_name: String,
    def: crate::plugins::ToolDef,
}

impl PluginTool {
    /// Build a `PluginTool` from its registration name + the parsed `ToolDef`.
    pub fn new(reg_name: String, def: crate::plugins::ToolDef) -> Self {
        Self { reg_name, def }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &'static str {
        // The trait's `name()` returns `&'static str` for the built-ins; a plugin tool's real
        // name is dynamic, so `registration_name()` is the seam the registry uses. This returns
        // a placeholder literal — the registry never keys on `name()` for a `PluginTool`.
        "plugin"
    }
    fn description(&self) -> &'static str {
        // Same caveat as `name()` — the trait's `description()` returns `&'static str` for the
        // built-ins. The `spec()` builder uses `registration_name()` for the agent-visible name;
        // the description here is the placeholder. The agent sees the real description via
        // `description_value()` below (wired into `spec()` via the override).
        "plugin tool"
    }
    fn parameters(&self) -> Value {
        self.def.input_schema.clone()
    }
    /// The registration name (`<plugin>:<tool>`) — overrides the default `self.name().to_string()`
    /// so the registry keys this tool under its prefixed name, NOT the `"plugin"` placeholder.
    fn registration_name(&self) -> String {
        self.reg_name.clone()
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        dispatch_plugin_tool(&self.def, args, ctx).await
    }
}

/// Dispatch a plugin tool's `execute()` to the right handler based on `ToolDef.handler`. The
/// handler logic mirrors the C2 hook handlers (command/http) + the C3 MCP client (mcp_tool) +
/// the C4 subagent path (agent) + the C2 prompt renderer (prompt). Each returns the tool's text
/// result or an error string.
async fn dispatch_plugin_tool(
    def: &crate::plugins::ToolDef,
    args: &Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    use crate::plugins::ToolHandler;
    match def.handler {
        ToolHandler::Command => dispatch_command(def, args).await,
        ToolHandler::Http => dispatch_http(def, args).await,
        ToolHandler::McpTool => dispatch_mcp_tool(def, args).await,
        ToolHandler::Prompt => dispatch_prompt(def, args),
        ToolHandler::Agent => dispatch_agent(def, args, ctx).await,
    }
}

/// `command` handler: spawn the command, pipe the tool args JSON to stdin, capture stdout, enforce
/// `timeout_ms`. stdout (trimmed) is the tool result. Reuses the `util::no_window_tokio` helper
/// so the packaged app does not flash a conhost window. Mirrors `hooks::run_command_handler` but
/// for a tool (returns the result, not a deny/allow outcome).
async fn dispatch_command(def: &crate::plugins::ToolDef, args: &Value) -> Result<String, String> {
    let cmd_str = def.command.as_deref().unwrap_or("").trim().to_string();
    if cmd_str.is_empty() {
        return Err("plugin tool: empty command".into());
    }
    let payload_json = serde_json::to_vec(args).unwrap_or_else(|_| b"{}".to_vec());
    // Shell out so the plugin can use pipelines/redirects (matches the hook command handler).
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", &cmd_str]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", &cmd_str]);
        c
    };
    crate::util::no_window_tokio(&mut cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("plugin tool spawn failed: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stdin, &payload_json).await;
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let timeout = std::time::Duration::from_millis(def.timeout_ms);
    let wait_fut = child.wait();
    match tokio::time::timeout(timeout, wait_fut).await {
        Ok(Ok(status)) => {
            let stdout_val = match stdout.as_mut() {
                Some(s) => {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(s, &mut buf).await;
                    String::from_utf8_lossy(&buf).trim().to_string()
                }
                None => String::new(),
            };
            let stderr_val = match stderr.as_mut() {
                Some(s) => {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(s, &mut buf).await;
                    String::from_utf8_lossy(&buf).trim().to_string()
                }
                None => String::new(),
            };
            let code = status.code().unwrap_or(-1);
            if code == 0 {
                Ok(stdout_val)
            } else {
                let detail = if !stdout_val.is_empty() {
                    stdout_val
                } else if !stderr_val.is_empty() {
                    stderr_val
                } else {
                    format!("plugin command exited with code {code}")
                };
                Err(format!(
                    "plugin tool '{name}' failed: {detail}",
                    name = def.name
                ))
            }
        }
        Ok(Err(e)) => Err(format!("plugin tool wait failed: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            Err(format!(
                "plugin tool '{}' timed out after {} ms",
                def.name, def.timeout_ms
            ))
        }
    }
}

/// `http` handler: POST the tool args as JSON to `url`, enforce `timeout_ms`, return the response
/// body as the tool result. SSRF guard: the URL was validated at manifest load time (https or
/// loopback http only). Mirrors `hooks::run_http_handler` but for a tool.
async fn dispatch_http(def: &crate::plugins::ToolDef, args: &Value) -> Result<String, String> {
    let url = def.url.as_deref().unwrap_or("").trim().to_string();
    if url.is_empty() {
        return Err("plugin tool: empty url".into());
    }
    // Defense-in-depth: re-validate the SSRF guard at call time (the manifest could have been
    // edited after load; the trust boundary is the plugin dir, but a stale URL is still a
    // hazard if the loader was hot-reloaded).
    crate::mcp::validate_http_url(&url)
        .map_err(|e| format!("plugin tool '{name}': {e}", name = def.name))?;
    eprintln!("plugins: http POST {url} (tool {name})", name = def.name);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(def.timeout_ms))
        .build()
        .map_err(|e| format!("plugin tool http client build failed: {e}"))?;
    let resp = client
        .post(&url)
        .json(args)
        .send()
        .await
        .map_err(|e| format!("plugin tool http send failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(body.trim().to_string())
    } else {
        Err(format!(
            "plugin tool '{name}' http returned status {status}: {body}",
            name = def.name,
            body = body.trim()
        ))
    }
}

/// `mcp_tool` handler: delegate to `mcp::registry::call(server, tool, args)`. The plugin declares
/// which MCP server + tool name; the MCP result is flattened to text (matching the `mcp_call`
/// tool). This wires the C2 `mcp_tool` stub + the C3 MCP client together for plugin tools.
async fn dispatch_mcp_tool(def: &crate::plugins::ToolDef, args: &Value) -> Result<String, String> {
    let server = def.server.as_deref().unwrap_or("").trim().to_string();
    let tool = def.tool.as_deref().unwrap_or("").trim().to_string();
    if server.is_empty() || tool.is_empty() {
        return Err(format!(
            "plugin tool '{name}': mcp_tool handler requires server + tool",
            name = def.name
        ));
    }
    let result = crate::mcp::registry::call(&server, &tool, args)
        .await
        .map_err(|e| format!("plugin tool '{name}' mcp call failed: {e}", name = def.name))?;
    // Flatten the `content` array (matching the `mcp_call` tool's flattening).
    let mut out = String::new();
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for block in content {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                out.push_str(text);
                out.push('\n');
            } else if let Some(t) = block.get("type").and_then(|v| v.as_str()) {
                out.push_str(&format!("[{t}]\n"));
            }
        }
    } else {
        out.push_str(&result.to_string());
    }
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_error {
        Err(format!(
            "plugin tool '{name}' mcp tool {server}/{tool} returned an error: {out}",
            name = def.name
        ))
    } else {
        Ok(out)
    }
}

/// `prompt` handler: render the template (or the full args JSON when no template) into a string
/// that becomes the tool's result. The agent sees this as the tool's "result" — a way for a
/// plugin to inject a templated prompt into the agent context via a tool call. Mirrors
/// `hooks::render_prompt` + `substitute_template`.
fn dispatch_prompt(def: &crate::plugins::ToolDef, args: &Value) -> Result<String, String> {
    let tmpl = match def.template.as_deref() {
        Some(t) if !t.trim().is_empty() => t,
        _ => return Ok(serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".into())),
    };
    Ok(crate::hooks::substitute_template(tmpl, args))
}

/// `agent` handler: spawn a subagent via `subagent::run_single_agent_public` with the tool args
/// rendered as the task. The subagent's output text is the tool's result. This wires the C2
/// `agent` stub for plugin tools. The recursion guard in `hooks::run_agent_handler` is NOT
/// applied here (a plugin tool call is a deliberate agent action, not a lifecycle hook
/// recursion); the subagent's own `SubagentStop` hook fires normally after the run.
async fn dispatch_agent(
    def: &crate::plugins::ToolDef,
    args: &Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    let agent_name = def.agent.as_deref().unwrap_or("").trim().to_string();
    if agent_name.is_empty() {
        return Err(format!(
            "plugin tool '{name}': agent handler requires an agent name",
            name = def.name
        ));
    }
    let task = serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".into());
    let cwd = ctx.cwd.to_string_lossy().to_string();
    let result =
        crate::agent::subagent::run_single_agent_public(&agent_name, &task, None, &cwd).await;
    if result.is_failed() {
        return Err(format!(
            "plugin tool '{}' agent '{}' failed: {}",
            def.name,
            agent_name,
            result.error_message.as_deref().unwrap_or("(no detail)")
        ));
    }
    // Return the subagent's final assistant text message as the tool result (handles the
    // `Vec<Value>` message shape + assistant-role + text-content-block extraction).
    Ok(result.final_output())
}

#[cfg(test)]
mod plugin_tool_tests {
    use super::*;
    use crate::agent::tools::ToolCtx;
    use crate::plugins::{ToolDef, ToolHandler};
    use crate::util::dotz_config_dir_test_lock;
    use serde_json::json;

    /// `PluginTool::registration_name()` must return the prefixed `<plugin>:<tool>` name (NOT the
    /// `"plugin"` placeholder), so the registry keys it correctly.
    #[test]
    fn plugin_tool_registration_name_is_prefixed() {
        let def = ToolDef {
            name: "do_thing".into(),
            description: "does a thing".into(),
            handler: ToolHandler::Command,
            command: Some("echo".into()),
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let t = PluginTool::new("my-plugin:do_thing".into(), def);
        assert_eq!(t.registration_name(), "my-plugin:do_thing");
        assert_eq!(t.name(), "plugin", "placeholder name is never the key");
    }

    /// `PluginTool::spec()` must use the prefixed registration name (so the agent sees
    /// `<plugin>:<tool>` in its tool list, matching the `/api/sessions/:id/tools` surface).
    #[test]
    fn plugin_tool_spec_uses_prefixed_name() {
        let def = ToolDef {
            name: "do_thing".into(),
            description: "x".into(),
            handler: ToolHandler::Command,
            command: Some("echo".into()),
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let t = PluginTool::new("alpha:do_thing".into(), def);
        let spec = t.spec();
        assert_eq!(
            spec.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str()),
            Some("alpha:do_thing")
        );
    }

    /// `command` handler: spawn a command, pipe args JSON via stdin, return stdout.
    #[tokio::test]
    async fn plugin_tool_command_handler_executes_and_returns_stdout() {
        let def = ToolDef {
            name: "echo_args".into(),
            description: "echoes back".into(),
            handler: ToolHandler::Command,
            // A cross-platform script that reads JSON from stdin and echoes a fixed result.
            command: Some("echo command-handler-result".to_string()),
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 10_000,
        };
        let out = dispatch_command(&def, &json!({"query": "x"})).await;
        let result = out.expect("command handler should succeed");
        assert!(
            result.contains("command-handler-result"),
            "command handler must return stdout, got: {result}"
        );
    }

    /// `http` handler: POST the args to a URL, return the response body. Spins up a tiny
    /// in-process HTTP server so the test is hermetic.
    #[tokio::test]
    async fn plugin_tool_http_handler_posts_and_returns_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nok-body";
            let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
        });
        let def = ToolDef {
            name: "http_tool".into(),
            description: "http tool".into(),
            handler: ToolHandler::Http,
            command: None,
            args: None,
            url: Some(format!("http://127.0.0.1:{port}/tool")),
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 5_000,
        };
        let result = dispatch_http(&def, &json!({"q": "x"}))
            .await
            .expect("http handler should succeed");
        assert!(
            result.contains("ok-body"),
            "http handler must return the response body, got: {result}"
        );
    }

    /// `http` handler: a non-https non-loopback URL is rejected at call time (SSRF guard, defense
    /// in depth — the manifest load also validates).
    #[tokio::test]
    async fn plugin_tool_denies_non_https_http_url() {
        let def = ToolDef {
            name: "ssrf".into(),
            description: "ssrf test".into(),
            handler: ToolHandler::Http,
            command: None,
            args: None,
            url: Some("http://example.com/tool".into()),
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 5_000,
        };
        let err = dispatch_http(&def, &json!({})).await.unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("https") || err.contains("loopback"),
            "non-https non-loopback http url must reject with SSRF hint: {err}"
        );
    }

    /// `mcp_tool` handler: delegate to `mcp::registry::call`. We inject a mock MCP client and
    /// verify the plugin tool routes the (server, tool, args) to it.
    /// Shared capture buffer type for the mock MCP transport (factored out to satisfy
    /// clippy::type_complexity — a `Mutex<Vec<...>>` behind an `Arc` is the canonical "captured
    /// requests" pattern, but the type is too long to inline twice).
    type PluginMcpCapture =
        std::sync::Arc<std::sync::Mutex<Vec<(String, Option<serde_json::Value>)>>>;

    #[tokio::test]
    async fn plugin_tool_mcp_tool_handler_delegates_to_mcp_registry() {
        let captured: PluginMcpCapture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let call_result = serde_json::json!({
            "content": [{ "type": "text", "text": "mcp-tool-result" }],
            "isError": false
        });
        struct PluginMcpMock {
            captured: PluginMcpCapture,
            call_result: serde_json::Value,
        }
        #[async_trait::async_trait]
        impl crate::mcp::client::Transport for PluginMcpMock {
            async fn request(
                &mut self,
                method: &str,
                params: Option<serde_json::Value>,
            ) -> Result<serde_json::Value, crate::mcp::client::McpError> {
                self.captured
                    .lock()
                    .unwrap()
                    .push((method.to_string(), params.clone()));
                if method == "tools/call" {
                    return Ok(self.call_result.clone());
                }
                if method == "initialize" {
                    return Ok(serde_json::json!({
                        "protocolVersion": crate::mcp::PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": { "name": "mock", "version": "0.1" }
                    }));
                }
                Ok(serde_json::json!({}))
            }
            async fn notify(
                &mut self,
                _method: &str,
                _params: Option<serde_json::Value>,
            ) -> Result<(), crate::mcp::client::McpError> {
                Ok(())
            }
            async fn close(&mut self) -> Result<(), crate::mcp::client::McpError> {
                Ok(())
            }
        }
        let transport = PluginMcpMock {
            captured: captured.clone(),
            call_result: call_result.clone(),
        };
        let mut client =
            crate::mcp::client::Client::from_transport("plugin-mock-server", Box::new(transport));
        client.initialize().await.unwrap();
        let handle = crate::mcp::registry::ClientHandle::new(client);
        crate::mcp::registry::test_insert("plugin-mock-server", handle);

        let def = ToolDef {
            name: "delegate".into(),
            description: "delegates to mcp".into(),
            handler: ToolHandler::McpTool,
            command: None,
            args: None,
            url: None,
            headers: None,
            server: Some("plugin-mock-server".into()),
            tool: Some("some_tool".into()),
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let result = dispatch_mcp_tool(&def, &json!({"path": "/x"}))
            .await
            .expect("mcp_tool handler should succeed");
        assert!(
            result.contains("mcp-tool-result"),
            "mcp_tool handler must return the flattened MCP result, got: {result}"
        );
        let cap = captured.lock().unwrap().clone();
        let call = cap
            .iter()
            .find(|(m, _)| m == "tools/call")
            .expect("tools/call was sent");
        let params = call.1.as_ref().expect("tools/call must have params");
        assert_eq!(
            params.get("name").and_then(|v| v.as_str()),
            Some("some_tool")
        );
        crate::mcp::registry::test_remove("plugin-mock-server").await;
    }

    /// `prompt` handler: render the template against the args.
    #[tokio::test]
    async fn plugin_tool_prompt_handler_returns_rendered_template() {
        let def = ToolDef {
            name: "prompt_tool".into(),
            description: "renders a prompt".into(),
            handler: ToolHandler::Prompt,
            command: None,
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: Some("Result for {{query}}".into()),
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let result = dispatch_prompt(&def, &json!({"query": "alpha"}));
        let result = result.expect("prompt handler should succeed");
        assert!(
            result.contains("Result for alpha"),
            "prompt handler must render the template, got: {result}"
        );
    }

    /// `prompt` handler with no template renders the full args JSON.
    #[tokio::test]
    async fn plugin_tool_prompt_handler_default_renders_full_args() {
        let def = ToolDef {
            name: "prompt_tool".into(),
            description: "renders a prompt".into(),
            handler: ToolHandler::Prompt,
            command: None,
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: None,
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let result = dispatch_prompt(&def, &json!({"q": "x"}));
        let result = result.expect("default prompt should succeed");
        assert!(
            result.contains("\"q\""),
            "default prompt must render the full args JSON, got: {result}"
        );
    }

    /// `agent` handler: spawn a subagent. We verify the wiring via the recursion-guard path:
    /// a missing agent fails with a clear error (proving the dispatch path is wired, not a
    /// stub). A real subagent spawn would require a live provider key, so the error path is the
    /// hermetic proof. We use a cwd with no `.pi/agents/` so discovery finds nothing → the
    /// subagent run fails fast with a clear error.
    ///
    /// The `dotz_config_dir_test_lock` guard is a std Mutex held across the `.await` points.
    /// This is intentional (the awaited tasks never acquire `dotz_config_dir_test_lock`, so the
    /// deadlock the lint guards against cannot occur) — matches the same idiom in
    /// `memory::tests::recall_async_keeps_reactor_free_while_embedder_mutex_is_held` and
    /// `mcp::registry::tests::with_tmp_dir_async`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn plugin_tool_agent_handler_spawns_subagent() {
        let _guard = dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp =
            std::env::temp_dir().join(format!("dotz-plugin-agent-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let def = ToolDef {
            name: "delegate".into(),
            description: "delegates to a subagent".into(),
            handler: ToolHandler::Agent,
            command: None,
            args: None,
            url: None,
            headers: None,
            server: None,
            tool: None,
            agent: Some("definitely-not-a-real-agent".into()),
            template: None,
            input_schema: json!({"type": "object"}),
            timeout_ms: 30_000,
        };
        let ctx = ToolCtx {
            cwd: tmp.clone(),
            tx: None,
            run_id: None,
        };
        let err = dispatch_agent(&def, &json!({"task": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("agent") || err.contains("failed"),
            "agent handler dispatch must reach the subagent path + fail with a clear error, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Plugin tools do NOT emit `step_*` events — they dispatch through the normal tool path. This
    /// is a static grep guard: the plugin tool dispatch code in `extra_tools.rs` must NOT emit
    /// `step_tool`/`step_thinking`.
    #[test]
    fn plugin_tool_dispatch_does_not_emit_step_events() {
        let src = include_str!("extra_tools.rs");
        // The dispatch functions (dispatch_command / dispatch_http / dispatch_mcp_tool /
        // dispatch_prompt / dispatch_agent) must NOT emit step_tool/step_thinking events.
        // We check the whole module (the dispatch fns are in this file) — the only emitter of
        // step_* events is `agent::subagent`.
        assert!(
            !src.contains("\"step_tool\"") && !src.contains("\"step_thinking\""),
            "extra_tools.rs plugin dispatch must NOT emit step_tool/step_thinking events"
        );
    }

    /// Plugin hooks registered via `plugins::register_hooks` must reach the global hook registry.
    /// We install a plugin-style hook directly via `hooks::append_plugin_hooks` and verify it fires.
    #[tokio::test]
    async fn plugin_hooks_registered_via_hook_registry() {
        use crate::hooks::{self, HandlerType, HookConfig, HookEvent};
        // Use a static lock to serialize the global registry mutation.
        static PLUGIN_HOOK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _g = PLUGIN_HOOK_TEST_LOCK.lock().await;
        // Reset the global registry, then append a plugin hook. We use a `PreToolUse` command
        // hook that exits non-zero (deny) so the firing is observable via the `deny` + `reason`
        // fields (non-PreToolUse events drop the command's `reason` per the existing C2 `fire()`
        // contract — only `injected_prompt` from prompt handlers accumulates for non-PreToolUse
        // events).
        {
            let mut g = crate::hooks::cell_for_tests();
            *g = None;
        }
        let cfg = HookConfig {
            event: HookEvent::PreToolUse,
            handler: HandlerType::Command,
            command: Some("echo plugin-hook-fired && exit 1".into()),
            url: None,
            headers: None,
            template: None,
            agent: None,
            server: None,
            tool: None,
            timeout_ms: 5_000,
        };
        hooks::append_plugin_hooks(vec![cfg]);
        let o = hooks::fire(HookEvent::PreToolUse, &serde_json::json!({})).await;
        assert!(o.deny, "plugin hook must fire + deny on PreToolUse");
        let reason = o.reason.expect("plugin hook must return a reason");
        assert!(
            reason.contains("plugin-hook-fired"),
            "plugin hook reason must come from the command stdout, got: {reason}"
        );
        // Cleanup: reset the global registry so other tests see no plugin hooks.
        {
            let mut g = crate::hooks::cell_for_tests();
            *g = None;
        }
    }

    /// Plugin MCP servers registered via `plugins::register_mcp_servers` must be validated (a
    /// malformed entry is rejected loudly). The actual connection is NOT auto-triggered (the
    /// operator adds the validated server to their `~/.dotz/mcp.json` manually — see the
    /// `# ponytail:` comment in `register_mcp_servers`). We verify a valid `[[mcp_servers]]` row
    /// parses + validates, and a malformed row is rejected.
    #[tokio::test]
    async fn plugin_mcp_servers_registered_via_mcp_registry() {
        // A valid stdio server row parses + validates.
        let valid = crate::plugins::McpServerRow {
            name: "valid-server".into(),
            transport: "stdio".into(),
            command: Some("npx".into()),
            args: Some(vec!["-y".into(), "my-mcp-server".into()]),
            env: None,
            url: None,
            headers: None,
        };
        let cfg = crate::plugins::mcp_row_to_config(&valid).expect("valid server must parse");
        assert_eq!(cfg.transport, crate::mcp::TransportType::Stdio);
        assert_eq!(cfg.command.as_deref(), Some("npx"));

        // A malformed transport is rejected loudly.
        let bad = crate::plugins::McpServerRow {
            name: "bad-server".into(),
            transport: "carrier-pigeon".into(),
            command: None,
            args: None,
            env: None,
            url: None,
            headers: None,
        };
        let err = crate::plugins::mcp_row_to_config(&bad).unwrap_err();
        assert!(
            err.contains("carrier-pigeon"),
            "unknown transport must reject: {err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Serialize tests that mutate the process-global `DOTZ_GATE_TIMEOUT_MS` env var.
    /// Uses an async-aware Mutex because the tests hold the lock across `.await` points; a
    /// `std::sync::MutexGuard` held across an await can block the tokio executor and risks
    /// deadlocks with other async tasks.
    static GATE_TIMEOUT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dotz-gate-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn default_gate_command_uses_cargo_for_rust_project() {
        let dir = tmp_dir();
        fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(default_gate_command(&dir), "cargo test");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_gate_command_uses_npm_for_node_project() {
        let dir = tmp_dir();
        fs::write(dir.join("package.json"), "{}\n").unwrap();
        assert_eq!(default_gate_command(&dir), "npm test");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_gate_command_prefers_cargo_when_both_manifests_exist() {
        let dir = tmp_dir();
        fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.join("package.json"), "{}\n").unwrap();
        assert_eq!(default_gate_command(&dir), "cargo test");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_gate_command_fallback_is_cargo() {
        let dir = tmp_dir();
        assert_eq!(default_gate_command(&dir), "cargo test");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_gate_command_uses_package_name_when_present() {
        let dir = tmp_dir();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"dotz-core\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(
            default_gate_command(&dir),
            "cargo test -p dotz-core",
            "a named crate should be scoped to its package"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_gate_command_uses_dotz_core_for_workspace_root() {
        let dir = tmp_dir();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"dotz-core\", \"src-tauri\"]\n",
        )
        .unwrap();
        assert_eq!(
            default_gate_command(&dir),
            "cargo test -p dotz-core",
            "the dotz workspace root should default to the runtime crate gate"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cargo_package_name_rejects_prefixed_keys() {
        // A key like `nameable` must NOT be mistaken for `name`; only an exact key match counts.
        let toml = "[package]\nnameable = true\nname = \"real-crate\"\n";
        assert_eq!(
            cargo_package_name(toml),
            Some("real-crate".into()),
            "prefixed keys must not be parsed as the package name"
        );
    }

    #[test]
    fn cargo_package_name_strips_trailing_comment() {
        // Trailing TOML comments must be removed before unquoting so they don't leak into the
        // derived `cargo test -p <name>` gate command.
        let toml = "[package]\nname = \"dotz-core\" # the core runtime crate\n";
        assert_eq!(
            cargo_package_name(toml),
            Some("dotz-core".into()),
            "trailing comments must be stripped from the package name"
        );
    }

    #[test]
    fn parse_counts_reads_node_test_after_keyword() {
        assert_eq!(parse_counts("# pass 5\n# fail 1"), (5, 1));
        assert_eq!(parse_counts("# pass 5 / # fail 0"), (5, 0));
    }

    #[test]
    fn parse_counts_reads_number_before_keyword() {
        assert_eq!(parse_counts("5 passed, 1 failed"), (5, 1));
        assert_eq!(parse_counts("8 passed / 0 failed"), (8, 0));
    }

    #[tokio::test]
    async fn gate_timeout_clamps_invalid_values() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_GATE_TIMEOUT_MS").ok();

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("DOTZ_GATE_TIMEOUT_MS") };
        assert_eq!(
            gate_timeout().as_secs(),
            600,
            "default gate timeout is 10 minutes"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "500") };
        assert_eq!(
            gate_timeout().as_millis(),
            1_000,
            "below-minimum value clamps to 1s"
        );

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "30000") };
        assert_eq!(gate_timeout().as_millis(), 30_000, "valid value preserved");

        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "100000000") };
        assert_eq!(
            gate_timeout().as_millis(),
            3_600_000,
            "above-maximum value clamps to 1h"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_GATE_TIMEOUT_MS") },
        }
    }

    /// A hung gate command must not block the RSI loop forever. `run_gate` honors
    /// `DOTZ_GATE_TIMEOUT_MS`, kills the child process, and returns a clear timeout error.
    #[tokio::test]
    async fn run_gate_times_out_on_hung_command() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_GATE_TIMEOUT_MS").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "1000") };

        let dir = tmp_dir();
        // ~2s of wall-clock time; the 1s timeout must fire first and kill the child.
        let command = if cfg!(windows) {
            "ping -n 3 127.0.0.1"
        } else {
            "sleep 2"
        };

        let start = std::time::Instant::now();
        let result = run_gate(&dir, Some(command)).await;
        let elapsed = start.elapsed();

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_GATE_TIMEOUT_MS") },
        }
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "timed-out gate must report ok:false: {result}"
        );
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("[timeout]"),
            "timed-out gate must report a timeout error, got: {error}"
        );
        // The point is that the configured 1s timeout fired, not the 10-minute default — so any
        // bound far below the default proves it. Keep margin above 1s: under a saturated parallel
        // test suite the timeout future + kill/reap can be scheduled several seconds late, which
        // made a tight 3s bound flaky without ever indicating a real regression.
        assert!(
            elapsed < Duration::from_secs(15),
            "gate timeout should fire on the configured 1s timeout, not the default, elapsed: {elapsed:?}"
        );
    }

    /// Failure-catalog #1 (the autopsy's top pattern): an empty test suite must NOT count
    /// green. A gate command that exits 0 while emitting zero parseable test counts
    /// (passed:0 / failed:0) is unobservable evidence — `run_gate` must report ok:false
    /// with an explicit `unobservable` marker, never `{ok:true, passed:0}`.
    #[tokio::test]
    async fn run_gate_empty_suite_exit_zero_reads_red() {
        let dir = tmp_dir();
        // Exits 0, prints no test counts — the exact "empty suite counted green" shape.
        let result = run_gate(&dir, Some("echo build finished cleanly")).await;
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "exit-0 with zero parsed test evidence must be RED: {result}"
        );
        assert_eq!(
            result.get("unobservable").and_then(|v| v.as_bool()),
            Some(true),
            "zero-evidence gate must carry the unobservable marker: {result}"
        );
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("unobservable"),
            "error should explain the unobservable verdict, got: {error}"
        );
        assert_eq!(result.get("exitCode").and_then(|v| v.as_i64()), Some(0));
    }

    /// The inverse guard: real test evidence with exit 0 still reads green, so the
    /// unobservable tripwire does not break legitimate gate runs.
    #[tokio::test]
    async fn run_gate_with_parsed_counts_exit_zero_reads_green() {
        let dir = tmp_dir();
        let result = run_gate(&dir, Some("echo 5 passed, 0 failed")).await;
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "exit-0 with parsed counts must stay green: {result}"
        );
        assert_eq!(result.get("passed").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(result.get("failed").and_then(|v| v.as_i64()), Some(0));
        assert!(
            result.get("unobservable").is_none(),
            "observable gate must not carry the unobservable marker: {result}"
        );
    }

    /// A timed-out gate child must be tree-killed, not just have its shell reaped while the
    /// real workload (the descendant) keeps running as an orphan. Before the process-group fix,
    /// `start_kill` only terminated the shell (`sh`); the `sleep 30` grandchild survived and
    /// kept consuming CPU until it naturally exited — a real orphaned-process leak on every
    /// timed-out RSI gate run.
    ///
    /// We verify the tree-kill on Unix by spawning `sleep 30` as a *background child* of the
    /// shell, writing the `sleep` process's PID (not the shell's `$$`) to a file, and confirming
    /// that PID is gone after `run_gate` returns. On Windows we verify the timeout result shape.
    #[tokio::test]
    async fn run_gate_reaps_child_after_timeout() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_GATE_TIMEOUT_MS").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "1000") };

        let dir = tmp_dir();
        let pidfile = dir.join("pid");

        // `sleep 30 & echo $! > pidfile; wait` — the shell forks `sleep 30` as a background
        // child, writes the *sleep*'s PID ($!) to the pidfile, then waits. The 1s timeout fires
        // during the `wait`; the process-group kill (`kill -9 -<pgrp>`) must reap both the shell
        // AND the `sleep 30` grandchild. Before the fix, only the shell was killed and `sleep 30`
        // survived as an orphan.
        let command = if cfg!(windows) {
            // ~3s of wall-clock time; the 1s timeout fires first.
            "ping -n 4 127.0.0.1".to_string()
        } else {
            format!("sleep 30 & echo $! > {} ; wait", pidfile.to_string_lossy())
        };

        let result = run_gate(&dir, Some(&command)).await;

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_GATE_TIMEOUT_MS") },
        }

        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "timed-out gate must report ok:false: {result}"
        );
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("[timeout]"),
            "timed-out gate must report a timeout error, got: {error}"
        );

        #[cfg(unix)]
        {
            // The pidfile contains the PID of the `sleep 30` descendant, NOT the shell. Give the
            // shell a moment to have forked `sleep` and written the pidfile before we read it.
            let mut pid_text = String::new();
            for _ in 0..20 {
                pid_text = std::fs::read_to_string(&pidfile).unwrap_or_default();
                if !pid_text.trim().is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = fs::remove_dir_all(&dir);
            let pid: i32 = pid_text.trim().parse().unwrap_or(-1);
            assert!(
                pid > 0,
                "test should have captured the descendant pid: '{pid_text}'"
            );
            // `kill -0` is a non-destructive probe: success means the process still exists. The
            // process-group kill must have reaped the `sleep 30` grandchild, so this must fail.
            // Give the kernel a moment to reap the killed process.
            let mut gone = false;
            for _ in 0..20 {
                gone = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .output()
                    .map(|o| !o.status.success())
                    .unwrap_or(true);
                if gone {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(
                gone,
                "timed-out gate descendant (sleep pid {pid}) should have been tree-killed, not still running as an orphan"
            );
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// The gate-timeout test lock must be async-safe so it can be held across `.await` without
    /// blocking the tokio executor. We hold the lock while a concurrent task sleeps; both
    /// finish, confirming the async Mutex does not pin an executor thread.
    #[tokio::test]
    async fn gate_timeout_lock_can_be_held_across_await() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().await;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        })
        .await
        .unwrap();
    }

    #[test]
    fn parse_counts_returns_zero_for_missing_counts() {
        assert_eq!(parse_counts("Tests completed."), (0, 0));
    }

    /// Keywords embedded inside unrelated words (e.g. "passphrase", "failure") must not be
    /// mistaken for standalone pass/fail tokens. Without a word-boundary check a token like
    /// "pass" inside "passphrase" could pick up a nearby number and fake a pass count.
    #[test]
    fn parse_counts_ignores_keyword_inside_longer_word() {
        assert_eq!(parse_counts("5 passphrase, 1 failure"), (0, 0));
    }

    /// Both the bare keyword ("pass" / "fail") and the past-tense form ("passed" / "failed")
    /// should be counted, but only when they stand alone.
    #[test]
    fn parse_counts_counts_standalone_pass_and_passed() {
        assert_eq!(parse_counts("5 pass, 1 fail"), (5, 1));
        assert_eq!(parse_counts("5 passed, 1 failed"), (5, 1));
    }

    /// A panic while holding the RSI baselines mutex must not permanently brick the RSI loop.
    /// With poison recovery, `rsi_baseline` can still store a new baseline and `rsi_compare` can
    /// read it back after a previous lock owner panicked mid-operation.
    #[test]
    fn baselines_guard_recovers_from_poisoned_mutex() {
        // Ensure the mutex is initialized.
        drop(baselines().lock().unwrap());

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = baselines().lock().unwrap();
            panic!("intentional baselines mutex poison");
        }));
        assert!(poisoned.is_err(), "baselines mutex should be poisoned");

        let key = "proj:/tmp/recover-test".to_string();
        {
            let mut guard = baselines_guard();
            guard.insert(key.clone(), json!({ "ok": true, "passed": 7, "failed": 0 }));
        }

        let value = baselines_guard().get(&key).cloned();
        assert_eq!(
            value.and_then(|v| v.get("passed").and_then(|x| x.as_i64())),
            Some(7)
        );
    }

    /// A panic while holding the human-gate mutex must not permanently brick the gate UI or the
    /// /ws approval handler. With poison recovery, `resolve_gate` keeps working after a previous
    /// lock owner panicked.
    #[test]
    fn gates_guard_recovers_from_poisoned_mutex() {
        // Ensure the mutex is initialized.
        drop(gates().lock().unwrap());

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gates().lock().unwrap();
            panic!("intentional gates mutex poison");
        }));
        assert!(poisoned.is_err(), "gates mutex should be poisoned");

        // resolve_gate must not panic on a poisoned mutex; with no matching gate it is a no-op.
        resolve_gate("no-such-gate", true, None);

        // A live gate can still be registered and resolved after recovery.
        let (tx, mut rx) = oneshot::channel();
        gates_guard().insert("recover-gate".to_string(), tx);
        resolve_gate("recover-gate", false, Some("needs work".to_string()));
        let result = rx
            .try_recv()
            .expect("gate resolution should deliver after poison recovery");
        assert_eq!(result, (false, Some("needs work".to_string())));
    }

    /// Serialize tests that mutate process-global env vars used by the file-persisting tools
    /// (`DOTZ_CONFIG_DIR`). An async-aware mutex is required because the tests hold the lock
    /// across `.await` points.
    static TOOL_ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `agents_md` must read and write the project AGENTS.md asynchronously without blocking the
    /// tokio runtime. This covers the conversion from `std::fs` to `tokio::fs` in the doctrine tool.
    #[tokio::test]
    async fn agents_md_tool_reads_and_writes_async() {
        let dir = tmp_dir();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            tx: None,
            run_id: None,
        };
        let tool = AgentsMdTool;

        let write_result = tool
            .execute(
                &json!({ "action": "write", "content": "# Doctrine\n\nBe concise." }),
                &ctx,
            )
            .await;
        assert!(
            write_result.is_ok(),
            "agents_md write should succeed: {write_result:?}"
        );
        assert!(
            dir.join("AGENTS.md").exists(),
            "AGENTS.md should exist after write"
        );

        let read_result = tool.execute(&json!({ "action": "read" }), &ctx).await;
        assert_eq!(
            read_result.unwrap(),
            "# Doctrine\n\nBe concise.",
            "agents_md read should return the written doctrine"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `create_skill` must persist a user skill file asynchronously and rebuild the skill index
    /// on the blocking pool. This covers the `tokio::fs` conversion and the `spawn_blocking`
    /// reload in the skill-creation tool.
    #[tokio::test]
    async fn create_skill_tool_persists_async_and_reloads_index() {
        let _guard = TOOL_ENV_TEST_LOCK.lock().await;
        let prev_config = std::env::var("DOTZ_CONFIG_DIR")
            .ok()
            .map(std::path::PathBuf::from);
        let tmp = tmp_dir();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &tmp) };

        let tool = CreateSkillTool;
        let ctx = ToolCtx {
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            tx: None,
            run_id: None,
        };
        let name = format!(
            "testskill{}",
            uuid::Uuid::new_v4().to_string().replace("-", "")
        );
        let result = tool
            .execute(
                &json!({
                    "name": name,
                    "description": "A test skill",
                    "body": "Use this skill to verify async create_skill."
                }),
                &ctx,
            )
            .await;

        let file = tmp
            .join("ai-agents")
            .join("skills")
            .join(&name)
            .join("SKILL.md");
        assert!(result.is_ok(), "create_skill should succeed: {result:?}");
        assert!(
            file.exists(),
            "create_skill should write the skill file at {}",
            file.display()
        );

        let _ = fs::remove_dir_all(&tmp);
        // Restore DOTZ_CONFIG_DIR before rebuilding the index so other tests are not exposed
        // to the deleted temp directory, then clear the test skill from the global index.
        match &prev_config {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        crate::skills::reload_index();
    }

    // ---- C3: mcp_call tool tests ----

    /// Shared capture buffer type for the mock MCP transport. Test-only; factored out to
    /// satisfy clippy::type_complexity (a `Mutex<Vec<...>>` behind an `Arc` is the canonical
    /// "captured requests" pattern, but the type is too long to inline twice).
    type McpCapture = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<serde_json::Value>)>>>;

    /// A mock MCP transport for the `mcp_call` routing tests. Returns a canned `tools/call`
    /// response so the test can assert the request was routed through the registry to the
    /// server. The transport captures the (method, params) pairs it received.
    struct MockMcpTransport {
        captured: McpCapture,
        call_result: serde_json::Value,
    }

    #[async_trait::async_trait]
    impl crate::mcp::client::Transport for MockMcpTransport {
        async fn request(
            &mut self,
            method: &str,
            params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, crate::mcp::client::McpError> {
            self.captured
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            if method == "tools/call" {
                return Ok(self.call_result.clone());
            }
            // initialize, tools/list, etc. — return minimal valid responses.
            if method == "initialize" {
                return Ok(serde_json::json!({
                    "protocolVersion": crate::mcp::PROTOCOL_VERSION,
                    "capabilities": {},
                    "serverInfo": { "name": "mock", "version": "0.1" }
                }));
            }
            Ok(serde_json::json!({}))
        }
        async fn notify(
            &mut self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<(), crate::mcp::client::McpError> {
            Ok(())
        }
        async fn close(&mut self) -> Result<(), crate::mcp::client::McpError> {
            Ok(())
        }
    }

    /// The `mcp_call` tool must be registered AND active by default so the agent can route
    /// calls to connected MCP servers. This is the runnable guard for the agent-dispatch
    /// wiring (acceptance criterion #23).
    #[test]
    fn mcp_call_tool_registered_in_tool_registry() {
        let r = crate::agent::tools::ToolRegistry::new();
        let all = r.all_names();
        let active = r.active_names();
        assert!(
            all.contains(&"mcp_call".to_string()),
            "mcp_call must be registered"
        );
        assert!(
            active.contains(&"mcp_call".to_string()),
            "mcp_call must be active by default"
        );
    }

    /// `mcp_call` routes the agent's (server, tool, args) onto `mcp::registry::call`, which
    /// dispatches to the connected server's `Client::call_tool`. We inject a mock client into
    /// the registry (via `test_insert`) and verify the call reaches it. This is the
    /// end-to-end agent-dispatch test (acceptance criterion #24).
    #[tokio::test]
    async fn mcp_call_tool_routes_to_registry() {
        let captured: McpCapture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let call_result = serde_json::json!({
            "content": [
                { "type": "text", "text": "hello from mock mcp server" }
            ],
            "isError": false
        });
        let transport = MockMcpTransport {
            captured: captured.clone(),
            call_result: call_result.clone(),
        };
        let mut client =
            crate::mcp::client::Client::from_transport("mock-server", Box::new(transport));
        client.initialize().await.unwrap();
        let handle = crate::mcp::registry::ClientHandle::new(client);
        crate::mcp::registry::test_insert("mock-server", handle);

        // Build the tool context (the agent dispatch path). The mcp_call tool doesn't use
        // the ctx (it routes via the global registry), so cwd is irrelevant.
        let ctx = crate::agent::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        let tool = super::McpCallTool;
        let result = tool
            .execute(
                &serde_json::json!({
                    "server": "mock-server",
                    "tool": "read_file",
                    "args": { "path": "/tmp/x" }
                }),
                &ctx,
            )
            .await
            .expect("mcp_call should succeed");

        // The tool flattened the content blocks into text.
        assert!(
            result.contains("hello from mock mcp server"),
            "mcp_call should return the tool's text content, got: {result}"
        );

        // The (server, tool, args) reached the mock transport as a `tools/call` request with
        // the right shape.
        let captured = captured.lock().unwrap().clone();
        let call = captured
            .iter()
            .find(|(m, _)| m == "tools/call")
            .expect("tools/call was sent to the mock server");
        let params = call
            .1
            .as_ref()
            .expect("tools/call request must have params");
        assert_eq!(
            params.get("name").and_then(|v| v.as_str()),
            Some("read_file")
        );
        assert_eq!(
            params
                .get("arguments")
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str()),
            Some("/tmp/x")
        );

        // Cleanup: remove the mock server from the registry so other tests don't see it.
        crate::mcp::registry::test_remove("mock-server").await;
    }

    /// `mcp_call` with a server name that isn't in the registry returns a clear error (not a
    /// panic, not a silent no-op). This is the agent-dispatch error path (acceptance
    /// criterion #25).
    #[tokio::test]
    async fn mcp_call_tool_denies_unknown_server() {
        // Ensure the unknown server is not in the registry.
        crate::mcp::registry::test_remove("definitely-not-here").await;
        let ctx = crate::agent::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        let tool = super::McpCallTool;
        let err = tool
            .execute(
                &serde_json::json!({
                    "server": "definitely-not-here",
                    "tool": "any",
                    "args": {}
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("not connected"),
            "mcp_call on an unknown server should say 'not connected', got: {err}"
        );
    }
}
