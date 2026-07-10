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

/// A concrete artifact a step produced — the primary, inspectable result of a worker
/// step (and, in future, screenshots / file previews / etc). The UI node drawer
/// renders the artifact ABOVE the prose output so "the result" is a concrete
/// change the operator can review, not a summary they have to cross-reference.
///
/// The `kind` field discriminates the renderer:
///   - `git_diff`      → `content` is a unified diff string (already colored by the
///     UI's diff renderer).
///   - `patch_review`  → `content` is a reviewer's structured findings markdown;
///     rendered as a markdown block with approve/reject buttons.
///
/// `title` is a short label (e.g. "3 files changed" or "review round 1").
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
}

/// One tool call a step made — the graph renders each as a panel-colored sub-node (chip) on its
/// step node. `panel` is `panel_for_tool(tool_name)` (None for plain file/shell tools). This is the
/// durable, reload-surviving record; live chips also stream in via `step_tool` events mid-run.
///
/// `args` + `result` carry the inspectable payload so the graph node drawer is the single source
/// of truth for "what did this tool do?" — not just a chip that links out to the chat. Both are
/// truncated at [`ToolCallRef::CAP`] bytes on a char boundary to keep the workflow store + WS
/// frames bounded (a 5MB file read must never bloat the graph state).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRef {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    /// The raw arguments the agent passed (JSON-serialized, capped). Present from the `start`
    /// phase so the drawer shows the call shape the instant it fires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// The tool's textual result (capped). Populated at the `end` phase; None while running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl ToolCallRef {
    /// Max bytes of `args`/`result` retained. Generous enough for a real command/file path
    /// preview + a normal tool result, tight enough that a runaway tool never balloons the
    /// in-memory workflow store or the WS frame.
    pub const CAP: usize = 4096;

    /// Truncate `s` to `CAP` bytes on a UTF-8 char boundary, appending an ellipsis marker once.
    /// Used for `args`/`result` so the drawer shows useful content without unbounded storage.
    pub fn cap_str(s: &str) -> String {
        if s.len() <= Self::CAP {
            return s.to_string();
        }
        let mut end = Self::CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = s[..end].to_string();
        out.push_str("…[truncated]");
        out
    }
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
    /// Per-step model override: when set, the executor uses this model instead of the
    /// run's default (or the agent's default). Format: "provider/model-id".
    /// The UI node drawer surfaces a model picker that patches this field.
    #[serde(rename = "model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The concrete artifact this step produced. For worker steps this is the
    /// `git diff` of the cwd at completion; for review steps this is the
    /// reviewer's findings markdown. The UI renders this as the primary
    /// inspectable result — above the prose `output` — so the operator can
    /// review the actual change without cross-referencing a terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
    /// Absolute working dir for this step. When set, the executor runs the subagent here
    /// (the subagent→workflow bridge resolves each dispatched subagent's `cwd` — e.g. a pantheon
    /// episode dir — to an absolute path). When None, the executor's process cwd is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The tool calls this step made, panel-tagged — rendered as sub-node chips on the graph.
    /// Populated from the subagent's messages at completion; streamed live via `step_tool` events.
    #[serde(rename = "toolCalls", skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRef>>,
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

/// Pre-computed progress counts for a workflow run, so the UI can render a progress
/// bar / status line (`3/7 done · 1 running · 1 failed`) without walking the step DAG
/// on every render. Surfaced as a `summary` field on every REST response and WebSocket
/// event that carries a run (or a step-state delta).
///
/// `steps` is the total node count; `completed` counts terminal successes (`done`),
/// `failed` counts `error`, and `running` counts in-flight steps. The remaining counts
/// (`pending`, `ready`, `skipped`, `interrupted`) are included so the UI can distinguish
/// "not started" from "skipped" from "interrupted by restart" without iterating steps.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkflowSummary {
    pub steps: usize,
    pub completed: usize,
    pub failed: usize,
    pub running: usize,
    pub pending: usize,
    pub ready: usize,
    pub skipped: usize,
    pub interrupted: usize,
}

/// Compute a `WorkflowSummary` from a run's steps. O(n) over the step list; called once
/// per REST response or WS event so the UI never has to walk the DAG itself.
pub fn workflow_summary(run: &WorkflowRun) -> WorkflowSummary {
    let mut s = WorkflowSummary {
        steps: run.steps.len(),
        ..Default::default()
    };
    for step in &run.steps {
        match step.status.as_str() {
            "done" => s.completed += 1,
            "error" => s.failed += 1,
            "running" => s.running += 1,
            "pending" => s.pending += 1,
            "ready" => s.ready += 1,
            "skipped" => s.skipped += 1,
            "interrupted" => s.interrupted += 1,
            _ => {}
        }
    }
    s
}

/// Serialize a run to a JSON object and attach its pre-computed `summary`. Used by every
/// REST handler that returns a run so the UI gets progress counts in the same payload
/// — no second round-trip, no client-side DAG walk.
fn run_with_summary(run: &WorkflowRun) -> Value {
    let mut val = serde_json::to_value(run).unwrap_or_else(|_| json!({}));
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "summary".to_string(),
            serde_json::to_value(workflow_summary(run)).unwrap_or_else(|_| json!({})),
        );
    }
    val
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
    /// Per-step model override: "provider/model-id". When set, this step uses this
    /// model instead of the run/agent default.
    #[serde(default)]
    pub model: Option<String>,
    /// Absolute working dir for this step (the bridge resolves each subagent's cwd before create).
    #[serde(default)]
    pub cwd: Option<String>,
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

pub(crate) fn emit_event(run_id: &str, event: Value) {
    let _ = events_tx().send(json!({
        "kind": "workflow",
        "runId": run_id,
        "event": event,
    }));
}

fn emit_workflow_start(run: &WorkflowRun) {
    emit_event(
        &run.id,
        json!({ "type": "workflow_start", "run": run_with_summary(run) }),
    );
}

fn emit_workflow_end(run: &WorkflowRun) {
    emit_event(
        &run.id,
        json!({ "type": "workflow_end", "run": run_with_summary(run) }),
    );
}

