//! Inter-agent context bus — a shared key-value scratchpad for workflow runs.
//!
//! Problem: in a multi-agent workflow (scout → planner → worker → reviewer), each subagent
//! currently starts from scratch. The lead agent must either (a) cram the entire repo +
//! prior findings into every task prompt (wastes context, loses structure) or (b) route
//! everything through itself as a human bottleneck (defeats fan-out).
//!
//! Solution: a per-run `HashMap<String, Value>` that any subagent can write to and later
//! subagents can read from. The workflow executor creates the bus when a run starts and
//! destroys it when the run terminates. Subagents get a `context_read` / `context_write`
//! tool pair (restricted to their own run's bus, keyed by run id, so cross-run isolation is
//! structural — no run can see another run's bus).
//!
//! The bus also auto-populates: when a step finishes, the executor writes the step's output
//! under `step:<id>:output` so downstream steps can reference structured prior results
//! without parsing raw text.
//!
//! Wire-up:
//!   - `run_single_agent_inner` receives `Option<&ContextBus>` and, when present, (a)
//!     prepends a compact context summary to the task prompt and (b) makes the
//!     `context_read`/`context_write` tools active for that subagent.
//!   - The workflow executor creates the bus before the loop and passes it to every
//!     `run_single_agent_public` call.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One run's scratchpad: key → JSON value. Keys are arbitrary agent-chosen strings
/// (e.g. "scout:files", "planner:plan", "reviewer:gap_list").
type BusMap = HashMap<String, Value>;

/// All active run buses, keyed by run id.
fn global_buses() -> &'static Mutex<HashMap<String, BusMap>> {
    static BUS: OnceLock<Mutex<HashMap<String, BusMap>>> = OnceLock::new();
    BUS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A handle to one run's context bus. Cheap to clone (it's an Arc<Mutex<..>> internally).
#[derive(Clone)]
pub struct ContextBus {
    pub(crate) run_id: String,
}

impl ContextBus {
    /// Create a new empty bus for a run. Any prior bus for the same run id is replaced, so
    /// re-creating a bus (e.g. on executor restart) starts clean.
    pub fn create(run_id: &str) -> Self {
        let mut guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(run_id.to_string(), HashMap::new());
        Self {
            run_id: run_id.to_string(),
        }
    }

    /// Pre-populate a (fresh, empty) bus from a resumed run's completed-step outputs.
    ///
    /// On server restart the in-memory bus is gone (it lived only in the prior process).
    /// Re-creating it empty means a resumed worker step can't see what the scout found
    /// before the shutdown — context-bearing chains (scout → planner → worker → reviewer)
    /// silently regress to "every agent starts from scratch". This method restores the
    /// bus contents the executor expects by replaying every terminal step's `output`
    /// onto the bus under the same keys the executor's own auto-population path writes
    /// (`step:<id>:output` + `step:<id>:summary`), so downstream subagents read the
    /// exact data shape they would have seen without a restart.
    pub fn preload_from_run(&self, run: &crate::workflows::WorkflowRun) {
        for step in &run.steps {
            if step.status != "done" && step.status != "error" {
                continue;
            }
            let Some(ref out) = step.output else {
                continue;
            };
            self.write(
                &format!("step:{}:output", step.id),
                serde_json::Value::String(out.clone()),
            );
            let summary = out.lines().next().unwrap_or("").to_string();
            self.write(
                &format!("step:{}:summary", step.id),
                serde_json::Value::String(summary),
            );
        }
    }

    /// Destroy a run's bus (called when the run terminates). A missing bus is a no-op.
    pub fn destroy(run_id: &str) {
        let mut guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(run_id);
    }

