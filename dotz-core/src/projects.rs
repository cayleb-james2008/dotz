//! dotz projects store — a persistent registry of project workspaces. Port of src/projects.ts +
//! the `/api/projects*` routes from server.ts (lines 225-284). Each project pins a cwd, profile,
//! model, and thinking level. Persisted as a single JSON array under the dotz data dir
//! (~/.dotz/projects.json, honoring DOTZ_CONFIG_DIR) so it survives restarts and is human-editable.
//!
//! Self-contained: owns its state via a module-level OnceLock<Mutex<Vec<Project>>> cache (mirrors the
//! Node module-singleton `projectStore`). The cache is the source of truth in-process; every mutating
//! op also writes the full array back to disk. No AppState fields, no axum State.
use crate::profiles;
use crate::types::{self, ModelRef};
use axum::{Json, Router, extract::Path, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

/// Lock the project store, recovering from a poisoned mutex. A panic while holding the store lock
/// (e.g. inside a serde error path or a callback) must not permanently brick the projects REST
/// endpoints or the agent session binder.
fn store_guard() -> std::sync::MutexGuard<'static, Vec<Project>> {
    store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolve a project's cwd by id (for memory scoping; mirrors server.ts cwdForProject).
/// None id or unknown id => None (global scope).
pub fn cwd_for_project(id: Option<&str>) -> Option<String> {
    let id = id?;
    let g = store_guard();
    g.iter().find(|p| p.id == id).map(|p| p.cwd.clone())
}

/// Fetch a full project by id (clone), for the agent session binder. None => unknown id.
/// (Named `find`, not `get`, to avoid clashing with the `axum::routing::get` import.)
pub fn find(id: &str) -> Option<Project> {
    store_guard().iter().find(|p| p.id == id).cloned()
}

fn projects_file() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("DOTZ_PROJECTS_FILE")
        && !p.trim().is_empty()
    {
        return std::path::PathBuf::from(p);
    }
    crate::config::dotz_dir().join("projects.json")
}