fn emit_step_state(run_id: &str, step: &WorkflowStep, summary: Option<&WorkflowSummary>) {
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
    if let Some(tc) = &step.tool_calls {
        event["toolCalls"] = json!(tc);
    }
    if let Some(t) = &step.thinking {
        event["thinking"] = json!(t);
    }
    // Attach the run-level summary so the UI can update its progress bar on every
    // step-state transition without refetching the run or walking the DAG client-side.
    if let Some(s) = summary {
        event["summary"] = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
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
    let Ok(s) = serde_json::to_string_pretty(runs) else {
        return;
    };
    // Atomic write: serialize to a sibling temp file in the same directory, then rename over the
    // destination. `std::fs::write` truncates the target before writing, so a crash mid-write
    // would leave a truncated `workflows.json` that `read_all` silently drops — permanently
    // losing every workflow run's history and breaking `startup_resume` (which replays from this
    // file). The temp-then-rename dance means a crash at worst leaves the previous complete
    // history intact; the rename is atomic on both Unix and Windows (the std impl uses
    // MoveFileExW with MOVEFILE_REPLACE_EXISTING). Mirrors `run_record::write_unlocked`.
    let tmp = file.with_extension("json.tmp");
    if std::fs::write(&tmp, &s).is_ok() && std::fs::rename(&tmp, &file).is_err() {
        // Exotic cross-device / permission edge: fall back to a direct write so the history
        // is still persisted, accepting the non-atomic window only on that path.
        let _ = std::fs::write(&file, &s);
        let _ = std::fs::remove_file(&tmp);
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

/// Remove every workflow run belonging to a project — from the in-memory active store AND the
/// on-disk `workflows.json` history — and delete each removed run's run-record file. Returns the
/// number of distinct runs purged. Called when a project is deleted from dotz.
pub fn purge_project(project_id: &str) -> usize {
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut active = store().lock().unwrap_or_else(|e| e.into_inner());
        let doomed: Vec<String> = active
            .values()
            .filter(|r| r.project_id.as_deref() == Some(project_id))
            .map(|r| r.id.clone())
            .collect();
        for id in doomed {
            active.remove(&id);
            ids.insert(id);
        }
    }
    let kept: Vec<WorkflowRun> = read_all()
        .into_iter()
        .filter(|r| {
            if r.project_id.as_deref() == Some(project_id) {
                ids.insert(r.id.clone());
                false
            } else {
                true
            }
        })
        .collect();
    write_all(&kept);
    for id in &ids {
        crate::run_record::delete(id);
    }
    ids.len()
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
            model: s.model.clone(),
            artifact: None,
            cwd: s.cwd.clone(),
            tool_calls: None,
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
    pub artifact: Option<Artifact>,
    pub tool_calls: Option<Vec<ToolCallRef>>,
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
            if patch.artifact.is_some() && step.artifact != patch.artifact {
                step.artifact = patch.artifact.clone();
                changed.insert(step.id.clone());
            }
            if patch.tool_calls.is_some() && step.tool_calls != patch.tool_calls {
                step.tool_calls = patch.tool_calls.clone();
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
                    model: None,
                    artifact: None,
                    cwd: None,
                    tool_calls: None,
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
                    model: None,
                    artifact: None,
                    cwd: None,
                    tool_calls: None,
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
    let summary = workflow_summary(&run_snapshot.0);
    for step in &run_snapshot.0.steps {
        if run_snapshot.1.contains(&step.id) {
            emit_step_state(run_id, step, Some(&summary));
        }
    }
    if run_snapshot.2 {
        emit_workflow_end(&run_snapshot.0);
    }
    Some(run_snapshot.0)
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
    let summary = workflow_summary(&run.0);
    for step in &run.0.steps {
        if run.1.contains(&step.id) {
            emit_step_state(id, step, Some(&summary));
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
                                p.status == "done" || p.status == "skipped" || p.status == "error"
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
        .filter(|r| r.status != "done" && r.status != "error" && r.status != "aborted")
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
        if (interrupted.unwrap_or(0) > 0 || restored.status == "running")
            && resume(&restored.id).is_some()
        {
            resumed += 1;
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
    let runs: Vec<Value> = list_history(q.project_id.as_deref())
        .iter()
        .map(run_with_summary)
        .collect();
    Json(json!({ "runs": runs }))
}

/// GET /api/workflows/active → { runs: [...] } (in-memory active map).
async fn list_active_handler() -> Json<Value> {
    let runs: Vec<Value> = list_active().iter().map(run_with_summary).collect();
    Json(json!({ "runs": runs }))
}

/// GET /api/workflows/:id → run (active, else history fallback) or 404.
/// The run carries a pre-computed `summary` so the UI can render progress counts
/// (`3/7 done · 1 running · 1 failed`) without walking the step DAG.
async fn get_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Some(run) = get_active(&id) {
        return Ok(Json(run_with_summary(&run)));
    }
    if let Some(run) = list_history(None).into_iter().find(|r| r.id == id) {
        return Ok(Json(run_with_summary(&run)));
    }
    Err(not_found("no such workflow run"))
}

/// POST /api/workflows → validate DAG (cycle → 400), assign step ids, start, return the run.
async fn create_handler(
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
    let run_budget = body
        .get("budget")
        .and_then(|v| serde_json::from_value::<Budget>(v.clone()).ok());

    let run = match create(
        project_id,
        session_id,
        label,
        origin,
        max_repair_rounds,
        &inputs,
        run_budget,
    ) {
        Ok(run) => run,
        Err(CycleError) => return Err(bad("workflow steps form a cycle")),
    };

    // Mark started (mirrors server.ts: create() then start()).
    let started = start(&run.id).unwrap_or(run);
    Ok(Json(run_with_summary(&started)))
}

#[derive(Deserialize)]
struct StepBody {
    #[serde(rename = "stepId")]
    step_id: Option<String>,
    status: Option<String>,
    output: Option<String>,
    error: Option<String>,
    usage: Option<Usage>,
    artifact: Option<Artifact>,
}

const ALLOWED_STATUS: [&str; 7] = [
    "pending",
    "ready",
    "running",
    "done",
    "error",
    "skipped",
    "interrupted",
];

/// POST /api/workflows/:id/step → update a step + propagate, return the run.
/// 404 unknown run/step, 409 finished run, 400 bad stepId/status.
async fn step_handler(
    Path(id): Path<String>,
    body: Option<Json<StepBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
            return Err(bad(
                "interrupted is a server-initiated status; use POST /:id/resume to resume",
            ));
        }
    }

    let patch = StepPatch {
        status: body.status,
        output: body.output,
        error: body.error,
        usage: body.usage,
        artifact: body.artifact,
        tool_calls: None,
    };
    match step_state(&id, &step_id, patch) {
        Some(updated) => Ok(Json(run_with_summary(&updated))),
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
async fn resume_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
        Some(run) => Ok(Json(run_with_summary(&run))),
        None => Err(not_found("no such workflow run")),
    }
}

/// POST /api/workflows/:id/execute → drive the run to completion via real subagent dispatch.
/// Returns { run: ... } with the final run state. 404 for unknown run.
/// Creates a git-backed checkpoint (if the project is in a git repo) before executing,
/// so a failed run can be rolled back via POST /:id/rollback.
async fn execute_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // If the run is already terminal, return it directly without re-executing.
    if let Some(run) = get_active(&id) {
        if run.status == "done" || run.status == "error" || run.status == "aborted" {
            return Ok(Json(json!({ "run": run_with_summary(&run) })));
        }
    } else {
        return Err(not_found("no such workflow run"));
    }

    let checkpoint = match crate::checkpoint::save_checkpoint(
        &id,
        &std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy(),
    ) {
        Ok(sha) => Some(sha),
        Err(crate::checkpoint::CheckpointError::NotAGitRepo) => None, // graceful degradation
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.message() })),
            ))
        }
    };

    match crate::workflow_executor::run_workflow(&id).await {
        Some(run) => Ok(Json(
            json!({ "run": run_with_summary(&run), "checkpoint": checkpoint }),
        )),
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

/// GET /api/workflows/:id/record → the full reproducible run record (prompt + model +
/// thinking + tools + skill set + provider response messages per step). 404 when no
/// record was captured (the run predates recording, or recording failed). The UI
/// renders this in a "Run Record" drawer alongside the graph so the operator can
/// inspect exactly what each agent saw, thought, called, and got back — without
/// re-running the workflow.
async fn record_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match crate::run_record::load(&id) {
        Some(record) => Ok(Json(
            serde_json::to_value(&record).unwrap_or_else(|_| json!({})),
        )),
        None => Err(not_found("no run record for this workflow")),
    }
}

/// POST /api/workflows/:id/replay → rebuild a fresh run from the captured record
/// (same agent/task/model/parents/thinking/auto_repair/budget) and re-execute it.
/// Returns the newly-created (pending) run; the executor is spawned so the UI
/// watches the live `pending → running → …` transition over WebSocket. 404 when
/// no record exists or it has no steps. This is the orchestration-regression bisect:
/// replay a recorded run after a code/provider/config change and diff the new
/// record against the old to localize which step drifted.
async fn replay_handler(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match crate::run_record::replay(&id) {
        Some(run) => Ok(Json(run_with_summary(&run))),
        None => Err(not_found("no replayable run record (missing or empty)")),
    }
}

// ---- live-editable handlers ----

/// Input for rerunning a failed step with optional feedback appended to its task.
#[derive(Debug, Deserialize)]
struct RerunBody {
    #[serde(default)]
    feedback: Option<String>,
}