    /// Read a key. Returns None if the key does not exist OR if the run's bus has been
    /// destroyed.
    pub fn read(&self, key: &str) -> Option<Value> {
        let guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&self.run_id)?.get(key).cloned()
    }

    /// List all keys currently in the bus (for context injection + UI).
    pub fn list_keys(&self) -> Vec<String> {
        let guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        guard
            .get(&self.run_id)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Write a key. Creates the run's bus if it doesn't exist (defensive — the executor
    /// always creates it first, but a tool call path shouldn't crash if it did).
    pub fn write(&self, key: &str, value: Value) {
        let mut guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard.entry(self.run_id.clone()).or_default();
        entry.insert(key.to_string(), value);
    }

    /// Read all entries (used for full-context injection into a task prompt).
    pub fn read_all(&self) -> HashMap<String, Value> {
        let guard = global_buses().lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&self.run_id).cloned().unwrap_or_default()
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

/// Inject the bus contents into a task prompt as a compact JSON block. The block is
/// placed BEFORE the task so the agent sees the shared context before reading the
/// task-specific instructions. Empty bus → no injection (saves tokens).
pub fn inject_context_into_task(task: &str, bus: &ContextBus) -> String {
    let entries = bus.read_all();
    if entries.is_empty() {
        return task.to_string();
    }
    // Serialize the bus keys into a single JSON object. Truncate each value's string
    // representation to 8 KB so a runaway write can't blow up the prompt.
    let mut compact = HashMap::new();
    for (k, v) in &entries {
        let val_str = serde_json::to_string(v).unwrap_or_default();
        let trimmed = if val_str.len() > 8192 {
            // Back up to the nearest UTF-8 char boundary at or before byte 8192 so the slice
            // doesn't panic when a multi-byte character straddles the cut point — which is
            // the common case for real agent output (code comments, non-ASCII docs, emoji).
            let mut end = 8192;
            while !val_str.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…[truncated]", &val_str[..end])
        } else {
            val_str
        };
        compact.insert(k.clone(), Value::String(trimmed));
    }
    let ctx_json = serde_json::to_string(&compact).unwrap_or_default();
    format!(
        "## Shared Context Bus\nThe following structured data was produced by prior agents in this workflow. Reference it as needed for the task below. You can also use `context_read`/`context_write` to access or add data.\n\n```json\n{ctx_json}\n```\n\n## Task\n{task}"
    )
}

// ---- context_read tool (registered in the subagent tool registry) ----

use crate::agent::tools::{Tool, ToolCtx};

pub struct ContextReadTool;

#[async_trait::async_trait]
impl Tool for ContextReadTool {
    fn name(&self) -> &'static str {
        "context_read"
    }
    fn description(&self) -> &'static str {
        "Read a value from the shared inter-agent context bus for this workflow run. Args: {key: string}. Returns the JSON value or an error if the key does not exist. The run is determined automatically from the calling subagent's context."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to read from the context bus (e.g. \"scout:files\", \"planner:plan\")" }
            },
            "required": ["key"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("key is required")?;
        let run_id = ctx
            .run_id
            .as_ref()
            .ok_or("context bus is only available inside a workflow run")?;
        let bus = ContextBus {
            run_id: run_id.clone(),
        };
        match bus.read(key) {
            Some(v) => serde_json::to_string_pretty(&v).map_err(|e| e.to_string()),
            None => Err(format!("key '{key}' not found in context bus")),
        }
    }
}

pub struct ContextWriteTool;

