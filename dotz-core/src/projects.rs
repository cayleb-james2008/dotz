//! dotz projects store — a persistent registry of project workspaces. Port of src/projects.ts +
//! the `/api/projects*` routes from server.ts (lines 225-284). Each project pins a cwd, profile,
//! model, and thinking level. Persisted as a single JSON array under the dotz data dir
//! (~/.dotz/projects.json, honoring DOTZ_CONFIG_DIR) so it survives restarts and is human-editable.
//!
//! Self-contained: owns its state via a module-level OnceLock<Mutex<Vec<Project>>> cache (mirrors the
//! Node module-singleton `projectStore`). The cache is the source of truth in-process; every mutating
//! op also writes the full array back to disk. No AppState fields, no axum State.
use crate::types::{self, ModelRef};
use axum::{extract::Path, http::StatusCode, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::Path as FsPath,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// Depth limit for `GET /api/projects/:id/files` (mirrors server.ts FILE_TREE_MAX_DEPTH).
const FILE_TREE_MAX_DEPTH: usize = 3;

/// Project definition — persistent workspace with its own cwd, profile, model, thinking level.
/// Serializes to the exact camelCase shape the Node oracle returns. `appUrl`/`gateCommand` are
/// omitted from the JSON entirely when absent (mirrors the spread-when-truthy in projects.ts).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub cwd: String,
    #[serde(rename = "profileId")]
    pub profile_id: String,
    pub model: ModelRef,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    #[serde(rename = "appUrl", skip_serializing_if = "Option::is_none", default)]
    pub app_url: Option<String>,
    #[serde(
        rename = "gateCommand",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub gate_command: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

// ---- store: OnceLock<Mutex<Vec<Project>>> cache, lazily hydrated from disk on first access ----

static STORE: OnceLock<Mutex<Vec<Project>>> = OnceLock::new();

fn store() -> &'static Mutex<Vec<Project>> {
    STORE.get_or_init(|| Mutex::new(read_all_from_disk()))
}

/// Resolve a project's cwd by id (for memory scoping; mirrors server.ts cwdForProject).
/// None id or unknown id => None (global scope).
pub fn cwd_for_project(id: Option<&str>) -> Option<String> {
    let id = id?;
    let g = store().lock().unwrap();
    g.iter().find(|p| p.id == id).map(|p| p.cwd.clone())
}

/// Fetch a full project by id (clone), for the agent session binder. None => unknown id.
/// (Named `find`, not `get`, to avoid clashing with the `axum::routing::get` import.)
pub fn find(id: &str) -> Option<Project> {
    store().lock().unwrap().iter().find(|p| p.id == id).cloned()
}

fn projects_file() -> std::path::PathBuf {
    crate::config::dotz_dir().join("projects.json")
}

/// Read+parse the JSON store. Guards a hand-edited non-array store ({}, null, 42, …) by treating it
/// as empty (matches readAll's `Array.isArray(parsed) ? … : []`). Missing/unreadable file → [].
fn read_all_from_disk() -> Vec<Project> {
    let raw = match std::fs::read_to_string(projects_file()) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<Project>>(&raw) {
        Ok(v) => v,
        Err(_) => Vec::new(),
    }
}

