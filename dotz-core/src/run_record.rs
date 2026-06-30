//! Run record — a full, reproducible capture of one workflow run.
//!
//! For each step we persist: the prompt (task), the resolved model, the model
//! override configured on the step, the thinking config, the agent's declared
//! skill set (its active tool list), the full provider response message stream
//! (thinking blocks + text + tool calls + tool results), usage, stop reason,
//! error, exit code, and timing. The record is written to
//! `<dotz_dir>/ai-agents/run-records/<run_id>.json` (override the directory with
//! `DOTZ_RUN_RECORD_DIR`) so it survives server restarts and can be inspected
//! offline — the operator can open the JSON directly to debug a misbehaving step
//! without re-running it.
//!
//! `replay` rebuilds a fresh run from a captured record — same agent / task /
//! model / parents / thinking / auto_repair / per-step budget / run budget —
//! and re-executes it. This makes orchestration regressions bisectable: replay
//! a recorded run after a code/provider/config change and diff the new record
//! against the old one to see which step's behavior drifted and why.
//!
//! The capture is purely additive: it runs alongside the existing workflow
//! store (`workflows.rs`) and executor (`workflow_executor.rs`) without
//! altering their behavior. If recording fails (disk full, permission denied),
//! the run proceeds unaffected — a missing record degrades to "no replay",
//! never to a failed run.
use crate::agent::subagent::SingleResult;
use crate::config::dotz_dir;
use crate::types::Budget;
use crate::workflows::{self, CreateStepInput, WorkflowRun, WorkflowStep};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Directory holding one `<run_id>.json` record per run. Override with
/// `DOTZ_RUN_RECORD_DIR`; defaults to `<dotz_dir>/ai-agents/run-records`.
fn record_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DOTZ_RUN_RECORD_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dotz_dir().join("ai-agents").join("run-records")
}

fn record_path(run_id: &str) -> PathBuf {
    record_dir().join(format!("{run_id}.json"))
}

/// Serializes all record file IO so parallel step captures (a wide fan-out) do
/// not lose updates to the shared `<run_id>.json`. Record writes are rare
/// (one per step completion) so a single global guard is plenty.
fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// One step's full reproducible capture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_id: String,
    pub agent: String,
    /// "user" | "project" | "unknown" | "timeout" — where the agent definition
    /// came from (mirrors `SingleResult.agent_source`).
    pub agent_source: String,
    /// The prompt sent to the agent.
    pub task: String,
    /// The effective model that actually ran (after provider failover). May
    /// differ from `model_override` when `provider_health` rewrote the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model override configured on the step (what the operator picked in
    /// the UI node drawer). `None` means "use the agent/run default".
    #[serde(rename = "modelOverride", skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    /// The thinking config attached to the step (extended-thinking budget, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(rename = "toolCallIds", skip_serializing_if = "Option::is_none")]
    pub tool_call_ids: Option<Vec<String>>,
    #[serde(rename = "sandboxRunId", skip_serializing_if = "Option::is_none")]
    pub sandbox_run_id: Option<String>,
    #[serde(rename = "browserSessionId", skip_serializing_if = "Option::is_none")]
    pub browser_session_id: Option<String>,
    /// The agent's declared skill set — the active tool list the subagent ran
    /// with. Empty for unknown-agent / timeout steps (no agent was resolved).
    #[serde(rename = "skillSet", default)]
    pub skill_set: Vec<String>,
    /// The full provider response stream: every assistant message + tool-result
    /// message, including thinking blocks, text, tool calls, and tool results.
    /// This is the raw material a replay reproduces and a regression bisect diffs.
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::workflows::Usage>,
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "exitCode")]
    pub exit_code: i64,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(rename = "autoRepair", default)]
    pub auto_repair: bool,
    #[serde(rename = "repairRound", default)]
    pub repair_round: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<crate::workflows::Artifact>,
}

/// A full reproducible run record — one file per run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub label: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub status: String,
    /// The run-level budget cap (carried into replays so a regression repro
    /// doesn't accidentally burn the provider balance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Max repair rounds configured on the run (carried into replays).
    #[serde(rename = "maxRepairRounds", default)]
    pub max_repair_rounds: u32,
    pub steps: Vec<StepRecord>,
}

