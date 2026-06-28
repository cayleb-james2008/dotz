//! dotz workflow-template store — REST surface for the UI's TEMPLATES panel.
//!
//! Bundled presets live in `.pi/prompts/*.md` (the six workflow slash commands). User templates
//! shadow bundled presets by id and are persisted as JSON under `~/.dotz/ai-agents/templates/`.
//! `POST /api/templates/:id/run` expands `$@`/`$*` with the supplied args and dispatches the
//! resulting prompt to a live session, mirroring typing the slash command in the composer.
use crate::{agent::session, config, skills};
use axum::{
    extract::Path,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TemplateMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "hasArgs")]
    pub has_args: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn bundled_dir() -> PathBuf {
    let configured = skills::pi_dir().join("prompts");
    if configured.exists() {
        return configured;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join(".pi").join("prompts"))
        .filter(|p| p.exists())
        .unwrap_or(configured)
}

fn user_dir() -> PathBuf {
    config::dotz_dir().join("ai-agents/templates")
}

fn ensure_user_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(user_dir())
}

/// Serialize mutations to the user template store so concurrent create/update/delete/fork do not
/// race on the same JSON file.
fn user_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Slug a human name into a file-safe id: lowercase, alnum/hyphen/underscore, collapsed.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = true;
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
            prev_sep = false;
        } else if !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// True when the template body uses shell-style `$@` or `$*` argument expansion.
fn has_args(body: &str) -> bool {
    body.contains("$@") || body.contains("$*")
}

fn meta_from(t: &Template) -> TemplateMeta {
    TemplateMeta {
        id: t.id.clone(),
        name: t.name.clone(),
        description: t.description.clone(),
        source: t.source.clone(),
        origin: t.origin.clone(),
        tags: t.tags.clone(),
        has_args: has_args(&t.body),
        updated_at: t.updated_at,
    }
}

/// Parse a bundled `.pi/prompts/*.md` file. The leading YAML frontmatter carries the description;
/// everything after the closing `---` is the prompt body.
fn parse_bundled(path: &StdPath) -> Option<Template> {
    let raw = std::fs::read_to_string(path).ok()?;
    let id = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut body = raw.clone();

    if let Some(after_open) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    {
        let mut lines = after_open.lines();
        let mut fm_lines: Vec<&str> = Vec::new();
        let mut found_close = false;
        for line in lines.by_ref() {
            if line == "---" {
                found_close = true;
                break;
            }
            fm_lines.push(line);
        }
        if found_close {
            for line in fm_lines {
                let mut parts = line.splitn(2, ':');
                let key = parts.next().map(|k| k.trim());
                let val = parts
                    .next()
                    .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string());
                match key {
                    Some("name") => name = val,
                    Some("description") => {
                        if let Some(v) = val {
                            description = v;
                        }
                    }
                    _ => {}
                }
            }
            body = lines.collect::<Vec<_>>().join("\n");
            body = body.trim_start().to_string();
        }
    }

    let updated = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(now_ms);

    Some(Template {
        id: id.clone(),
        name: name.unwrap_or(id),
        description,
        body,
        source: "bundled".to_string(),
        origin: Some(path.to_string_lossy().to_string()),
        tags: None,
        created_at: updated,
        updated_at: updated,
    })
}

fn user_path(id: &str) -> PathBuf {
    user_dir().join(format!("{id}.json"))
}

fn load_user(id: &str) -> Option<Template> {
    let path = user_path(id);
    let raw = std::fs::read_to_string(&path).ok()?;
    let mut t: Template = serde_json::from_str(&raw).ok()?;
    t.id = id.to_string();
    t.source = "user".to_string();
    t.origin = None;
    Some(t)
}

fn save_user(t: &Template) -> std::io::Result<()> {
    ensure_user_dir()?;
    let mut stored = t.clone();
    stored.source = "user".to_string();
    stored.origin = None;
    std::fs::write(user_path(&t.id), serde_json::to_string_pretty(&stored)?)
}

fn bundled_templates() -> Vec<Template> {
    let dir = bundled_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(t) = parse_bundled(&path) {
            out.push(t);
        }
    }
    out
}

fn user_templates() -> Vec<Template> {
    let dir = user_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        if let Some(mut t) = load_user(&id) {
            t.id = id;
            out.push(t);
        }
    }
    out
}

