//! dotz workflow store — port of src/workflows.ts + the /api/workflows routes (server.ts 384-456).
//!
//! First-class WorkflowRun objects that make the implicit subagent orchestration
//! (single/parallel/chain) observable + controllable by the UI. This is the observability/control
//! layer only: it records step DAGs, their statuses, outputs, and usage. Pure persistence, no agent
//! runtime.
//!
//! Self-contained: the in-memory active map lives in a module-level `OnceLock<Mutex<..>>`
//! (mirrors the Node module-singleton `workflowStore`). History persists to JSON at
//! `<dotz_dir>/ai-agents/workflows.json` (same path Node uses), honoring DOTZ_CONFIG_DIR via
//! `crate::config::dotz_dir()`.
//!
//! Resumability: a run that was `running` when the server shut down is persisted as
//! `interrupted` on the next boot. The startup scan (`startup_resume`) marks every
//! in-flight step `interrupted` and re-queues the run so the executor can pick it up
//! where it stopped — the desktop-app lifecycle no longer costs a half-finished task.
//! Callers may also explicitly `POST /api/workflows/:id/resume` to retry an interrupted
//! run after inspecting its state.
use crate::config::dotz_dir;
use crate::types::Budget;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use tokio::sync::broadcast;
use uuid::Uuid;

// ---- types (port of WorkflowStep / WorkflowRun from types.ts, serde camelCase) ----

/// Usage stats from the subagent run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<f64>,
}

/// A node in a workflow run's DAG — one agent executing one task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub agent: String,
    pub task: String,
    /// "pending" | "ready" | "running" | "done" | "error" | "skipped" | "interrupted".
    /// "interrupted" marks a step that was `running` when the server shut down — it was
    /// in-flight and never completed. On resume, the executor re-runs interrupted steps.
    pub status: String,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "sandboxRunId", skip_serializing_if = "Option::is_none")]
    pub sandbox_run_id: Option<String>,
    #[serde(rename = "browserSessionId", skip_serializing_if = "Option::is_none")]
    pub browser_session_id: Option<String>,
    #[serde(rename = "toolCallIds", skip_serializing_if = "Option::is_none")]
    pub tool_call_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// When true, a "done" transition with review findings auto-spawns a repair child + a
    /// re-review grandchild, keeping the run running until the re-review passes or the retry
    /// cap is hit. This makes "verified result" the default landing state for review steps.
    #[serde(rename = "autoRepair", default)]
    pub auto_repair: bool,
    /// The review-round index this step was spawned in (0 = original, 1 = first repair, …).
    /// Surfaced so the UI can render "repair round 2/3" on the step badge.
    #[serde(rename = "repairRound", default)]
    pub repair_round: u32,
    /// Per-step budget: when set, the executor aborts or downgrades the step before it
    /// can exceed the cost/token limits. A step's budget is consumed from the run's
    /// budget pool (when both are set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Cumulative cost actually consumed by this step (set at completion). Surfaced in
    /// the UI brain float so the operator can see per-step spend after a fan-out.
    #[serde(rename = "actualCost", skip_serializing_if = "Option::is_none")]
    pub actual_cost: Option<f64>,
    /// Cumulative total tokens actually consumed by this step (set at completion).
    #[serde(rename = "actualTokens", skip_serializing_if = "Option::is_none")]
    pub actual_tokens: Option<u64>,
    /// The subagent's full conversation history for this step, captured at completion.
    /// Persisted so the UI step-detail drawer can render tool calls / tool results /
    /// thinking blocks, and so a resumed interrupted step can re-inherit prior
    /// conversation context instead of re-executing already-completed tool calls.
    /// None when the step has not yet completed (or was interrupted before the
    /// subagent returned any output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Value>>,
}

/// A workflow run — a DAG of steps, observable by the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub label: String,
    pub steps: Vec<WorkflowStep>,
    /// "pending" | "running" | "done" | "error" | "aborted" | "interrupted".
    /// "interrupted" marks a run that was `running` when the server shut down. It is
    /// resumed on the next boot (or on explicit POST /:id/resume).
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Max repair rounds for auto-repair review cycles (default 3). 0 disables auto-repair.
    #[serde(rename = "maxRepairRounds", default)]
    pub max_repair_rounds: u32,
    /// Current repair-round counter, incremented each time a review step spawns a repair pair.
    #[serde(rename = "repairRounds", default)]
    pub repair_rounds: u32,
    /// Per-run budget: the cap on cumulative cost/tokens across ALL steps in this run.
    /// When the run's cumulative spend exceeds this, remaining ready steps are skipped
    /// and the run is aborted. Prevents a fan-out from burning the provider balance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Cumulative cost actually consumed across all steps so far (set incrementally
    /// as each step completes). Surfaced in the brain float.
    #[serde(rename = "actualCost", skip_serializing_if = "Option::is_none")]
    pub actual_cost: Option<f64>,
    /// Cumulative total tokens consumed across all steps so far.
    #[serde(rename = "actualTokens", skip_serializing_if = "Option::is_none")]
    pub actual_tokens: Option<u64>,
}

// ---- create input (POST body steps) ----

#[derive(Debug, Deserialize)]
pub struct CreateStepInput {
    pub agent: String,
    pub task: String,
    #[serde(default)]
    pub parents: Option<Vec<Value>>,
    #[serde(rename = "sandboxRunId", default)]
    pub sandbox_run_id: Option<String>,
    #[serde(rename = "browserSessionId", default)]
    pub browser_session_id: Option<String>,
    #[serde(rename = "toolCallIds", default)]
    pub tool_call_ids: Option<Vec<String>>,
    #[serde(default)]
    pub thinking: Option<String>,
    /// When true, a "done" transition with review findings auto-spawns a repair cycle.
    #[serde(rename = "autoRepair", default)]
    pub auto_repair: bool,
    /// Per-step budget: cost/token limits for this individual step only. When set, the
    /// executor aborts or downgrades the step before it can exceed these limits.
    #[serde(default)]
    pub budget: Option<Budget>,
}

// ---- module-level store (OnceLock<Mutex<..>>; mirrors the Node module-singleton) ----

/// In-memory active runs, keyed by run id. Bounded by ACTIVE_CAP (terminal runs evicted first).
fn store() -> &'static Mutex<HashMap<String, WorkflowRun>> {
    static STORE: OnceLock<Mutex<HashMap<String, WorkflowRun>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---- live workflow event broadcast (UI graph panel consumes {kind:"workflow", runId, event}) ----

static EVENTS: OnceLock<broadcast::Sender<Value>> = OnceLock::new();

fn events_tx() -> &'static broadcast::Sender<Value> {
    EVENTS.get_or_init(|| broadcast::channel::<Value>(1024).0)
}

/// Subscribe to live workflow events. Every WebSocket fan-out calls this once per connection.
pub fn subscribe_events() -> broadcast::Receiver<Value> {
    events_tx().subscribe()
}

fn emit_event(run_id: &str, event: Value) {
    let _ = events_tx().send(json!({
        "kind": "workflow",
        "runId": run_id,
        "event": event,
    }));
}

fn emit_workflow_start(run: &WorkflowRun) {
    emit_event(&run.id, json!({ "type": "workflow_start", "run": run }));
}

fn emit_workflow_end(run: &WorkflowRun) {
    emit_event(&run.id, json!({ "type": "workflow_end", "run": run }));
}

fn emit_step_state(run_id: &str, step: &WorkflowStep) {
    let mut event = json!({
        "type": "step_state",
        "stepId": step.id,
        "status": step.status,
    });
    if let Some(o) = &step.output {
        event["output"] = json!(o);
    }
    if let Some(e) = &step.error {
        event["error"] = json!(e);
    }
    if let Some(u) = &step.usage {
        event["usage"] = json!(u);
    }
    if let Some(s) = &step.sandbox_run_id {
        event["sandboxRunId"] = json!(s);
    }
    if let Some(b) = &step.browser_session_id {
        event["browserSessionId"] = json!(b);
    }
    if let Some(t) = &step.tool_call_ids {
        event["toolCallIds"] = json!(t);
    }
    if let Some(t) = &step.thinking {
        event["thinking"] = json!(t);
    }
    emit_event(run_id, event);
}

/// Lock the workflow store, recovering from a poisoned lock. A panic while holding the store lock
/// (e.g. inside the workflow bridge or a step-state callback) must not permanently brick the
/// workflow REST endpoints or the `/api/health` active-count read.
fn store_guard() -> std::sync::MutexGuard<'static, HashMap<String, WorkflowRun>> {
    store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const ACTIVE_CAP: usize = 100;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Truncate a string to at most `cap` bytes, respecting UTF-8 boundaries.
fn truncate_bytes(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = s.len() - end;
    format!("{}\n\n[context truncated: {omitted} bytes omitted]", &s[..end])
}

/// Heuristic: does a review step's output contain actionable findings?
/// Matches the reviewer agent's output format (## Critical / ## Warnings sections) and a
/// generic "issues found" pattern. A review that reports no issues (e.g. "No critical issues
/// found" or an empty/clean summary) must NOT trigger a repair cycle.
fn review_has_findings(output: Option<&str>) -> bool {
    let text = match output {
        Some(s) if !s.trim().is_empty() => s,
        _ => return false,
    };
    let lower = text.to_lowercase();

    // Explicit "no issues" verdict — never trigger repair.
    if lower.contains("no critical issues")
        || lower.contains("no issues found")
        || lower.contains("no findings")
        || lower.contains("looks good")
        || lower.contains("lgtm")
    {
        return false;
    }

    // Actionable findings: the reviewer agent emits "## Critical" and "## Warnings" sections.
    // If either section header is present AND is followed by a non-empty bullet, there are
    // findings to fix.
    let section_has_findings = |section: &str| -> bool {
        let Some(idx) = lower.find(section) else {
            return false;
        };
        let after = &lower[idx + section.len()..];
        // Look for a bullet ("- " or "* ") or a numbered list ("1. ") within the next
        // 1000 chars — that's the findings list under the header.
        let window = after.chars().take(1000).collect::<String>();
        window.contains("- ") || window.contains("* ") || window.contains("1. ")
    };

    section_has_findings("## critical")
        || section_has_findings("## warnings")
        || section_has_findings("## must fix")
        || section_has_findings("## should fix")
}

// ---- disk history (<dotz_dir>/ai-agents/workflows.json) ----

