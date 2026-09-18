//! Workflow executor — promotes the workflow DAG from observability to execution engine.
//!
//! The workflow store (`workflows.rs`) records step DAGs and propagates state transitions.
//! This module DRIVES a run to completion: it finds `ready` steps, dispatches them as
//! subagent tasks (bounded concurrency), records results via `step_state()` (which handles
//! readiness propagation, skip cascading, auto-repair, and run completion), and loops until
//! the run is terminal.
//!
//! Failure-rerouting: when a step errors, `step_state()`'s error sweep marks the run errored
//! and skips every still-runnable step — so a failed upstream step can't leave siblings
//! stuck `pending`.
//!
//! Branch-on-verification-outcome: when a review step with `auto_repair:true` finishes with
//! findings, `step_state()` spawns a repair child + re-review grandchild and keeps the run
//! running until the re-review passes or the retry cap is hit. The executor just lets the
//! auto-repair loop run — it does not need to understand verification semantics.
//!
//! Bounded concurrency: a tokio Semaphore (default 4, configurable via DOTZ_WF_CONCURRENCY)
//! gates how many steps execute in parallel, matching the subagent fan-out pool.
use crate::agent::subagent::{SingleResult, run_single_agent_with_bus};
use crate::checkpoint::git_diff_artifact;
use crate::context_bus::ContextBus;
use crate::run_record;
use crate::workflows;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Default number of steps that may execute concurrently in a workflow run.
/// Matches the subagent MAX_CONCURRENCY (4) — keeps the in-process fan-out bounded so a
/// wide parallel step does not spawn unbounded LLM streams.
const DEFAULT_CONCURRENCY: usize = 4;

/// Upper bound for DOTZ_WF_CONCURRENCY. A misconfigured env var (e.g. 99999) would spawn
/// that many concurrent LLM streams and almost certainly OOM the process or hammer the
/// provider into rate-limiting. Clamp to a sane ceiling.
const MAX_CONCURRENCY: usize = 16;