/// All templates: bundled first, then user templates shadow by id.
fn list_all() -> Vec<Template> {
    let _guard = user_lock();
    let bundled = bundled_templates();
    let user = user_templates();
    let mut by_id: HashMap<String, Template> = HashMap::new();
    for t in bundled {
        by_id.insert(t.id.clone(), t);
    }
    for t in user {
        by_id.insert(t.id.clone(), t);
    }
    let mut out: Vec<Template> = by_id.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn list_meta() -> Vec<TemplateMeta> {
    list_all().iter().map(meta_from).collect()
}

pub fn get_template(id: &str) -> Option<Template> {
    let _guard = user_lock();
    if let Some(t) = load_user(id) {
        return Some(t);
    }
    bundled_templates().into_iter().find(|t| t.id == id)
}

fn validate_template_payload(name: &str, body: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name is required".to_string());
    }
    if body.trim().is_empty() {
        return Err("body is required".to_string());
    }
    Ok(())
}

pub fn create(
    name: String,
    body: String,
    description: Option<String>,
    tags: Option<Vec<String>>,
) -> Result<Template, String> {
    validate_template_payload(&name, &body)?;
    let id = slugify(&name);
    if id.is_empty() {
        return Err("name must contain alphanumeric characters".to_string());
    }
    let _guard = user_lock();
    if user_path(&id).exists() {
        return Err(format!("template '{id}' already exists"));
    }
    let ts = now_ms();
    let t = Template {
        id,
        name,
        description: description.unwrap_or_default(),
        body,
        source: "user".to_string(),
        origin: None,
        tags,
        created_at: ts,
        updated_at: ts,
    };
    save_user(&t).map_err(|e| format!("failed to save template: {e}"))?;
    Ok(t)
}

pub fn update(
    id: &str,
    name: Option<String>,
    body: Option<String>,
    description: Option<String>,
    tags: Option<Vec<String>>,
) -> Result<Template, String> {
    let _guard = user_lock();
    let mut t = load_user(id).ok_or_else(|| "no such user template".to_string())?;
    if let Some(n) = name {
        validate_template_payload(&n, &t.body)?;
        t.name = n;
    }
    if let Some(b) = body {
        validate_template_payload(&t.name, &b)?;
        t.body = b;
    }
    if let Some(d) = description {
        t.description = d;
    }
    if let Some(tags) = tags {
        t.tags = Some(tags);
    }
    t.updated_at = now_ms();
    save_user(&t).map_err(|e| format!("failed to save template: {e}"))?;
    Ok(t)
}

pub fn delete_template(id: &str) -> bool {
    let _guard = user_lock();
    let path = user_path(id);
    if !path.exists() {
        return false;
    }
    let _ = std::fs::remove_file(&path);
    true
}

pub fn fork(id: &str, name: Option<String>) -> Result<Template, String> {
    let bundled = bundled_templates()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| "no such bundled template".to_string())?;
    let _guard = user_lock();
    let ts = now_ms();
    let t = Template {
        id: id.to_string(),
        name: name.unwrap_or_else(|| bundled.name.clone()),
        description: bundled.description.clone(),
        body: bundled.body.clone(),
        source: "user".to_string(),
        origin: None,
        tags: bundled.tags.clone(),
        created_at: ts,
        updated_at: ts,
    };
    save_user(&t).map_err(|e| format!("failed to save template: {e}"))?;
    Ok(t)
}

/// Expand `$@` / `$*` in a template body with the provided args, mirroring shell word expansion.
fn expand_body(body: &str, args: &[String]) -> String {
    let replacement = args.join(" ");
    body.replace("$@", &replacement)
        .replace("$*", &replacement)
        .trim()
        .to_string()
}