fn workflows_file() -> PathBuf {
    if let Ok(p) = std::env::var("DOTZ_WORKFLOWS_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dotz_dir().join("ai-agents").join("workflows.json")
}

fn read_all() -> Vec<WorkflowRun> {
    match std::fs::read_to_string(workflows_file()) {
        Ok(raw) => serde_json::from_str::<Vec<WorkflowRun>>(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_all(runs: &[WorkflowRun]) {
    let file = workflows_file();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(runs) {
        let _ = std::fs::write(&file, s);
    }
}

/// Upsert a run into the on-disk history (read-modify-write of the shared workflows.json).
fn persist(run: &WorkflowRun) {
    let mut all = read_all();
    match all.iter().position(|r| r.id == run.id) {
        Some(idx) => all[idx] = run.clone(),
        None => all.push(run.clone()),
    }
    write_all(&all);
}

/// Evict the oldest TERMINAL runs once the active map exceeds ACTIVE_CAP. They remain on disk
/// (history) + reachable via the /:id route's history fallback; in-flight runs are never evicted.
fn prune_active(active: &mut HashMap<String, WorkflowRun>) {
    if active.len() <= ACTIVE_CAP {
        return;
    }
    let mut finished: Vec<(String, i64)> = active
        .values()
        .filter(|r| r.status == "done" || r.status == "error" || r.status == "aborted")
        .map(|r| (r.id.clone(), r.updated_at))
        .collect();
    finished.sort_by_key(|(_, updated)| *updated);
    for (id, _) in finished {
        if active.len() <= ACTIVE_CAP {
            break;
        }
        active.remove(&id);
    }
}

// ---- core store ops (port of WorkflowStore methods) ----

/// Marker for a cyclic submission (maps to a 400 in the POST handler).
#[derive(Debug)]
pub struct CycleError;

/// Create a new run with the given steps (parents/children resolved from inputs).
/// Returns Err(CycleError) when the submitted steps form a cycle.
pub fn create(
    project_id: Option<String>,
    session_id: Option<String>,
    label: String,
    origin: Option<String>,
    max_repair_rounds: u32,
    inputs: &[CreateStepInput],
    run_budget: Option<Budget>,
) -> Result<WorkflowRun, CycleError> {
    let now = now_ms();

    let mut steps: Vec<WorkflowStep> = inputs
        .iter()
        .map(|s| WorkflowStep {
            id: new_id(),
            agent: s.agent.clone(),
            task: s.task.clone(),
            status: if s.parents.as_ref().map(|p| !p.is_empty()).unwrap_or(false) {
                "pending".to_string()
            } else {
                "ready".to_string()
            },
            parents: Vec::new(),
            children: Vec::new(),
            output: None,
            error: None,
            usage: None,
            sandbox_run_id: s.sandbox_run_id.clone(),
            browser_session_id: s.browser_session_id.clone(),
            tool_call_ids: s.tool_call_ids.clone(),
            thinking: s.thinking.clone(),
            started_at: None,
            ended_at: None,
            auto_repair: s.auto_repair,
            repair_round: 0,
            budget: s.budget.clone(),
            actual_cost: None,
            actual_tokens: None,
            messages: None,
        })
        .collect();

    // Resolve parent refs to real step ids. A ref may be a positional index into the input steps
    // (integer or string "0") OR an already-assigned step id; map both. Only a non-empty all-digits
    // string ref < len is a positional index; otherwise treat the ref as a literal id. Skip invalid
    // types, self-refs, unknown ids, and duplicates.
    let ids: Vec<String> = steps.iter().map(|s| s.id.clone()).collect();
    let len = steps.len();
    for (idx, input) in inputs.iter().enumerate() {
        let Some(parent_refs) = input.parents.as_ref() else {
            continue;
        };
        for raw in parent_refs {
            let resolved = match raw {
                Value::Number(n) => match n.as_i64() {
                    Some(i) if i >= 0 && (i as usize) < len => ids[i as usize].clone(),
                    _ => continue,
                },
                Value::String(s) => {
                    let trimmed = s.trim();
                    let is_index =
                        !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit());
                    if is_index {
                        // Numeric strings are positional refs just like Number values;
                        // out-of-range positional refs are skipped, not treated as literal ids.
                        match trimmed.parse::<usize>() {
                            Ok(n) if n < len => ids[n].clone(),
                            _ => continue,
                        }
                    } else {
                        s.clone()
                    }
                }
                _ => continue,
            };
            let self_id = &steps[idx].id;
            if resolved != *self_id
                && ids.contains(&resolved)
                && !steps[idx].parents.contains(&resolved)
            {
                steps[idx].parents.push(resolved);
            }
        }
    }

    // Resolve children from parents.
    let parent_map: Vec<(String, Vec<String>)> = steps
        .iter()
        .map(|s| (s.id.clone(), s.parents.clone()))
        .collect();
    for (child_id, parents) in &parent_map {
        for pid in parents {
            if let Some(parent) = steps.iter_mut().find(|s| &s.id == pid) {
                parent.children.push(child_id.clone());
            }
        }
    }

    // Reject cyclic graphs (Kahn's algorithm): if a topological order can't cover every step, a
    // cycle exists.
    {
        let mut indeg: HashMap<String, isize> = steps
            .iter()
            .map(|s| (s.id.clone(), s.parents.len() as isize))
            .collect();
        let mut queue: Vec<String> = steps
            .iter()
            .filter(|s| s.parents.is_empty())
            .map(|s| s.id.clone())
            .collect();
        let mut ordered = 0usize;
        let mut q = 0usize;
        while q < queue.len() {
            ordered += 1;
            let current = queue[q].clone();
            q += 1;
            if let Some(step) = steps.iter().find(|s| s.id == current) {
                for child_id in step.children.clone() {
                    let d = indeg.get(&child_id).copied().unwrap_or(0) - 1;
                    indeg.insert(child_id.clone(), d);
                    if d == 0 {
                        queue.push(child_id);
                    }
                }
            }
        }
        if ordered < steps.len() {
            return Err(CycleError);
        }
    }

    // A step left with no valid parents after resolution must be runnable, not stuck "pending".
    for step in steps.iter_mut() {
        if step.parents.is_empty() && step.status == "pending" {
            step.status = "ready".to_string();
        }
    }

    let run = WorkflowRun {
        id: new_id(),
        project_id,
        session_id,
        label,
        steps,
        status: "pending".to_string(),
        origin,
        created_at: now,
        updated_at: now,
        started_at: None,
        ended_at: None,
        max_repair_rounds,
        repair_rounds: 0,
        budget: run_budget,
        actual_cost: None,
        actual_tokens: None,
    };

    {
        let mut active = store_guard();
        active.insert(run.id.clone(), run.clone());
        prune_active(&mut active);
    }
    persist(&run);
    Ok(run)
}

/// Mark a run as started and persist the transition.
pub fn start(id: &str) -> Option<WorkflowRun> {
    let run = {
        let mut active = store_guard();
        let run = active.get_mut(id)?;
        let now = now_ms();
        run.status = "running".to_string();
        run.started_at = Some(now);
        run.updated_at = now;
        run.clone()
    };
    persist(&run);
    emit_workflow_start(&run);
    Some(run)
}

/// A validated step-state patch (built by the POST handler, also used by the workflow executor).
#[derive(Default)]
pub struct StepPatch {
    pub status: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub usage: Option<Usage>,
}

/// Update a step's state and propagate readiness to children. Returns the updated run (clone).
pub fn step_state(run_id: &str, step_id: &str, patch: StepPatch) -> Option<WorkflowRun> {
    let run_snapshot = {
        let mut active = store_guard();
        let run = active.get_mut(run_id)?;
        let now = now_ms();
        let mut changed: HashSet<String> = HashSet::new();

        let step_idx = run.steps.iter().position(|s| s.id == step_id)?;
        {
            let step = &mut run.steps[step_idx];
            if let Some(s) = &patch.status {
                if step.status != *s {
                    changed.insert(step.id.clone());
                    step.status = s.clone();
                }
            }
            if patch.output.is_some() && step.output != patch.output {
                step.output = patch.output.clone();
                changed.insert(step.id.clone());
            }
            if patch.error.is_some() && step.error != patch.error {
                step.error = patch.error.clone();
                changed.insert(step.id.clone());
            }
            if patch.usage.is_some() && step.usage != patch.usage {
                step.usage = patch.usage.clone();
                changed.insert(step.id.clone());
            }
            if step.status == "running" && step.started_at.is_none() {
                step.started_at = Some(now);
            }
            if (step.status == "done" || step.status == "error" || step.status == "skipped")
                && step.ended_at.is_none()
            {
                step.ended_at = Some(now);
            }
        }
        run.updated_at = now;

        let step_status = run.steps[step_idx].status.clone();

        // Propagate readiness: when all parents done, pending → ready.
        if step_status == "done" {
            let children = run.steps[step_idx].children.clone();
            for child_id in children {
                let Some(ci) = run.steps.iter().position(|s| s.id == child_id) else {
                    continue;
                };
                if run.steps[ci].status != "pending" {
                    continue;
                }
                let all_parents_done = run.steps[ci].parents.iter().all(|pid| {
                    run.steps
                        .iter()
                        .find(|s| &s.id == pid)
                        .map(|p| p.status == "done")
                        .unwrap_or(false)
                });
                if all_parents_done {
                    run.steps[ci].status = "ready".to_string();
                    changed.insert(child_id);
                }
            }
        }

        // Auto-repair cycle: when a review step (auto_repair: true) finishes with findings,
        // spawn a repair child (worker) and a re-review grandchild (reviewer) and keep the
        // run running until the re-review passes or the retry cap is hit. This makes
        // "verified result" the default landing state, not the exception.
        if step_status == "done" {
            let step = &run.steps[step_idx];
            if step.auto_repair
                && run.status != "aborted"
                && run.status != "error"
                && run.status != "done"
                && review_has_findings(step.output.as_deref())
                && run.repair_rounds < run.max_repair_rounds
            {
                let next_round = run.repair_rounds + 1;
                run.repair_rounds = next_round;
                let review_step_id = step.id.clone();

                // The reviewer's task includes the original findings so the worker can fix them.
                let findings = step.output.clone().unwrap_or_default();
                let repair_task = format!(
                    "[Auto-repair round {next_round}] The review step '{}' found the following issues that must be fixed. Apply the minimal correct fix; do NOT rewrite unrelated code; do NOT introduce new features. Review findings:\n\n{findings}",
                    review_step_id
                );
                let re_review_task = format!(
                    "[Re-review round {next_round}] Re-audit the implementation after the repair step applied fixes for the original review findings. Verify the Critical and Warnings issues are resolved and no regressions were introduced. Output the standard review report.\n\nOriginal findings for reference:\n{findings}"
                );

                let repair_step = WorkflowStep {
                    id: new_id(),
                    agent: "worker".to_string(),
                    task: repair_task,
                    status: "ready".to_string(),
                    parents: vec![review_step_id.clone()],
                    children: Vec::new(),
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
                    repair_round: next_round,
                    budget: None,
                    actual_cost: None,
                    actual_tokens: None,
                    messages: None,
                };
                let re_review_step = WorkflowStep {
                    id: new_id(),
                    agent: "reviewer".to_string(),
                    task: re_review_task,
                    status: "pending".to_string(),
                    parents: vec![repair_step.id.clone()],
                    children: Vec::new(),
                    output: None,
                    error: None,
                    usage: None,
                    sandbox_run_id: None,
                    browser_session_id: None,
                    tool_call_ids: None,
                    thinking: None,
                    started_at: None,
                    ended_at: None,
                    auto_repair: true,
                    repair_round: next_round,
                    budget: None,
                    actual_cost: None,
                    actual_tokens: None,
                    messages: None,
                };

                // Wire the review step → repair → re-review chain.
                if let Some(review) = run.steps.iter_mut().find(|s| s.id == review_step_id) {
                    review.children.push(repair_step.id.clone());
                    changed.insert(review_step_id);
                }
                // The new steps must be in `changed` so the broadcast loop emits step_state
                // events for them — otherwise the UI graph panel won't show the spawned nodes.
                changed.insert(repair_step.id.clone());
                changed.insert(re_review_step.id.clone());
                run.steps.push(repair_step);
                run.steps.push(re_review_step);
                // The run stays running — the re-review must pass (or the cap must be hit).
                run.status = "running".to_string();
            }
        }

        // Cascade an explicit skip to descendants so a skipped branch doesn't leave children
        // pending forever (matching the error-path sweep, but without marking the run errored).
        if step_status == "skipped" {
            let mut queue: Vec<String> = run.steps[step_idx].children.clone();
            while let Some(child_id) = queue.pop() {
                if let Some(ci) = run.steps.iter().position(|s| s.id == child_id) {
                    if run.steps[ci].status == "pending"
                        || run.steps[ci].status == "ready"
                        || run.steps[ci].status == "running"
                    {
                        run.steps[ci].status = "skipped".to_string();
                        changed.insert(child_id);
                        if run.steps[ci].ended_at.is_none() {
                            run.steps[ci].ended_at = Some(now);
                        }
                    }
                    for next in run.steps[ci].children.clone() {
                        queue.push(next);
                    }
                }
            }
            run.updated_at = now;
        }

        // If every step is terminal (done|skipped), finish the run — evaluated after a "done" OR a
        // "skipped" transition. Guarded so a late update on an already terminal run can't resurrect it.
        let all_terminal = run
            .steps
            .iter()
            .all(|s| s.status == "done" || s.status == "skipped");
        let became_terminal = if (step_status == "done" || step_status == "skipped")
            && run.status != "aborted"
            && run.status != "error"
            && run.status != "done"
            && all_terminal
        {
            run.status = "done".to_string();
            run.ended_at = Some(now);
            run.updated_at = now;
            true
        } else if step_status == "error" {
            // Mark the run errored on the FIRST transition to terminal, then sweep every still-runnable
            // step to "skipped" (mirroring abort) so a child of the errored step isn't left "pending".
            if run.status != "error" && run.status != "aborted" && run.status != "done" {
                run.status = "error".to_string();
                run.ended_at = Some(now);
                for s in run.steps.iter_mut() {
                    if s.status == "pending" || s.status == "ready" || s.status == "running" {
                        s.status = "skipped".to_string();
                        changed.insert(s.id.clone());
                        if s.ended_at.is_none() {
                            s.ended_at = Some(now);
                        }
                    }
                }
            }
            run.updated_at = now;
            run.status == "error" || run.status == "done" || run.status == "aborted"
        } else {
            false
        };

        let run = run.clone();
        (run, changed, became_terminal)
    };
    persist(&run_snapshot.0);
    for step in &run_snapshot.0.steps {
        if run_snapshot.1.contains(&step.id) {
            emit_step_state(run_id, step);
        }
    }
    if run_snapshot.2 {
        emit_workflow_end(&run_snapshot.0);
    }
    Some(run_snapshot.0)
}

/// Set the subagent message history on a step. Called by the executor after a step
/// completes (success or failure) so the full conversation — tool calls, tool
/// results, thinking blocks — is preserved on the step for the UI step-detail
/// drawer AND for resume: a step that was interrupted can re-inherit its prior
/// conversation instead of re-executing already-completed tool calls.
pub fn step_messages(
    run_id: &str,
    step_id: &str,
    messages: Vec<Value>,
) -> Option<WorkflowRun> {
    let run = {
        let mut active = store_guard();
        let run = active.get_mut(run_id)?;
        let step = run.steps.iter_mut().find(|s| s.id == step_id)?;
        step.messages = Some(messages);
        run.updated_at = now_ms();
        run.clone()
    };
    persist(&run);
    Some(run)
}

/// Build a resume-context prefix from the messages of completed steps in a run.
///
/// When a step was interrupted (server restart mid-flight), the executor must
/// re-dispatch it. Without context, the subagent starts from scratch — re-calling
/// tools it already called, re-reading files it already read. This function
/// assembles a compact "prior context" block from the last completed step's
/// messages (if any) so the executor can inject it into the interrupted step's
/// task prompt.
///
/// The context is a single string containing the last completed step's
/// conversation in a condensed format: each message role + text content,
/// truncated to `max_bytes` total. Tool-call/result messages are kept as
/// one-liners (name + first 200 chars of input/output) to bound token cost.
///
/// Returns None when no completed step has messages to inject.
pub fn build_resume_context(run: &WorkflowRun, max_bytes: usize) -> Option<String> {
    // Find the last completed step that has messages. Prefer steps that are
    // `done` (fully completed) over `error` (partial — but their messages are
    // still useful as context).
    let source = run
        .steps
        .iter()
        .rev()
        .find(|s| s.messages.is_some() && (s.status == "done" || s.status == "error"));
    let source = match source {
        Some(s) => s,
        None => return None,
    };
    let messages = source.messages.as_ref()?;
    if messages.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str(&format!(
        "## Prior context from completed step '{}' (agent: {}, status: {})\n\n",
        source.id, source.agent, source.status
    ));
    out.push_str("The following is the subagent conversation from a previously-completed step in this workflow. Use this context to avoid re-doing work (re-reading files, re-calling tools) that was already completed.\n\n");

    for msg in messages {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        match role {
            "user" => {
                let text = text_content(msg);
                let truncated = truncate_bytes(&text, 1024);
                out.push_str(&format!("[user]\n{}\n\n", truncated));
            }
            "assistant" => {
                let text = text_content(msg);
                let truncated = truncate_bytes(&text, 1024);
                let tool_calls = msg.get("tool_calls").and_then(|tc| tc.as_array());
                out.push_str(&format!("[assistant]\n{}\n", truncated));
                if let Some(tcs) = tool_calls {
                    for tc in tcs {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");
                        let args_truncated = truncate_bytes(args, 200);
                        out.push_str(&format!("  - tool_call({}): {}\n", name, args_truncated));
                    }
                }
                out.push('\n');
            }
            "tool" => {
                let text = text_content(msg);
                let truncated = truncate_bytes(&text, 512);
                let tool_call_id = msg
                    .get("tool_call_id")
                    .and_then(|id| id.as_str())
                    .unwrap_or("?");
                out.push_str(&format!("[tool:{}]\n{}\n\n", tool_call_id, truncated));
            }
            _ => {}
        }
        if out.len() >= max_bytes {
            out.push_str("[context truncated]\n");
            break;
        }
    }

    if out.len() > max_bytes {
        truncate_bytes(&out, max_bytes).to_string().into()
    } else {
        Some(out)
    }
}

/// Extract the text content from a message JSON (handles both plain string content
/// and array-of-content-blocks format).
fn text_content(msg: &Value) -> String {
    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
        return text.to_string();
    }
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        return blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

/// Abort a run: mark aborted + sweep every non-terminal step to skipped.
pub fn abort(id: &str) -> Option<WorkflowRun> {
    let run = {
        let mut active = store_guard();
        let run = active.get_mut(id)?;
        let now = now_ms();
        let mut changed: HashSet<String> = HashSet::new();
        run.status = "aborted".to_string();
        run.ended_at = Some(now);
        run.updated_at = now;
        for step in run.steps.iter_mut() {
            if step.status == "running" || step.status == "ready" || step.status == "pending" {
                step.status = "skipped".to_string();
                changed.insert(step.id.clone());
                if step.ended_at.is_none() {
                    step.ended_at = Some(now);
                }
            }
        }
        let run = run.clone();
        (run, changed)
    };
    persist(&run.0);
    for step in &run.0.steps {
        if run.1.contains(&step.id) {
            emit_step_state(id, step);
        }
    }
    emit_workflow_end(&run.0);
    Some(run.0)
}

pub fn get_active(id: &str) -> Option<WorkflowRun> {
    store_guard().get(id).cloned()
}

// ---- resumability: startup scan + explicit resume ----

/// Mark every `running` step in a run as `interrupted` (server restarted mid-flight).
/// Returns the number of steps that were interrupted. Only steps in-flight at shutdown
/// are marked — already-terminal steps are untouched so the run's history is preserved.
pub fn mark_interrupted(run_id: &str) -> Option<usize> {
    let mut active = store_guard();
    let run = active.get_mut(run_id)?;
    let now = now_ms();
    let mut count = 0;
    for step in run.steps.iter_mut() {
        if step.status == "running" {
            step.status = "interrupted".to_string();
            step.ended_at = Some(now);
            step.error = Some("server restarted while step was running".to_string());
            count += 1;
        }
    }
    if count > 0 {
        run.status = "interrupted".to_string();
        run.updated_at = now;
    }
    let run = run.clone();
    persist(&run);
    Some(count)
}

/// Load a run from the on-disk history into the in-memory active map so it can be
/// resumed. Returns the run if it was loaded (i.e. it was in `running` or `interrupted`
/// state and not already in the active map), or None if it doesn't exist or is terminal.
/// This is the startup path: the server calls it for every non-terminal run in
/// `workflows.json` so half-finished runs survive a restart.
pub fn restore_for_resume(run_id: &str) -> Option<WorkflowRun> {
    // Already active — nothing to restore.
    if store_guard().contains_key(run_id) {
        return get_active(run_id);
    }
    // Load from history.
    let run = list_history(None).into_iter().find(|r| r.id == run_id)?;
    // Only restore non-terminal runs.
    if run.status == "done" || run.status == "error" || run.status == "aborted" {
        return None;
    }
    // Insert into active map.
    let mut active = store_guard();
    active.insert(run_id.to_string(), run.clone());
    prune_active(&mut active);
    persist(&run);
    Some(run)
}

/// Resume an interrupted (or running) run: mark it running and spawn the executor task.
/// Returns the resumed run. If the run is already terminal, returns it as-is.
/// If the run doesn't exist in the active map, tries to restore it from history first.
///
/// The actual execution happens in a spawned tokio task (the same `run_workflow` loop
/// the POST /:id/execute handler uses), so the HTTP response returns immediately with
/// the run in `running` state. The UI subscribes to workflow WS events to track
/// step completion.
pub fn resume(run_id: &str) -> Option<WorkflowRun> {
    let run = resume_sync(run_id)?;

    // Spawn the executor task. This mirrors what the execute handler does.
    let rid = run_id.to_string();
    tokio::spawn(async move {
        let _ = crate::workflow_executor::run_workflow(&rid).await;
    });

    Some(run)
}

/// The synchronous part of `resume`: load the run, reset interrupted steps to ready,
/// mark running, persist. Returns the resumed run. Skips the tokio spawn so this
/// is testable from a non-async context.
pub fn resume_sync(run_id: &str) -> Option<WorkflowRun> {
    // Load the run — either from active map or from history.
    let mut run = get_active(run_id).or_else(|| restore_for_resume(run_id))?;

    // Already terminal — nothing to resume.
    if run.status == "done" || run.status == "error" || run.status == "aborted" {
        return Some(run);
    }

    // If the run was interrupted (shutdown mid-flight), the in-flight steps are marked
    // `interrupted`. Reset them to `ready` so the executor will re-dispatch them.
    // This is the key to resumability: a step that was running at shutdown gets a
    // fresh chance to execute.
    if run.status == "interrupted" {
        for step in run.steps.iter_mut() {
            if step.status == "interrupted" {
                step.status = "ready".to_string();
                step.error = None;
                step.ended_at = None;
                step.started_at = None;
            }
        }
    }

    // Also handle the case where a run is `running` but has no `running` steps in
    // the active map (e.g. it was restored from history and the step status was
    // clobbered). Find steps that are pending with all parents done → mark ready.
    // This is a lighter version of the readiness propagation in `step_state`.
    let ids: Vec<String> = run.steps.iter().map(|s| s.id.clone()).collect();
    for id in &ids {
        let should_be_ready = {
            let step = run.steps.iter().find(|s| &s.id == id).unwrap();
            step.status == "pending"
                && step.parents.iter().all(|pid| {
                    ids.iter().any(|i| i == pid)
                        && run
                            .steps
                            .iter()
                            .find(|s| &s.id == pid)
                            .map(|p| {
                                p.status == "done"
                                    || p.status == "skipped"
                                    || p.status == "error"
                            })
                            .unwrap_or(false)
                })
        };
        if should_be_ready {
            let step = run.steps.iter_mut().find(|s| &s.id == id).unwrap();
            step.status = "ready".to_string();
        }
    }

    // Mark running and persist.
    let now = now_ms();
    run.status = "running".to_string();
    if run.started_at.is_none() {
        run.started_at = Some(now);
    }
    run.updated_at = now;

    {
        let mut active = store_guard();
        active.insert(run_id.to_string(), run.clone());
        prune_active(&mut active);
    }
    persist(&run);
    emit_workflow_start(&run);

    Some(run)
}

/// Startup-time scan: load every non-terminal run from the on-disk history into the
/// active map, mark any in-flight steps as `interrupted`, and auto-resume them.
///
/// This is called once when the server boots (`server::serve_with_shutdown`).
/// Without it, a run that was `running` at shutdown would be lost forever — the
/// in-memory active map is empty after a restart.
///
/// Returns the number of runs that were resumed.
pub fn startup_resume() -> usize {
    let non_terminal: Vec<WorkflowRun> = list_history(None)
        .into_iter()
        .filter(|r| {
            r.status != "done"
                && r.status != "error"
                && r.status != "aborted"
        })
        .collect();

    let mut resumed = 0;
    for run in non_terminal {
        // Restore into active map (skips if already present).
        let Some(restored) = restore_for_resume(&run.id) else {
            continue;
        };
        // Mark in-flight steps as interrupted (they were running at shutdown).
        let interrupted = mark_interrupted(&restored.id);
        // Resume only if there's something to re-dispatch. A run whose every step
        //terminal (shouldn't happen given the filter above, but guard anyway) is
        // left in `interrupted` state for operator inspection.
        if interrupted.unwrap_or(0) > 0 || restored.status == "running" {
            if resume(&restored.id).is_some() {
                resumed += 1;
            }
        }
    }
    resumed
}

fn list_active() -> Vec<WorkflowRun> {
    store_guard().values().cloned().collect()
}

/// Number of workflow runs currently in the active map. Surfaced in `/api/health`.
pub fn active_count() -> usize {
    store_guard().len()
}

fn list_history(project_id: Option<&str>) -> Vec<WorkflowRun> {
    let all = read_all();
    match project_id {
        Some(pid) => all
            .into_iter()
            .filter(|r| r.project_id.as_deref() == Some(pid))
            .collect(),
        None => all,
    }
}

// ---- HTTP handlers (port of server.ts /api/workflows routes) ----

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(rename = "projectId")]
    project_id: Option<String>,
}

/// GET /api/workflows?projectId= → { runs: [...] } (history, optionally filtered).
async fn list_history_handler(Query(q): Query<HistoryQuery>) -> Json<Value> {
    let runs = list_history(q.project_id.as_deref());
    Json(json!({ "runs": runs }))
}

/// GET /api/workflows/active → { runs: [...] } (in-memory active map).
async fn list_active_handler() -> Json<Value> {
    Json(json!({ "runs": list_active() }))
}

/// GET /api/workflows/:id → run (active, else history fallback) or 404.
async fn get_handler(
    Path(id): Path<String>,
) -> Result<Json<WorkflowRun>, (StatusCode, Json<Value>)> {
    if let Some(run) = get_active(&id) {
        return Ok(Json(run));
    }
    if let Some(run) = list_history(None).into_iter().find(|r| r.id == id) {
        return Ok(Json(run));
    }
    Err(not_found("no such workflow run"))
}

/// POST /api/workflows → validate DAG (cycle → 400), assign step ids, start, return the run.
async fn create_handler(
    body: Option<Json<Value>>,
) -> Result<Json<WorkflowRun>, (StatusCode, Json<Value>)> {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));

    let steps_val = body.get("steps");
    let steps_arr = match steps_val.and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return Err(bad("steps (non-empty array) is required")),
    };

    // Validate each step's SHAPE (string agent + task; parents, if present, must be an array).
    for s in steps_arr {
        let obj = match s.as_object() {
            Some(o) => o,
            None => return Err(bad("each step needs a string agent and task")),
        };
        if !obj.get("agent").map(|v| v.is_string()).unwrap_or(false)
            || !obj.get("task").map(|v| v.is_string()).unwrap_or(false)
        {
            return Err(bad("each step needs a string agent and task"));
        }
        if let Some(p) = obj.get("parents") {
            // Match server.ts:410 — a present `parents` that isn't an array (incl. null) is a 400.
            if !p.is_array() {
                return Err(bad("step parents must be an array"));
            }
            for item in p.as_array().unwrap() {
                if !item.is_string() && !item.is_number() {
                    return Err(bad("step parents must be strings or numbers"));
                }
            }
        }
    }

    let inputs: Vec<CreateStepInput> = match serde_json::from_value(json!(steps_arr)) {
        Ok(v) => v,
        Err(_) => return Err(bad("each step needs a string agent and task")),
    };

    let project_id = str_or_null(body.get("projectId"));
    let session_id = str_or_null(body.get("sessionId"));
    let label = body
        .get("label")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("untitled workflow")
        .to_string();
    let origin = body
        .get("origin")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let max_repair_rounds = body
        .get("maxRepairRounds")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(3)
        .clamp(0, 10);

    // Parse optional run-level budget from the request body. Each step can also carry
    // its own per-step budget; the run budget is the cumulative cap across all steps.
    let run_budget = body.get("budget").and_then(|v| serde_json::from_value::<Budget>(v.clone()).ok());

    let run = match create(project_id, session_id, label, origin, max_repair_rounds, &inputs, run_budget) {
        Ok(run) => run,
        Err(CycleError) => return Err(bad("workflow steps form a cycle")),
    };

    // Mark started (mirrors server.ts: create() then start()).
    let started = start(&run.id).unwrap_or(run);
    Ok(Json(started))
}