/// Initialize (or refresh) the record for a run with the run's metadata. Called
/// once at the start of `run_workflow`. Idempotent: re-running overwrites the
/// metadata but preserves already-captured steps, so a resume re-inits cleanly
/// without clobbering steps that completed before the restart.
pub fn init_run(run: &WorkflowRun) {
    let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
    let steps = load_unlocked(&run.id).map(|r| r.steps).unwrap_or_default();
    let record = RunRecord {
        run_id: run.id.clone(),
        label: run.label.clone(),
        created_at: run.created_at,
        started_at: run.started_at,
        ended_at: run.ended_at,
        status: run.status.clone(),
        budget: run.budget.clone(),
        max_repair_rounds: run.max_repair_rounds,
        steps,
    };
    write_unlocked(&record);
}

/// Capture (or update) a step's full record after the step completes. Reads the
/// post-completion `WorkflowStep` from the active store for the static config +
/// final status / timing / usage, and merges the `SingleResult` for the
/// provider response stream, skill set, exit code, stop reason, and effective
/// model.
///
/// `result` is `None` when the step hit the executor's wall-clock timeout (no
/// `SingleResult` was produced); we still record the step with
/// `agent_source = "timeout"` and `stop_reason = "timeout"` so the gap is
/// visible in the record rather than silently missing.
///
/// Failure-tolerant: any IO/parse error is swallowed — recording must never
/// break a run.
pub fn capture_step(run_id: &str, step_id: &str, result: Option<&SingleResult>) {
    let Some(run) = workflows::get_active(run_id) else {
        return;
    };
    let Some(step) = run.steps.iter().find(|s| s.id == step_id) else {
        return;
    };
    let rec = build_step_record(step, result);

    let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
    let mut record = load_unlocked(run_id).unwrap_or_else(|| RunRecord {
        run_id: run_id.to_string(),
        label: run.label.clone(),
        created_at: run.created_at,
        started_at: run.started_at,
        ended_at: run.ended_at,
        status: run.status.clone(),
        budget: run.budget.clone(),
        max_repair_rounds: run.max_repair_rounds,
        steps: Vec::new(),
    });
    // Refresh run-level metadata (status may have transitioned since init).
    record.status = run.status.clone();
    record.started_at = run.started_at.or(record.started_at);
    record.ended_at = run.ended_at.or(record.ended_at);
    // Upsert by step_id (a re-executed interrupted step updates in place).
    match record.steps.iter().position(|s| s.step_id == step_id) {
        Some(i) => record.steps[i] = rec,
        None => record.steps.push(rec),
    }
    write_unlocked(&record);
}

