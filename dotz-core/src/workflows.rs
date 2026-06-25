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
use crate::config::dotz_dir;
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
    /// "pending" | "ready" | "running" | "done" | "error" | "skipped".
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
    /// "pending" | "running" | "done" | "error" | "aborted".
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
}

// ---- create input (POST body steps) ----

#[derive(Debug, Deserialize)]
struct CreateStepInput {
    agent: String,
    task: String,
    #[serde(default)]
    parents: Option<Vec<Value>>,
    #[serde(rename = "sandboxRunId", default)]
    sandbox_run_id: Option<String>,
    #[serde(rename = "browserSessionId", default)]
    browser_session_id: Option<String>,
    #[serde(rename = "toolCallIds", default)]
    tool_call_ids: Option<Vec<String>>,
    #[serde(default)]
    thinking: Option<String>,
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
struct CycleError;

/// Create a new run with the given steps (parents/children resolved from inputs).
/// Returns Err(CycleError) when the submitted steps form a cycle.
fn create(
    project_id: Option<String>,
    session_id: Option<String>,
    label: String,
    origin: Option<String>,
    inputs: &[CreateStepInput],
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
fn start(id: &str) -> Option<WorkflowRun> {
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

/// A validated step-state patch (built by the POST handler).
#[derive(Default)]
struct StepPatch {
    status: Option<String>,
    output: Option<String>,
    error: Option<String>,
    usage: Option<Usage>,
}

/// Update a step's state and propagate readiness to children. Returns the updated run (clone).
fn step_state(run_id: &str, step_id: &str, patch: StepPatch) -> Option<WorkflowRun> {
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

/// Abort a run: mark aborted + sweep every non-terminal step to skipped.
fn abort(id: &str) -> Option<WorkflowRun> {
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

fn get_active(id: &str) -> Option<WorkflowRun> {
    store_guard().get(id).cloned()
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

    let run = match create(project_id, session_id, label, origin, &inputs) {
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

const ALLOWED_STATUS: [&str; 6] = ["pending", "ready", "running", "done", "error", "skipped"];

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

/// Register the /api/workflows routes with stateless handlers.
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
            let run = create(None, None, "test".into(), None, &inputs).unwrap();
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
                &[step("a", "A", None), step("b", "B", Some(vec![json!(0)]))],
            )
            .unwrap();
            let string = create(
                None,
                None,
                "string".into(),
                None,
                &[step("a", "A", None), step("b", "B", Some(vec![json!("0")]))],
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
            let run = create(None, None, "test".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "test".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "test".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "error-sweep".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "abort-sweep".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "skip".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "skip-cascade".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "skip-leaf".into(), None, &inputs).unwrap();
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
                    create(None, None, "cycle".into(), None, &inputs),
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
            let run = create(None, None, "dup".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "test".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "count".into(), None, &[step("a", "A", None)]).unwrap();
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
            let run = create(None, None, "events".into(), None, &inputs).unwrap();
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
            let run = create(None, None, "abort-events".into(), None, &inputs).unwrap();
            let _ = start(&run.id).unwrap();
            // Drain the workflow_start event.
            let _ = rx.try_recv();

            let updated = abort(&run.id).unwrap();
            let mut swept = 0;
            let mut seen_end = false;
            while let Ok(frame) = rx.try_recv() {
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
}