/// POST /api/workflows/:id/step/:stepId/rerun — reset a failed (or any non-running) step
/// to `ready` so the executor will re-dispatch it. Optional feedback is appended to
/// the step's task so the subagent sees the operator's guidance. Returns the updated run.
///
/// This is the primary "steer" mechanism: the operator inspects a failed step's error
/// in the node drawer, types feedback, and re-runs. The step's prior output/error/usage
/// are cleared so the subagent starts fresh; the feedback is appended as a new section.
async fn rerun_step_handler(
    Path((id, step_id)): Path<(String, String)>,
    body: Option<Json<RerunBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let run = match get_active(&id) {
        Some(r) => r,
        None => return Err(not_found("no such workflow run")),
    };
    // Only allow rerun on a run that is still active (not terminal).
    if run.status == "done" || run.status == "aborted" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("run is {} — cannot rerun a step", run.status) })),
        ));
    }
    // Find the step.
    let step = match run.steps.iter().find(|s| s.id == step_id) {
        Some(s) => s.clone(),
        None => return Err(not_found("no such step")),
    };
    // Cannot rerun a step that is currently running.
    if step.status == "running" {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({ "error": "step is currently running — wait for it to complete or abort the run" }),
            ),
        ));
    }

    // Build the new task: append feedback if provided.
    let new_task = match body {
        Some(Json(RerunBody { feedback: Some(fb) })) if !fb.trim().is_empty() => {
            format!(
                "{}

[Operator feedback for rerun]
{}",
                step.task,
                fb.trim()
            )
        }
        _ => step.task.clone(),
    };

    // Reset the step to ready: clear error, output, usage, timestamps. Keep the
    // new task (with appended feedback) and the agent/model/budget.
    let patch = StepPatch {
        status: Some("ready".into()),
        output: None,
        error: None,
        usage: None,
        artifact: None,
        tool_calls: None,
    };
    // Apply the state change first.
    let updated = match step_state(&id, &step_id, patch) {
        Some(r) => r,
        None => return Err(not_found("no such workflow run")),
    };
    // Update the task in-memory (step_state doesn't touch task). We need a direct
    // mutation on the store. Use a targeted approach: update the active map.
    {
        let mut active = store_guard();
        if let Some(run) = active.get_mut(&id) {
            if let Some(s) = run.steps.iter_mut().find(|s| s.id == step_id) {
                s.task = new_task;
                s.started_at = None;
                s.ended_at = None;
            }
            run.updated_at = now_ms();
            persist(run);
            // Emit a step_state event so the UI reflects the reset + new task.
            let step = run.steps.iter().find(|s| s.id == step_id).cloned().unwrap();
            let summary = workflow_summary(run);
            emit_step_state(&id, &step, Some(&summary));
            return Ok(Json(run_with_summary(run)));
        }
    }
    // Fallback (shouldn't reach): return the step_state result.
    Ok(Json(run_with_summary(&updated)))
}

/// Input for patching a step's parents (re-wiring dependencies).
#[derive(Debug, Deserialize)]
struct PatchStepBody {
    #[serde(default)]
    parents: Option<Vec<Value>>,
    #[serde(default)]
    model: Option<String>,
}

/// POST /api/workflows/:id/step/:stepId/patch — re-wire a step's parents and/or
/// update its model. Re-validates the DAG (cycle detection) and re-propagates
/// readiness. Returns the updated run.
///
/// This is the "re-wire" mechanism: the operator drags edges in the graph panel
/// to change dependencies. A step that was pending on step A can be reparented to
/// step B, or made a root node (empty parents). The engine re-checks for cycles
/// before committing the change.
async fn patch_step_handler(
    Path((id, step_id)): Path<(String, String)>,
    body: Option<Json<PatchStepBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body = match body {
        Some(Json(b)) => b,
        None => return Err(bad("body is required")),
    };
    let run = match get_active(&id) {
        Some(r) => r,
        None => return Err(not_found("no such workflow run")),
    };
    if run.status == "done" || run.status == "aborted" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("run is {} — cannot patch a step", run.status) })),
        ));
    }
    // Find the step index.
    let step_idx = match run.steps.iter().position(|s| s.id == step_id) {
        Some(i) => i,
        None => return Err(not_found("no such step")),
    };

    // Build a proposed parent set.
    let proposed_parents: Vec<String> = match &body.parents {
        Some(refs) => {
            let len = run.steps.len();
            let mut resolved = Vec::new();
            for raw in refs {
                let resolved_id = match raw {
                    Value::Number(n) => match n.as_i64() {
                        Some(i) if i >= 0 && (i as usize) < len => run.steps[i as usize].id.clone(),
                        _ => continue,
                    },
                    Value::String(s) => {
                        let trimmed = s.trim();
                        let is_index =
                            !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit());
                        if is_index {
                            match trimmed.parse::<usize>() {
                                Ok(n) if n < len => run.steps[n].id.clone(),
                                _ => continue,
                            }
                        } else if run.steps.iter().any(|st| st.id == trimmed) {
                            trimmed.to_string()
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };
                if resolved_id != step_id && !resolved.contains(&resolved_id) {
                    resolved.push(resolved_id);
                }
            }
            resolved
        }
        None => run.steps[step_idx].parents.clone(), // keep existing
    };

    // Cycle detection: temporarily set parents, run topological sort, reject if cyclic.
    {
        let mut test_run = run.clone();
        test_run.steps[step_idx].parents = proposed_parents.clone();
        // Rebuild children from parents.
        // Clear existing children references for this step.
        let step_id_c = step_id.clone();
        for s in test_run.steps.iter_mut() {
            s.children.retain(|c| c != &step_id_c);
        }
        // Re-add children based on new parents.
        let new_parents = proposed_parents.clone();
        for pid in &new_parents {
            if let Some(p) = test_run.steps.iter_mut().find(|s| &s.id == pid) {
                if !p.children.contains(&step_id_c) {
                    p.children.push(step_id_c.clone());
                }
            }
        }
        // Kahn's algorithm.
        let mut indeg: HashMap<String, isize> = test_run
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.parents.len() as isize))
            .collect();
        let mut queue: Vec<String> = test_run
            .steps
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
            if let Some(step) = test_run.steps.iter().find(|s| s.id == current) {
                for child_id in step.children.clone() {
                    let d = indeg.get(&child_id).copied().unwrap_or(0) - 1;
                    indeg.insert(child_id.clone(), d);
                    if d == 0 {
                        queue.push(child_id);
                    }
                }
            }
        }
        if ordered < test_run.steps.len() {
            return Err(bad("proposed parents form a cycle"));
        }
    }

    // Commit: update parents, children, model, and re-propagate readiness.
    let new_model = body.model.clone();
    let updated = {
        let mut active = store_guard();
        let run = match active.get_mut(&id) {
            Some(r) => r,
            None => return Err(not_found("no such workflow run")),
        };
        let now = now_ms();
        // Update children references: remove old, add new.
        let step_id_c = step_id.clone();
        for s in run.steps.iter_mut() {
            s.children.retain(|c| c != &step_id_c);
        }
        // Determine the new status BEFORE mutating the step (avoids borrow conflict).
        let new_status = {
            let step = run.steps.iter().find(|s| s.id == step_id).unwrap();
            let all_parents_terminal = step.parents.iter().all(|pid| {
                run.steps
                    .iter()
                    .find(|s| &s.id == pid)
                    .map(|p| p.status == "done" || p.status == "skipped" || p.status == "error")
                    .unwrap_or(false)
            });
            if (step.status == "pending" && (all_parents_terminal || step.parents.is_empty()))
                || (step.status == "error" && body.parents.is_some())
            {
                Some("ready")
            } else {
                None
            }
        };
        if let Some(step) = run.steps.iter_mut().find(|s| s.id == step_id) {
            step.parents = proposed_parents.clone();
            if let Some(m) = new_model {
                step.model = if m.is_empty() { None } else { Some(m) };
            }
            if let Some(s) = new_status {
                step.status = s.to_string();
            }
            if step.status == "ready" && body.parents.is_some() {
                step.error = None;
            }
        }
        // Re-add children for new parents.
        for pid in &proposed_parents {
            if let Some(p) = run.steps.iter_mut().find(|s| &s.id == pid) {
                if !p.children.contains(&step_id_c) {
                    p.children.push(step_id_c.clone());
                }
            }
        }
        run.updated_at = now;
        // If the run was errored and we just reparented something, set it back to
        // running so the executor can pick up the newly-ready step.
        if run.status == "error" && body.parents.is_some() {
            run.status = "running".to_string();
            run.ended_at = None;
        }
        persist(run);
        let run = run.clone();
        // Emit step_state for the patched step.
        let step = run.steps.iter().find(|s| s.id == step_id).cloned().unwrap();
        let summary = workflow_summary(&run);
        emit_step_state(&id, &step, Some(&summary));
        run_with_summary(&run)
    };
    Ok(Json(updated))
}