/// Per-step wall-clock timeout. A hung provider or infinite tool loop must not stall the
/// entire run forever. Defaults to 5 minutes; override with `DOTZ_WF_STEP_TIMEOUT_MS`.
/// Clamped to [1s, 1h].
fn step_timeout() -> Duration {
    const MIN_MS: u64 = 1_000;
    const MAX_MS: u64 = 3_600_000;
    std::env::var("DOTZ_WF_STEP_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(300))
}

fn concurrency() -> usize {
    std::env::var("DOTZ_WF_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_CONCURRENCY))
        .unwrap_or(DEFAULT_CONCURRENCY)
}

/// Ready step data collected from the workflow run for dispatch.
/// Replaces a 5-tuple (id, agent, task, model, cwd) to avoid clippy::type_complexity.
#[derive(Debug, Clone)]
struct ReadyStep {
    id: String,
    agent: String,
    task: String,
    model: Option<String>,
    cwd: Option<String>,
}

/// Extract the tool calls a subagent made from its recorded `messages`, panel-tagged, for the
/// step's durable `toolCalls` (graph sub-node chips). Assistant tool-call blocks are
/// `{"type":"toolCall","id","name","arguments"}` (event.rs), tool results are top-level
/// `{"role":"tool","toolCallId","content":[{"type":"text","text"}],...,"isError":true}`
/// (subagent.rs) — a call is errored if its result message carries `isError:true`. De-dupes by
/// tool-call id, preserving first-seen order. Captures capped `args` (from the toolCall block)
/// and `result` (the tool message's text content) so the graph drawer is a complete, inspectable
/// record of what each tool did — not just a colored chip.
fn tool_calls_from_messages(messages: &[serde_json::Value]) -> Vec<workflows::ToolCallRef> {
    use std::collections::{HashMap, HashSet};
    let mut errored: HashSet<String> = HashSet::new();
    // tool-call id → joined result text (from `{"role":"tool",...}` messages).
    let mut results: HashMap<String, String> = HashMap::new();
    for m in messages {
        if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
            if let Some(id) = m.get("toolCallId").and_then(|v| v.as_str()) {
                if m.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                    errored.insert(id.to_string());
                }
                if let Some(content) = m.get("content").and_then(|c| c.as_array()) {
                    let entry = results.entry(id.to_string()).or_default();
                    for b in content {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                if !entry.is_empty() {
                                    entry.push('\n');
                                }
                                entry.push_str(t);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for m in messages {
        let Some(content) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in content {
            if b.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
                let id = b
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = b
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() || !seen.insert(id.clone()) {
                    continue;
                }
                let args = b
                    .get("arguments")
                    .and_then(|a| serde_json::to_string(a).ok())
                    .map(|s| workflows::ToolCallRef::cap_str(&s));
                let result = results.get(&id).map(|s| workflows::ToolCallRef::cap_str(s));
                out.push(workflows::ToolCallRef {
                    is_error: errored.contains(&id),
                    panel: crate::agent::session::panel_for_tool(&name).map(str::to_string),
                    tool_call_id: id,
                    tool_name: name,
                    args,
                    result,
                });
            }
        }
    }
    out
}

/// Extract the accumulated reasoning (thinking + visible text) from the subagent's assistant
/// messages so the durable workflow step carries it after reload — making the graph the single
/// source of truth for reasoning, not only a live view. Joins all `thinking` blocks (the
/// model's private reasoning) then all `text` blocks (the visible narration) across every
/// assistant turn, in order. Returns None if no reasoning was produced (so the field stays
/// absent, not an empty string).
fn thinking_from_messages(messages: &[serde_json::Value]) -> Option<String> {
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    for m in messages {
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in content {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("thinking") => {
                    if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                        if !t.trim().is_empty() {
                            thinking_parts.push(t.to_string());
                        }
                    }
                }
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if !t.trim().is_empty() {
                            text_parts.push(t.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    if !thinking_parts.is_empty() {
        out.push_str(&thinking_parts.join("\n\n"));
    }
    if !text_parts.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n— narration —\n\n");
        }
        out.push_str(&text_parts.join("\n\n"));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Drive a workflow run to completion.
///
/// The run must already be created (via `workflows::create`) and in `pending` or `running`
/// state. This function:
/// 1. Marks the run started (`workflows::start`).
/// 2. Loops: find `ready` steps → spawn subagent tasks (bounded by Semaphore) → wait for
///    all in-flight → record results via `workflows::step_state` → repeat.
/// 3. Returns the final run (status `done`, `error`, or `aborted`).
///
/// The function is idempotent-safe: if the run is already terminal, it returns immediately.
/// Concurrent calls on the same run are NOT serialized — the caller must ensure only one
/// executor drives a run at a time (the REST endpoint does this by returning the running
/// run without re-entering).
pub async fn run_workflow(run_id: &str) -> Option<workflows::WorkflowRun> {
    // Snapshot the run. If it is already terminal, return immediately without starting.
    {
        let run = workflows::get_active(run_id)?;
        if run.status == "done" || run.status == "error" || run.status == "aborted" {
            return Some(run);
        }
    }

    // Mark started (emits workflow_start event).
    let started = workflows::start(run_id)?;
    // Initialize the reproducible run record so every captured step lands in
    // one persistent file the operator can inspect / replay later.
    run_record::init_run(&started);

    // Create the shared context bus for this run. Subagents inherit it so scout findings,
    // planner plans, and reviewer gap-lists flow between steps as structured data.
    let bus = ContextBus::create(run_id);

    // ---- Resume: pre-populate the bus from completed-step outputs ----
    // On server restart the in-memory bus is gone (it lived only in the prior process).
    // The executor's auto-population path (bottom of the loop) only writes outputs for
    // steps that complete during THIS session. Without preloading, a resumed run's
    // downstream steps lose every prior agent's findings/plans/gap-lists — the
    // subagent reads an empty bus and starts from scratch. Replay every terminal
    // step's output onto the bus so the resumed run sees the same context a
    // non-interrupted run would have.
    if let Some(run) = workflows::get_active(run_id) {
        let has_completed = run
            .steps
            .iter()
            .any(|s| s.status == "done" || s.status == "error");
        if has_completed {
            bus.preload_from_run(&run);
        }
    }

    let sem = Arc::new(Semaphore::new(concurrency()));
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();

    // ---- Budget tracking ----
    // Track cumulative cost/tokens across all steps. When the run's budget is set,
    // we check after each step completes; if the cumulative spend exceeds the cap,
    // remaining ready steps are skipped and the run is aborted.
    let mut cumulative_cost: f64;
    let mut cumulative_input_tokens: u64;
    let mut cumulative_output_tokens: u64;
    // Whether the near-limit (headroom) warning has already been emitted for this
    // run. The executor polls in a tight loop, so without this guard the warning
    // would fire on every iteration once cumulative spend crosses 80% — flooding
    // the event stream. Emit once, then let the hard-abort path take over at 100%.
    let mut budget_near_limit_warned = false;

    loop {
        // Snapshot the run's current state.
        let run = match workflows::get_active(run_id) {
            Some(r) => r,
            None => return None, // run was removed from active map
        };

        // Terminal → done.
        if run.status == "done" || run.status == "error" || run.status == "aborted" {
            return Some(run);
        }

        // ---- Budget accounting (top of loop) ----
        // Tally cumulative spend from all steps that have usage data. This drives
        // the budget check below BEFORE any new steps are dispatched.
        cumulative_cost = 0.0;
        cumulative_input_tokens = 0;
        cumulative_output_tokens = 0;
        for step in &run.steps {
            if let Some(ref usage) = step.usage {
                cumulative_cost += usage.cost.unwrap_or(0.0);
                cumulative_input_tokens += usage.input.unwrap_or(0.0) as u64;
                cumulative_output_tokens += usage.output.unwrap_or(0.0) as u64;
            }
        }

        // Collect ready steps (status == "ready" or "interrupted").
        // "interrupted" steps are in-flight steps that were running when the server
        // shut down — they must be re-dispatched to complete the run. If the run
        // budget is already exhausted, skip all remaining ready steps and abort
        // the run — prevents a fan-out from burning the balance.
        // Collect ready steps with their agent, task, and optional model override.
        // The WorkflowStep model field takes precedence over the agent's default;
        // the executor passes it to run_single_agent_with_bus which forwards it
        // to the provider. The stored step carries the model so a rerun with a
        // different model (picked from the UI node drawer) uses the chosen model.
        let ready_steps: Vec<ReadyStep> = run
            .steps
            .iter()
            .filter(|s| s.status == "ready" || s.status == "interrupted")
            .map(|s| ReadyStep {
                id: s.id.clone(),
                agent: s.agent.clone(),
                task: s.task.clone(),
                model: s.model.clone(),
                cwd: s.cwd.clone(),
            })
            .collect();

        // Budget check: if cumulative spend exceeds the run budget, skip all ready
        // steps and abort the run.
        if let Some(ref budget) = run.budget {
            if !budget.is_unbounded() {
                // Headroom signal: emit a near-limit warning once when cumulative
                // spend crosses 80% of the most-consumed budget dimension. This is
                // the prerequisite for the intended downgrade-before-abort behavior
                // — the runtime can only hard-abort at 100%, so the operator (and a
                // future auto-downgrade path) needs an earlier signal to act while
                // there is still headroom. `fraction_used` returns 0.0 when
                // unbounded, but we already gated on `!is_unbounded()` above.
                let fraction = budget.fraction_used(
                    cumulative_cost,
                    cumulative_input_tokens,
                    cumulative_output_tokens,
                );
                if !budget_near_limit_warned && fraction >= 0.8 {
                    budget_near_limit_warned = true;
                    workflows::emit_event(
                        run_id,
                        serde_json::json!({
                            "type": "budget_near_limit",
                            "fractionUsed": fraction,
                            "cost": cumulative_cost,
                            "inputTokens": cumulative_input_tokens,
                            "outputTokens": cumulative_output_tokens,
                            "budget": serde_json::to_value(budget)
                                .unwrap_or(serde_json::Value::Null),
                        }),
                    );
                }

                if budget.is_exceeded(
                    cumulative_cost,
                    cumulative_input_tokens,
                    cumulative_output_tokens,
                ) {
                    // Skip every ready step and abort the run.
                    for step in &ready_steps {
                        let _ = workflows::step_state(
                            run_id,
                            &step.id,
                            workflows::StepPatch {
                                status: Some("skipped".into()),
                                error: Some("run budget exceeded".into()),
                                ..Default::default()
                            },
                        );
                    }
                    // Mark the run aborted.
                    workflows::abort(run_id);
                    return workflows::get_active(run_id);
                }
            }
        }

        // If no ready steps and no running steps, the run is stuck (all remaining are
        // pending on a step that will never complete — e.g. a cycle that slipped past
        // validation, or a crashed in-flight task). Mark errored so the operator sees
        // the failure instead of a silently-hung run.
        if ready_steps.is_empty() {
            let any_running = run.steps.iter().any(|s| s.status == "running");
            if !any_running {
                // All steps are either terminal or pending-on-nothing. Mark the run errored
                // with whatever error the last step reported, or a generic stuck message.
                let last_error = run
                    .steps
                    .iter()
                    .rev()
                    .find(|s| s.error.is_some())
                    .and_then(|s| s.error.clone())
                    .unwrap_or_else(|| "run stuck: no ready or running steps".to_string());

                let _ = workflows::step_state(
                    run_id,
                    run.steps
                        .iter()
                        .find(|s| s.status == "pending")
                        .map(|s| s.id.as_str())
                        .unwrap_or(""),
                    workflows::StepPatch {
                        status: Some("error".into()),
                        error: Some(last_error),
                        ..Default::default()
                    },
                );
                return workflows::get_active(run_id);
            }
            // Steps are still running — wait a bit and re-poll.
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        // Spawn each ready step as a subagent task. The Semaphore bounds concurrency.
        let mut handles = Vec::new();
        for step in ready_steps {
            let step_id = step.id;
            let agent = step.agent;
            let task = step.task;
            let model = step.model;
            let step_cwd = step.cwd;
            // Mark the step as "running" so it won't be picked up again.
            let _ = workflows::step_state(
                run_id,
                &step_id,
                workflows::StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            );

            let sem = sem.clone();
            // Per-step cwd (the bridge sets it to e.g. a pantheon episode dir); else the run's cwd.
            let cwd = step_cwd.unwrap_or_else(|| cwd.clone());
            let rid = run_id.to_string();
            let sid = step_id.clone();
            let bus = bus.clone();
            // Per-step budget enforcement: if the step has its own budget, the
            // executor will check it after the step completes (the step's usage
            // is compared against its budget in the post-completion patch below).
            let model_override = model.clone();
            // Capture whether this step is a "worker" so we know to attach a
            // git-diff artifact on completion. The agent name convention is
            // "worker" for code-producing steps and "reviewer" / "scout" /
            // "planner" for non-editing steps.
            let is_worker = agent.contains("worker");
            let agent_name = agent.clone();
            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore not closed");
                let result = tokio::time::timeout(
                    step_timeout(),
                    run_single_agent_with_bus(
                        &agent_name,
                        &task,
                        model_override.as_deref(),
                        &cwd,
                        Some(&bus),
                        Some(&sid),
                    ),
                )
                .await;

                // Keep the SingleResult (if any) so the run record can capture
                // the full provider response stream, skill set, and effective
                // model. The executor-level timeout arm yields None.
                let single: Option<SingleResult> = result.ok();
                let (status, output, error) = match &single {
                    Some(result) => {
                        if result.is_failed() {
                            (
                                "error".to_string(),
                                Some(result.result_output()),
                                Some(result.result_output()),
                            )
                        } else {
                            ("done".to_string(), Some(result.final_output()), None)
                        }
                    }
                    None => {
                        // Timeout.
                        (
                            "error".to_string(),
                            None,
                            Some(format!("step '{sid}' timed out after {:?}", step_timeout())),
                        )
                    }
                };

                // For worker steps that completed successfully, capture the
                // working-tree diff as the step's primary artifact. This makes
                // "the result" a concrete, reviewable change in the UI node
                // drawer — not a prose summary the operator cross-references.
                // Non-worker steps (scout/planner/reviewer) produce no diff;
                // the artifact stays None and the UI falls back to `output`.
                let artifact = if is_worker && status == "done" {
                    git_diff_artifact(&cwd)
                } else {
                    None
                };

                // Durable per-step tool list (panel-tagged) → the graph renders each as a chip on
                // this node, surviving reload. Live chips also stream in via `step_tool` mid-run.
                let tool_calls = single
                    .as_ref()
                    .map(|r| tool_calls_from_messages(&r.messages))
                    .filter(|v| !v.is_empty());

                // Durable reasoning: extract accumulated thinking + narration from the subagent's
                // assistant messages so the graph node carries reasoning after reload, not only
                // live via step_thinking events. Capped to keep the store bounded.
                let thinking = single
                    .as_ref()
                    .and_then(|r| thinking_from_messages(&r.messages))
                    .map(|s| workflows::ToolCallRef::cap_str(&s));

                // Record the result via step_state (propagates readiness, handles auto-repair,
                // cascades errors, finishes the run).
                let _ = workflows::step_state(
                    &rid,
                    &sid,
                    workflows::StepPatch {
                        status: Some(status),
                        output,
                        error,
                        artifact,
                        tool_calls,
                        thinking,
                        ..Default::default()
                    },
                );
                // Capture the full reproducible step record (prompt + model +
                // thinking + tools + skill set + provider response messages) so
                // the run is debuggable offline and bisectable via replay. Best-
                // effort: a recording failure must never break the run.
                run_record::capture_step(&rid, &sid, single.as_ref());
            });
            handles.push(handle);
        }

        // Wait for all in-flight tasks to complete before re-polling. This bounds the
        // concurrency window: new `ready` steps opened by `step_state` propagation (e.g.
        // auto-repair children) will be picked up in the next loop iteration.
        for h in handles {
            let _ = h.await;
        }

        // Auto-populate the bus from completed steps' outputs so the NEXT batch of
        // ready steps can read them via context_read without parsing raw text. We
        // write under `step:<id>:output` (full text) and `step:<id>:summary` (first
        // line) so downstream agents can choose granularity.
        //
        // Also backfill the run record for any step that reached a terminal
        // state WITHOUT going through the spawn path above — most importantly
        // siblings skipped by an error sweep (a failed parent cascades `skipped`
        // to its children inside `step_state`, so the executor never dispatched
        // them and never captured their record). Without this backfill, a
        // replay's DAG would be missing those skipped nodes, and the record
        // would not faithfully reproduce the run's shape. These steps had no
        // provider call, so they get a `result = None` entry; `build_step_record`
        // derives a truthful `agent_source`/`stop_reason` from the step's actual
        // status (`skipped` vs `error`/timeout).
        if let Some(run) = workflows::get_active(run_id) {
            let recorded: std::collections::HashSet<String> = run_record::load(run_id)
                .map(|r| r.steps.into_iter().map(|s| s.step_id).collect())
                .unwrap_or_default();
            for step in &run.steps {
                let terminal =
                    step.status == "done" || step.status == "error" || step.status == "skipped";
                if terminal && !recorded.contains(&step.id) {
                    run_record::capture_step(run_id, &step.id, None);
                }
                if step.status == "done" || step.status == "error" {
                    if let Some(ref out) = step.output {
                        bus.write(
                            &format!("step:{}:output", step.id),
                            serde_json::Value::String(out.clone()),
                        );
                        let summary = out.lines().next().unwrap_or("").to_string();
                        bus.write(
                            &format!("step:{}:summary", step.id),
                            serde_json::Value::String(summary),
                        );
                    }
                }
            }
        }
    }
}

// The execute handler is wired into `workflows::router()` directly (see the
// `execute_handler` function in workflows.rs). This module exposes `run_workflow`
// for the REST layer and tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Budget;
    use crate::workflows::{self, CreateStepInput};
    use serde_json::{Value, json};
    use uuid::Uuid;

    // tokio Mutex (not std): several tests below intentionally hold this guard across `.await`
    // (run_workflow reads the process-global env vars mid-execution), so the lock MUST serialize
    // the whole async test body. An async-aware mutex is the correct type to hold across await;
    // the sync `#[test]` cases use `blocking_lock()` since they run outside a runtime.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
            cwd: None,
        }
    }

    /// `tool_calls_from_messages` must capture capped `args` (from the toolCall block) AND
    /// `result` (the matching tool-message text), so the graph drawer is the inspectable record
    /// of what each tool did — not just a colored chip. Verifies the join by tool-call id and
    /// that a non-errored tool result is not flagged.
    #[test]
    fn tool_calls_from_messages_captures_args_and_result() {
        let messages = vec![
            json!({"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls -la"}}]}),
            json!({"role":"tool","toolCallId":"call_1","content":[{"type":"text","text":"total 0\ndrwxr-xr-x 2 root root 40 Jul 9 12:00 ."}]}),
        ];
        let refs = tool_calls_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        let tc = &refs[0];
        assert_eq!(tc.tool_call_id, "call_1");
        assert_eq!(tc.tool_name, "bash");
        assert!(!tc.is_error, "non-error tool result must not be flagged");
        assert_eq!(
            tc.args.as_deref(),
            Some(r#"{"command":"ls -la"}"#),
            "args must be the JSON-serialized arguments"
        );
        assert_eq!(
            tc.result.as_deref(),
            Some("total 0\ndrwxr-xr-x 2 root root 40 Jul 9 12:00 ."),
            "result must be the joined tool-message text"
        );
    }

    /// An errored tool result (`isError:true` on the tool message) must surface as
    /// `is_error: true` on the ToolCallRef, with the error text captured as `result`.
    #[test]
    fn tool_calls_from_messages_flags_errored_result() {
        let messages = vec![
            json!({"role":"assistant","content":[{"type":"toolCall","id":"c2","name":"read","arguments":{"path":"/nope"}}]}),
            json!({"role":"tool","toolCallId":"c2","isError":true,"content":[{"type":"text","text":"no such file"}]}),
        ];
        let refs = tool_calls_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert!(refs[0].is_error, "isError tool message must flag the ref");
        assert_eq!(refs[0].result.as_deref(), Some("no such file"));
    }

    /// `ToolCallRef::cap_str` truncates on a UTF-8 char boundary (never panics on multibyte)
    /// and appends the marker once. Verifies the bound + marker for an over-CAP input and a
    /// no-op for an under-CAP input (so small args/results are unchanged).
    #[test]
    fn tool_call_ref_cap_str_truncates_on_char_boundary() {
        let under = "small";
        assert_eq!(workflows::ToolCallRef::cap_str(under), "small");

        let big = "a".repeat(workflows::ToolCallRef::CAP + 200);
        let capped = workflows::ToolCallRef::cap_str(&big);
        assert!(capped.len() <= workflows::ToolCallRef::CAP + "…[truncated]".len());
        assert!(
            capped.ends_with("…[truncated]"),
            "over-cap input must end with the truncation marker"
        );

        let multi = "🦀".repeat(workflows::ToolCallRef::CAP);
        let capped_multi = workflows::ToolCallRef::cap_str(&multi);
        assert!(capped_multi.len() <= workflows::ToolCallRef::CAP + "…[truncated]".len());
        assert!(capped_multi.is_char_boundary(capped_multi.len()));
    }

    /// `thinking_from_messages` extracts the accumulated reasoning (thinking + narration) from
    /// assistant messages so the durable step carries it after reload — making the graph the
    /// single source of truth for reasoning, not only a live view. Returns None when no
    /// reasoning was produced so the field stays absent rather than empty.
    #[test]
    fn thinking_from_messages_extracts_reasoning() {
        let messages = vec![
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"planning the ls"},{"type":"text","text":"I'll list files."}]}),
            json!({"role":"tool","toolCallId":"c1","content":[{"type":"text","text":"a\nb"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"Found 2 files."}]}),
        ];
        let t = thinking_from_messages(&messages).expect("reasoning present");
        assert!(
            t.contains("planning the ls"),
            "thinking block extracted: {t}"
        );
        assert!(
            t.contains("I'll list files."),
            "first narration extracted: {t}"
        );
        assert!(
            t.contains("Found 2 files."),
            "second narration extracted: {t}"
        );
        assert!(
            t.contains("— narration —"),
            "thinking + narration separated: {t}"
        );
    }

    /// No reasoning → None (so the durable field stays absent, not an empty string).
    #[test]
    fn thinking_from_messages_returns_none_when_empty() {
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
            json!({"role":"tool","toolCallId":"c1","content":[{"type":"text","text":"ok"}]}),
        ];
        assert_eq!(thinking_from_messages(&messages), None);
    }

    fn set_tmp_workflows_file() -> std::path::PathBuf {
        let file = std::env::temp_dir().join(format!("dotz-wf-exec-test-{}.json", Uuid::new_v4()));
        std::env::set_var("DOTZ_WORKFLOWS_FILE", file.to_string_lossy().to_string());
        file
    }

    /// The concurrency pool must clamp to sane bounds. A zero or negative value would
    /// deadlock the Semaphore; an enormous value would spawn unbounded LLM streams.
    #[test]
    fn concurrency_clamps_to_sane_bounds() {
        // Default when unset.
        std::env::remove_var("DOTZ_WF_CONCURRENCY");
        let c = concurrency();
        assert!(
            (1..=MAX_CONCURRENCY).contains(&c),
            "default concurrency out of bounds: {c}"
        );

        // Valid override is preserved.
        std::env::set_var("DOTZ_WF_CONCURRENCY", "2");
        assert_eq!(concurrency(), 2);

        // Below-min clamps to 1.
        std::env::set_var("DOTZ_WF_CONCURRENCY", "0");
        assert_eq!(concurrency(), 1);

        // Above-max clamps to MAX_CONCURRENCY.
        std::env::set_var("DOTZ_WF_CONCURRENCY", "99999");
        assert_eq!(concurrency(), MAX_CONCURRENCY);

        // Invalid string falls back to default.
        std::env::set_var("DOTZ_WF_CONCURRENCY", "not-a-number");
        let c = concurrency();
        assert!((1..=MAX_CONCURRENCY).contains(&c));

        std::env::remove_var("DOTZ_WF_CONCURRENCY");
    }

    /// The step timeout must clamp to sane bounds. A zero value would time out before
    /// the provider stream starts; an enormous value defeats the purpose of the cap.
    ///
    /// Takes ENV_LOCK like every other DOTZ_WF_STEP_TIMEOUT_MS test in this file: this test
    /// mutates the process-global env var, and without the lock it races under the parallel
    /// suite against the other tests below that set DOTZ_WF_STEP_TIMEOUT_MS=5000 — observed
    /// as a flaky `left: 5, right: 300` failure (read back another test's 5000ms instead of
    /// the unset default) when this test ran unlocked.
    #[test]
    fn step_timeout_clamps_to_sane_bounds() {
        let _guard = ENV_LOCK.blocking_lock();
        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        let t = step_timeout();
        assert_eq!(t.as_secs(), 300, "default step timeout is 5 minutes");

        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        assert_eq!(step_timeout().as_millis(), 5000);

        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "50");
        assert_eq!(step_timeout().as_millis(), 1000, "below-min clamps to 1s");

        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "100000000");
        assert_eq!(
            step_timeout().as_millis(),
            3_600_000,
            "above-max clamps to 1h"
        );

        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
    }

    /// run_workflow must return None for an unknown run id.
    #[tokio::test]
    async fn run_workflow_returns_none_for_unknown_run() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        let result = run_workflow("no-such-run-id").await;
        assert!(result.is_none());
    }

    /// run_workflow must return the run immediately if it is already terminal.
    #[tokio::test]
    async fn run_workflow_returns_immediately_for_terminal_run() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        let run = workflows::create(
            None,
            None,
            "terminal".into(),
            None,
            3,
            &[step("a", "A", None)],
            None,
        )
        .unwrap();
        // Abort makes the run terminal.
        let _ = workflows::abort(&run.id);
        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().status, "aborted");
    }

    /// A single-step run with an unknown agent must complete with error status (the
    /// subagent returns an error for unknown agents, and step_state records it).
    #[tokio::test]
    async fn run_workflow_marks_unknown_agent_step_as_error() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        // Keep the timeout short so the test doesn't wait 5 minutes.
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        let run = workflows::create(
            None,
            None,
            "unknown-agent".into(),
            None,
            3,
            &[step("dotz-does-not-exist", "do something", None)],
            None,
        )
        .unwrap();

        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        let run = result.unwrap();
        assert_eq!(
            run.status, "error",
            "run should be errored: {:?}",
            run.steps
        );
        assert_eq!(run.steps[0].status, "error");
        assert!(
            run.steps[0]
                .error
                .as_ref()
                .map(|e| e.contains("Unknown agent"))
                .unwrap_or(false),
            "error should mention unknown agent: {:?}",
            run.steps[0].error
        );

        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    /// A two-step DAG where the first step fails must skip the second step (failure-rerouting).
    #[tokio::test]
    async fn run_workflow_skips_children_of_failed_step() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        let run = workflows::create(
            None,
            None,
            "fail-cascade".into(),
            None,
            3,
            &[
                step("dotz-fail-step-1", "first", None),
                step("dotz-fail-step-2", "second", Some(vec![json!(0)])),
            ],
            None,
        )
        .unwrap();

        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        let run = result.unwrap();
        assert_eq!(run.status, "error");
        // Both steps should be terminal: first is error, second is skipped.
        assert_eq!(run.steps[0].status, "error");
        assert_eq!(run.steps[1].status, "skipped");

        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    /// The context bus must auto-populate `step:<id>:output` and `step:<id>:summary`
    /// entries for completed steps, so a downstream step can read prior results via
    /// context_read without parsing raw text.
    #[tokio::test]
    async fn context_bus_auto_populates_from_completed_steps() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        // Two-step chain: step 0 is an unknown agent (errors), step 1 depends on step 0.
        // The bus should contain step 0's output after the run completes.
        let run = workflows::create(
            None,
            None,
            "bus-pop".into(),
            None,
            3,
            &[
                step("dotz-bus-step-a", "alpha", None),
                step("dotz-bus-step-b", "beta", Some(vec![json!(0)])),
            ],
            None,
        )
        .unwrap();

        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        let run = result.unwrap();

        // Step 0 errored (unknown agent), step 1 was skipped.
        assert_eq!(run.steps[0].status, "error");
        assert_eq!(run.steps[1].status, "skipped");

        // The executor creates the bus internally; read from it after the run.
        // Auto-population happens inside the executor loop.
        let bus = ContextBus {
            run_id: run.id.clone(),
        };

        // The bus must contain step 0's output and summary.
        let step0_output_key = format!("step:{}:output", run.steps[0].id);
        let step0_summary_key = format!("step:{}:summary", run.steps[0].id);
        assert!(
            bus.read(&step0_output_key).is_some(),
            "bus should auto-populate step output key"
        );
        let summary = bus.read(&step0_summary_key);
        assert!(
            summary.is_some(),
            "bus should auto-populate step summary key"
        );
        // Summary must be a single line (first line of the output).
        let summary_str = summary.unwrap().as_str().unwrap().to_string();
        assert!(
            !summary_str.contains('\n'),
            "summary should be a single line"
        );

        // Clean up.
        ContextBus::destroy(&run.id);
        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    /// The context bus must survive run completion so callers can inspect it. The
    /// caller is responsible for destroying it explicitly (bounded: one bus per run).
    #[tokio::test]
    async fn context_bus_persists_after_run_completion() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        let run = workflows::create(
            None,
            None,
            "bus-persist".into(),
            None,
            3,
            &[step("dotz-bus-persist", "do stuff", None)],
            None,
        )
        .unwrap();

        let run_id = run.id.clone();
        let result = run_workflow(&run_id).await;
        assert!(result.is_some());

        // The bus persists after completion — caller inspects, then destroys.
        let bus = ContextBus {
            run_id: run_id.clone(),
        };
        assert!(
            !bus.read_all().is_empty(),
            "bus should persist and contain step data after run completion"
        );
        ContextBus::destroy(&run_id);
        assert!(
            bus.read_all().is_empty(),
            "after destroy, bus should be empty"
        );

        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    /// On resume, the executor must pre-populate the fresh context bus from
    /// completed steps' outputs so downstream subagents see prior agent data
    /// (scout findings, planner plans, reviewer gap-lists) — the same data
    /// they would have seen without a restart. Without this, a resumed run's
    /// worker step reads an empty bus and starts from scratch.
    #[tokio::test]
    async fn run_workflow_preloads_context_bus_on_resume() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        // Two-step chain: step 0 (unknown agent) will error, step 1 depends on step 0.
        // After step 0 errors, step 1 is skipped. The bus should contain step 0's
        // output after the run completes.
        let run = workflows::create(
            None,
            None,
            "resume-bus-prepop".into(),
            None,
            3,
            &[
                step("dotz-bus-step-0", "alpha", None),
                step("dotz-bus-step-1", "beta", Some(vec![json!(0)])),
            ],
            None,
        )
        .unwrap();

        let run_id = run.id.clone();
        let step0_id = run.steps[0].id.clone();

        // Manually complete step 0 with output (simulating a step that finished
        // before the server shut down).
        let run = workflows::start(&run_id).unwrap();
        let _ = workflows::step_state(
            &run_id,
            &step0_id,
            workflows::StepPatch {
                status: Some("done".into()),
                output: Some("scout found 42 files in src/".into()),
                ..Default::default()
            },
        );

        // Step 1 should now be ready (parent done). Complete it too.
        let step1_id = run.steps[1].id.clone();
        let run = workflows::get_active(&run_id).unwrap();
        assert_eq!(run.steps[1].status, "ready");
        let _ = workflows::step_state(
            &run_id,
            &step1_id,
            workflows::StepPatch {
                status: Some("done".into()),
                output: Some("plan complete".into()),
                ..Default::default()
            },
        );

        // Run is now done. Destroy the bus (simulates shutdown).
        ContextBus::destroy(&run_id);

        // Now create a NEW run with TWO steps: step 0 completes before
        // shutdown, step 1 was running at shutdown (interrupted). This is
        // the real resume scenario: some steps finished, some were in-flight.
        let run2 = workflows::create(
            None,
            None,
            "resume-bus-prepop-2".into(),
            None,
            3,
            &[
                step("dotz-bus-step-resume-done", "done task", None),
                step(
                    "dotz-bus-step-resume-running",
                    "running task",
                    Some(vec![json!(0)]),
                ),
            ],
            None,
        )
        .unwrap();
        let run2_id = run2.id.clone();
        let run2_step0_id = run2.steps[0].id.clone();
        let run2_step1_id = run2.steps[1].id.clone();

        // Mark step 0 as done with output, step 1 as running, then mark
        // interrupted (simulates shutdown mid-flight).
        let _run2 = workflows::start(&run2_id).unwrap();
        let _ = workflows::step_state(
            &run2_id,
            &run2_step0_id,
            workflows::StepPatch {
                status: Some("done".into()),
                output: Some("pre-shutdown scout results".into()),
                ..Default::default()
            },
        );
        // Mark step 1 as running (simulates the executor picking it up).
        let _ = workflows::step_state(
            &run2_id,
            &run2_step1_id,
            workflows::StepPatch {
                status: Some("running".into()),
                ..Default::default()
            },
        );
        // Mark interrupted (this is what startup_resume does) — step 1
        // becomes interrupted, step 0 stays done.
        let _ = workflows::mark_interrupted(&run2_id);

        // Destroy the bus (simulates shutdown destroying the in-memory bus).
        ContextBus::destroy(&run2_id);

        // Call run_workflow — this is what resume() does. The executor should
        // pre-populate the bus from the completed step's output.
        let result = run_workflow(&run2_id).await;
        assert!(result.is_some());

        // The bus must contain step 0's output (completed before shutdown)
        // AND step 1's key must NOT exist (it was interrupted, not done).
        let bus = ContextBus {
            run_id: run2_id.clone(),
        };
        let output_key = format!("step:{}:output", run2_step0_id);
        let loaded = bus.read(&output_key);
        assert!(
            loaded.is_some(),
            "bus should contain completed step output after resume"
        );
        assert_eq!(
            loaded.unwrap().as_str().unwrap(),
            "pre-shutdown scout results",
            "preloaded output must match the step's persisted output"
        );

        // The summary key must also be present.
        let summary_key = format!("step:{}:summary", run2_step0_id);
        let summary = bus.read(&summary_key);
        assert!(
            summary.is_some(),
            "bus should contain completed step summary after resume"
        );
        assert_eq!(
            summary.unwrap().as_str().unwrap(),
            "pre-shutdown scout results",
            "preloaded summary must be the first line of the output"
        );

        // Step 1 was interrupted, then re-executed by the executor, so its
        // output IS on the bus now (from the executor's auto-population).
        // The key assertion is that step 0's preloaded output (from BEFORE
        // shutdown) is present — that's what the resume scenario needs.
        let step1_key = format!("step:{}:output", run2_step1_id);
        assert!(
            bus.read(&step1_key).is_some(),
            "step 1 was re-executed and auto-populated by the executor"
        );

        // Clean up.
        ContextBus::destroy(&run_id);
        ContextBus::destroy(&run2_id);
        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    // ---- Budget enforcement tests ----

    /// A run-level budget must cause remaining ready steps to be skipped once the
    /// cumulative cost exceeds the cap. We set a tiny budget ($0.001) and a single
    /// step that will exceed it (the unknown-agent step incurs some cost tracking
    /// even on error). Since unknown-agent steps don't actually call a provider,
    // we test the budget-skip logic directly: create a run with a budget, manually
    /// simulate spend by completing one step with usage, then verify the next
    /// ready step is skipped.
    #[tokio::test]
    async fn run_budget_exceeded_skips_remaining_steps() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();
        std::env::set_var("DOTZ_WF_STEP_TIMEOUT_MS", "5000");
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "4000");

        // Two independent steps (no parent-child). Step 0 will complete with
        // usage exceeding the tiny budget; step 1 should then be skipped.
        let run = workflows::create(
            None,
            None,
            "budget-skip".into(),
            None,
            3,
            &[
                step("dotz-budget-a", "task a", None),
                step("dotz-budget-b", "task b", None),
            ],
            Some(Budget {
                max_cost: Some(0.0001),
                max_tokens: None,
                max_input_tokens: None,
            }),
        )
        .unwrap();

        // Manually set step 0 to "done" with usage that exceeds the tiny budget.
        // Using "done" (not "error") avoids the error-sweep that would mark step 1
        // as skipped before the budget check fires.
        let run = workflows::start(&run.id).unwrap();
        let step0_id = run.steps[0].id.clone();
        let step1_id = run.steps[1].id.clone();

        let _ = workflows::step_state(
            &run.id,
            &step0_id,
            workflows::StepPatch {
                status: Some("done".into()),
                output: Some("step 0 output".into()),
                usage: Some(workflows::Usage {
                    input: Some(100.0),
                    output: Some(50.0),
                    cost: Some(1.0), // $1.00 >> $0.0001 budget
                    turns: Some(1.0),
                }),
                ..Default::default()
            },
        );

        // Now the executor loop should see the budget is exceeded and skip step 1.
        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        let run = result.unwrap();

        // Step 1 should be skipped due to budget.
        let step1 = run.steps.iter().find(|s| s.id == step1_id).unwrap();
        assert_eq!(
            step1.status, "skipped",
            "step 1 should be skipped when run budget is exceeded: {:?}",
            step1
        );
        assert!(
            step1.error.as_ref().is_some_and(|e| e.contains("budget")),
            "step 1 error should mention budget: {:?}",
            step1.error
        );

        std::env::remove_var("DOTZ_WF_STEP_TIMEOUT_MS");
        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
    }

    /// A run with no budget set must NOT skip steps even when cumulative spend is high.
    /// This is the regression guard: the budget feature must be opt-in.
    #[tokio::test]
    async fn run_without_budget_never_skips_on_cost() {
        let _guard = ENV_LOCK.lock().await;
        let _file = set_tmp_workflows_file();

        // No budget on the run. Agent name must NOT contain "budget" so we can
        // distinguish the error message from a budget-related skip.
        let run = workflows::create(
            None,
            None,
            "no-budget".into(),
            None,
            3,
            &[step("dotz-nb-step-a", "task a", None)],
            None,
        )
        .unwrap();

        let run_id = run.id.clone();
        let result = run_workflow(&run_id).await;
        assert!(result.is_some());
        let run = result.unwrap();

        // The step should error (unknown agent) but NOT be skipped due to budget.
        assert_eq!(run.steps[0].status, "error");
        assert!(
            run.steps[0]
                .error
                .as_ref()
                .is_some_and(|e| !e.contains("budget")),
            "step should not mention budget when no budget is set: {:?}",
            run.steps[0].error
        );
    }

    /// Budget fields on a Budget must deserialize correctly from JSON.
    #[test]
    fn budget_deserialize_from_json() {
        let json = r#"{"maxCost": 1.5, "maxTokens": 100000, "maxInputTokens": 50000}"#;
        let budget: Budget = serde_json::from_str(json).unwrap();
        assert_eq!(budget.max_cost, Some(1.5));
        assert_eq!(budget.max_tokens, Some(100_000));
        assert_eq!(budget.max_input_tokens, Some(50_000));
    }

    /// Budget JSON with only some fields set must deserialize correctly (missing
    /// fields become None = no limit).
    #[test]
    fn budget_deserialize_partial() {
        let json = r#"{"maxCost": 2.0}"#;
        let budget: Budget = serde_json::from_str(json).unwrap();
        assert_eq!(budget.max_cost, Some(2.0));
        assert!(budget.max_tokens.is_none());
        assert!(budget.max_input_tokens.is_none());
    }

    /// A workflow run created with a budget must persist it (round-trip through
    /// the store and back).
    #[test]
    fn run_budget_round_trips_through_store() {
        let _guard = ENV_LOCK.blocking_lock();
        let _file = set_tmp_workflows_file();
        let budget = Budget {
            max_cost: Some(10.0),
            max_tokens: Some(50_000),
            ..Default::default()
        };
        let run = workflows::create(
            None,
            None,
            "budget-roundtrip".into(),
            None,
            3,
            &[step("a", "task", None)],
            Some(budget.clone()),
        )
        .unwrap();

        // The budget must be on the run.
        assert_eq!(run.budget, Some(budget));

        // Re-fetch from the active store — it must still be there.
        let fetched = workflows::get_active(&run.id).unwrap();
        assert!(fetched.budget.is_some());
        assert_eq!(fetched.budget.unwrap().max_cost, Some(10.0));
    }
}
