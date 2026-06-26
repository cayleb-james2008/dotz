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
use crate::agent::subagent::run_single_agent_public;
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

    let sem = Arc::new(Semaphore::new(concurrency()));
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();

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

        // Collect ready steps (status == "ready").
        let ready_ids: Vec<(String, String, String)> = run
            .steps
            .iter()
            .filter(|s| s.status == "ready")
            .map(|s| (s.id.clone(), s.agent.clone(), s.task.clone()))
            .collect();

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
        for (step_id, agent, task) in ready_ids {
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
            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore not closed");
                let result = tokio::time::timeout(
                    step_timeout(),
                    run_single_agent_public(&agent, &task, None, &cwd),
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
                            Some(format!(
                                "step '{sid}' timed out after {:?}",
                                step_timeout()
                            )),
                        )
                    }
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
    }
}

/// The execute handler is wired into `workflows::router()` directly (see the
/// `execute_handler` function in workflows.rs). This module exposes `run_workflow`
/// for the REST layer and tests.

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn set_tmp_workflows_file() -> std::path::PathBuf {
        let file =
            std::env::temp_dir().join(format!("dotz-wf-exec-test-{}.json", Uuid::new_v4()));
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
        assert!(c >= 1 && c <= MAX_CONCURRENCY, "default concurrency out of bounds: {c}");

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
        assert_eq!(step_timeout().as_millis(), 3_600_000, "above-max clamps to 1h");

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
        )
        .unwrap();

        let result = run_workflow(&run.id).await;
        assert!(result.is_some());
        let run = result.unwrap();
        assert_eq!(run.status, "error", "run should be errored: {:?}", run.steps);
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
}
