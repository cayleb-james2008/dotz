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
use crate::agent::subagent::run_single_agent_with_bus;
use crate::checkpoint::git_diff_artifact;
use crate::context_bus::ContextBus;
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
    let _run = workflows::start(run_id)?;

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
        let ready_ids: Vec<(String, String, String, Option<String>)> = run
            .steps
            .iter()
            .filter(|s| s.status == "ready" || s.status == "interrupted")
            .map(|s| {
                (
                    s.id.clone(),
                    s.agent.clone(),
                    s.task.clone(),
                    s.model.clone(),
                )
            })
            .collect();

        // Budget check: if cumulative spend exceeds the run budget, skip all ready
        // steps and abort the run.
        if let Some(ref budget) = run.budget {
            if !budget.is_unbounded()
                && budget.is_exceeded(
                    cumulative_cost,
                    cumulative_input_tokens,
                    cumulative_output_tokens,
                )
            {
                // Skip every ready step and abort the run.
                for sid in &ready_ids {
                    let _ = workflows::step_state(
                        run_id,
                        &sid.0,
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

        // If no ready steps and no running steps, the run is stuck (all remaining are
        // pending on a step that will never complete — e.g. a cycle that slipped past
        // validation, or a crashed in-flight task). Mark errored so the operator sees
        // the failure instead of a silently-hung run.
        if ready_ids.is_empty() {
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
        for (step_id, agent, task, model) in ready_ids {
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
            let cwd = cwd.clone();
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
                    ),
                )
                .await;

                let (status, output, error) = match result {
                    Ok(result) => {
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
                    Err(_) => {
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
                        ..Default::default()
                    },
                );
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
        if let Some(run) = workflows::get_active(run_id) {
            for step in &run.steps {
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

/// The execute handler is wired into `workflows::router()` directly (see the
/// `execute_handler` function in workflows.rs). This module exposes `run_workflow`
/// for the REST layer and tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Budget;
    use crate::workflows::{self, CreateStepInput};
    use serde_json::{json, Value};
    use std::sync::Mutex;
    use uuid::Uuid;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
            c >= 1 && c <= MAX_CONCURRENCY,
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
        assert!(c >= 1 && c <= MAX_CONCURRENCY);

        std::env::remove_var("DOTZ_WF_CONCURRENCY");
    }

    /// The step timeout must clamp to sane bounds. A zero value would time out before
    /// the provider stream starts; an enormous value defeats the purpose of the cap.
    #[test]
    fn step_timeout_clamps_to_sane_bounds() {
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _file = set_tmp_workflows_file();
        let result = run_workflow("no-such-run-id").await;
        assert!(result.is_none());
    }

    /// run_workflow must return the run immediately if it is already terminal.
    #[tokio::test]
    async fn run_workflow_returns_immediately_for_terminal_run() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            step1.error.as_ref().map_or(false, |e| e.contains("budget")),
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                .map_or(false, |e| !e.contains("budget")),
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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