#[derive(Deserialize)]
struct StepBody {
    #[serde(rename = "stepId")]
    step_id: Option<String>,
    status: Option<String>,
    output: Option<String>,
    error: Option<String>,
    usage: Option<Usage>,
}

const ALLOWED_STATUS: [&str; 7] = ["pending", "ready", "running", "done", "error", "skipped", "interrupted"];

/// POST /api/workflows/:id/step → update a step + propagate, return the run.
/// 404 unknown run/step, 409 finished run, 400 bad stepId/status.
async fn step_handler(
    Path(id): Path<String>,
    body: Option<Json<StepBody>>,
) -> Result<Json<WorkflowRun>, (StatusCode, Json<Value>)> {
    let run = match get_active(&id) {
        Some(r) => r,
        None => return Err(not_found("no such workflow run")),
    };
    if run.status == "done" || run.status == "error" || run.status == "aborted" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("run is {} — its steps can no longer be updated", run.status)
            })),
        ));
    }

    let body = match body {
        Some(Json(b)) => b,
        None => return Err(bad("stepId is required")),
    };
    let step_id = match body.step_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Err(bad("stepId is required")),
    };
    if !run.steps.iter().any(|s| s.id == step_id) {
        return Err(not_found("no such step"));
    }
    if let Some(st) = &body.status {
        if !ALLOWED_STATUS.contains(&st.as_str()) {
            return Err(bad("invalid status"));
        }
        // "interrupted" is a server-initiated state (set on startup when a step was
        // running at shutdown). Clients may not patch a step to interrupted — they
        // resume a run via POST /:id/resume instead, which resets interrupted steps
        // to ready and re-dispatches them through the executor.
        if st == "interrupted" {
            return Err(bad("interrupted is a server-initiated status; use POST /:id/resume to resume"));
        }
    }

    let patch = StepPatch {
        status: body.status,
        output: body.output,
        error: body.error,
        usage: body.usage,
    };
    match step_state(&id, &step_id, patch) {
        Some(updated) => Ok(Json(updated)),
        None => Err(not_found("no such workflow run")),
    }
}