/// Input for injecting steps into an existing run.
#[derive(Debug, Deserialize)]
struct InsertStepsBody {
    steps: Vec<CreateStepInput>,
}

/// POST /api/workflows/:id/insert — inject new steps into an existing run.
/// The new steps are appended to the run's DAG with their parent refs resolved
/// against the EXISTING steps (indices 0..existing_count). Readiness is
/// propagated so newly-injected ready steps get picked up by the executor.
///
/// This is the "inject a manual step" mechanism: the operator adds a human-
/// authored step mid-run (e.g. "run the tests manually and paste output")
/// and the executor picks it up without restarting the run.
async fn insert_steps_handler(
    Path(id): Path<String>,
    body: Option<Json<InsertStepsBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body = match body {
        Some(Json(b)) => b,
        None => return Err(bad("steps (non-empty array) is required")),
    };
    if body.steps.is_empty() {
        return Err(bad("steps (non-empty array) is required"));
    }
    let run = match get_active(&id) {
        Some(r) => r,
        None => return Err(not_found("no such workflow run")),
    };
    if run.status == "done" || run.status == "aborted" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("run is {} — cannot insert steps", run.status) })),
        ));
    }

    let existing_count = run.steps.len();
    let now = now_ms();
    let mut new_steps: Vec<WorkflowStep> = Vec::with_capacity(body.steps.len());
    let mut new_step_ids: Vec<String> = Vec::with_capacity(body.steps.len());

    for input in &body.steps {
        let id = new_id();
        new_step_ids.push(id.clone());
        let parents = match &input.parents {
            Some(refs) => {
                let mut resolved = Vec::new();
                for raw in refs {
                    let resolved_id = match raw {
                        Value::Number(n) => match n.as_i64() {
                            Some(i) if i >= 0 && (i as usize) < existing_count => {
                                run.steps[i as usize].id.clone()
                            }
                            _ => continue,
                        },
                        Value::String(s) => {
                            let trimmed = s.trim();
                            let is_index =
                                !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit());
                            if is_index {
                                match trimmed.parse::<usize>() {
                                    Ok(n) if n < existing_count => run.steps[n].id.clone(),
                                    _ => continue,
                                }
                            } else if let Some(idx) =
                                new_step_ids.iter().position(|sid| sid == trimmed)
                            {
                                // Allow refs to other newly-inserted steps by their
                                // assigned id (the UI may reference a just-added step).
                                // We map positional refs to ids above; literal refs
                                // to new-step ids are resolved after all ids are known.
                                run.steps
                                    .iter()
                                    .find(|st| st.id == trimmed)
                                    .map(|st| st.id.clone())
                                    .unwrap_or_else(|| new_step_ids[idx].clone())
                            } else if run.steps.iter().any(|st| st.id == trimmed) {
                                trimmed.to_string()
                            } else {
                                continue;
                            }
                        }
                        _ => continue,
                    };
                    if !resolved.contains(&resolved_id) {
                        resolved.push(resolved_id);
                    }
                }
                resolved
            }
            None => Vec::new(),
        };
        let status = if parents.is_empty() {
            "ready".to_string()
        } else {
            "pending".to_string()
        };
        new_steps.push(WorkflowStep {
            id,
            agent: input.agent.clone(),
            task: input.task.clone(),
            status,
            parents,
            children: Vec::new(),
            output: None,
            error: None,
            usage: None,
            sandbox_run_id: input.sandbox_run_id.clone(),
            browser_session_id: input.browser_session_id.clone(),
            tool_call_ids: input.tool_call_ids.clone(),
            thinking: input.thinking.clone(),
            started_at: None,
            ended_at: None,
            auto_repair: input.auto_repair,
            repair_round: 0,
            budget: input.budget.clone(),
            actual_cost: None,
            actual_tokens: None,
            model: input.model.clone(),
            artifact: None,
            cwd: input.cwd.clone(),
            tool_calls: None,
        });
    }

    // Commit: append new steps, resolve children, propagate readiness, emit events.
    let updated = {
        let mut active = store_guard();
        let run = match active.get_mut(&id) {
            Some(r) => r,
            None => return Err(not_found("no such workflow run")),
        };
        // Append new steps.
        for step in &new_steps {
            run.steps.push(step.clone());
        }
        // Rebuild children references for all parents (existing + new).
        // Clear and rebuild children from parents to keep consistency.
        for s in run.steps.iter_mut() {
            s.children.clear();
        }
        let parent_pairs: Vec<(String, Vec<String>)> = run
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.parents.clone()))
            .collect();
        for (child_id, parents) in &parent_pairs {
            for pid in parents {
                if let Some(p) = run.steps.iter_mut().find(|s| &s.id == pid) {
                    if !p.children.contains(child_id) {
                        p.children.push(child_id.clone());
                    }
                }
            }
        }
        // Re-propagate readiness: any pending step whose parents are all done/terminal
        // becomes ready. Collect ids first to avoid borrow conflict.
        let mut changed = HashSet::new();
        let pending_to_ready: Vec<String> = run
            .steps
            .iter()
            .filter(|s| s.status == "pending")
            .filter(|s| {
                s.parents.is_empty()
                    || s.parents.iter().all(|pid| {
                        run.steps
                            .iter()
                            .find(|st| &st.id == pid)
                            .map(|p| {
                                p.status == "done" || p.status == "skipped" || p.status == "error"
                            })
                            .unwrap_or(false)
                    })
            })
            .map(|s| s.id.clone())
            .collect();
        for step in run.steps.iter_mut() {
            if pending_to_ready.contains(&step.id) {
                step.status = "ready".to_string();
                changed.insert(step.id.clone());
            }
        }
        // If the run was errored and we added steps, set it back to running.
        if run.status == "error" {
            run.status = "running".to_string();
            run.ended_at = None;
        }
        run.updated_at = now;
        persist(run);
        let run = run.clone();
        // Emit step_state events for every new/changed step so the UI graph updates.
        let summary = workflow_summary(&run);
        for step in &run.steps {
            if new_step_ids.contains(&step.id) || changed.contains(&step.id) {
                emit_step_state(&id, step, Some(&summary));
            }
        }
        run_with_summary(&run)
    };
    Ok(Json(updated))
}

// Public helpers (used by handlers AND tests) — these encapsulate the store
// operations so the logic is testable without a full axum test server.

