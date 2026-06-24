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
            "read" => Ok(std::fs::read_to_string(&file).unwrap_or_default()),
            "write" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or("content is required for write")?;
                std::fs::write(&file, content).map_err(|e| e.to_string())?;
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
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(format!("{name}.md"));
        if file.exists() {
            return Err(format!("agent \"{name}\" already exists"));
        }
        let body = if desc.is_empty() {
            format!("# {name}\n\n{prompt}\n")
        } else {
            format!("# {name}\n\n> {desc}\n\n{prompt}\n")
        };
        std::fs::write(&file, body).map_err(|e| e.to_string())?;
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
        if file.exists() {
            return Err(format!("skill \"{name}\" already exists"));
        }
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Frontmatter with JSON-quoted values (mirrors createUserSkill in skills.ts).
        let content = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            serde_json::to_string(name).unwrap_or_default(),
            serde_json::to_string(&description).unwrap_or_default(),
            body
        );
        std::fs::write(&file, content).map_err(|e| e.to_string())?;
        // Rebuild the skill index so the new skill is immediately visible to list_skills,
        // get_skill, and the `skill` tool — without this the cache would stay stale until restart.
        crate::skills::reload_index();
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

    let (program, args): (&str, Vec<String>) = if cfg!(windows) {
        ("cmd", vec!["/C".into(), command.clone()])
    } else {
        ("sh", vec!["-c".into(), command.clone()])
    };

    let mut child = match tokio::process::Command::new(program)
        .args(&args)
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
            let _ = child.start_kill();
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
fn parse_counts(text: &str) -> (i64, i64) {
    let num_around = |kw: &str| -> i64 {
        for line in text.lines() {
            let l = line.to_lowercase();
            if let Some(idx) = l.find(kw) {
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
                let mut k = idx + kw.len();
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
        baselines().lock().unwrap().insert(key, result.clone());
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
        let base = baselines().lock().unwrap().get(&key).cloned();
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
static GATES: OnceLock<Mutex<HashMap<String, oneshot::Sender<(bool, Option<String>)>>>> =
    OnceLock::new();
fn gates() -> &'static Mutex<HashMap<String, oneshot::Sender<(bool, Option<String>)>>> {
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called by the /ws handler on a {kind:"gate.approve"|"gate.reject"} client message.
pub fn resolve_gate(gate_id: &str, approved: bool, feedback: Option<String>) {
    if let Some(tx) = gates().lock().unwrap().remove(gate_id) {
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
        gates().lock().unwrap().insert(id.clone(), tx);
        // Emit the gate request over the session WS (app.js renders the approval card).
        let _ = ws_tx.send(json!({ "kind": "gate", "gateId": id, "plan": plan }));
        let result = tokio::time::timeout(Duration::from_secs(300), rx).await;
        gates().lock().unwrap().remove(&id);
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

/// Register all the extra tools into a registry's add-closure.
pub fn register(add: &mut dyn FnMut(Box<dyn Tool>)) {
    add(Box::new(AgentsMdTool));
    add(Box::new(CreateAgentTool));
    add(Box::new(CreateSkillTool));
    add(Box::new(RsiBaselineTool));
    add(Box::new(RsiCompareTool));
    add(Box::new(HumanGateTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Serialize tests that mutate the process-global `DOTZ_GATE_TIMEOUT_MS` env var.
    static GATE_TIMEOUT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn gate_timeout_clamps_invalid_values() {
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().unwrap();
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
        let _guard = GATE_TIMEOUT_TEST_LOCK.lock().unwrap();
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
        assert!(
            elapsed < Duration::from_secs(3),
            "gate timeout should return promptly, elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn parse_counts_returns_zero_for_missing_counts() {
        assert_eq!(parse_counts("Tests completed."), (0, 0));
    }
}
