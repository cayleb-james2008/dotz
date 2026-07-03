//! The remaining pi tools, ported from the dotz-tools extension: agents_md (doctrine), create_agent,
//! create_skill, rsi_baseline/rsi_compare (the gate metrics), and human_gate (WS approval round-trip).
use super::tools::{Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

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
// (RESOURCE_NAME kept as documentation of the regex valid_name enforces.)
const _: &str = RESOURCE_NAME;

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
    let mut child = match tokio::process::Command::new("cmd")
        .arg("/C")
        .arg(&command)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "error": format!("gate run failed: {e}"),
                "ok": false,
                "passed": 0,
                "failed": 0,
            });
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
            json!({
                "exitCode": status.code(),
                "ok": status.success(),
                "passed": passed,
                "failed": failed,
                "tail": text.chars().rev().take(800).collect::<String>().chars().rev().collect::<String>(),
            })
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
            #[cfg(windows)]
            {
                if let Some(pid) = child.id() {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
            }
            #[cfg(not(windows))]
            {
                if let Some(pid) = child.id() {
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &format!("-{pid}")])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
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
        let verdict = if nf > bf || np < bp {
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
        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("terminal");
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

        std::env::remove_var("DOTZ_GATE_TIMEOUT_MS");
        assert_eq!(
            gate_timeout().as_secs(),
            600,
            "default gate timeout is 10 minutes"
        );

        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "500");
        assert_eq!(
            gate_timeout().as_millis(),
            1_000,
            "below-minimum value clamps to 1s"
        );

        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "30000");
        assert_eq!(gate_timeout().as_millis(), 30_000, "valid value preserved");

        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "100000000");
        assert_eq!(
            gate_timeout().as_millis(),
            3_600_000,
            "above-maximum value clamps to 1h"
        );

        match prev {
            Some(p) => std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_GATE_TIMEOUT_MS"),
        }
    }

    /// A hung gate command must not block the RSI loop forever. `run_gate` honors
    /// `DOTZ_GATE_TIMEOUT_MS`, kills the child process, and returns a clear timeout error.
    #[tokio::test]
    async fn run_gate_times_out_on_hung_command() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_GATE_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "1000");

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
            Some(p) => std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_GATE_TIMEOUT_MS"),
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
        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "1000");

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
            Some(p) => std::env::set_var("DOTZ_GATE_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_GATE_TIMEOUT_MS"),
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
            "agents_md write should succeed: {:?}",
            write_result
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
        std::env::set_var("DOTZ_CONFIG_DIR", &tmp);

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
        assert!(result.is_ok(), "create_skill should succeed: {:?}", result);
        assert!(
            file.exists(),
            "create_skill should write the skill file at {}",
            file.display()
        );

        let _ = fs::remove_dir_all(&tmp);
        // Restore DOTZ_CONFIG_DIR before rebuilding the index so other tests are not exposed
        // to the deleted temp directory, then clear the test skill from the global index.
        match &prev_config {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        crate::skills::reload_index();
    }
}