#[async_trait::async_trait]
impl Tool for ContextWriteTool {
    fn name(&self) -> &'static str {
        "context_write"
    }
    fn description(&self) -> &'static str {
        "Write a structured value to the shared inter-agent context bus for this workflow run. Args: {key: string, value: any JSON-serializable}. Use this to share findings, plans, gap-lists, file inventories, etc. with other agents in your workflow. The run is determined automatically from the calling subagent's context."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to write (e.g. \"scout:files\", \"reviewer:gap_list\")" },
                "value": { "description": "Any JSON-serializable value" }
            },
            "required": ["key", "value"]
        })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("key is required")?;
        let value = args.get("value").ok_or("value is required")?;
        let run_id = ctx
            .run_id
            .as_ref()
            .ok_or("context bus is only available inside a workflow run")?;
        let bus = ContextBus {
            run_id: run_id.clone(),
        };
        bus.write(key, value.clone());
        Ok(format!("wrote '{key}' to context bus"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_bus(run_id: &str) -> ContextBus {
        // Each test uses a unique run id so they don't share state even if run in parallel.
        ContextBus::create(run_id)
    }

    #[test]
    fn bus_create_then_read_write() {
        let bus = isolated_bus("test-1");
        assert!(bus.read("foo").is_none());
        bus.write("foo", json!("bar"));
        assert_eq!(bus.read("foo"), Some(json!("bar")));
    }

    #[test]
    fn bus_list_keys() {
        let bus = isolated_bus("test-2");
        bus.write("a", json!(1));
        bus.write("b", json!(2));
        let mut keys = bus.list_keys();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn bus_destroy_removes_all_entries() {
        let bus = isolated_bus("test-3");
        bus.write("x", json!("hello"));
        assert!(bus.read("x").is_some());
        ContextBus::destroy("test-3");
        assert!(bus.read("x").is_none());
    }

    #[test]
    fn bus_isolation_between_runs() {
        let bus_a = isolated_bus("test-4a");
        let bus_b = isolated_bus("test-4b");
        bus_a.write("k", json!("a-value"));
        bus_b.write("k", json!("b-value"));
        assert_eq!(bus_a.read("k"), Some(json!("a-value")));
        assert_eq!(bus_b.read("k"), Some(json!("b-value")));
    }

    #[test]
    fn inject_context_empty_bus_returns_task_unchanged() {
        let bus = isolated_bus("test-5");
        let task = "Do the thing";
        assert_eq!(inject_context_into_task(task, &bus), task);
    }

    #[test]
    fn inject_context_with_entries_prepends_json_block() {
        let bus = isolated_bus("test-6");
        bus.write("scout:files", json!(["src/lib.rs", "src/main.rs"]));
        bus.write("planner:plan", json!({"steps": ["add tests", "refactor"]}));
        let task = "Implement the plan";
        let injected = inject_context_into_task(task, &bus);
        assert!(
            injected.contains("Shared Context Bus"),
            "should contain the context header"
        );
        assert!(
            injected.contains("src/lib.rs"),
            "should contain the scout's file list"
        );
        assert!(
            injected.contains("## Task"),
            "should still contain the task section"
        );
        assert!(
            injected.contains("Implement the plan"),
            "should contain the original task text"
        );
    }

    #[test]
    fn bus_write_then_read_complex_json() {
        let bus = isolated_bus("test-7");
        let value = json!({
            "findings": [
                {"file": "src/lib.rs", "line": 42, "severity": "high", "desc": "off-by-one"},
                {"file": "src/main.rs", "line": 10, "severity": "low", "desc": "unused import"}
            ],
            "metadata": {"agent": "reviewer", "round": 1}
        });
        bus.write("review:gap_list", value.clone());
        let read = bus.read("review:gap_list").unwrap();
        assert_eq!(read["findings"].as_array().unwrap().len(), 2);
        assert_eq!(read["findings"][0]["severity"], "high");
        assert_eq!(read["metadata"]["agent"], "reviewer");
    }

    #[test]
    fn bus_read_nonexistent_key_returns_none() {
        let bus = isolated_bus("test-8");
        assert!(bus.read("no-such-key").is_none());
    }

    #[test]
    fn bus_destroy_is_idempotent() {
        let bus = isolated_bus("test-9");
        bus.write("k", json!(1));
        ContextBus::destroy("test-9");
        ContextBus::destroy("test-9"); // second destroy is a no-op
        assert!(bus.read("k").is_none());
    }

    /// `preload_from_run` must populate the bus with completed steps' outputs
    /// under the same keys the executor auto-population path uses.
    #[test]
    fn preload_from_run_populates_bus_from_completed_steps() {
        let bus = isolated_bus("test-preload");

        // Construct a minimal WorkflowRun with two completed steps.
        let run = crate::workflows::WorkflowRun {
            id: "run-1".into(),
            project_id: None,
            session_id: None,
            label: "test".into(),
            steps: vec![
                crate::workflows::WorkflowStep {
                    id: "step-0".into(),
                    agent: "scout".into(),
                    task: "explore".into(),
                    status: "done".into(),
                    parents: vec![],
                    children: vec!["step-1".into()],
                    output: Some("files found\n- src/lib.rs\n- src/main.rs".into()),
                    error: None,
                    usage: None,
                    sandbox_run_id: None,
                    browser_session_id: None,
                    tool_call_ids: None,
                    thinking: None,
                    started_at: Some(1000),
                    ended_at: Some(2000),
                    auto_repair: false,
                    repair_round: 0,
                    budget: None,
                    actual_cost: None,
                    actual_tokens: None,
                    model: None,
                    artifact: None,
                    cwd: None,
                    tool_calls: None,
                },
                crate::workflows::WorkflowStep {
                    id: "step-1".into(),
                    agent: "planner".into(),
                    task: "plan".into(),
                    status: "done".into(),
                    parents: vec!["step-0".into()],
                    children: vec![],
                    output: Some("plan:\n1. add tests".into()),
                    error: None,
                    usage: None,
                    sandbox_run_id: None,
                    browser_session_id: None,
                    tool_call_ids: None,
                    thinking: None,
                    started_at: Some(2000),
                    ended_at: Some(3000),
                    auto_repair: false,
                    repair_round: 0,
                    budget: None,
                    actual_cost: None,
                    actual_tokens: None,
                    model: None,
                    artifact: None,
                    cwd: None,
                    tool_calls: None,
                },
                // A pending step (not yet run) — must NOT be preloaded.
                crate::workflows::WorkflowStep {
                    id: "step-2".into(),
                    agent: "worker".into(),
                    task: "implement".into(),
                    status: "pending".into(),
                    parents: vec!["step-1".into()],
                    children: vec![],
                    output: None,
                    error: None,
                    usage: None,
                    sandbox_run_id: None,
                    browser_session_id: None,
                    tool_call_ids: None,
                    thinking: None,
                    started_at: None,
                    ended_at: None,
                    auto_repair: false,
                    repair_round: 0,
                    budget: None,
                    actual_cost: None,
                    actual_tokens: None,
                    model: None,
                    artifact: None,
                    cwd: None,
                    tool_calls: None,
                },
            ],
            status: "running".into(),
            origin: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(1000),
            ended_at: None,
            max_repair_rounds: 3,
            repair_rounds: 0,
            budget: None,
            actual_cost: None,
            actual_tokens: None,
        };

        bus.preload_from_run(&run);

        // Both completed steps must be on the bus.
        let step0_out = bus.read("step:step-0:output");
        assert!(
            step0_out.is_some(),
            "step-0 output should be preloaded on the bus"
        );
        assert_eq!(
            step0_out.unwrap().as_str().unwrap(),
            "files found\n- src/lib.rs\n- src/main.rs"
        );

        let step0_summary = bus.read("step:step-0:summary").unwrap();
        assert_eq!(
            step0_summary.as_str().unwrap(),
            "files found",
            "summary must be the first line of the output"
        );

        let step1_out = bus.read("step:step-1:output");
        assert!(
            step1_out.is_some(),
            "step-1 output should be preloaded on the bus"
        );
        assert_eq!(step1_out.unwrap().as_str().unwrap(), "plan:\n1. add tests");

        // The pending step must NOT appear on the bus.
        assert!(
            bus.read("step:step-2:output").is_none(),
            "pending step must not be preloaded"
        );
        assert!(
            bus.read("step:step-2:summary").is_none(),
            "pending step summary must not be preloaded"
        );
    }

    /// `preload_from_run` must skip steps with no output (e.g. errored steps
    /// that failed before producing output) without crashing.
    #[test]
    fn preload_from_run_skips_steps_without_output() {
        let bus = isolated_bus("test-preload-empty");
        let run = crate::workflows::WorkflowRun {
            id: "run-2".into(),
            project_id: None,
            session_id: None,
            label: "test".into(),
            steps: vec![crate::workflows::WorkflowStep {
                id: "step-err".into(),
                agent: "worker".into(),
                task: "fail".into(),
                status: "error".into(),
                parents: vec![],
                children: vec![],
                output: None,
                error: Some("something went wrong".into()),
                usage: None,
                sandbox_run_id: None,
                browser_session_id: None,
                tool_call_ids: None,
                thinking: None,
                started_at: Some(1000),
                ended_at: Some(2000),
                auto_repair: false,
                repair_round: 0,
                budget: None,
                actual_cost: None,
                actual_tokens: None,
                model: None,
                artifact: None,
                cwd: None,
                tool_calls: None,
            }],
            status: "error".into(),
            origin: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(1000),
            ended_at: Some(2000),
            max_repair_rounds: 3,
            repair_rounds: 0,
            budget: None,
            actual_cost: None,
            actual_tokens: None,
        };

        bus.preload_from_run(&run);

        // Errored step with no output must not appear on the bus.
        assert!(
            bus.read("step:step-err:output").is_none(),
            "errored step with no output must not be preloaded"
        );
    }

    /// `inject_context_into_task` truncates each bus value's JSON representation to 8 KB.
    /// The old code used `&val_str[..8192]`, which panics when byte 8192 falls inside a
    /// multi-byte UTF-8 character — the common case for real agent output (code comments,
    /// non-ASCII docs, emoji). This test plants a value whose JSON serialization places the
    /// second byte of a 2-byte char exactly at byte 8192 and confirms the injection no
    /// longer panics and still emits the `[truncated]` marker.
    #[test]
    fn inject_context_truncates_multibyte_value_without_panicking() {
        let bus = isolated_bus("test-trunc-mb");
        // 8190 ASCII chars + "é" (U+00E9, 2 UTF-8 bytes 0xC3 0xA9). JSON wraps the string
        // in quotes, so the serialized form is: `"` + 8190·'a' + 0xC3 0xA9 + `"` = 8194
        // bytes. Byte 8192 is 0xA9 — the second byte of "é" — which is NOT a char
        // boundary, so `&val_str[..8192]` panics without char-boundary handling.
        let big = format!("{}é", "a".repeat(8190));
        bus.write("scout:big", json!(big));
        let injected = inject_context_into_task("do the thing", &bus);
        assert!(
            injected.contains("[truncated]"),
            "truncated value must carry the [truncated] marker"
        );
        assert!(
            injected.contains("## Task\ndo the thing"),
            "injection must still include the task section"
        );
    }

    // ---- context_read / context_write tool isolation ----
    //
    // The bus tools must derive the run id from the ToolCtx (set by the subagent runtime from
    // its ContextBus), NOT from a `runId` argument supplied by the LLM. A subagent has no way to
    // learn its run id, so requiring it as an arg made the tools unusable; and trusting it would
    // let one run read/write another run's bus by passing a foreign id — breaking the
    // "structural cross-run isolation" the module doc promises.

    use crate::agent::tools::{Tool, ToolCtx};
    use std::path::PathBuf;

    fn ctx_with_run(run_id: Option<&str>) -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from("."),
            tx: None,
            run_id: run_id.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn context_write_then_read_round_trips_via_ctx_run_id() {
        // Two distinct runs with the same key must stay isolated — the read sees only its own run.
        let _a = isolated_bus("tool-rt-a");
        let _b = isolated_bus("tool-rt-b");
        let write = ContextWriteTool;
        let read = ContextReadTool;

        let ctx_a = ctx_with_run(Some("tool-rt-a"));
        let ctx_b = ctx_with_run(Some("tool-rt-b"));

        write
            .execute(
                &json!({"key": "scout:notes", "value": "found a bug"}),
                &ctx_a,
            )
            .await
            .unwrap();
        write
            .execute(&json!({"key": "scout:notes", "value": "all clear"}), &ctx_b)
            .await
            .unwrap();

        let got_a = read
            .execute(&json!({"key": "scout:notes"}), &ctx_a)
            .await
            .unwrap();
        assert!(
            got_a.contains("found a bug"),
            "read A must see A's value: {got_a}"
        );

        let got_b = read
            .execute(&json!({"key": "scout:notes"}), &ctx_b)
            .await
            .unwrap();
        assert!(
            got_b.contains("all clear"),
            "read B must see B's value: {got_b}"
        );
    }

    #[tokio::test]
    async fn context_tools_ignore_foreign_run_id_arg() {
        // A `runId` arg naming a *different* run must NOT override the ctx run id — otherwise
        // one workflow run could read another's bus by passing a foreign id.
        let own = isolated_bus("tool-iso-own");
        let foreign = isolated_bus("tool-iso-foreign");
        // Plant a secret on the foreign bus.
        foreign.write("secret", json!("other-run-data"));

        let write = ContextWriteTool;
        let read = ContextReadTool;
        let ctx_own = ctx_with_run(Some("tool-iso-own"));

        // Writing with a foreign runId arg must land on the OWN bus, not the foreign one.
        write
            .execute(
                &json!({"key": "k", "value": "mine", "runId": "tool-iso-foreign"}),
                &ctx_own,
            )
            .await
            .unwrap();
        assert_eq!(
            own.read("k"),
            Some(json!("mine")),
            "write must target the ctx run, ignoring the runId arg"
        );
        assert_eq!(foreign.read("k"), None, "foreign run bus must be untouched");

        // Reading with a foreign runId arg must NOT see the foreign run's secret.
        let err_or_val = read
            .execute(
                &json!({"key": "secret", "runId": "tool-iso-foreign"}),
                &ctx_own,
            )
            .await;
        assert!(
            err_or_val.is_err() || !err_or_val.unwrap().contains("other-run-data"),
            "read must not leak the foreign run's data via a runId arg"
        );
    }

    #[tokio::test]
    async fn context_tools_refuse_without_ctx_run_id() {
        // The lead session / non-workflow contexts have no run id — the bus tools must refuse
        // rather than silently touching some default or caller-supplied run.
        let ctx_none = ctx_with_run(None);
        let write = ContextWriteTool;
        let read = ContextReadTool;

        let werr = write
            .execute(
                &json!({"key": "k", "value": 1, "runId": "anything"}),
                &ctx_none,
            )
            .await
            .unwrap_err();
        assert!(
            werr.contains("only available inside a workflow run"),
            "write without ctx run id must refuse, got: {werr}"
        );

        let rerr = read
            .execute(&json!({"key": "k", "runId": "anything"}), &ctx_none)
            .await
            .unwrap_err();
        assert!(
            rerr.contains("only available inside a workflow run"),
            "read without ctx run id must refuse, got: {rerr}"
        );
    }
}