fn build_step_record(step: &WorkflowStep, result: Option<&SingleResult>) -> StepRecord {
    let (
        messages,
        skill_set,
        agent_source,
        exit_code,
        stop_reason,
        error_message,
        effective_model,
    ) = match result {
        Some(r) => (
            r.messages.clone(),
            r.skill_set.clone(),
            r.agent_source.clone(),
            r.exit_code,
            r.stop_reason.clone(),
            r.error_message.clone(),
            r.model.clone(),
        ),
        // No `SingleResult` was produced — the step never made a provider call.
        // Distinguish the two real causes so the record is truthful rather than
        // blanket-labeling every gap "timeout":
        //   - `skipped` → cascaded-skip from a failed parent / exhausted budget;
        //     the executor never dispatched it. `agent_source = "skipped"`, no
        //     stop reason (the step didn't run, so it didn't "stop").
        //   - `error`    → the executor's wall-clock timeout fired mid-stream.
        //     `agent_source = "timeout"`, `stop_reason = "timeout"`, exit 1.
        //   - anything else → defensive: no provider call, no synthetic label.
        None => {
            let (src, stop, code) = match step.status.as_str() {
                "skipped" => ("skipped".to_string(), None, 0),
                "error" => ("timeout".to_string(), Some("timeout".to_string()), 1),
                _ => (String::new(), None, 0),
            };
            (Vec::new(), Vec::new(), src, code, stop, None, None)
        }
    };
    StepRecord {
        step_id: step.id.clone(),
        agent: step.agent.clone(),
        agent_source,
        task: step.task.clone(),
        // Prefer the effective model (what actually ran) and fall back to the
        // override so an unknown-agent step (no SingleResult) still records the
        // configured model.
        model: effective_model.or(step.model.clone()),
        model_override: step.model.clone(),
        parents: step.parents.clone(),
        children: step.children.clone(),
        thinking: step.thinking.clone(),
        tool_call_ids: step.tool_call_ids.clone(),
        sandbox_run_id: step.sandbox_run_id.clone(),
        browser_session_id: step.browser_session_id.clone(),
        skill_set,
        messages,
        usage: step.usage.clone(),
        // Prefer the provider's stop reason; fall back to a synthesized "error"
        // marker when the step errored but the provider didn't report one.
        stop_reason: stop_reason.or_else(|| {
            (step.status == "error").then(|| "error".to_string())
        }),
        // Prefer the provider's error message; fall back to the step's recorded
        // error (e.g. the executor's timeout message).
        error_message: error_message.or(step.error.clone()),
        exit_code,
        started_at: step.started_at,
        ended_at: step.ended_at,
        status: step.status.clone(),
        output: step.output.clone(),
        auto_repair: step.auto_repair,
        repair_round: step.repair_round,
        budget: step.budget.clone(),
        artifact: step.artifact.clone(),
    }
}

/// Load a run's record from disk. Returns `None` if no record was captured
/// (the run predates recording, or recording failed).
pub fn load(run_id: &str) -> Option<RunRecord> {
    let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
    load_unlocked(run_id)
}

fn load_unlocked(run_id: &str) -> Option<RunRecord> {
    match std::fs::read_to_string(record_path(run_id)) {
        Ok(raw) => serde_json::from_str::<RunRecord>(&raw).ok(),
        Err(_) => None,
    }
}