/// POST /api/workflows/:id/abort → abort, return { ok: true }. 404 for unknown run.
async fn abort_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if get_active(&id).is_none() {
        return Err(not_found("no such workflow run"));
    }
    abort(&id);
    Ok(Json(json!({ "ok": true })))
}

/// POST /api/workflows/:id/resume → resume an interrupted/running run via the executor.
/// 200 with the run (running), 404 unknown run, 409 already terminal.
async fn resume_handler(
    Path(id): Path<String>,
) -> Result<Json<WorkflowRun>, (StatusCode, Json<Value>)> {
    // Reject the resume if the run is already terminal. `resume()` returns the
    // run as-is in that case, but the UI should see a 409 so it doesn't render a
    // "running" badge on a run that hasn't actually restarted.
    if let Some(run) = get_active(&id) {
        if run.status == "done" || run.status == "error" || run.status == "aborted" {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("run is {} — cannot resume", run.status)
                })),
            ));
        }
    }
    match resume(&id) {
        Some(run) => Ok(Json(run)),
        None => Err(not_found("no such workflow run")),
    }
}

/// POST /api/workflows/:id/execute → drive the run to completion via real subagent dispatch.
/// Returns { run: ... } with the final run state. 404 for unknown run.
async fn execute_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // If the run is already terminal, return it directly without re-executing.
    if let Some(run) = get_active(&id) {
        if run.status == "done" || run.status == "error" || run.status == "aborted" {
            return Ok(Json(json!({ "run": run })));
        }
    } else {
        return Err(not_found("no such workflow run"));
    }

    match crate::workflow_executor::run_workflow(&id).await {
        Some(run) => Ok(Json(json!({ "run": run }))),
        None => Err(not_found("no such workflow run")),
    }
}