/// Patch a step's parents. Returns Err(CycleError) if the proposed parents form a cycle.
pub fn patch_parents(
    run_id: &str,
    step_id: &str,
    proposed_parents: Vec<Value>,
) -> Result<WorkflowRun, CycleError> {
    let run = get_active(run_id).ok_or(CycleError)?;
    let step_idx = run
        .steps
        .iter()
        .position(|s| s.id == step_id)
        .ok_or(CycleError)?;
    let len = run.steps.len();

    let resolved: Vec<String> = proposed_parents
        .iter()
        .filter_map(|raw| match raw {
            Value::Number(n) => match n.as_i64() {
                Some(i) if i >= 0 && (i as usize) < len => Some(run.steps[i as usize].id.clone()),
                _ => None,
            },
            Value::String(s) => {
                let trimmed = s.trim();
                let is_index = !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit());
                if is_index {
                    match trimmed.parse::<usize>() {
                        Ok(n) if n < len => Some(run.steps[n].id.clone()),
                        _ => None,
                    }
                } else if run.steps.iter().any(|st| st.id == trimmed) {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    // Validate no self-ref.
    let resolved: Vec<String> = resolved.into_iter().filter(|id| id != step_id).collect();

    // Cycle detection.
    {
        let mut test_run = run.clone();
        test_run.steps[step_idx].parents = resolved.clone();
        let mut indeg: HashMap<String, isize> = test_run
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.parents.len() as isize))
            .collect();
        let mut queue: Vec<String> = test_run
            .steps
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
            if let Some(step) = test_run.steps.iter().find(|s| s.id == current) {
                for child_id in step.children.clone() {
                    let d = indeg.get(&child_id).copied().unwrap_or(0) - 1;
                    indeg.insert(child_id.clone(), d);
                    if d == 0 {
                        queue.push(child_id);
                    }
                }
            }
        }
        if ordered < test_run.steps.len() {
            return Err(CycleError);
        }
    }

    // Commit.
    let updated = {
        let mut active = store_guard();
        let run = active.get_mut(run_id).ok_or(CycleError)?;
        let now = now_ms();
        // Rebuild children references.
        let step_id_c = step_id.to_string();
        for s in run.steps.iter_mut() {
            s.children.retain(|c| c != &step_id_c);
        }
        // Apply the parent change FIRST, THEN determine the new status based on
        // the updated parents. This avoids a stale-read bug where the old parents
        // would incorrectly keep the step pending.
        // Compute all_terminal BEFORE the mutable borrow: read the statuses of the
        // proposed parents from the current step list (before we mutate parents).
        let all_terminal = resolved.iter().all(|pid| {
            run.steps
                .iter()
                .find(|s| &s.id == pid)
                .map(|p| p.status == "done" || p.status == "skipped" || p.status == "error")
                .unwrap_or(false)
        });
        if let Some(step) = run.steps.iter_mut().find(|s| s.id == step_id) {
            step.parents = resolved.clone();
            if step.status == "pending" && (all_terminal || step.parents.is_empty()) {
                step.status = "ready".to_string();
            }
            // If the step was errored and we just rewired it, reset to ready so
            // the operator's re-wire can be acted on.
            if step.status == "error" {
                step.status = "ready".to_string();
                step.error = None;
            }
            if step.status == "ready" {
                step.error = None;
            }
        }
        for pid in &resolved {
            if let Some(p) = run.steps.iter_mut().find(|s| &s.id == pid) {
                if !p.children.contains(&step_id_c) {
                    p.children.push(step_id_c.clone());
                }
            }
        }
        if run.status == "error" {
            run.status = "running".to_string();
            run.ended_at = None;
        }
        run.updated_at = now;
        persist(run);
        let step = run.steps.iter().find(|s| s.id == step_id).cloned().unwrap();
        let summary = workflow_summary(run);
        emit_step_state(run_id, &step, Some(&summary));
        run.clone()
    };
    Ok(updated)
}

/// Insert new steps into an existing run. Parent refs resolve against existing steps
/// (indices 0..existing_count). Returns the updated run.
pub fn insert_steps(run_id: &str, inputs: &[CreateStepInput]) -> Result<WorkflowRun, CycleError> {
    let run = get_active(run_id).ok_or(CycleError)?;
    let existing_count = run.steps.len();
    let now = now_ms();
    let mut new_step_ids: Vec<String> = Vec::with_capacity(inputs.len());

    let new_steps: Vec<WorkflowStep> = inputs
        .iter()
        .map(|input| {
            let id = new_id();
            new_step_ids.push(id.clone());
            let parents = match &input.parents {
                Some(refs) => {
                    let mut resolved = Vec::new();
                    for raw in refs {
                        let resolved_id = match raw {
                            Value::Number(n) => match n.as_i64() {
                                Some(i) if i >= 0 && (i as usize) < existing_count => {
                                    run.steps[i as usize].id.clone()
                                }
                                _ => continue,
                            },
                            Value::String(s) => {
                                let trimmed = s.trim();
                                let is_index = !trimmed.is_empty()
                                    && trimmed.chars().all(|c| c.is_ascii_digit());
                                if is_index {
                                    match trimmed.parse::<usize>() {
                                        Ok(n) if n < existing_count => run.steps[n].id.clone(),
                                        _ => continue,
                                    }
                                } else if run.steps.iter().any(|st| st.id == trimmed) {
                                    trimmed.to_string()
                                } else {
                                    continue;
                                }
                            }
                            _ => continue,
                        };
                        if !resolved.contains(&resolved_id) {
                            resolved.push(resolved_id);
                        }
                    }
                    resolved
                }
                None => Vec::new(),
            };
            let status = if parents.is_empty() {
                "ready".to_string()
            } else {
                "pending".to_string()
            };
            WorkflowStep {
                id,
                agent: input.agent.clone(),
                task: input.task.clone(),
                status,
                parents,
                children: Vec::new(),
                output: None,
                error: None,
                usage: None,
                sandbox_run_id: input.sandbox_run_id.clone(),
                browser_session_id: input.browser_session_id.clone(),
                tool_call_ids: input.tool_call_ids.clone(),
                thinking: input.thinking.clone(),
                started_at: None,
                ended_at: None,
                auto_repair: input.auto_repair,
                repair_round: 0,
                budget: input.budget.clone(),
                actual_cost: None,
                actual_tokens: None,
                model: input.model.clone(),
                artifact: None,
                cwd: input.cwd.clone(),
                tool_calls: None,
            }
        })
        .collect();

    let updated = {
        let mut active = store_guard();
        let run = active.get_mut(run_id).ok_or(CycleError)?;
        for step in &new_steps {
            run.steps.push(step.clone());
        }
        // Rebuild children.
        for s in run.steps.iter_mut() {
            s.children.clear();
        }
        let parent_pairs: Vec<(String, Vec<String>)> = run
            .steps
            .iter()
            .map(|s| (s.id.clone(), s.parents.clone()))
            .collect();
        for (child_id, parents) in &parent_pairs {
            for pid in parents {
                if let Some(p) = run.steps.iter_mut().find(|s| &s.id == pid) {
                    if !p.children.contains(child_id) {
                        p.children.push(child_id.clone());
                    }
                }
            }
        }
        // Propagate readiness. Collect ids first to avoid borrow conflict.
        let pending_to_ready: Vec<String> = run
            .steps
            .iter()
            .filter(|s| s.status == "pending")
            .filter(|s| {
                s.parents.is_empty()
                    || s.parents.iter().all(|pid| {
                        run.steps
                            .iter()
                            .find(|st| &st.id == pid)
                            .map(|p| {
                                p.status == "done" || p.status == "skipped" || p.status == "error"
                            })
                            .unwrap_or(false)
                    })
            })
            .map(|s| s.id.clone())
            .collect();
        for step in run.steps.iter_mut() {
            if pending_to_ready.contains(&step.id) {
                step.status = "ready".to_string();
            }
        }
        if run.status == "error" {
            run.status = "running".to_string();
            run.ended_at = None;
        }
        run.updated_at = now;
        persist(run);
        let summary = workflow_summary(run);
        for step in run.steps.iter() {
            if new_step_ids.contains(&step.id) {
                emit_step_state(run_id, step, Some(&summary));
            }
        }
        run.clone()
    };
    Ok(updated)
}

/// Patch a step's model. Pass None to clear the override.
pub fn patch_model(
    run_id: &str,
    step_id: &str,
    model: Option<&str>,
) -> Result<WorkflowRun, CycleError> {
    let updated = {
        let mut active = store_guard();
        let run = active.get_mut(run_id).ok_or(CycleError)?;
        let now = now_ms();
        if let Some(step) = run.steps.iter_mut().find(|s| s.id == step_id) {
            step.model = model.map(|s| s.to_string());
        }
        run.updated_at = now;
        persist(run);
        let step = run.steps.iter().find(|s| s.id == step_id).cloned().unwrap();
        let summary = workflow_summary(run);
        emit_step_state(run_id, &step, Some(&summary));
        run.clone()
    };
    Ok(updated)
}