/// Persist the full array (pretty-printed, matching `JSON.stringify(projects, null, 2)`).
fn write_all(projects: &[Project]) {
    let _ = std::fs::create_dir_all(crate::config::dotz_dir());
    if let Ok(s) = serde_json::to_string_pretty(projects) {
        let _ = std::fs::write(projects_file(), s);
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- validation helpers (port of server.ts trust-boundary helpers) ----

fn is_non_empty_str(v: Option<&Value>) -> bool {
    v.and_then(|x| x.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// `m` is a valid ModelRef: an object with non-empty string `provider` and `modelId`.
fn parse_valid_model(v: &Value) -> Option<ModelRef> {
    let obj = v.as_object()?;
    let provider = obj.get("provider")?.as_str()?;
    let model_id = obj.get("modelId")?.as_str()?;
    if provider.trim().is_empty() || model_id.trim().is_empty() {
        return None;
    }
    Some(ModelRef {
        provider: provider.to_string(),
        model_id: model_id.to_string(),
    })
}

/// Validate a cwd: MUST be an absolute path that EXISTS as a directory. Returns an error string for
/// the caller to 400 on, or None when valid (mirrors validateCwd).
fn validate_cwd(cwd: &str) -> Option<String> {
    if !FsPath::new(cwd).is_absolute() {
        return Some("cwd must be an absolute path".to_string());
    }
    match std::fs::metadata(cwd) {
        Ok(md) if md.is_dir() => None,
        Ok(_) => Some("cwd is not a directory".to_string()),
        Err(_) => Some("cwd directory does not exist".to_string()),
    }
}

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such project" })),
    )
}

// ---- file tree (port of buildFileTree) ----

/// One node in the recursive, depth-limited file tree. `children` present only on dirs.
fn build_file_tree(cwd: &FsPath, depth: usize) -> Vec<Value> {
    if depth >= FILE_TREE_MAX_DEPTH {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(cwd) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name == "node_modules" || name == ".git" {
            continue;
        }
        let full = cwd.join(&*name);
        let ft = match ent.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let children = build_file_tree(&full, depth + 1);
            result.push(json!({
                "path": full.to_string_lossy(),
                "type": "dir",
                "children": children,
            }));
        } else if ft.is_file() || ft.is_symlink() {
            result.push(json!({
                "path": full.to_string_lossy(),
                "type": "file",
            }));
        }
    }
    result
}

// ---- handlers ----

/// GET /api/projects -> { projects: [Project…] }
async fn list_projects() -> Json<Value> {
    let all = store().lock().unwrap().clone();
    Json(json!({ "projects": all }))
}

/// POST /api/projects -> Project (or 400). Validates in the SAME order as server.ts so error
/// messages line up: name+cwd required → model → optional-field types → thinkingLevel value → cwd.
async fn create_project(
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));

    if !is_non_empty_str(body.get("name")) || !is_non_empty_str(body.get("cwd")) {
        return Err(bad("name and cwd are required (non-empty strings)"));
    }
    // name/cwd confirmed non-empty strings above.
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let cwd = body
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // model: if present (key exists; `null` counts as present, like JS `!== undefined`), must be a
    // valid { provider, modelId } — a present-but-invalid model (incl. null) 400s.
    let model = match body.get("model") {
        Some(m) => match parse_valid_model(m) {
            Some(mr) => Some(mr),
            None => return Err(bad("model must be { provider, modelId }")),
        },
        None => None,
    };

    // The remaining optional fields must be strings if present. A present-but-non-string value
    // (incl. null, which is `!== undefined` && `typeof !== "string"` in JS) 400s.
    for k in ["profileId", "thinkingLevel", "appUrl", "gateCommand"] {
        if let Some(v) = body.get(k) {
            if !v.is_string() {
                return Err(bad(format!("{k} must be a string")));
            }
        }
    }

    // thinkingLevel value check (after the string-type check, matching server.ts).
    let thinking_input = body.get("thinkingLevel").and_then(|v| v.as_str());
    if let Some(t) = thinking_input {
        if !types::is_valid_thinking(t) {
            return Err(bad(format!(
                "thinkingLevel must be one of: {}",
                types::THINKING_LEVELS.join(", ")
            )));
        }
    }

    if let Some(err) = validate_cwd(&cwd) {
        return Err(bad(err));
    }

    // Build the project with create-time defaults (mirrors ProjectStore.create).
    let now = now_millis();
    let profile_id = body
        .get("profileId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("workflow")
        .to_string();
    let thinking_level = thinking_input
        .filter(|s| !s.is_empty())
        .unwrap_or("high")
        .to_string();
    let app_url = body
        .get("appUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let gate_command = body
        .get("gateCommand")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let project = Project {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        cwd,
        profile_id,
        model: model.unwrap_or_else(types::default_model),
        thinking_level,
        app_url,
        gate_command,
        created_at: now,
        updated_at: now,
    };

    {
        let mut guard = store().lock().unwrap();
        guard.push(project.clone());
        write_all(&guard);
    }
    Ok(Json(serde_json::to_value(&project).unwrap()))
}

/// GET /api/projects/:id -> Project or 404.
async fn get_project(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let guard = store().lock().unwrap();
    match guard.iter().find(|p| p.id == id) {
        Some(p) => Ok(Json(serde_json::to_value(p).unwrap())),
        None => Err(not_found()),
    }
}

/// PATCH /api/projects/:id partial -> Project or 404 (or 400). Allow-lists patchable fields and
/// type-checks each — never spreads the raw body (closes mass-assignment). id/createdAt preserved.
async fn patch_project(
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let raw = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));

    // Build the validated patch first so a bad field 400s before we touch the store.
    let mut new_name: Option<String> = None;
    let mut new_cwd: Option<String> = None;
    let mut new_profile_id: Option<String> = None;
    let mut new_model: Option<ModelRef> = None;
    let mut new_thinking: Option<String> = None;
    let mut new_app_url: Option<String> = None;
    let mut new_gate_command: Option<String> = None;

    if let Some(v) = raw.get("name") {
        if !is_non_empty_str(Some(v)) {
            return Err(bad("name must be a non-empty string"));
        }
        new_name = Some(v.as_str().unwrap().trim().to_string());
    }
    if let Some(v) = raw.get("cwd") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("cwd must be a string")),
        };
        if let Some(err) = validate_cwd(s) {
            return Err(bad(err));
        }
        new_cwd = Some(s.to_string());
    }
    if let Some(v) = raw.get("profileId") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("profileId must be a string")),
        };
        new_profile_id = Some(s.to_string());
    }
    if let Some(v) = raw.get("model") {
        match parse_valid_model(v) {
            Some(mr) => new_model = Some(mr),
            None => return Err(bad("model must be { provider, modelId }")),
        }
    }
    if let Some(v) = raw.get("thinkingLevel") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("thinkingLevel must be a string")),
        };
        if !types::is_valid_thinking(s) {
            return Err(bad(format!(
                "thinkingLevel must be one of: {}",
                types::THINKING_LEVELS.join(", ")
            )));
        }
        new_thinking = Some(s.to_string());
    }
    if let Some(v) = raw.get("appUrl") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("appUrl must be a string")),
        };
        new_app_url = Some(s.to_string());
    }
    if let Some(v) = raw.get("gateCommand") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("gateCommand must be a string")),
        };
        new_gate_command = Some(s.to_string());
    }

    let mut guard = store().lock().unwrap();
    let idx = match guard.iter().position(|p| p.id == id) {
        Some(i) => i,
        None => return Err(not_found()),
    };
    {
        let p = &mut guard[idx];
        if let Some(v) = new_name {
            p.name = v;
        }
        if let Some(v) = new_cwd {
            p.cwd = v;
        }
        if let Some(v) = new_profile_id {
            p.profile_id = v;
        }
        if let Some(v) = new_model {
            p.model = v;
        }
        if let Some(v) = new_thinking {
            p.thinking_level = v;
        }
        if let Some(v) = new_app_url {
            p.app_url = Some(v);
        }
        if let Some(v) = new_gate_command {
            p.gate_command = Some(v);
        }
        p.updated_at = now_millis();
    }
    let updated = guard[idx].clone();
    write_all(&guard);
    Ok(Json(serde_json::to_value(&updated).unwrap()))
}

/// DELETE /api/projects/:id -> { ok: bool }. ok=false when no project matched.
async fn delete_project(Path(id): Path<String>) -> Json<Value> {
    let mut guard = store().lock().unwrap();
    let before = guard.len();
    guard.retain(|p| p.id != id);
    let removed = guard.len() != before;
    if removed {
        write_all(&guard);
    }
    Json(json!({ "ok": removed }))
}

/// GET /api/projects/:id/files -> { tree: [FileTreeNode…] } or 404.
async fn project_files(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = {
        let guard = store().lock().unwrap();
        match guard.iter().find(|p| p.id == id) {
            Some(p) => p.cwd.clone(),
            None => return Err(not_found()),
        }
    };
    let tree = build_file_tree(FsPath::new(&cwd), 0);
    Ok(Json(json!({ "tree": tree })))
}

/// Register the `/api/projects*` routes with stateless handlers. Drops into the shared router via
/// `.merge(projects::router())`.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).patch(patch_project).delete(delete_project),
        )
        .route("/api/projects/{id}/files", get(project_files))
}