fn parse_args(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s.split_whitespace().map(String::from).collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn run(id: &str, session_id: &str, args: &Value) -> Result<(), String> {
    let t = get_template(id).ok_or_else(|| format!("no such template: {id}"))?;
    let args = parse_args(args);
    let prompt = expand_body(&t.body, &args);
    if prompt.is_empty() {
        return Err("expanded template body is empty".to_string());
    }
    let sess = session::get(session_id).ok_or_else(|| "no such session".to_string())?;
    tokio::spawn(async move {
        session::run_turn(sess, prompt).await;
    });
    Ok(())
}

// ---- axum handlers ----

async fn list_handler() -> Json<Value> {
    Json(json!({ "templates": list_meta() }))
}

async fn get_handler(Path(id): Path<String>) -> Result<Json<Template>, (StatusCode, Json<Value>)> {
    get_template(&id)
        .map(Json)
        .ok_or_else(|| not_found("no such template"))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    body: String,
    description: Option<String>,
    tags: Option<Vec<String>>,
}

async fn create_handler(
    body: Option<Json<CreateBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| CreateBody {
        name: String::new(),
        body: String::new(),
        description: None,
        tags: None,
    });
    create(b.name, b.body, b.description, b.tags)
        .map(|t| Json(json!({ "template": t })))
        .map_err(|e| bad(e))
}

#[derive(Deserialize, Default)]
struct UpdateBody {
    name: Option<String>,
    body: Option<String>,
    description: Option<String>,
    tags: Option<Vec<String>>,
}

async fn update_handler(
    Path(id): Path<String>,
    body: Option<Json<UpdateBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_default();
    update(&id, b.name, b.body, b.description, b.tags)
        .map(|t| Json(json!({ "template": t })))
        .map_err(|e| {
            if e.contains("no such user template") {
                not_found(&e)
            } else {
                bad(e)
            }
        })
}

async fn delete_handler(Path(id): Path<String>) -> Json<Value> {
    Json(json!({ "ok": delete_template(&id) }))
}

#[derive(Deserialize)]
struct ForkBody {
    name: Option<String>,
}

async fn fork_handler(
    Path(id): Path<String>,
    body: Option<Json<ForkBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name = body.and_then(|Json(v)| v.name);
    fork(&id, name)
        .map(|t| Json(json!({ "template": t })))
        .map_err(|e| not_found(&e))
}

#[derive(Deserialize)]
struct RunBody {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    args: Value,
}

async fn run_handler(
    Path(id): Path<String>,
    body: Option<Json<RunBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| RunBody {
        session_id: String::new(),
        args: Value::Null,
    });
    if b.session_id.is_empty() {
        return Err(bad("sessionId is required"));
    }
    run(&id, &b.session_id, &b.args)
        .map(|_| Json(json!({ "ok": true })))
        .map_err(|e| {
            if e.starts_with("no such") {
                not_found(&e)
            } else {
                bad(e)
            }
        })
}

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn not_found(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
}