/// Reset a step to ready (clearing error/output/usage) and dispatch the executor.
/// Called by the WebSocket `workflow.rerun` handler. The feedback, if provided, is
/// appended to the step's task before dispatch.
pub async fn rerun_step_and_dispatch(
    run_id: &str,
    step_id: &str,
    feedback: Option<String>,
) -> Option<WorkflowRun> {
    let run = get_active(run_id)?;
    if run.status == "done" || run.status == "aborted" {
        return Some(run);
    }
    // Find the step.
    let step = run.steps.iter().find(|s| s.id == step_id)?.clone();
    if step.status == "running" {
        return Some(run);
    }
    // Build the new task with appended feedback.
    let new_task = match feedback {
        Some(fb) if !fb.trim().is_empty() => {
            format!(
                "{}\n\n[Operator feedback for rerun]\n{}",
                step.task,
                fb.trim()
            )
        }
        _ => step.task.clone(),
    };
    // Reset the step via step_state (status → ready, clears error/output/usage
    // when we pass Some("") — but step_state only touches fields that are Some,
    // so we need to clear via direct store mutation after).
    let _ = step_state(
        run_id,
        step_id,
        StepPatch {
            status: Some("ready".into()),
            ..Default::default()
        },
    );
    // Directly clear error/output/usage and update task.
    {
        let mut active = store_guard();
        let run = active.get_mut(run_id)?;
        let now = now_ms();
        if let Some(s) = run.steps.iter_mut().find(|s| s.id == step_id) {
            s.task = new_task;
            s.error = None;
            s.output = None;
            s.usage = None;
            s.started_at = None;
            s.ended_at = None;
        }
        // If the run was errored, set it back to running so the executor can
        // pick up the newly-ready step.
        if run.status == "error" {
            run.status = "running".to_string();
            run.ended_at = None;
        }
        run.updated_at = now;
        persist(run);
        let step = run.steps.iter().find(|s| s.id == step_id).cloned().unwrap();
        let summary = workflow_summary(run);
        emit_step_state(run_id, &step, Some(&summary));
    }
    // Spawn the executor to drive the run.
    let rid = run_id.to_string();
    tokio::spawn(async move {
        let _ = crate::workflow_executor::run_workflow(&rid).await;
    });
    get_active(run_id)
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
        .route(
            "/api/workflows/{id}/step/{step_id}/rerun",
            post(rerun_step_handler),
        )
        .route(
            "/api/workflows/{id}/step/{step_id}/patch",
            post(patch_step_handler),
        )
        .route("/api/workflows/{id}/insert", post(insert_steps_handler))
        // Reproducible run record: GET the captured record, POST /replay to rebuild
        // it into a fresh run (debugging + orchestration-regression bisect).
        .route("/api/workflows/{id}/record", get(record_handler))
        .route("/api/workflows/{id}/replay", post(replay_handler))
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
            model: None,
            cwd: None,
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
            model: None,
            cwd: None,
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
            let run = create(
                None,
                None,
                "count".into(),
                None,
                3,
                &[step("a", "A", None)],
                None,
            )
            .unwrap();
            assert_eq!(
                active_count(),
                baseline + 1,
                "active_count should include the new run"
            );

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
                count,
                guard.len(),
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

            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
            let run = create(None, None, "events".into(), None, 3, &inputs, None).unwrap();
            let child_id = run.steps[1].id.clone();

            // start() emits workflow_start.
            let started = start(&run.id).unwrap();
            let start_frame = rx
                .try_recv()
                .expect("workflow_start event should be broadcast");
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
            assert!(
                seen_end,
                "workflow_end should be broadcast when run finishes"
            );
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
            assert_eq!(
                swept,
                updated.steps.len(),
                "every swept step should emit step_state"
            );
            assert!(seen_end, "abort should emit workflow_end");
        });
    }

    // ---- auto-repair cycle tests ----

    /// When a review step with auto_repair:true finishes with findings, the engine must spawn
    /// a repair child (worker) and a re-review grandchild (reviewer), keeping the run running.
    #[test]
    fn auto_repair_spawns_worker_and_reviewer_children_on_findings() {
        with_tmp_workflows_file(|| {
            let review_step = step_with_auto_repair("reviewer", "Review the implementation", None);
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
            assert_eq!(
                updated.status, "running",
                "run should stay running when repair is pending"
            );
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
            let review_step = step_with_auto_repair("reviewer", "Review the implementation", None);
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
            let review_step = step_with_auto_repair("reviewer", "Review the implementation", None);
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
            let review_step = step_with_auto_repair("reviewer", "Review the implementation", None);
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
        assert!(review_has_findings(Some("## Critical\n- bug in foo\n")));
        assert!(review_has_findings(Some("## Warnings\n- magic number\n")));
        assert!(review_has_findings(Some("## Must Fix\n- security issue\n")));
        assert!(review_has_findings(Some("## Should Fix\n- code smell\n")));
        // Numbered list.
        assert!(review_has_findings(Some("## Critical\n1. bug\n")));
    }

    /// review_has_findings heuristic: rejects clean reviews.
    #[test]
    fn review_has_findings_rejects_clean_reviews() {
        assert!(!review_has_findings(None));
        assert!(!review_has_findings(Some("")));
        assert!(!review_has_findings(Some("   ")));
        assert!(!review_has_findings(Some("No critical issues found.")));
        assert!(!review_has_findings(Some("No issues found.")));
        assert!(!review_has_findings(Some("No findings.")));
        assert!(!review_has_findings(Some("Looks good.")));
        assert!(!review_has_findings(Some("LGTM")));
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
            assert!(
                seen_repair_ready,
                "repair step ready event should be broadcast"
            );
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
            let s1 = after
                .steps
                .iter()
                .find(|s| s.id == run.steps[1].id)
                .unwrap();
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
            assert!(
                get_active(&run_id).is_none(),
                "active map should be empty after remove"
            );

            // But it's still in history.
            let in_history = list_history(None).into_iter().find(|r| r.id == run_id);
            assert!(in_history.is_some(), "run should be in history");

            // restore_for_resume should load it back.
            let restored = restore_for_resume(&run_id).unwrap();
            assert_eq!(restored.id, run_id);
            assert_eq!(restored.status, "running");
            assert!(
                get_active(&run_id).is_some(),
                "run should be back in active map"
            );
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
            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
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
            assert_eq!(
                s0.status, "ready",
                "interrupted step should be reset to ready"
            );
            assert!(s0.error.is_none(), "error should be cleared on resume");
            assert!(s0.ended_at.is_none(), "endedAt should be cleared on resume");
            assert!(
                s0.started_at.is_none(),
                "startedAt should be cleared on resume"
            );

            // Step 1 (was pending) should still be pending.
            let s1 = resumed
                .steps
                .iter()
                .find(|s| s.id == run.steps[1].id)
                .unwrap();
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

    // ---- live-editable workflow tests ----

    /// A failed step can be rerun: its status resets to ready, error/output/usage are
    /// cleared, and the operator's feedback is appended to the task.
    #[test]
    fn rerun_failed_step_resets_and_appends_feedback() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("worker", "Fix the bug", None)];
            let run = create(None, None, "rerun-test".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step_id = run.steps[0].id.clone();

            // Mark the step as errored.
            let updated = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("error".into()),
                    error: Some("something went wrong".into()),
                    output: Some("partial output".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let errored = updated.steps.iter().find(|s| s.id == step_id).unwrap();
            assert_eq!(errored.status, "error");
            assert!(errored.error.is_some());

            // Rerun with feedback.
            let (run_id, step_id_c) = (run.id.clone(), step_id.clone());
            let feedback = "Check the null guard on line 42";
            let _body = RerunBody {
                feedback: Some(feedback.to_string()),
            };

            // Simulate what rerun_step_handler does inline (avoids needing a full axum
            // test server; the handler logic is the store ops + emit).
            let new_task = format!(
                "{}\n\n[Operator feedback for rerun]\n{}",
                errored.task, feedback
            );
            // step_state only patches fields that are Some; passing None means "don't touch".
            // The handler resets status to ready via step_state, then directly clears
            // error/output/usage/timestamps on the store.
            let _ = step_state(
                &run_id,
                &step_id_c,
                StepPatch {
                    status: Some("ready".into()),
                    ..Default::default()
                },
            );
            {
                let mut active = store_guard();
                let r = active.get_mut(&run_id).unwrap();
                let s = r.steps.iter_mut().find(|s| s.id == step_id_c).unwrap();
                s.task = new_task.clone();
                s.started_at = None;
                s.ended_at = None;
                s.output = None;
                s.error = None;
                s.usage = None;
            }

            let after = get_active(&run_id).unwrap();
            let s = after.steps.iter().find(|s| s.id == step_id_c).unwrap();
            assert_eq!(s.status, "ready", "rerun should reset step to ready");
            assert!(s.error.is_none(), "rerun should clear error");
            assert!(s.output.is_none(), "rerun should clear output");
            assert!(s.usage.is_none(), "rerun should clear usage");
            assert!(
                s.task.contains(feedback),
                "rerun should append feedback to task: {}",
                s.task
            );
            assert_eq!(s.task, new_task);
        });
    }

    /// Rerun without feedback preserves the original task.
    #[test]
    fn rerun_without_feedback_preserves_task() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("worker", "Fix the bug", None)];
            let run = create(None, None, "rerun-nofb".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let step_id = run.steps[0].id.clone();
            let original_task = run.steps[0].task.clone();

            // Error the step.
            let _ = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("error".into()),
                    error: Some("fail".into()),
                    ..Default::default()
                },
            );

            // Rerun without feedback.
            let _ = step_state(
                &run.id,
                &step_id,
                StepPatch {
                    status: Some("ready".into()),
                    output: None,
                    error: None,
                    usage: None,
                    artifact: None,
                    tool_calls: None,
                },
            );

            let after = get_active(&run.id).unwrap();
            let s = after.steps.iter().find(|s| s.id == step_id).unwrap();
            assert_eq!(s.status, "ready");
            assert_eq!(
                s.task, original_task,
                "no-feedback rerun should preserve task"
            );
        });
    }

    /// Patching a step's parents to an empty set makes it a root node (ready immediately
    // if it was pending).
    #[test]
    fn patch_parents_empty_makes_ready() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
            let run = create(None, None, "patch-empty".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let child_id = run.steps[1].id.clone();
            assert_eq!(run.steps[1].status, "pending");

            // Patch child to have no parents.
            let updated = super::patch_parents(&run.id, &child_id, vec![]).unwrap();
            let child = updated.steps.iter().find(|s| s.id == child_id).unwrap();
            assert_eq!(child.parents, Vec::<String>::new());
            assert_eq!(
                child.status, "ready",
                "child with no parents should be ready"
            );
        });
    }

    /// Patching parents to a different valid set rewires the DAG correctly.
    #[test]
    fn patch_parents_to_valid_set_rewires_dag() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", None),
                step("c", "C", Some(vec![json!(0)])),
            ];
            let run = create(None, None, "patch-rewire".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let c_id = run.steps[2].id.clone();
            let b_id = run.steps[1].id.clone();

            // Patch C to depend on B instead of A.
            let updated = super::patch_parents(&run.id, &c_id, vec![json!(1)]).unwrap();
            let c = updated.steps.iter().find(|s| s.id == c_id).unwrap();
            assert_eq!(c.parents, vec![b_id.clone()]);

            // B should now have C as a child; A should not.
            let a = updated
                .steps
                .iter()
                .find(|s| s.id == run.steps[0].id)
                .unwrap();
            assert!(!a.children.contains(&c_id));
            let b = updated.steps.iter().find(|s| s.id == b_id).unwrap();
            assert!(b.children.contains(&c_id));
        });
    }

    /// Patching parents to form a cycle is rejected.
    #[test]
    fn patch_parents_cycle_is_rejected() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None), step("b", "B", Some(vec![json!(0)]))];
            let run = create(None, None, "patch-cycle".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let a_id = run.steps[0].id.clone();
            let _b_id = run.steps[1].id.clone();

            // Try to make A depend on B (which depends on A) — should fail.
            let result = super::patch_parents(&run.id, &a_id, vec![json!(1)]);
            assert!(result.is_err(), "cycle should be rejected");
        });
    }

    /// Inserting steps into an existing run appends them and propagates readiness.
    #[test]
    fn insert_steps_appends_and_propagates_readiness() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "insert".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();
            let a_id = run.steps[0].id.clone();

            // Complete step A.
            let _ = step_state(
                &run.id,
                &a_id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            );

            // Insert a new step B that depends on A.
            let new_step = step("b", "B", Some(vec![json!(0)]));
            let updated = super::insert_steps(&run.id, &[new_step]).unwrap();

            // Should have 2 steps now.
            assert_eq!(updated.steps.len(), 2);
            let b = updated.steps.iter().find(|s| s.agent == "b").unwrap();
            assert_eq!(
                b.status, "ready",
                "new step with done parent should be ready"
            );
            assert_eq!(b.parents, vec![a_id]);
        });
    }

    /// Inserting a step with no parents makes it immediately ready.
    #[test]
    fn insert_step_no_parents_is_ready() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(
                None,
                None,
                "insert-noparents".into(),
                None,
                3,
                &inputs,
                None,
            )
            .unwrap();
            let run = start(&run.id).unwrap();

            // Insert a root step.
            let new_step = step("b", "B", None);
            let updated = super::insert_steps(&run.id, &[new_step]).unwrap();
            let b = updated.steps.iter().find(|s| s.agent == "b").unwrap();
            assert_eq!(b.status, "ready");
            assert!(b.parents.is_empty());
        });
    }

    /// Patching a step's model updates the model field.
    #[test]
    fn patch_model_updates_field() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("a", "A", None)];
            let run = create(None, None, "patch-model".into(), None, 3, &inputs, None).unwrap();
            let step_id = run.steps[0].id.clone();
            assert!(run.steps[0].model.is_none());

            // Patch model via the public patch function.
            let updated = super::patch_model(&run.id, &step_id, Some("openrouter/gpt-4o")).unwrap();
            let s = updated.steps.iter().find(|s| s.id == step_id).unwrap();
            assert_eq!(s.model, Some("openrouter/gpt-4o".to_string()));

            // Clear model.
            let updated = super::patch_model(&run.id, &step_id, None).unwrap();
            let s = updated.steps.iter().find(|s| s.id == step_id).unwrap();
            assert!(s.model.is_none());
        });
    }

    /// A step created with a model field preserves it through store round-trip.
    #[test]
    fn step_model_persists_through_create() {
        with_tmp_workflows_file(|| {
            let mut input = step("a", "A", None);
            input.model = Some("anthropic/claude-sonnet-4-20250514".to_string());
            let run = create(None, None, "model-persist".into(), None, 3, &[input], None).unwrap();
            let s = &run.steps[0];
            assert_eq!(
                s.model,
                Some("anthropic/claude-sonnet-4-20250514".to_string())
            );
        });
    }

    /// Patching a step's artifact via `step_state` stores the artifact and
    /// round-trips through the store. This mirrors what the executor does for
    // worker steps after subagent completion.
    #[test]
    fn patch_artifact_sets_field_and_roundtrips() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("worker", "implement", None)];
            let run = create(None, None, "artifact-patch".into(), None, 3, &inputs, None).unwrap();
            let step_id = run.steps[0].id.clone();
            assert!(run.steps[0].artifact.is_none());

            let diff = Artifact {
                kind: "git_diff".to_string(),
                title: Some("1 file changed, 2 insertions(+)".to_string()),
                content: "diff --git a/README.md b/README.md\n...".to_string(),
            };

            let patch = StepPatch {
                artifact: Some(diff.clone()),
                ..Default::default()
            };
            let updated = super::step_state(&run.id, &step_id, patch).unwrap();
            let s = updated.steps.iter().find(|s| s.id == step_id).unwrap();
            assert_eq!(
                s.artifact,
                Some(diff),
                "artifact must round-trip through step_state"
            );
        });
    }

    /// A step's artifact serializes to JSON and deserializes back correctly,
    /// including the `kind`, `title`, and `content` fields.
    #[test]
    fn artifact_serde_roundtrip() {
        let original = Artifact {
            kind: "git_diff".to_string(),
            title: Some("3 files changed".to_string()),
            content: "diff --git a.rs b.rs\n...".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize artifact");
        let deserialized: Artifact = serde_json::from_str(&json).expect("deserialize artifact");
        assert_eq!(deserialized.kind, "git_diff");
        assert_eq!(deserialized.title, Some("3 files changed".to_string()));
        assert_eq!(deserialized.content, original.content);
    }

    /// A step without an artifact serializes with the field omitted (skip_serializing_if).
    #[test]
    fn artifact_none_serializes_as_absent() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("worker", "task", None)];
            let run = create(None, None, "artifact-absent".into(), None, 3, &inputs, None).unwrap();
            let s = &run.steps[0];
            let json = serde_json::to_value(s).expect("serialize step");
            assert!(
                json.get("artifact").is_none(),
                "artifact field must be absent when None (skip_serializing_if)"
            );
        });
    }

    /// Build a `WorkflowRun` with steps in the given statuses — a synthetic workflow for
    /// summary tests that doesn't require the create/start/step_state lifecycle.
    fn synthetic_run(statuses: &[&str]) -> WorkflowRun {
        let now = now_ms();
        let steps: Vec<WorkflowStep> = statuses
            .iter()
            .enumerate()
            .map(|(i, st)| WorkflowStep {
                id: format!("s{i}"),
                agent: "worker".into(),
                task: format!("task {i}"),
                status: st.to_string(),
                parents: if i > 0 {
                    vec![format!("s{}", i - 1)]
                } else {
                    Vec::new()
                },
                children: if i + 1 < statuses.len() {
                    vec![format!("s{}", i + 1)]
                } else {
                    Vec::new()
                },
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
            })
            .collect();
        WorkflowRun {
            id: "synthetic".into(),
            project_id: None,
            session_id: None,
            label: "synthetic".into(),
            steps,
            status: "running".into(),
            origin: None,
            created_at: now,
            updated_at: now,
            started_at: Some(now),
            ended_at: None,
            max_repair_rounds: 3,
            repair_rounds: 0,
            budget: None,
            actual_cost: None,
            actual_tokens: None,
        }
    }

    /// `workflow_summary` must count each terminal/in-flight/pending state correctly over a
    /// synthetic workflow with a mix of every step status, so the UI can render progress
    /// counts (`3/7 done · 1 running · 1 failed`) without walking the DAG.
    #[test]
    fn workflow_summary_counts_mixed_step_states() {
        let run = synthetic_run(&[
            "done",        // completed
            "done",        // completed
            "done",        // completed
            "error",       // failed
            "running",     // running
            "pending",     // pending
            "ready",       // ready
            "skipped",     // skipped
            "interrupted", // interrupted
        ]);
        let s = workflow_summary(&run);
        assert_eq!(s.steps, 9, "steps must be the total node count");
        assert_eq!(s.completed, 3, "completed counts done steps");
        assert_eq!(s.failed, 1, "failed counts error steps");
        assert_eq!(s.running, 1, "running counts in-flight steps");
        assert_eq!(s.pending, 1);
        assert_eq!(s.ready, 1);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.interrupted, 1);
        // All per-status counts must sum to the total step count.
        assert_eq!(
            s.completed + s.failed + s.running + s.pending + s.ready + s.skipped + s.interrupted,
            s.steps,
            "all per-status counts must sum to the total step count"
        );
    }

    /// An empty workflow (no steps) must produce a zeroed summary, not panic.
    #[test]
    fn workflow_summary_empty_run() {
        let run = synthetic_run(&[]);
        let s = workflow_summary(&run);
        assert_eq!(s, WorkflowSummary::default());
    }

    /// `run_with_summary` must attach a `summary` object to the serialized run so the
    /// REST response carries progress counts the UI can read without walking the DAG.
    #[test]
    fn run_with_summary_attaches_summary_field() {
        let run = synthetic_run(&["done", "running", "pending"]);
        let val = run_with_summary(&run);
        let summary = val.get("summary").expect("summary field must be present");
        assert_eq!(summary["steps"], json!(3));
        assert_eq!(summary["completed"], json!(1));
        assert_eq!(summary["running"], json!(1));
        assert_eq!(summary["pending"], json!(1));
        assert_eq!(summary["failed"], json!(0));
        // The run's own fields must still be present (additive, not replacing).
        assert_eq!(val["id"], json!("synthetic"));
        assert_eq!(val["label"], json!("synthetic"));
    }

    /// After a real step-state transition through `step_state`, `workflow_summary` must reflect
    /// the updated counts — proving the summary stays in sync with the live store.
    #[test]
    fn workflow_summary_reflects_step_state_transition() {
        with_tmp_workflows_file(|| {
            let inputs = vec![
                step("a", "A", None),
                step("b", "B", Some(vec![json!(0)])),
                step("c", "C", Some(vec![json!(0)])),
            ];
            let run = create(None, None, "summary-live".into(), None, 3, &inputs, None).unwrap();
            let run = start(&run.id).unwrap();

            // Before any step completes: 1 ready (root), 2 pending (children).
            let s = workflow_summary(&run);
            assert_eq!(s.steps, 3);
            assert_eq!(s.completed, 0);
            assert_eq!(s.ready, 1);
            assert_eq!(s.pending, 2);

            // Complete step 0 -> it's done, and both children become ready.
            let parent_id = run.steps[0].id.clone();
            let updated = step_state(
                &run.id,
                &parent_id,
                StepPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let s = workflow_summary(&updated);
            assert_eq!(s.completed, 1, "one step done after transition");
            assert_eq!(s.ready, 2, "both children ready after parent done");
            assert_eq!(s.pending, 0);
        });
    }

    /// `write_all` must atomically persist workflow history via a temp-then-rename so a crash
    /// mid-write never leaves a truncated `workflows.json` that `read_all` silently drops
    /// (permanently losing all run history and breaking `startup_resume`). This test verifies
    /// the round-trip is intact AND that no stale `.tmp` file is left behind after a successful
    /// write — the observable contract of the atomic-write dance.
    #[test]
    fn write_all_is_atomic_and_round_trips_without_leaving_tmp() {
        with_tmp_workflows_file(|| {
            let inputs = vec![step("scout", "explore", None)];
            let run = create(
                None,
                None,
                "atomic-write-test".into(),
                None,
                3,
                &inputs,
                None,
            )
            .unwrap();

            // persist() calls write_all() under the hood.
            persist(&run);

            // The history file must be valid JSON that read_all can parse back.
            let all = read_all();
            assert_eq!(all.len(), 1, "persisted run should be readable");
            assert_eq!(all[0].id, run.id);
            assert_eq!(all[0].label, "atomic-write-test");

            // No stale .tmp file should remain after a successful atomic rename.
            let file = workflows_file();
            let tmp = file.with_extension("json.tmp");
            assert!(
                !tmp.exists(),
                "atomic write must not leave a stale .tmp file after success"
            );

            // A second persist (upsert path) must also be atomic and leave no .tmp.
            persist(&run);
            assert_eq!(read_all().len(), 1, "upsert should not duplicate");
            assert!(!tmp.exists(), "upsert atomic write must not leave .tmp");
        });
    }
}