fn write_unlocked(record: &RunRecord) {
    let path = record_path(&record.run_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(record) {
        let _ = std::fs::write(&path, s);
    }
}

/// Rebuild a fresh run from a captured record and re-execute it. The new run
/// clones every step's `agent` / `task` / `model_override` / `thinking` /
/// `auto_repair` / per-step `budget` / `parents` (mapped from old step ids to
/// positional indices) and the run-level `budget` + `maxRepairRounds`. Returns
/// the newly-created (pending) run; the executor is spawned so the UI sees the
/// live `pending → running → …` transition over WebSocket just like the original.
///
/// Returns `None` if no record exists or it has no steps.
pub fn replay(run_id: &str) -> Option<WorkflowRun> {
    let record = load(run_id)?;
    if record.steps.is_empty() {
        return None;
    }
    let inputs: Vec<CreateStepInput> = record
        .steps
        .iter()
        .map(|s| {
            // Map old parent ids → positional indices into the recorded step
            // list. `workflows::create` resolves numeric parent refs to the new
            // step ids, so the replayed DAG mirrors the recorded DAG's shape.
            let parents: Vec<Value> = s
                .parents
                .iter()
                .filter_map(|pid| record.steps.iter().position(|x| x.step_id == *pid))
                .map(|i| json!(i))
                .collect();
            CreateStepInput {
                agent: s.agent.clone(),
                task: s.task.clone(),
                parents: Some(parents),
                sandbox_run_id: s.sandbox_run_id.clone(),
                browser_session_id: s.browser_session_id.clone(),
                tool_call_ids: s.tool_call_ids.clone(),
                thinking: s.thinking.clone(),
                auto_repair: s.auto_repair,
                budget: s.budget.clone(),
                model: s.model_override.clone(),
            }
        })
        .collect();

    let label = format!("{} (replay)", record.label);
    let run = workflows::create(
        None,
        None,
        label,
        Some("replay".to_string()),
        record.max_repair_rounds,
        &inputs,
        record.budget.clone(),
    )
    .ok()?;

    let rid = run.id.clone();
    // Spawn the executor so this function returns promptly with the pending
    // run; the UI watches the live transition over WebSocket. Mirrors how
    // `workflows::resume` decouples "start" from "drive to completion".
    tokio::spawn(async move {
        let _ = crate::workflow_executor::run_workflow(&rid).await;
    });
    Some(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::{SingleResult, SubUsage};
    use crate::workflows::{self, CreateStepInput, StepPatch, Usage};
    use serde_json::json;
    use std::time::Duration;
    use uuid::Uuid;

    /// Serializes these tests on the process-global env vars they mutate
    /// (`DOTZ_RUN_RECORD_DIR`, `DOTZ_WORKFLOWS_FILE`). Mirrors the `ENV_LOCK`
    /// pattern used by the `workflows` and `workflow_executor` test suites —
    /// without it, parallel tests clobber each other's env mid-run and a
    /// `capture_step`/`load` round-trip races against a sibling test that reset
    /// the dir (observed as a flaky `load().unwrap()` on `None`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Point recording at a unique temp dir for the duration of the test, AND
    /// hold the env-var lock for the whole test body (released on drop). Without
    /// the lock, parallel tests clobber each other's `DOTZ_RUN_RECORD_DIR` /
    /// `DOTZ_WORKFLOWS_FILE` mid-run — observed as a flaky `load().unwrap()` on
    /// `None` when a sibling test reset the dir between a `capture_step` and the
    /// asserting `load`. Mirrors the `ENV_LOCK` pattern in the `workflows` and
    /// `workflow_executor` suites.
    #[allow(dead_code)] // the guard field is held only for its Drop (lock release)
    struct TmpDir(PathBuf, std::sync::MutexGuard<'static, ()>);
    impl TmpDir {
        fn new() -> Self {
            let g = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let d = std::env::temp_dir().join(format!("dotz-runrec-{}", Uuid::new_v4()));
            std::env::set_var("DOTZ_RUN_RECORD_DIR", d.to_string_lossy().to_string());
            Self(d, g)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            std::env::remove_var("DOTZ_RUN_RECORD_DIR");
        }
    }

    fn wf_file() -> std::path::PathBuf {
        // Use a unique workflows file so these tests don't collide with the
        // workflow_executor suite (which mutates DOTZ_WORKFLOWS_FILE under its
        // own ENV_LOCK).
        let f = std::env::temp_dir().join(format!("dotz-runrec-wf-{}.json", Uuid::new_v4()));
        std::env::set_var("DOTZ_WORKFLOWS_FILE", f.to_string_lossy().to_string());
        f
    }

    fn step(agent: &str, task: &str, parents: Option<Vec<Value>>) -> CreateStepInput {
        CreateStepInput {
            agent: agent.into(),
            task: task.into(),
            parents,
            sandbox_run_id: None,
            browser_session_id: None,
            tool_call_ids: None,
            thinking: None,
            auto_repair: false,
            budget: None,
            model: None,
        }
    }

    fn fake_single_result(agent: &str, task: &str, model: &str) -> SingleResult {
        SingleResult {
            agent: agent.into(),
            agent_source: "user".into(),
            task: task.into(),
            exit_code: 0,
            messages: vec![
                json!({
                    "role": "user",
                    "content": [{"type": "text", "text": task}],
                    "timestamp": 0,
                }),
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "planning the work", "thinkingSignature": "sig"},
                        {"type": "text", "text": "done: 3 files"},
                        {"type": "toolCall", "id": "tc_1", "name": "read", "arguments": {"path": "src/lib.rs"}},
                    ],
                    "provider": "ollama",
                    "model": model,
                    "usage": {"input": 100, "output": 50, "totalTokens": 150, "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.002}},
                    "stopReason": "tool_use",
                    "timestamp": 1,
                }),
            ],
            usage: SubUsage::default(),
            model: Some(model.into()),
            stop_reason: Some("tool_use".into()),
            error_message: None,
            step: None,
            skill_set: vec!["read".into(), "write".into(), "bash".into()],
        }
    }

    /// init_run must create the record file with the run's metadata and an empty
    /// steps list, and preserve already-captured steps on re-init (resume case).
    #[test]
    fn init_run_writes_metadata_and_preserves_steps_on_reinit() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        let run = workflows::create(
            None,
            None,
            "init-test".into(),
            None,
            3,
            &[step("scout", "find files", None)],
            None,
        )
        .unwrap();
        init_run(&run);

        let record = load(&run.id).expect("init_run wrote a record");
        assert_eq!(record.run_id, run.id);
        assert_eq!(record.label, "init-test");
        assert_eq!(record.max_repair_rounds, 3);
        assert!(record.steps.is_empty(), "fresh init has no steps");

        // Re-init after a step was captured must NOT clobber the step.
        // Simulate by directly writing a record with a step, then re-init.
        {
            let _g = lock().lock().unwrap();
            let mut r = load_unlocked(&run.id).unwrap();
            r.steps.push(StepRecord {
                step_id: "fake".into(),
                agent: "scout".into(),
                agent_source: "user".into(),
                task: "find files".into(),
                model: None,
                model_override: None,
                parents: Vec::new(),
                children: Vec::new(),
                thinking: None,
                tool_call_ids: None,
                sandbox_run_id: None,
                browser_session_id: None,
                skill_set: Vec::new(),
                messages: Vec::new(),
                usage: None,
                stop_reason: None,
                error_message: None,
                exit_code: 0,
                started_at: None,
                ended_at: None,
                status: "done".into(),
                output: Some("ok".into()),
                auto_repair: false,
                repair_round: 0,
                budget: None,
                artifact: None,
            });
            write_unlocked(&r);
        }
        init_run(&run);
        let record = load(&run.id).unwrap();
        assert_eq!(record.steps.len(), 1, "re-init must preserve steps");
        assert_eq!(record.steps[0].step_id, "fake");
    }

    /// capture_step must merge the WorkflowStep's static config + final status
    /// with the SingleResult's provider response stream, skill set, and
    /// effective model — producing a faithful, replayable record.
    #[test]
    fn capture_step_merges_store_and_single_result() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        let run = workflows::create(
            None,
            None,
            "capture-test".into(),
            None,
            3,
            &[
                step("worker", "edit lib.rs", None),
                step("reviewer", "review", Some(vec![json!(0)])),
            ],
            None,
        )
        .unwrap();
        init_run(&run);
        let run = workflows::start(&run.id).unwrap();
        let step0_id = run.steps[0].id.clone();

        // Complete step 0 with usage + output, then capture its SingleResult.
        let _ = workflows::step_state(
            &run.id,
            &step0_id,
            StepPatch {
                status: Some("done".into()),
                output: Some("edited lib.rs".into()),
                usage: Some(Usage {
                    input: Some(100.0),
                    output: Some(50.0),
                    cost: Some(0.002),
                    turns: Some(1.0),
                }),
                ..Default::default()
            },
        );
        let single = fake_single_result("worker", "edit lib.rs", "ollama/minimax-m3");
        capture_step(&run.id, &step0_id, Some(&single));

        let record = load(&run.id).expect("record present after capture");
        assert_eq!(record.steps.len(), 1);
        let s = &record.steps[0];
        assert_eq!(s.step_id, step0_id);
        assert_eq!(s.agent, "worker");
        assert_eq!(s.task, "edit lib.rs");
        assert_eq!(s.status, "done");
        assert_eq!(s.output.as_deref(), Some("edited lib.rs"));
        assert_eq!(s.model.as_deref(), Some("ollama/minimax-m3"));
        assert_eq!(s.agent_source, "user");
        assert_eq!(s.exit_code, 0);
        assert_eq!(s.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(s.skill_set, vec!["read", "write", "bash"]);
        // The full provider response stream is captured: 1 user + 1 assistant
        // message with a thinking block, a text block, and a tool call.
        assert_eq!(s.messages.len(), 2, "messages: user + assistant");
        let asst = &s.messages[1];
        assert_eq!(asst["role"], "assistant");
        let blocks = asst["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "thinking + text + toolCall");
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[2]["type"], "toolCall");
        // Usage from the store is captured.
        assert_eq!(s.usage.as_ref().unwrap().cost, Some(0.002));
    }

    /// capture_step with `result = None` (executor-level timeout) must still
    /// record the step with `agent_source = "timeout"` and `stop_reason =
    /// "timeout"`, so a replay/inspect sees the gap rather than a hole.
    #[test]
    fn capture_step_records_timeout_with_no_single_result() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        let run = workflows::create(
            None,
            None,
            "timeout-test".into(),
            None,
            3,
            &[step("scout", "scout task", None)],
            None,
        )
        .unwrap();
        init_run(&run);
        let run = workflows::start(&run.id).unwrap();
        let step_id = run.steps[0].id.clone();
        let _ = workflows::step_state(
            &run.id,
            &step_id,
            StepPatch {
                status: Some("error".into()),
                error: Some("step timed out".into()),
                ..Default::default()
            },
        );
        capture_step(&run.id, &step_id, None);

        let record = load(&run.id).unwrap();
        let s = &record.steps[0];
        assert_eq!(s.agent_source, "timeout");
        assert_eq!(s.stop_reason.as_deref(), Some("timeout"));
        assert_eq!(s.status, "error");
        assert_eq!(s.error_message.as_deref(), Some("step timed out"));
        assert!(s.messages.is_empty());
        assert!(s.skill_set.is_empty());
    }

    /// A skipped step (no provider call — cascaded by a failed parent / exhausted
    /// budget) must be recorded with a TRUTHFUL label: `agent_source = "skipped"`
    /// and NO stop reason — not the misleading `"timeout"` used for an
    /// executor-level wall-clock timeout. The two gaps have different causes and
    /// a replay/inspect must distinguish them.
    #[test]
    fn capture_step_labels_skipped_step_distinctly_from_timeout() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        let run = workflows::create(
            None,
            None,
            "skip-label-test".into(),
            None,
            3,
            &[step("worker", "task", None)],
            None,
        )
        .unwrap();
        init_run(&run);
        let run = workflows::start(&run.id).unwrap();
        let step_id = run.steps[0].id.clone();
        let _ = workflows::step_state(
            &run.id,
            &step_id,
            StepPatch {
                status: Some("skipped".into()),
                error: Some("parent failed".into()),
                ..Default::default()
            },
        );
        capture_step(&run.id, &step_id, None);

        let record = load(&run.id).unwrap();
        let s = &record.steps[0];
        assert_eq!(s.status, "skipped");
        assert_eq!(s.agent_source, "skipped", "skipped ≠ timeout");
        assert!(s.stop_reason.is_none(), "a skipped step did not run — it has no stop reason");
        assert_eq!(s.exit_code, 0);
        assert!(s.messages.is_empty());
        assert!(s.skill_set.is_empty());
    }

    /// A second capture_step for the same step (e.g. an interrupted step that
    /// was re-executed on resume) must UPDATE the existing record in place,
    /// not append a duplicate.
    #[test]
    fn capture_step_upserts_existing_step_in_place() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        let run = workflows::create(
            None,
            None,
            "upsert-test".into(),
            None,
            3,
            &[step("worker", "task", None)],
            None,
        )
        .unwrap();
        init_run(&run);
        let run = workflows::start(&run.id).unwrap();
        let step_id = run.steps[0].id.clone();
        let _ = workflows::step_state(
            &run.id,
            &step_id,
            StepPatch {
                status: Some("done".into()),
                output: Some("first".into()),
                ..Default::default()
            },
        );
        let single = fake_single_result("worker", "task", "ollama/minimax-m3");
        capture_step(&run.id, &step_id, Some(&single));
        assert_eq!(load(&run.id).unwrap().steps.len(), 1);

        // Re-capture (resume re-execution): must update, not duplicate.
        let single2 = fake_single_result("worker", "task", "openrouter/gpt-4o");
        capture_step(&run.id, &step_id, Some(&single2));
        let record = load(&run.id).unwrap();
        assert_eq!(record.steps.len(), 1, "re-capture must upsert, not append");
        assert_eq!(record.steps[0].model.as_deref(), Some("openrouter/gpt-4o"));
    }

    /// replay must build a new run that mirrors the recorded DAG's shape
    /// (parents), agent/task/model/budget, and run-level budget + max repair
    /// rounds — and spawn the executor so the new run drives to completion.
    #[tokio::test]
    async fn replay_rebuilds_dag_shape_and_drives_to_completion() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        // No timeout env vars needed: the unknown-agent path returns a
        // `SingleResult` synchronously without any provider call, so the
        // executor's `step_timeout()` wrapper never fires.

        // Record a 2-step chain: step 0 (unknown agent → error), step 1 depends
        // on step 0 (skipped by the error sweep). The record must capture BOTH
        // steps — including the skipped child (which never went through the
        // spawn path) — so the replayed DAG mirrors the original's shape.
        let run = workflows::create(
            None,
            None,
            "replay-source".into(),
            None,
            3,
            &[
                step("dotz-replay-a", "alpha", None),
                step("dotz-replay-b", "beta", Some(vec![json!(0)])),
            ],
            Some(Budget {
                max_cost: Some(10.0),
                ..Default::default()
            }),
        )
        .unwrap();
        let run_id = run.id.clone();

        // Drive the original run to completion via the executor (which captures
        // each step's record). Both steps terminate fast (unknown agent / skip).
        let _ = crate::workflow_executor::run_workflow(&run_id).await;

        let record = load(&run_id).expect("executor captured the record");
        assert_eq!(record.steps.len(), 2, "both steps recorded (incl. the skipped child)");
        // The skipped child must be in the record with status "skipped".
        let skipped = record
            .steps
            .iter()
            .find(|s| s.status == "skipped")
            .expect("the skipped step is recorded");
        assert!(skipped.messages.is_empty(), "skipped step had no provider call");
        assert_eq!(record.budget.as_ref().unwrap().max_cost, Some(10.0));
        assert_eq!(record.max_repair_rounds, 3);

        // Replay: build a fresh run from the record and let it drive to completion.
        let replayed = replay(&run_id).expect("replay returns a new run");
        assert!(replayed.label.ends_with("(replay)"), "label gets a replay tag");
        assert_eq!(replayed.origin.as_deref(), Some("replay"));
        assert_eq!(replayed.steps.len(), 2, "replay clones both steps");
        // The parent edge must be preserved (numeric ref resolved to the new
        // step 0 id).
        assert_eq!(
            replayed.steps[1].parents,
            vec![replayed.steps[0].id.clone()],
            "replay preserves the DAG edge"
        );
        // The run-level budget is carried over.
        assert_eq!(replayed.budget.as_ref().unwrap().max_cost, Some(10.0));

        // Wait for the spawned executor to finish (poll the active store).
        let replay_id = replayed.id.clone();
        for _ in 0..200 {
            if let Some(r) = workflows::get_active(&replay_id) {
                if r.status == "done" || r.status == "error" || r.status == "aborted" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The executor marks the run terminal inside `step_state` (visible via
        // `get_active`) BEFORE its post-loop record backfill for skipped steps.
        // Give that backfill a beat to land while `DOTZ_RUN_RECORD_DIR` is still
        // pointed at our temp dir, so the replay's own record doesn't fall back
        // to the real `dotz_dir` and pollute the operator's run-records store.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let final_run = workflows::get_active(&replay_id).expect("replay run exists");
        assert!(
            final_run.status == "error" || final_run.status == "done",
            "replay drove to a terminal state: {}",
            final_run.status
        );
    }

    /// replay must return None when no record exists, and None for a record
    /// with no steps (defensive — should not happen in practice).
    #[test]
    fn replay_returns_none_for_missing_or_empty_record() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        assert!(replay("no-such-run").is_none());

        // An empty record (init_run only, no captures).
        let run = workflows::create(
            None,
            None,
            "empty".into(),
            None,
            3,
            &[step("a", "A", None)],
            None,
        )
        .unwrap();
        init_run(&run);
        assert!(replay(&run.id).is_none(), "empty record → no replay");
    }

    /// load must return None for an unknown run id (no file → None, not a panic).
    #[test]
    fn load_returns_none_for_unknown_run() {
        let _rec = TmpDir::new();
        let _wf = wf_file();
        assert!(load("no-such-run-id").is_none());
    }
}