/// Read+parse the JSON store. Guards a hand-edited non-array store ({}, null, 42, …) by treating it
/// as empty (matches readAll's `Array.isArray(parsed) ? … : []`). Missing/unreadable file → [].
///
/// A parse failure degrades to an empty store (same as before) but is now surfaced to stderr:
/// silently returning `[]` on a corrupt or hand-edited store dropped every persisted project with
/// no trace, so an operator who fat-fingered projects.json saw their whole registry vanish on the
/// next read with nothing to diagnose. Degradation is unchanged — only the diagnosability improves.
fn read_all_from_disk() -> Vec<Project> {
    let path = projects_file();
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<Project>>(&raw) {
        Ok(projects) => projects,
        Err(e) => {
            eprintln!(
                "projects: {} is not a valid project array ({e}); starting with an empty project \
                 store. Fix or remove the file to recover persisted projects.",
                path.display()
            );
            Vec::new()
        }
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

/// `m` is a valid ModelRef: an object with a known `provider` and non-empty `modelId`.
/// Normalizes the provider to lowercase and strips a redundant "provider/" prefix from the
/// model id so persisted project models match the session/config normalization and reach the
/// upstream API as bare ids.
fn parse_valid_model(v: &Value) -> Option<ModelRef> {
    let obj = v.as_object()?;
    let provider = obj.get("provider")?.as_str()?.trim().to_lowercase();
    let model_id = obj.get("modelId")?.as_str()?;
    if provider.is_empty() || model_id.trim().is_empty() {
        return None;
    }
    if !types::is_known_provider(&provider) {
        return None;
    }
    let model_id = types::strip_matching_provider_prefix(&provider, model_id);
    Some(ModelRef { provider, model_id })
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
    // Collect + sort entries so the file tree renders in a stable, predictable order
    // (directories first, then files, alphabetically within each group) instead of the
    // arbitrary filesystem iteration order, which differs per platform and per call and
    // makes the operator's file panel jump around on refresh.
    let mut collected: Vec<(String, std::path::PathBuf, bool)> = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy().to_string();
        // Skip heavy/non-source directories so the file panel stays fast and noise-free.
        // `node_modules` and `.git` are skipped to match server.ts; `target` is the Rust
        // build-output dir (dotz's own repo is Rust, and any Rust project the agent works
        // on fills it with tens of thousands of generated files). The agent's own `grep`
        // and `find` tools already skip `target`, so excluding it here keeps the operator's
        // file panel consistent with what the agent searches.
        if name == "node_modules" || name == ".git" || name == "target" {
            continue;
        }
        let full = cwd.join(&name);
        let ft = match ent.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_dir = ft.is_dir();
        if is_dir || ft.is_file() || ft.is_symlink() {
            collected.push((name, full, is_dir));
        }
    }
    collected.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut result = Vec::new();
    for (_name, full, is_dir) in collected {
        if is_dir {
            let children = build_file_tree(&full, depth + 1);
            result.push(json!({
                "path": full.to_string_lossy(),
                "type": "dir",
                "children": children,
            }));
        } else {
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
    let all = store_guard().clone();
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
            None => {
                return Err(bad(format!(
                    "model must be a known provider and non-empty modelId (providers: {})",
                    types::provider_ids().join(", ")
                )));
            }
        },
        None => None,
    };

    // The remaining optional fields must be strings if present. A present-but-non-string value
    // (incl. null, which is `!== undefined` && `typeof !== "string"` in JS) 400s.
    for k in ["profileId", "thinkingLevel", "appUrl", "gateCommand"] {
        if let Some(v) = body.get(k)
            && !v.is_string()
        {
            return Err(bad(format!("{k} must be a string")));
        }
    }

    // thinkingLevel value check (after the string-type check, matching server.ts).
    let thinking_input = body.get("thinkingLevel").and_then(|v| v.as_str());
    if let Some(t) = thinking_input
        && !types::is_valid_thinking(t)
    {
        return Err(bad(format!(
            "thinkingLevel must be one of: {}",
            types::THINKING_LEVELS.join(", ")
        )));
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
    if !profiles::is_valid(&profile_id) {
        return Err(bad(format!(
            "profileId must be one of: {}",
            profiles::summaries()
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
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
        let mut guard = store_guard();
        guard.push(project.clone());
        write_all(&guard);
    }
    Ok(Json(serde_json::to_value(&project).unwrap()))
}

/// GET /api/projects/:id -> Project or 404.
async fn get_project(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let guard = store_guard();
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
        // Mirror create_project: an empty profileId falls back to the default "workflow".
        new_profile_id = Some({
            let t = s.trim().to_string();
            if t.is_empty() {
                "workflow".to_string()
            } else {
                t
            }
        });
        if let Some(ref id) = new_profile_id
            && !profiles::is_valid(id)
        {
            return Err(bad(format!(
                "profileId must be one of: {}",
                profiles::summaries()
                    .iter()
                    .map(|s| s.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    if let Some(v) = raw.get("model") {
        match parse_valid_model(v) {
            Some(mr) => new_model = Some(mr),
            None => {
                return Err(bad(format!(
                    "model must be a known provider and non-empty modelId (providers: {})",
                    types::provider_ids().join(", ")
                )));
            }
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
        // Mirror create_project: empty/whitespace appUrl removes the field.
        new_app_url = Some(s.trim().to_string()).filter(|s| !s.is_empty());
    }
    if let Some(v) = raw.get("gateCommand") {
        let s = match v.as_str() {
            Some(s) => s,
            None => return Err(bad("gateCommand must be a string")),
        };
        // Mirror create_project: empty/whitespace gateCommand removes the field.
        new_gate_command = Some(s.trim().to_string()).filter(|s| !s.is_empty());
    }

    let mut guard = store_guard();
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
        if raw.get("appUrl").is_some() {
            p.app_url = new_app_url;
        }
        if raw.get("gateCommand").is_some() {
            p.gate_command = new_gate_command;
        }
        p.updated_at = now_millis();
    }
    let updated = guard[idx].clone();
    write_all(&guard);
    Ok(Json(serde_json::to_value(&updated).unwrap()))
}

/// DELETE /api/projects/:id -> { ok, purged: { memories, workflows, sessions } }.
/// Removes the project from dotz AND purges its dotz-side cache/memories (all under ~/.dotz):
/// project-scoped mem0 rows, its workflow runs + run-records, and any live sessions. NEVER touches
/// the project folder on disk — `<cwd>/.ai-agents/MEMORY.md` and all files there are left intact.
async fn delete_project(Path(id): Path<String>) -> Json<Value> {
    // Capture the cwd BEFORE removal — it's needed to scope the memory purge.
    let cwd = find(&id).map(|p| p.cwd);
    let removed = {
        let mut guard = store_guard();
        let before = guard.len();
        guard.retain(|p| p.id != id);
        let removed = guard.len() != before;
        if removed {
            write_all(&guard);
        }
        removed
    };
    if !removed {
        return Json(json!({ "ok": false }));
    }
    let memories = cwd
        .as_deref()
        .map(crate::memory::purge_project)
        .unwrap_or(0);
    let workflows = crate::workflows::purge_project(&id);
    let sessions = crate::agent::session::dispose_for_project(&id);
    Json(json!({
        "ok": true,
        "purged": { "memories": memories, "workflows": workflows, "sessions": sessions }
    }))
}

/// GET /api/projects/:id/files -> { tree: [FileTreeNode…] } or 404.
async fn project_files(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = {
        let guard = store_guard();
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
        .merge(agents_md::router())
}

/// AGENTS.md doctrine REST surface for the UI's DOCTRINE panel.
///
/// `GET /api/agents_md?projectId=<id>` reads the project's root `AGENTS.md`.
/// `PATCH /api/agents_md?projectId=<id>` overwrites it.
mod agents_md {
    use super::{bad, cwd_for_project, not_found};
    use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn resolve_path(q: &HashMap<String, String>) -> Result<PathBuf, (StatusCode, Json<Value>)> {
        let pid = q.get("projectId").cloned().unwrap_or_default();
        if pid.is_empty() {
            return Err(bad("projectId is required"));
        }
        let cwd = cwd_for_project(Some(&pid)).ok_or_else(not_found)?;
        Ok(PathBuf::from(&cwd).join("AGENTS.md"))
    }

    async fn get_agents_md(
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
        let path = resolve_path(&q)?;
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        Ok(Json(
            json!({ "content": content, "path": path.to_string_lossy() }),
        ))
    }

    async fn patch_agents_md(
        Query(q): Query<HashMap<String, String>>,
        body: Option<Json<Value>>,
    ) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
        let path = resolve_path(&q)?;
        let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
        let content = body
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad("content must be a string"))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, content).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
        Ok(Json(
            json!({ "ok": true, "content": content, "path": path.to_string_lossy() }),
        ))
    }

    pub fn router() -> Router<()> {
        Router::new().route("/api/agents_md", get(get_agents_md).patch(patch_agents_md))
    }

    #[cfg(test)]
    mod tests {
        use super::super::{create_project, with_tmp_projects_file};
        use super::*;

        fn tmp_dir() -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("dotz-agents-md-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[tokio::test]
        async fn get_agents_md_returns_empty_when_missing() {
            let _g = with_tmp_projects_file();
            let dir = tmp_dir();
            let body = Json(json!({ "name": "demo", "cwd": dir.to_string_lossy() }));
            let created = create_project(Some(body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();

            let mut q = HashMap::new();
            q.insert("projectId".to_string(), id);
            let resp = get_agents_md(Query(q)).await.unwrap().0;
            assert_eq!(resp["content"], "");
            assert!(resp["path"].as_str().unwrap().contains("AGENTS.md"));
        }

        #[tokio::test]
        async fn patch_agents_md_writes_and_get_reads_back() {
            let _g = with_tmp_projects_file();
            let dir = tmp_dir();
            let body = Json(json!({ "name": "demo", "cwd": dir.to_string_lossy() }));
            let created = create_project(Some(body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();

            let mut q = HashMap::new();
            q.insert("projectId".to_string(), id.clone());
            let patch_body = Json(json!({ "content": "# Doctrine\n\nRule 1." }));
            let patched = patch_agents_md(Query(q.clone()), Some(patch_body))
                .await
                .unwrap()
                .0;
            assert_eq!(patched["ok"], true);
            assert_eq!(patched["content"], "# Doctrine\n\nRule 1.");

            let resp = get_agents_md(Query(q)).await.unwrap().0;
            assert_eq!(resp["content"], "# Doctrine\n\nRule 1.");
        }

        #[tokio::test]
        async fn agents_md_rejects_missing_project_id() {
            let err = get_agents_md(Query(HashMap::new())).await.unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn agents_md_returns_404_for_unknown_project() {
            let mut q = HashMap::new();
            q.insert("projectId".to_string(), "not-a-real-id".to_string());
            let err = get_agents_md(Query(q)).await.unwrap_err();
            assert_eq!(err.0, StatusCode::NOT_FOUND);
        }
    }
}

/// Test-only helper: isolated projects file + store reset.
#[cfg(test)]
static LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
struct TmpFileGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    file: std::path::PathBuf,
    prev: Option<String>,
}

#[cfg(test)]
impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        match &self.prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_PROJECTS_FILE", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_PROJECTS_FILE") },
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Point DOTZ_PROJECTS_FILE at an isolated temp file, reset the in-memory store, and return a
/// guard that restores the previous env + removes the temp file when dropped.
#[cfg(test)]
fn with_tmp_projects_file() -> TmpFileGuard {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let file =
        std::env::temp_dir().join(format!("dotz-projects-test-{}.json", uuid::Uuid::new_v4()));
    let prev = std::env::var("DOTZ_PROJECTS_FILE").ok();
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DOTZ_PROJECTS_FILE", &file) };
    {
        let mut store_guard = store_guard();
        *store_guard = Vec::new();
    }
    TmpFileGuard {
        _lock: guard,
        file,
        prev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // (std::sync::Mutex is not imported here because the shared LOCK lives in the parent module.)
    use super::with_tmp_projects_file;

    // Serialize projects tests: they share the module-level in-memory store.
    // (LOCK lives in the parent module so the test helper is reusable.)

    #[test]
    fn patch_project_clears_empty_optional_strings() {
        let _g = with_tmp_projects_file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let create_body = Json(json!({
                "name": "demo",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
                "appUrl": "http://localhost:3000",
                "gateCommand": "npm test"
            }));
            let created = create_project(Some(create_body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();
            assert!(created["appUrl"].as_str().is_some());
            assert!(created["gateCommand"].as_str().is_some());

            let patch_body = Json(json!({
                "appUrl": "",
                "gateCommand": "   "
            }));
            let patched = patch_project(Path(id), Some(patch_body)).await.unwrap().0;
            assert!(
                patched["appUrl"].is_null(),
                "empty appUrl should be removed, got: {:?}",
                patched["appUrl"]
            );
            assert!(
                patched["gateCommand"].is_null(),
                "whitespace-only gateCommand should be removed, got: {:?}",
                patched["gateCommand"]
            );
        });
    }

    #[test]
    fn patch_project_defaults_empty_profile_id() {
        let _g = with_tmp_projects_file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let create_body = Json(json!({
                "name": "demo",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
                "profileId": "frontend"
            }));
            let created = create_project(Some(create_body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();
            assert_eq!(created["profileId"], "frontend");

            let patch_body = Json(json!({ "profileId": "" }));
            let patched = patch_project(Path(id), Some(patch_body)).await.unwrap().0;
            assert_eq!(patched["profileId"], "workflow");
        });
    }

    #[test]
    fn patch_project_preserves_untouched_optional_fields() {
        let _g = with_tmp_projects_file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let create_body = Json(json!({
                "name": "demo",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
                "appUrl": "http://localhost:3000",
                "gateCommand": "npm test"
            }));
            let created = create_project(Some(create_body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();

            let patch_body = Json(json!({ "name": "renamed" }));
            let patched = patch_project(Path(id), Some(patch_body)).await.unwrap().0;
            assert_eq!(patched["name"], "renamed");
            assert_eq!(patched["appUrl"], "http://localhost:3000");
            assert_eq!(patched["gateCommand"], "npm test");
        });
    }

    /// create_project must reject a non-empty, unknown profileId instead of storing it and
    /// causing later session creation to fail. Empty/missing profileId still defaults to workflow.
    #[test]
    fn create_project_rejects_invalid_profile_id() {
        let _g = with_tmp_projects_file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let body = Json(json!({
                "name": "bad-profile",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
                "profileId": "not-a-profile"
            }));
            let err = create_project(Some(body)).await.unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert!(
                err.1.0["error"]
                    .as_str()
                    .unwrap()
                    .contains("profileId must be one of:"),
                "error should list valid profileIds, got: {:?}",
                err.1.0
            );
        });
    }

    /// patch_project must reject an invalid profileId update, preserving the trust boundary that
    /// only known profiles can be persisted.
    #[test]
    fn patch_project_rejects_invalid_profile_id() {
        let _g = with_tmp_projects_file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let create_body = Json(json!({
                "name": "demo",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
            }));
            let created = create_project(Some(create_body)).await.unwrap().0;
            let id = created["id"].as_str().unwrap().to_string();

            let patch_body = Json(json!({ "profileId": "bogus" }));
            let err = patch_project(Path(id), Some(patch_body)).await.unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert!(
                err.1.0["error"]
                    .as_str()
                    .unwrap()
                    .contains("profileId must be one of:"),
                "error should list valid profileIds, got: {:?}",
                err.1.0
            );
        });
    }

    /// create_project must reject a model with an unknown provider instead of persisting it and
    /// causing a runtime failure when the session tries to resolve the provider.
    #[tokio::test]
    async fn create_project_rejects_unknown_provider() {
        let _g = with_tmp_projects_file();
        let dir = std::env::temp_dir().join(format!(
            "dotz-project-provider-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let body = Json(json!({
            "name": "bad-provider",
            "cwd": dir.to_string_lossy(),
            "model": { "provider": "not-a-provider", "modelId": "anything" }
        }));
        let err = create_project(Some(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1.0["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("model must be a known provider"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("ollama"),
            "provider list should include ollama: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// patch_project must reject a model update with an unknown provider, preserving the same
    /// trust boundary as create_project.
    #[tokio::test]
    async fn patch_project_rejects_unknown_provider() {
        let _g = with_tmp_projects_file();
        let dir =
            std::env::temp_dir().join(format!("dotz-patch-provider-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let create_body = Json(json!({
            "name": "demo",
            "cwd": dir.to_string_lossy(),
        }));
        let created = create_project(Some(create_body)).await.unwrap().0;
        let id = created["id"].as_str().unwrap().to_string();

        let patch_body = Json(json!({
            "model": { "provider": "fake-provider", "modelId": "x" }
        }));
        let err = patch_project(Path(id), Some(patch_body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1.0["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("model must be a known provider"),
            "unexpected error: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// create_project must accept a mixed-case provider string and store it as the canonical
    /// lowercase id. Without this normalization, sessions bound to the project resolve the
    /// provider against lowercase endpoint metadata and fail to stream.
    #[tokio::test]
    async fn create_project_normalizes_uppercase_provider() {
        let _g = with_tmp_projects_file();
        let dir =
            std::env::temp_dir().join(format!("dotz-project-case-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let body = Json(json!({
            "name": "case-test",
            "cwd": dir.to_string_lossy(),
            "model": { "provider": "Ollama", "modelId": "glm-5.2" }
        }));
        let created = create_project(Some(body)).await.unwrap().0;
        assert_eq!(created["model"]["provider"], "ollama");
        assert_eq!(created["model"]["modelId"], "glm-5.2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model id pasted with a redundant provider prefix must be normalized to a bare id so the
    /// upstream API receives "glm-5.2" instead of "ollama/glm-5.2". Mismatched prefixes are
    /// preserved so cross-provider namespaces are not corrupted.
    #[tokio::test]
    async fn create_project_normalizes_redundant_provider_prefix() {
        let _g = with_tmp_projects_file();
        let dir =
            std::env::temp_dir().join(format!("dotz-project-prefix-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let body = Json(json!({
            "name": "prefix-test",
            "cwd": dir.to_string_lossy(),
            "model": { "provider": "ollama", "modelId": "ollama/glm-5.2" }
        }));
        let created = create_project(Some(body)).await.unwrap().0;
        assert_eq!(created["model"]["provider"], "ollama");
        assert_eq!(created["model"]["modelId"], "glm-5.2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// patch_project must apply the same model normalization as create_project.
    #[tokio::test]
    async fn patch_project_normalizes_model_case_and_prefix() {
        let _g = with_tmp_projects_file();
        let dir = std::env::temp_dir().join(format!(
            "dotz-patch-normalize-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let create_body = Json(json!({
            "name": "demo",
            "cwd": dir.to_string_lossy(),
        }));
        let created = create_project(Some(create_body)).await.unwrap().0;
        let id = created["id"].as_str().unwrap().to_string();

        let patch_body = Json(json!({
            "model": { "provider": "OpenRouter", "modelId": "openrouter/nex-agi/nex-n2-pro:free" }
        }));
        let patched = patch_project(Path(id), Some(patch_body)).await.unwrap().0;
        assert_eq!(patched["model"]["provider"], "openrouter");
        assert_eq!(patched["model"]["modelId"], "nex-agi/nex-n2-pro:free");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A panic while holding the project-store mutex (e.g. inside a serde callback) poisons it.
    /// Every accessor must recover via `store_guard()` so the store remains usable; otherwise a
    /// single panic would brick session creation, memory scoping, and the projects REST surface.
    #[test]
    fn store_guard_recovers_from_poisoned_mutex() {
        let _g = with_tmp_projects_file();

        // Create a project so the store is non-empty.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let id = rt.block_on(async {
            let body = Json(json!({
                "name": "poison-test",
                "cwd": std::env::current_dir().unwrap().to_string_lossy(),
            }));
            let created = create_project(Some(body)).await.unwrap().0;
            created["id"].as_str().unwrap().to_string()
        });

        // Intentionally poison the store mutex while holding the lock.
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store().lock().unwrap();
            panic!("intentional projects store poison");
        }));
        assert!(poisoned.is_err(), "mutex should be poisoned");

        // Subsequent reads must recover instead of panicking.
        let found = find(&id);
        assert!(found.is_some(), "find should recover from poisoned mutex");
        assert_eq!(found.unwrap().name, "poison-test");

        // Subsequent mutations must also recover.
        rt.block_on(async {
            let body = Json(json!({ "name": "renamed" }));
            let patched = patch_project(Path(id), Some(body)).await.unwrap().0;
            assert_eq!(patched["name"], "renamed");
        });
    }

    /// `build_file_tree` must return entries in a stable, predictable order: directories
    /// first, then files, alphabetically within each group. Without sorting the order is the
    /// filesystem's arbitrary iteration order, which differs per platform and per call and
    /// makes the operator's file panel jump around on every refresh. Hidden dirs (.git,
    /// node_modules) must be skipped.
    #[test]
    fn build_file_tree_sorts_dirs_before_files_alphabetically() {
        let dir = std::env::temp_dir().join(format!("dotz-filetree-sort-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Mix of dirs + files with names that are NOT already in sorted order, so a pass
        // through an unsorted iterator would produce a different sequence.
        std::fs::write(dir.join("zebra.txt"), b"").unwrap();
        std::fs::create_dir_all(dir.join("alpha_dir")).unwrap();
        std::fs::write(dir.join("mango.rs"), b"").unwrap();
        std::fs::create_dir_all(dir.join("beta_dir")).unwrap();
        std::fs::write(dir.join("apple.txt"), b"").unwrap();
        // Hidden / skipped entries must not appear.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        // `target` is the Rust build-output dir; it must be skipped just like node_modules
        // and .git so the file panel does not descend into tens of thousands of generated
        // files (dotz's own repo is Rust).
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("built.rs"), b"").unwrap();

        let tree = build_file_tree(&dir, 0);

        // Extract (name, type) pairs in output order for assertion clarity.
        let order: Vec<(String, String)> = tree
            .iter()
            .map(|n| {
                let path = n["path"].as_str().unwrap_or("");
                let name = std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let ty = n["type"].as_str().unwrap_or("").to_string();
                (name, ty)
            })
            .collect();

        // Directories first (alpha_dir, beta_dir), then files (apple.txt, mango.rs, zebra.txt).
        assert_eq!(
            order,
            vec![
                ("alpha_dir".into(), "dir".into()),
                ("beta_dir".into(), "dir".into()),
                ("apple.txt".into(), "file".into()),
                ("mango.rs".into(), "file".into()),
                ("zebra.txt".into(), "file".into()),
            ],
            "file tree must be sorted: dirs first, then files, alphabetically within each group"
        );

        // Skipped dirs must not appear anywhere in the tree.
        assert!(
            !order
                .iter()
                .any(|(n, _)| n == ".git" || n == "node_modules" || n == "target"),
            ".git, node_modules, and target must be excluded from the file tree"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt or hand-edited projects.json (invalid JSON, or valid JSON that is not a project
    /// array) must degrade to an empty store rather than panicking. This pins the graceful-
    /// degradation contract the store relies on: read_all_from_disk never crashes the process,
    /// even on garbage input, so a fat-fingered file can't brick the REST endpoints or the session
    /// binder. The parse failure is surfaced to stderr (see read_all_from_disk) but still returns [].
    #[test]
    fn read_all_from_disk_degrades_on_corrupt_store() {
        let _g = with_tmp_projects_file();
        let path = projects_file();

        // Invalid JSON (truncated / garbage) → empty store, no panic.
        std::fs::write(&path, b"{ this is not valid json ]").unwrap();
        assert!(
            read_all_from_disk().is_empty(),
            "invalid JSON must degrade to an empty project store"
        );

        // Valid JSON but the wrong shape (an object, not an array) → empty store, no panic.
        std::fs::write(&path, br#"{"not":"an array"}"#).unwrap();
        assert!(
            read_all_from_disk().is_empty(),
            "a non-array JSON store must degrade to an empty project store"
        );

        // Valid JSON array of the wrong element type → empty store, no panic.
        std::fs::write(&path, b"[1, 2, 3]").unwrap();
        assert!(
            read_all_from_disk().is_empty(),
            "an array of the wrong element type must degrade to an empty project store"
        );

        // A well-formed store still round-trips (proves the degradation path didn't break parsing).
        std::fs::write(&path, b"[]").unwrap();
        assert!(
            read_all_from_disk().is_empty(),
            "an empty JSON array is a valid, empty project store"
        );
    }
}