// ---- helpers ----

/// JSON value → Option<String>: a string stays, anything else (null/absent/number) becomes None.
fn str_or_null(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn bad(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

fn not_found(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
}

/// Register the /api/workflows routes with stateless handlers, including the executor
/// endpoint that drives a run to completion via real subagent dispatch.
pub fn router() -> Router<()> {
    Router::new()
        .route(
            "/api/workflows",
            get(list_history_handler).post(create_handler),
        )
        .route("/api/workflows/active", get(list_active_handler))
        .route("/api/workflows/{id}", get(get_handler))
        .route("/api/workflows/{id}/step", post(step_handler))
        .route("/api/workflows/{id}/abort", post(abort_handler))
        .route("/api/workflows/{id}/execute", post(execute_handler))
        .route("/api/workflows/{id}/resume", post(resume_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
        }
    }

    fn step_with_auto_repair(
        agent: &str,
        task: &str,
        parents: Option<Vec<Value>>,
    ) -> CreateStepInput {
        CreateStepInput {
            agent: agent.into(),
            task: task.into(),
            parents,
            sandbox_run_id: None,
            browser_session_id: None,
            tool_call_ids: None,
            thinking: None,
            auto_repair: true,
            budget: None,
        }
    }

    fn with_tmp_workflows_file<T>(f: impl FnOnce() -> T) -> T {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let file =
            std::env::temp_dir().join(format!("dotz-workflows-test-{}.json", Uuid::new_v4()));
        std::env::set_var("DOTZ_WORKFLOWS_FILE", file.to_string_lossy().to_string());
        let result = f();
        let _ = std::fs::remove_file(&file);
        drop(guard);
        result
    }

    #[test]
    fn numeric_parent_refs_resolve_to_step_ids() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
                step("c", "C", Some(vec![json!(1)])),
            ];
            let run = create(None, None, "test".into(), None, 3, &inputs, None).unwrap();
            let ids: Vec<_> = run.steps.iter().map(|s| s.id.clone()).collect();

            assert!(run.steps[0].parents.is_empty());
            assert_eq!(run.steps[0].status, "ready");
            assert_eq!(run.steps[1].parents, vec![ids[0].clone()]);
            assert_eq!(run.steps[1].status, "pending");
            assert_eq!(run.steps[2].parents, vec![ids[1].clone()]);
            assert_eq!(run.steps[2].status, "pending");
        });
    }

    #[test]
    fn string_and_numeric_parent_refs_are_equivalent() {
        with_tmp_workflows_file(|| {
            let numeric = create(
                None,
                None,
                "numeric".into(),
                None,
                3,
                &[step("a", "A", None), step("b", "B", Some(vec![json!(0)]))],
                None,
            )
            .unwrap();
            let string = create(
                None,
                None,
                "string".into(),
                None,
                3,
                &[step("a", "A", None), step("b", "B", Some(vec![json!("0")]))],
                None,
            )
            .unwrap();
            assert_eq!(
                numeric.steps[1].parents,
                vec![numeric.steps[0].id.clone()],
                "numeric positional ref must resolve to step 0"
            );
            assert_eq!(
                string.steps[1].parents,
                vec![string.steps[0].id.clone()],
                "string positional ref must resolve to step 0"
            );
        });
    }

    #[test]
    fn out_of_range_numeric_parent_is_skipped() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(99)]))];
            let run = create(None, None, "test".into(), None, 3, &inputs, None).unwrap();
            assert!(run.steps[1].parents.is_empty());
            assert_eq!(run.steps[1].status, "ready");
        });
    }

    /// Numeric strings in parent refs must behave like Number refs: an out-of-range index is
    /// skipped, not silently treated as a literal step id. Before this fix, "99" would fall through
    /// to the literal-id path and rely on the outer `ids.contains` check to discard it; that path
    /// would have accepted a step id that happened to be all digits.
    #[test]
    fn out_of_range_numeric_string_parent_is_skipped() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!("99")])),
            ];
            let run = create(None, None, "test".into(), None, 3, &inputs, None).unwrap();
            assert!(run.steps[1].parents.is_empty());
            assert_eq!(run.steps[1].status, "ready");
        });
    }

    /// A mix of valid string positional refs, out-of-range numeric strings, and unknown literal ids
    /// must resolve correctly and dedupe.
    #[test]
    fn mixed_string_parent_refs_resolve_and_dedupe() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", None),
                step(
                    "c",
                    "C",
                    Some(vec![json!("0"), json!("no-such"), json!("0"), json!("99")]),
                ),
            ];
            let run = create(None, None, "test".into(), None, 3, &inputs, None).unwrap();
            let ids: Vec<_> = run.steps.iter().map(|s| s.id.clone()).collect();
            assert_eq!(run.steps[2].parents, vec![ids[0].clone()]);
            assert_eq!(run.steps[2].status, "pending");
        });
    }

    #[test]
    fn error_transition_sweeps_runnable_steps_to_skipped_with_ended_at() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
                step("c", "C", Some(vec![json!(0)])),
            ];
            let run = create(None, None, "error-sweep".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let parent_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &parent_id,
                StepPatch {
                    status: Some("error".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(updated.status, "error");
            let parent = updated.steps.iter().find(|s| s.id == parent_id).unwrap();
            assert_eq!(parent.status, "error");
            assert!(parent.ended_at.is_some(), "errored step must have endedAt");

            for s in &updated.steps {
                if s.id != parent_id {
                    assert_eq!(s.status, "skipped", "child of errored step must be skipped");
                    assert!(
                        s.ended_at.is_some(),
                        "auto-skipped step must have endedAt: {:?}",
                        s
                    );
                }
            }
        });
    }

    #[test]
    fn abort_sweeps_runnable_steps_to_skipped_with_ended_at() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
            let run = create(None, None, "abort-sweep".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            let updated = abort(&run.id).unwrap();
            assert_eq!(updated.status, "aborted");
            for s in &updated.steps {
                assert_eq!(s.status, "skipped");
                assert!(
                    s.ended_at.is_some(),
                    "aborted step must have endedAt: {:?}",
                    s
                );
            }
        });
    }

    #[test]
    fn explicit_skipped_patch_sets_ended_at() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "skip".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("skipped".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(updated.status, "done");
            let s = &updated.steps[0];
            assert_eq!(s.status, "skipped");
            assert!(
                s.ended_at.is_some(),
                "explicitly skipped step must have endedAt"
            );
        });
    }

    #[test]
    fn skipped_parent_cascades_to_descendants_and_finishes_run() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
                step("c", "C", Some(vec![json!(1)])),
            ];
            let run = create(None, None, "skip-cascade".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let parent_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &parent_id,
                StepPatch {
                    status: Some("skipped".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(
                updated.status, "done",
                "run should finish after skip cascade"
            );
            assert_eq!(
                updated.steps[0].status, "skipped",
                "parent should be skipped"
            );
            assert_eq!(
                updated.steps[1].status, "skipped",
                "child of skipped parent should be skipped"
            );
            assert_eq!(
                updated.steps[2].status, "skipped",
                "grandchild of skipped parent should be skipped"
            );
            for s in &updated.steps {
                assert!(
                    s.ended_at.is_some(),
                    "every skipped step must have endedAt: {:?}",
                    s
                );
            }
        });
    }

    #[test]
    fn skipped_leaf_finishes_run() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "skip-leaf".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("skipped".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(updated.status, "done");
            assert_eq!(updated.steps[0].status, "skipped");
            assert!(updated.steps[0].ended_at.is_some());
        });
    }

    #[test]
    fn cyclic_steps_return_cycle_error() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", Some(vec![json!(1)])),
                step("b", "B", Some(vec![json!(0)])),
            ];
            assert!(
                matches!(
                    create(None, None, "cycle".into(), None, 3, &inputs, None),
                    Err(CycleError)
                ),
                "a dependency cycle must be rejected"
            );
        });
    }

    #[test]
    fn duplicate_parents_are_deduped() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0), json!(0)])),
            ];
            let run = create(None, None, "dup".into(), None, 3, &inputs, None).unwrap();
            assert_eq!(run.steps[1].parents.len(), 1);
        });
    }

    #[test]
    fn unknown_literal_parent_id_is_skipped() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!("no-such-id")])),
            ];
            let run = create(None, None, "test".into(), None, 3, &inputs, None).unwrap();
            assert!(run.steps[1].parents.is_empty());
            assert_eq!(run.steps[1].status, "ready");
        });
    }

    /// `active_count` must reflect the number of runs in the in-memory active store so the
    /// `/api/health` endpoint can surface live workflow activity to the operator.
    #[test]
    fn active_count_returns_store_size() {
        with_tmp_workflows_file(|| {
            let baseline = active_count();
            let run = create(None, None, "count".into(), None, 3, &[step("a", "A", None)], None).unwrap();
            assert_eq!(active_count(), baseline + 1, "active_count should include the new run");

            // Abort marks the run terminal but keeps it in the active map (eviction only happens
            // when the cap is exceeded), so the count stays elevated.
            abort(&run.id);
            assert_eq!(
                active_count(),
                baseline + 1,
                "active_count should still include the aborted run"
            );
        });
    }

    /// A panic while holding the workflow store lock must not permanently brick the store.
    /// `store_guard` recovers from a poisoned mutex so subsequent reads and writes keep working.
    #[test]
    fn store_guard_recovers_from_poisoned_mutex() {
        with_tmp_workflows_file(|| {
            // Ensure the store singleton is initialized.
            drop(store().lock().unwrap());

            let m = store();
            let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = m.lock().unwrap();
                panic!("intentional workflow store mutex poison");
            }));
            assert!(poisoned.is_err(), "workflow store mutex should be poisoned");

            // Recovery: both public ops and direct guard usage must return a usable store.
            let count = active_count();
            let guard = store_guard();
            assert_eq!(
                count, guard.len(),
                "active_count and store_guard must agree after recovering from a poisoned mutex"
            );
        });
    }

    /// Workflow mutations must broadcast live events so the UI graph panel updates without
    /// polling. start → workflow_start, step_state → step_state, terminal transition → workflow_end.
    #[test]
    fn workflow_mutations_broadcast_live_events() {
        with_tmp_workflows_file(|| {
            let mut rx = subscribe_events();

            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
            ];
            let run = create(None, None, "events".into(), None, 3, &inputs, None).unwrap();
            let child_id = run.steps[1].id.clone();

            // start() emits workflow_start.
            let started = start(&run.id).unwrap();
            let start_frame = rx.try_recv().expect("workflow_start event should be broadcast");
            assert_eq!(start_frame["kind"], "workflow");
            assert_eq!(start_frame["runId"], started.id);
            assert_eq!(start_frame["event"]["type"], "workflow_start");
            assert_eq!(start_frame["event"]["run"]["status"], "running");

            // Completing the parent makes the child ready.
            let parent_id = run.steps[0].id.clone();
            let _ = step_state(
                &run.id,
                &parent_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("parent output".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            let mut seen_parent = false;
            let mut seen_child_ready = false;
            while let Ok(frame) = rx.try_recv() {
                if frame["event"]["type"] == "step_state" {
                    if frame["event"]["stepId"] == parent_id {
                        assert_eq!(frame["event"]["status"], "done");
                        assert_eq!(frame["event"]["output"], "parent output");
                        seen_parent = true;
                    }
                    if frame["event"]["stepId"] == child_id {
                        assert_eq!(frame["event"]["status"], "ready");
                        seen_child_ready = true;
                    }
                }
            }
            assert!(seen_parent, "parent step_state event should be broadcast");
            assert!(
                seen_child_ready,
                "child ready event should be broadcast when parent completes"
            );

            // Completing the child finishes the run.
            let _ = step_state(
                &run.id,
                &child_id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            let mut seen_child_done = false;
            let mut seen_end = false;
            while let Ok(frame) = rx.try_recv() {
                if frame["event"]["type"] == "step_state"
                    && frame["event"]["stepId"] == child_id
                    && frame["event"]["status"] == "done"
                {
                    seen_child_done = true;
                }
                if frame["event"]["type"] == "workflow_end" {
                    assert_eq!(frame["event"]["run"]["status"], "done");
                    seen_end = true;
                }
            }
            assert!(seen_child_done, "child done step_state should be broadcast");
            assert!(seen_end, "workflow_end should be broadcast when run finishes");
        });
    }

    /// abort() broadcasts step_state events for every swept step plus a workflow_end event so the
    /// UI graph reflects the terminal state immediately.
    #[test]
    fn abort_broadcasts_sweep_and_end_events() {
        with_tmp_workflows_file(|| {
            let mut rx = subscribe_events();

            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
            let run = create(None, None, "abort-events".into(), None, 3, &inputs, None).unwrap();
            let run_id = run.id.clone();
            let _ = start(&run.id).unwrap();
            // Drain the workflow_start event.
            let _ = rx.try_recv();

            let updated = abort(&run.id).unwrap();
            let mut swept = 0;
            let mut seen_end = false;
            while let Ok(frame) = rx.try_recv() {
                // Filter by runId — the global workflow event channel carries events from
                // every concurrent run (e.g. workflow_executor tests running in parallel).
                if frame["runId"] != json!(run_id) {
                    continue;
                }
                if frame["event"]["type"] == "step_state" {
                    assert_eq!(frame["event"]["status"], "skipped");
                    swept += 1;
                }
                if frame["event"]["type"] == "workflow_end" {
                    assert_eq!(frame["event"]["run"]["status"], "aborted");
                    seen_end = true;
                }
            }
            assert_eq!(swept, updated.steps.len(), "every swept step should emit step_state");
            assert!(seen_end, "abort should emit workflow_end");
        });
    }

    // ---- auto-repair cycle tests ----

    /// When a review step with auto_repair:true finishes with findings, the engine must spawn
    /// a repair child (worker) and a re-review grandchild (reviewer), keeping the run running.
    #[test]
    fn auto_repair_spawns_worker_and_reviewer_children_on_findings() {
        with_tmp_workflows_file(|| {
            let review_step = step_with_auto_repair(
                "reviewer",
                "Review the implementation",
                None,
            );
            let inputs = vec![review_step];
            let run = create(None, None, "auto-repair".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let review_id = run.steps[0].id.clone();
            assert_eq!(run.steps.len(), 1);
            assert_eq!(run.repair_rounds, 0);

            // Mark the review step done with findings.
            let updated = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some(
                        "## Critical\n- `src/foo.ts:42` - off-by-one error\n\n## Warnings\n- `src/foo.ts:100` - magic number\n\n## Summary\nNeeds fixes.".into(),
                    ),
                    ..Default::default()
                },
            )
            .unwrap();

            // The run must still be running (not done), with 3 steps: review + repair + re-review.
            assert_eq!(updated.status, "running", "run should stay running when repair is pending");
            assert_eq!(updated.steps.len(), 3, "review + repair + re-review steps");
            assert_eq!(updated.repair_rounds, 1);

            let repair = updated
                .steps
                .iter()
                .find(|s| s.agent == "worker")
                .expect("repair step must be present");
            let re_review = updated
                .steps
                .iter()
                .find(|s| s.agent == "reviewer" && s.id != review_id)
                .expect("re-review step must be present");

            // Wiring: review → repair → re-review.
            assert_eq!(repair.parents, vec![review_id.clone()]);
            assert_eq!(re_review.parents, vec![repair.id.clone()]);
            let orig_review = updated.steps.iter().find(|s| s.id == review_id).unwrap();
            assert_eq!(orig_review.children, vec![repair.id.clone()]);

            // The repair step is ready to run; the re-review is pending on the repair.
            assert_eq!(repair.status, "ready");
            assert_eq!(re_review.status, "pending");

            // The repair task must reference the findings.
            assert!(repair.task.contains("Auto-repair round 1"));
            assert!(repair.task.contains("off-by-one error"));
            assert!(re_review.task.contains("Re-review round 1"));

            // The re-review step must itself have auto_repair:true so the cycle can repeat.
            assert!(re_review.auto_repair);
            assert_eq!(re_review.repair_round, 1);
        });
    }

    /// A review step with auto_repair:true that reports NO findings must NOT spawn a repair
    /// cycle — the run should finish normally.
    #[test]
    fn auto_repair_does_not_trigger_on_clean_review() {
        with_tmp_workflows_file(|| {
            let review_step = step_with_auto_repair(
                "reviewer",
                "Review the implementation",
                None,
            );
            let inputs = vec![review_step];
            let run = create(None, None, "clean-review".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let review_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("No critical issues found. Looks good.".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // Clean review → run finishes, no extra steps.
            assert_eq!(updated.status, "done");
            assert_eq!(updated.steps.len(), 1);
            assert_eq!(updated.repair_rounds, 0);
        });
    }

    /// The auto-repair cycle is bounded by max_repair_rounds. After the cap is hit, the run
    /// finishes even if the latest review still reports findings.
    #[test]
    fn auto_repair_respects_max_rounds_cap() {
        with_tmp_workflows_file(|| {
            // max_repair_rounds = 1: one repair attempt, then the re-review's findings must
            // NOT spawn another cycle.
            let review_step = step_with_auto_repair(
                "reviewer",
                "Review the implementation",
                None,
            );
            let inputs = vec![review_step];
            let run = create(None, None, "capped-repair".into(), None, 1, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let review_id = run.steps[0].id.clone();

            // Round 1: review finds issues → spawns repair + re-review.
            let updated = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("## Critical\n- bug\n".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(updated.repair_rounds, 1);
            assert_eq!(updated.steps.len(), 3);

            // The repair step is ready; run it.
            let repair_id = updated
                .steps
                .iter()
                .find(|s| s.agent == "worker")
                .unwrap()
                .id
                .clone();
            let updated = step_state(
                &run.id,
                &repair_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("Fixed it.".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // Now the re-review step is ready. Mark it done with MORE findings. Since max=1,
            // the engine must NOT spawn another repair cycle — the run should finish.
            let re_review_id = updated
                .steps
                .iter()
                .find(|s| s.agent == "reviewer" && s.id != review_id)
                .unwrap()
                .id
                .clone();
            let after = step_state(
                &run.id,
                &re_review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("## Critical\n- still broken\n".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // Cap hit: run finishes, no additional steps spawned.
            assert_eq!(after.status, "done", "cap hit should finish the run");
            assert_eq!(after.steps.len(), 3, "no extra steps after cap");
            assert_eq!(after.repair_rounds, 1);
        });
    }

    /// When the re-review passes (clean output), the run should finish normally with the
    /// repair chain marked done.
    #[test]
    fn auto_repair_completes_when_re_review_passes() {
        with_tmp_workflows_file(|| {
            let review_step = step_with_auto_repair(
                "reviewer",
                "Review the implementation",
                None,
            );
            let inputs = vec![review_step];
            let run = create(None, None, "passing-repair".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let review_id = run.steps[0].id.clone();

            // Round 1: review finds issues.
            let updated = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("## Critical\n- bug\n".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let repair_id = updated
                .steps
                .iter()
                .find(|s| s.agent == "worker")
                .unwrap()
                .id
                .clone();

            // Run the repair step.
            let updated = step_state(
                &run.id,
                &repair_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("Fixed the bug.".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let re_review_id = updated
                .steps
                .iter()
                .find(|s| s.agent == "reviewer" && s.id != review_id)
                .unwrap()
                .id
                .clone();

            // Re-review passes — no findings.
            let after = step_state(
                &run.id,
                &re_review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("No critical issues found. All good.".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(after.status, "done");
            assert_eq!(after.steps.len(), 3);
            assert_eq!(after.repair_rounds, 1);
        });
    }

    /// review_has_findings heuristic: detects actionable findings in review output.
    #[test]
    fn review_has_findings_detects_critical_and_warnings() {
        assert!(review_has_findings(Some(
            "## Critical\n- bug in foo\n"
        )));
        assert!(review_has_findings(Some(
            "## Warnings\n- magic number\n"
        )));
        assert!(review_has_findings(Some(
            "## Must Fix\n- security issue\n"
        )));
        assert!(review_has_findings(Some(
            "## Should Fix\n- code smell\n"
        )));
        // Numbered list.
        assert!(review_has_findings(Some(
            "## Critical\n1. bug\n"
        )));
    }

    /// review_has_findings heuristic: rejects clean reviews.
    #[test]
    fn review_has_findings_rejects_clean_reviews() {
        assert!(!review_has_findings(None));
        assert!(!review_has_findings(Some("")));
        assert!(!review_has_findings(Some("   ")));
        assert!(!review_has_findings(Some(
            "No critical issues found."
        )));
        assert!(!review_has_findings(Some(
            "No issues found."
        )));
        assert!(!review_has_findings(Some(
            "No findings."
        )));
        assert!(!review_has_findings(Some(
            "Looks good."
        )));
        assert!(!review_has_findings(Some(
            "LGTM"
        )));
        // A section header with no bullets after it is not actionable.
        assert!(!review_has_findings(Some(
            "## Critical\n\n## Summary\nAll good."
        )));
    }

    /// A step without auto_repair must NOT trigger the repair cycle even if it is a reviewer
    /// with findings.
    #[test]
    fn non_auto_repair_step_does_not_trigger_cycle() {
        with_tmp_workflows_file(|| {
            let review_step = step("reviewer", "Review the implementation", None);
            let inputs = vec![review_step];
            let run = create(None, None, "no-auto".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let review_id = run.steps[0].id.clone();

            let updated = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("## Critical\n- bug\n".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // No auto_repair → run finishes, no extra steps.
            assert_eq!(updated.status, "done");
            assert_eq!(updated.steps.len(), 1);
        });
    }

    /// The auto-repair cycle must broadcast step_state events for the spawned steps so the
    /// UI graph panel reflects the new nodes immediately.
    #[test]
    fn auto_repair_broadcasts_spawned_step_events() {
        with_tmp_workflows_file(|| {
            let mut rx = subscribe_events();
            let review_step = step_with_auto_repair("reviewer", "Review", None);
            let inputs = vec![review_step];
            let run = create(None, None, "repair-events".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let _ = rx.try_recv(); // workflow_start

            let review_id = run.steps[0].id.clone();
            let _ = step_state(
                &run.id,
                &review_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("## Critical\n- bug\n".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            let mut seen_repair_ready = false;
            let mut seen_re_review_pending = false;
            while let Ok(frame) = rx.try_recv() {
                if frame["event"]["type"] == "step_state" {
                    let status = frame["event"]["status"].as_str().unwrap();
                    let agent = frame["event"]["stepId"].as_str().unwrap();
                    // The repair step (worker) should be "ready" and the re-review (reviewer)
                    // should be "pending".
                    if status == "ready" {
                        seen_repair_ready = true;
                    }
                    if status == "pending" {
                        seen_re_review_pending = true;
                    }
                    let _ = agent;
                }
            }
            assert!(seen_repair_ready, "repair step ready event should be broadcast");
            assert!(
                seen_re_review_pending,
                "re-review step pending event should be broadcast"
            );
        });
    }

    // ---- resumability tests ----

    /// `mark_interrupted` marks every `running` step as `interrupted` and sets the
    /// run status to `interrupted`. Already-terminal steps are untouched.
    #[test]
    fn mark_interrupted_marks_running_steps_and_run() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
                step("c", "C", Some(vec![json!(1)])),
            ];
            let run = create(None, None, "interrupt".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Mark step 0 as running (simulate executor picking it up).
            let running_id = run.steps[0].id.clone();
            let _ = step_state(
                &run.id,
                &running_id,
                StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // Steps 1 and 2 are still pending/ready.
            assert_eq!(run.steps[1].status, "pending");
            assert_eq!(run.steps[2].status, "pending");

            // Mark interrupted.
            let count = mark_interrupted(&run.id).unwrap();
            assert_eq!(count, 1, "only the running step should be interrupted");

            let after = get_active(&run.id).unwrap();
            assert_eq!(after.status, "interrupted");
            let interrupted_step = after.steps.iter().find(|s| s.id == running_id).unwrap();
            assert_eq!(interrupted_step.status, "interrupted");
            assert!(
                interrupted_step
                    .error
                    .as_ref()
                    .map(|e| e.contains("server restarted"))
                    .unwrap_or(false),
                "interrupted step should have server-restarted error"
            );
            assert!(interrupted_step.ended_at.is_some());

            // Non-running steps are untouched.
            let s1 = after.steps.iter().find(|s| s.id == run.steps[1].id).unwrap();
            assert_eq!(s1.status, "pending");
            assert!(s1.error.is_none());
        });
    }

    /// `mark_interrupted` is a no-op when no steps are running (e.g. all terminal).
    #[test]
    fn mark_interrupted_noop_when_no_running_steps() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", None)];
            let run = create(None, None, "noop".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Complete both steps.
            for s in run.steps.iter() {
                let _ = step_state(
                    &run.id,
                    &s.id,
                    StepPatch {
                        status: Some("done".into()),
                        ..Default::default()
                    },
                );
            }
            assert_eq!(get_active(&run.id).unwrap().status, "done");

            // mark_interrupted on a done run should return 0.
            let count = mark_interrupted(&run.id).unwrap();
            assert_eq!(count, 0);
            assert_eq!(get_active(&run.id).unwrap().status, "done");
        });
    }

    /// `restore_for_resume` loads a non-terminal run from history into the active map.
    #[test]
    fn restore_for_resume_loads_from_history() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "restore".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let run_id = run.id.clone();

            // Simulate shutdown: the run is in the active map AND history.
            // Remove from active map (simulates restart).
            {
                let mut active = store_guard();
                active.remove(&run_id);
            }
            assert!(get_active(&run_id).is_none(), "active map should be empty after remove");

            // But it's still in history.
            let in_history = list_history(None).into_iter().find(|r| r.id == run_id);
            assert!(in_history.is_some(), "run should be in history");

            // restore_for_resume should load it back.
            let restored = restore_for_resume(&run_id).unwrap();
            assert_eq!(restored.id, run_id);
            assert_eq!(restored.status, "running");
            assert!(get_active(&run_id).is_some(), "run should be back in active map");
        });
    }

    /// `restore_for_resume` returns None for terminal runs (done/error/aborted).
    #[test]
    fn restore_for_resume_rejects_terminal_runs() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "terminal".into(), None, 3, &inputs, None).unwrap();
            let run_id = run.id.clone();

            // Abort makes it terminal.
            abort(&run_id);

            // Remove from active map.
            {
                let mut active = store_guard();
                active.remove(&run_id);
            }

            // restore_for_resume should return None.
            assert!(
                restore_for_resume(&run_id).is_none(),
                "terminal run should not be restored"
            );
        });
    }

    /// `resume` resets `interrupted` steps to `ready` and marks the run `running`.
    #[test]
    fn resume_resets_interrupted_steps_to_ready() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
            ];
            let run = create(None, None, "resume-test".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Simulate shutdown: mark step 0 as running, then mark interrupted.
            let step0_id = run.steps[0].id.clone();
            let _ = step_state(
                &run.id,
                &step0_id,
                StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            );
            let _ = mark_interrupted(&run.id);

            // Verify interrupted state.
            let interrupted = get_active(&run.id).unwrap();
            assert_eq!(interrupted.status, "interrupted");
            let s0 = interrupted.steps.iter().find(|s| s.id == step0_id).unwrap();
            assert_eq!(s0.status, "interrupted");

            // Use resume_sync to test the state transition without spawning a tokio task.
            let resumed = resume_sync(&run.id).unwrap();
            assert_eq!(resumed.status, "running");

            // Step 0 should be ready (reset from interrupted).
            let s0 = resumed.steps.iter().find(|s| s.id == step0_id).unwrap();
            assert_eq!(s0.status, "ready", "interrupted step should be reset to ready");
            assert!(s0.error.is_none(), "error should be cleared on resume");
            assert!(s0.ended_at.is_none(), "endedAt should be cleared on resume");
            assert!(s0.started_at.is_none(), "startedAt should be cleared on resume");

            // Step 1 (was pending) should still be pending.
            let s1 = resumed.steps.iter().find(|s| s.id == run.steps[1].id).unwrap();
            assert_eq!(s1.status, "pending");
        });
    }

    /// `resume` returns the run as-is when it is already terminal.
    #[test]
    fn resume_returns_terminal_run_as_is() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "terminal-resume".into(), None, 3, &inputs, None).unwrap();
            let run_id = run.id.clone();
            abort(&run_id);

            let result = resume_sync(&run_id).unwrap();
            assert_eq!(result.status, "aborted");
        });
    }

    /// `resume` restores a run from history if it's not in the active map.
    #[test]
    fn resume_restores_from_history() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "resume-restore".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let run_id = run.id.clone();

            // Remove from active map (simulates restart).
            {
                let mut active = store_guard();
                active.remove(&run_id);
            }

            // resume_sync() should restore from history and resume.
            let resumed = resume_sync(&run_id).unwrap();
            assert_eq!(resumed.status, "running");
            assert!(get_active(&run_id).is_some());
        });
    }

    /// `startup_resume` resumes interrupted runs from history on server boot.
    /// It does NOT resume terminal runs (done/error/aborted).
    /// Uses `#[tokio::test]` because `resume()` spawns an executor task.
    #[tokio::test]
    async fn startup_resume_resumes_interrupted_runs() {
        with_tmp_workflows_file(|| {
            // Run 1: running (should be resumed).
            let run1 = create(
                None,
                None,
                "boot-1".into(),
                None,
                3,
                &[step("a", "A", None), step("b", "B", None)],
                None,
            )
            .unwrap();
            let run1 = start(&run1.id).unwrap();
            let run1_id = run1.id.clone();
            // Mark step 0 as running (simulate in-flight at shutdown).
            let _ = step_state(
                &run1_id,
                &run1.steps[0].id,
                StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            );

            // Run 2: done (should NOT be resumed).
            let run2 = create(
                None,
                None,
                "boot-2".into(),
                None,
                3,
                &[step("c", "C", None)],
                None,
            )
            .unwrap();
            let run2_id = run2.id.clone();
            let _ = step_state(
                &run2_id,
                &run2.steps[0].id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            );

            // Remove both from active map (simulates restart).
            {
                let mut active = store_guard();
                active.remove(&run1_id);
                active.remove(&run2_id);
            }

            // Run startup_resume.
            let resumed = startup_resume();
            assert_eq!(resumed, 1, "only the running run should be resumed");

            // Run 1 should be running with step 0 reset to ready.
            let r1 = get_active(&run1_id).unwrap();
            assert_eq!(r1.status, "running");
            let s0 = r1.steps.iter().find(|s| s.id == run1.steps[0].id).unwrap();
            assert_eq!(s0.status, "ready");

            // Run 2 should NOT be in the active map (it was done, not restored).
            assert!(
                get_active(&run2_id).is_none(),
                "done run should not be restored into active map"
            );
        });
    }

    /// `interrupted` is in the ALLOWED_STATUS list so it can be persisted and read.
    #[test]
    fn interrupted_is_in_allowed_status() {
        assert!(
            ALLOWED_STATUS.contains(&"interrupted"),
            "interrupted must be an allowed step status"
        );
    }

    /// A run with `interrupted` status is NOT considered terminal by the
    /// `list_history` filter (it should be restorable).
    #[test]
    fn interrupted_run_is_listed_in_history() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "hist".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            // Mark the step as running so mark_interrupted has something to interrupt.
            let step_id = run.steps[0].id.clone();
            let _ = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            );
            let _ = mark_interrupted(&run.id);

            let history = list_history(None);
            let found = history.iter().find(|r| r.id == run.id).unwrap();
            assert_eq!(found.status, "interrupted");
        });
    }

    // ---- subagent message persistence + resume context tests ----

    /// `step_messages` persists the subagent's conversation history onto a step.
    #[test]
    fn step_messages_persists_conversation_history() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", None)];
            let run = create(None, None, "messages".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step0_id = run.steps[0].id.clone();

            // Initially, the step has no messages.
            assert!(get_active(&run.id).unwrap().steps[0].messages.is_none());

            // Mark step done (required so step_messages can find the step).
            let _ = step_state(
                &run.id,
                &step0_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("Done!".into()),
                    ..Default::default()
                },
            );

            // Persist messages as plain Values (no nested JSON-in-JSON).
            let messages: Vec<Value> = vec![
                json!({"role": "user", "content": "Task: do something"}),
                json!({"role": "assistant", "content": "I did it"}),
                json!({"role": "tool", "tool_call_id": "t1", "content": "tool output"}),
            ];

            let updated = step_messages(&run.id, &step0_id, messages.clone()).unwrap();
            let s0 = updated.steps.iter().find(|s| s.id == step0_id).unwrap();
            assert_eq!(s0.messages, Some(messages.clone()));

            // Messages survive a round-trip through the store (persist + reload).
            let reloaded = get_active(&run.id).unwrap();
            let s0 = reloaded.steps.iter().find(|s| s.id == step0_id).unwrap();
            assert_eq!(s0.messages, Some(messages));
        });
    }

    /// `step_messages` returns None for an unknown run.
    #[test]
    fn step_messages_returns_none_for_unknown_run() {
        with_tmp_workflows_file(|| {
            let result = step_messages("no-such-run", "step-id", vec![]);
            assert!(result.is_none());
        });
    }

    /// `build_resume_context` extracts messages from the last completed step.
    #[test]
    fn build_resume_context_extracts_completed_step_messages() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", None)];
            let run = create(None, None, "ctx".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step0_id = run.steps[0].id.clone();

            // Complete step 0 with messages.
            let messages = vec![
                json!({"role": "user", "content": "Task: alpha"}),
                json!({"role": "assistant", "content": "Result of alpha"}),
            ];
            let _ = step_state(
                &run.id,
                &step0_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("Result of alpha".into()),
                    ..Default::default()
                },
            );
            let _ = step_messages(&run.id, &step0_id, messages);

            // Build context from the run.
            let run_snapshot = get_active(&run.id).unwrap();
            let ctx = build_resume_context(&run_snapshot, 8192);
            assert!(ctx.is_some(), "should build context from completed step");
            let ctx = ctx.unwrap();
            assert!(ctx.contains("Prior context"), "should have header");
            assert!(ctx.contains("alpha"), "should reference the step's task");
            assert!(ctx.contains("Result of alpha"), "should include assistant output");
        });
    }

    /// `build_resume_context` returns None when no completed step has messages.
    #[test]
    fn build_resume_context_returns_none_when_no_messages() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "no-ctx".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Step is done but has no messages.
            let _ = step_state(
                &run.id,
                &run.steps[0].id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("output".into()),
                    ..Default::default()
                },
            );

            let run_snapshot = get_active(&run.id).unwrap();
            let ctx = build_resume_context(&run_snapshot, 8192);
            assert!(ctx.is_none(), "no messages → no context");
        });
    }

    /// `build_resume_context` includes tool-call one-liners so the resumed subagent
    /// can see which tools were already called.
    #[test]
    fn build_resume_context_includes_tool_call_details() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "tool-ctx".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step0_id = run.steps[0].id.clone();

            // Build a tool_call message where arguments is a JSON string.
            let tool_call = {
                let mut m = serde_json::Map::new();
                m.insert("id".to_string(), json!("call_1"));
                m.insert("type".to_string(), json!("function"));
                let mut func = serde_json::Map::new();
                func.insert("name".to_string(), json!("bash"));
                // arguments is a JSON-encoded string
                let args_str = serde_json::to_string(&json!({"command": "cat Cargo.toml"})).unwrap();
                func.insert("arguments".to_string(), Value::String(args_str));
                m.insert("function".to_string(), Value::Object(func));
                Value::Object(m)
            };
            let messages: Vec<Value> = vec![
                json!({"role": "assistant", "content": null, "tool_calls": [tool_call]}),
                json!({"role": "tool", "tool_call_id": "call_1", "content": "[package]\nname = dotz\n"}),
            ];
            let _ = step_state(
                &run.id,
                &step0_id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            );
            let _ = step_messages(&run.id, &step0_id, messages);

            let run_snapshot = get_active(&run.id).unwrap();
            let ctx = build_resume_context(&run_snapshot, 8192).unwrap();
            assert!(ctx.contains("tool_call(bash)"), "should include tool call name");
            assert!(ctx.contains("Cargo.toml"), "should include tool call args");
        });
    }

    /// Full lifecycle: step completes with messages → interrupt → resume → context is
    /// available for the interrupted step. This is the core resumability guarantee.
    #[test]
    fn resume_preserves_completed_step_messages_for_context() {
        with_tmp_workflows_file(|| {
            // Two independent steps: A (completes with messages) and B (interrupted).
            let inputs = vec![step("a", "Task A", None), step("b", "Task B", None)];
            let run = create(None, None, "resume-ctx".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step_a_id = run.steps[0].id.clone();
            let step_b_id = run.steps[1].id.clone();

            // Step A completes with messages.
            let a_messages = vec![
                json!({"role": "user", "content": "Task: Task A"}),
                json!({"role": "assistant", "content": "Completed task A"}),
            ];
            let _ = step_state(
                &run.id,
                &step_a_id,
                StepPatch {
                    status: Some("done".into()),
                    output: Some("Completed task A".into()),
                    ..Default::default()
                },
            );
            let _ = step_messages(&run.id, &step_a_id, a_messages);

            // Step B is running when the server shuts down.
            let _ = step_state(
                &run.id,
                &step_b_id,
                StepPatch {
                    status: Some("running".into()),
                    ..Default::default()
                },
            );

            // Simulate shutdown: mark interrupted.
            let _ = mark_interrupted(&run.id);
            let interrupted = get_active(&run.id).unwrap();
            assert_eq!(interrupted.status, "interrupted");
            let s_b = interrupted.steps.iter().find(|s| s.id == step_b_id).unwrap();
            assert_eq!(s_b.status, "interrupted");

            // Resume.
            let resumed = resume_sync(&run.id).unwrap();
            assert_eq!(resumed.status, "running");

            // Step B should be ready (reset from interrupted).
            let s_b = resumed.steps.iter().find(|s| s.id == step_b_id).unwrap();
            assert_eq!(s_b.status, "ready");

            // Step A's messages should still be accessible (for the executor to
            // build resume context).
            let s_a = resumed.steps.iter().find(|s| s.id == step_a_id).unwrap();
            assert!(s_a.messages.is_some(), "step A messages should survive resume");

            // build_resume_context should produce a non-empty string referencing
            // step A's output.
            let ctx = build_resume_context(&resumed, 8192);
            assert!(ctx.is_some(), "resume context should be available");
            assert!(ctx.unwrap().contains("Task A"), "context should reference step A");
        });
    }

    /// `build_resume_context` respects the max_bytes cap.
    #[test]
    fn build_resume_context_respects_max_bytes_cap() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "cap".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step0_id = run.steps[0].id.clone();

            // Large message content.
            let big_text = "x".repeat(5000);
            let messages = vec![
                json!({"role": "assistant", "content": big_text}),
            ];
            let _ = step_state(
                &run.id,
                &step0_id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            );
            let _ = step_messages(&run.id, &step0_id, messages);

            let run_snapshot = get_active(&run.id).unwrap();
            // Cap at 100 bytes — output must be truncated.
            let ctx = build_resume_context(&run_snapshot, 100).unwrap();
            assert!(ctx.len() <= 200, "context should be roughly capped: got {} bytes", ctx.len());
            assert!(ctx.contains("truncated"), "should indicate truncation");
        });
    }

    /// `build_resume_context` prefers the LAST completed step (reverse order).
    #[test]
    fn build_resume_context_prefers_last_completed_step() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "first", None),
                step("b", "second", None),
                step("c", "third", None),
            ];
            let run = create(None, None, "last-step".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Complete steps A and C (C is last). Leave B pending.
            for step_id in &[&run.steps[0].id, &run.steps[2].id] {
                let _ = step_state(
                    &run.id,
                    step_id,
                    StepPatch {
                        status: Some("done".into()),
                        output: Some("output".into()),
                        ..Default::default()
                    },
                );
            }
            // Give C distinct messages.
            let c_messages = vec![
                json!({"role": "assistant", "content": "third step output"}),
            ];
            let _ = step_messages(&run.id, &run.steps[2].id, c_messages);

            let run_snapshot = get_active(&run.id).unwrap();
            let ctx = build_resume_context(&run_snapshot, 8192).unwrap();
            // Should reference C ("third"), not A ("first").
            assert!(ctx.contains("third"), "should use the last completed step");
        });
    }
}