/// The templates Router<()> to merge into `server::app()`.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/templates", get(list_handler).post(create_handler))
        .route(
            "/api/templates/{id}",
            get(get_handler)
                .patch(update_handler)
                .delete(delete_handler),
        )
        .route("/api/templates/{id}/fork", post(fork_handler))
        .route("/api/templates/{id}/run", post(run_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct DirGuard {
        prev: Option<String>,
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
                None => std::env::remove_var("DOTZ_CONFIG_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn isolated_user_dir() -> DirGuard {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        let dir =
            std::env::temp_dir().join(format!("dotz-templates-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        DirGuard {
            prev,
            dir,
            _lock: guard,
        }
    }

    #[test]
    fn parse_bundled_extracts_description_and_body() {
        let dir =
            std::env::temp_dir().join(format!("dotz-templates-md-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scout-and-plan.md");
        std::fs::write(
            &path,
            "---\nname: Scout Plan\ndescription: Scout gathers context, planner plans\n---\nPlan for: $@\n",
        )
        .unwrap();
        let t = parse_bundled(&path).unwrap();
        assert_eq!(t.id, "scout-and-plan");
        assert_eq!(t.name, "Scout Plan");
        assert_eq!(t.description, "Scout gathers context, planner plans");
        assert_eq!(t.body, "Plan for: $@");
        assert_eq!(t.source, "bundled");
        assert!(t.origin.as_ref().unwrap().contains("scout-and-plan.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_all_includes_bundled_presets() {
        let _g = isolated_user_dir();
        let all = list_all();
        let ids: Vec<String> = all.iter().map(|t| t.id.clone()).collect();
        assert!(
            ids.contains(&"implement".to_string()),
            "expected bundled preset 'implement', got: {ids:?}"
        );
        assert!(
            ids.contains(&"scout-and-plan".to_string()),
            "expected bundled preset 'scout-and-plan', got: {ids:?}"
        );
    }

    #[test]
    fn meta_flags_has_args_when_body_contains_expansion() {
        let t = Template {
            id: "a".into(),
            name: "A".into(),
            description: "".into(),
            body: "echo $@".into(),
            source: "bundled".into(),
            origin: None,
            tags: None,
            created_at: 0,
            updated_at: 0,
        };
        assert!(meta_from(&t).has_args);
        let t2 = Template {
            body: "hello".into(),
            ..t
        };
        assert!(!meta_from(&t2).has_args);
    }

    #[test]
    fn expand_body_replaces_arg_markers() {
        assert_eq!(
            expand_body("Plan for: $@", &["foo".into(), "bar".into()]),
            "Plan for: foo bar"
        );
        assert_eq!(
            expand_body("Implement: $* end", &["x".into()]),
            "Implement: x end"
        );
        assert_eq!(expand_body("Static", &[]), "Static");
    }

    #[test]
    fn create_persists_user_template_and_shadows_bundled() {
        let _g = isolated_user_dir();
        let t = create(
            "My Template".into(),
            "Run $@".into(),
            Some("does things".into()),
            Some(vec!["ops".into()]),
        )
        .unwrap();
        assert_eq!(t.id, "my-template");
        assert_eq!(t.source, "user");

        let fetched = get_template("my-template").unwrap();
        assert_eq!(fetched.name, "My Template");
        assert_eq!(fetched.body, "Run $@");
        assert_eq!(fetched.description, "does things");
        assert_eq!(fetched.tags, Some(vec!["ops".into()]));

        let meta = list_meta();
        assert!(meta.iter().any(|m| m.id == "my-template" && m.has_args));
    }

    #[test]
    fn update_edits_user_template_and_preserves_unchanged_fields() {
        let _g = isolated_user_dir();
        let t = create(
            "Updatable".into(),
            "first".into(),
            Some("desc".into()),
            None,
        )
        .unwrap();
        let updated = update(
            &t.id,
            Some("Renamed".into()),
            None,
            Some("new desc".into()),
            None,
        )
        .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.body, "first");
        assert_eq!(updated.description, "new desc");
        assert_eq!(updated.source, "user");
    }

    #[test]
    fn update_rejects_bundled_templates() {
        let _g = isolated_user_dir();
        let err = update("scout-and-plan", None, None, None, None).unwrap_err();
        assert!(err.contains("no such user template"));
    }

    #[test]
    fn delete_removes_user_template() {
        let _g = isolated_user_dir();
        let t = create("ToDelete".into(), "body".into(), None, None).unwrap();
        assert!(get_template(&t.id).is_some());
        assert!(delete_template(&t.id));
        assert!(get_template(&t.id).is_none());
        assert!(!delete_template(&t.id));
    }

    #[test]
    fn fork_copies_bundled_to_user_store() {
        let _g = isolated_user_dir();
        let forked = fork("scout-and-plan", Some("My Scout".into())).unwrap();
        assert_eq!(forked.id, "scout-and-plan");
        assert_eq!(forked.name, "My Scout");
        assert_eq!(forked.source, "user");
        assert!(forked.body.contains("scout"));

        let meta = list_meta();
        let m = meta.iter().find(|m| m.id == "scout-and-plan").unwrap();
        assert_eq!(m.source, "user");
        assert!(m.has_args);
    }

    #[test]
    fn create_rejects_empty_name_or_body() {
        let _g = isolated_user_dir();
        assert!(create("".into(), "body".into(), None, None).is_err());
        assert!(create("name".into(), "".into(), None, None).is_err());
    }

    #[test]
    fn run_expands_args_and_requires_session() {
        let _g = isolated_user_dir();
        let t = create("RunTest".into(), "hello $@".into(), None, None).unwrap();
        let err = run(&t.id, "no-such-session", &json!("world")).unwrap_err();
        assert!(err.contains("no such session"));
    }

    #[test]
    fn run_rejects_missing_template() {
        let err = run("missing-id", "no-such-session", &json!(null)).unwrap_err();
        assert!(err.contains("no such template"));
    }
}
