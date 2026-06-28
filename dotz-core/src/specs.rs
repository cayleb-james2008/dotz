//! Native OpenSpec-compatible change store for dotz.
//!
//! dotz owns the `openspec/changes/<slug>/` layout directly instead of depending on a global
//! `openspec` CLI. The layout stays compatible with OPSX conventions (`proposal.md`, `design.md`,
//! `tasks.md`, `specs/`) and adds `readiness.md` as the production gate artifact.
use crate::types::{ReadinessFinding, SpecArtifact, SpecChange, SpecStatus};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const META_FILE: &str = ".dotz.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SpecMeta {
    status: SpecStatus,
    #[serde(rename = "createdAt")]
    created_at: u64,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
    #[serde(rename = "archivedAt", skip_serializing_if = "Option::is_none")]
    archived_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateSpecChange {
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct PatchSpecChange {
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    title: Option<String>,
    description: Option<String>,
    proposal: Option<String>,
    design: Option<String>,
    tasks: Option<String>,
    readiness: Option<String>,
    spec: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = true;
    for ch in input.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn clean_id(id: &str) -> Option<String> {
    let trimmed = id.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_cwd(project_id: Option<&str>) -> Result<PathBuf, (StatusCode, Json<Value>)> {
    if let Some(id) = project_id.map(str::trim).filter(|s| !s.is_empty()) {
        return crate::projects::cwd_for_project(Some(id))
            .map(PathBuf::from)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "no such project" })),
                )
            });
    }
    std::env::current_dir().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to resolve cwd: {e}") })),
        )
    })
}

fn openspec_dir(cwd: &FsPath) -> PathBuf {
    cwd.join("openspec")
}

fn changes_dir(cwd: &FsPath) -> PathBuf {
    openspec_dir(cwd).join("changes")
}

fn specs_dir(cwd: &FsPath) -> PathBuf {
    openspec_dir(cwd).join("specs")
}

fn change_dir(cwd: &FsPath, id: &str) -> Result<PathBuf, String> {
    let id = clean_id(id).ok_or_else(|| "invalid change id".to_string())?;
    Ok(changes_dir(cwd).join(id))
}

fn read_meta(dir: &FsPath) -> SpecMeta {
    let path = dir.join(META_FILE);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SpecMeta>(&raw).ok())
        .unwrap_or_else(|| SpecMeta {
            status: SpecStatus::Draft,
            created_at: dir_mtime(dir),
            updated_at: dir_mtime(dir),
            archived_at: None,
        })
}

fn write_meta(dir: &FsPath, meta: &SpecMeta) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(META_FILE), raw).map_err(|e| e.to_string())
}

fn dir_mtime(dir: &FsPath) -> u64 {
    std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(now_ms)
}

fn read_file(dir: &FsPath, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

fn proposal_title_and_description(dir: &FsPath) -> (String, String) {
    let raw = read_file(dir, "proposal.md");
    let title = raw
        .lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled-change")
        })
        .to_string();
    let description = raw
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("Status:")
                && !line.starts_with("- ")
        })
        .unwrap_or("")
        .to_string();
    (title, description)
}

fn artifact(cwd: &FsPath, dir: &FsPath, kind: &str, path: PathBuf) -> SpecArtifact {
    let rel = path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    let exists = path.exists();
    let size_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
    SpecArtifact {
        kind: kind.to_string(),
        path: if rel.is_empty() {
            dir.to_string_lossy().to_string()
        } else {
            rel
        },
        exists,
        size_bytes,
    }
}

fn artifacts(cwd: &FsPath, dir: &FsPath, id: &str) -> Vec<SpecArtifact> {
    let spec_file = dir.join("specs").join(format!("{id}.md"));
    vec![
        artifact(cwd, dir, "proposal", dir.join("proposal.md")),
        artifact(cwd, dir, "design", dir.join("design.md")),
        artifact(cwd, dir, "tasks", dir.join("tasks.md")),
        artifact(cwd, dir, "readiness", dir.join("readiness.md")),
        artifact(cwd, dir, "spec", spec_file),
    ]
}

fn readiness_findings(dir: &FsPath) -> Vec<ReadinessFinding> {
    let raw = read_file(dir, "readiness.md");
    raw.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (status, rest) = if let Some(rest) = trimmed.strip_prefix("- [x]") {
                ("complete", rest)
            } else if let Some(rest) = trimmed.strip_prefix("- [X]") {
                ("complete", rest)
            } else if let Some(rest) = trimmed.strip_prefix("- [ ]") {
                ("pending", rest)
            } else {
                return None;
            };
            let title = rest.trim().trim_start_matches('-').trim().to_string();
            if title.is_empty() {
                return None;
            }
            let area = title
                .split(':')
                .next()
                .unwrap_or(&title)
                .trim_matches('*')
                .trim()
                .to_lowercase()
                .replace(' ', "_");
            Some(ReadinessFinding {
                id: slugify(&title),
                area,
                status: status.to_string(),
                title,
                detail: None,
                severity: Some("required".to_string()),
            })
        })
        .collect()
}

fn task_incomplete_count(dir: &FsPath) -> usize {
    read_file(dir, "tasks.md")
        .lines()
        .filter(|line| line.trim_start().starts_with("- [ ]"))
        .count()
}

fn materialize_change(cwd: &FsPath, dir: &FsPath, archived: bool) -> Option<SpecChange> {
    let id = dir.file_name()?.to_str()?.to_string();
    if id == "archive" {
        return None;
    }
    let (title, description) = proposal_title_and_description(dir);
    let mut meta = read_meta(dir);
    if archived {
        meta.status = SpecStatus::Archived;
        if meta.archived_at.is_none() {
            meta.archived_at = Some(meta.updated_at);
        }
    }
    let readiness = readiness_findings(dir);
    Some(SpecChange {
        id: id.clone(),
        title,
        description,
        status: meta.status,
        path: dir
            .strip_prefix(cwd)
            .unwrap_or(dir)
            .to_string_lossy()
            .replace('\\', "/"),
        artifacts: artifacts(cwd, dir, &id),
        readiness,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        archived_at: meta.archived_at,
    })
}

pub fn list_changes(cwd: &FsPath) -> Vec<SpecChange> {
    let mut out = Vec::new();
    let root = changes_dir(cwd);
    if let Ok(entries) = std::fs::read_dir(&root) {
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_dir() {
                if let Some(change) = materialize_change(cwd, &path, false) {
                    out.push(change);
                }
            }
        }
    }
    let archive_root = root.join("archive");
    if let Ok(entries) = std::fs::read_dir(&archive_root) {
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_dir() {
                if let Some(change) = materialize_change(cwd, &path, true) {
                    out.push(change);
                }
            }
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    out
}

pub fn status_for_cwd(cwd: &FsPath) -> Value {
    let changes = list_changes(cwd);
    let active = changes
        .iter()
        .filter(|c| c.status != SpecStatus::Archived)
        .count();
    let blocked = changes
        .iter()
        .filter(|c| c.status == SpecStatus::Blocked)
        .count();
    Json(json!({
        "cwd": cwd.to_string_lossy(),
        "openspecDir": openspec_dir(cwd).to_string_lossy(),
        "changes": changes,
        "active": active,
        "blocked": blocked,
        "native": true,
        "cli": openspec_cli_version(),
    }))
    .0
}

fn openspec_cli_version() -> Value {
    match std::process::Command::new("openspec")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => json!({
            "installed": true,
            "version": String::from_utf8_lossy(&out.stdout).trim(),
        }),
        _ => json!({ "installed": false }),
    }
}

fn unique_slug(cwd: &FsPath, base: &str) -> String {
    let mut slug = slugify(base);
    if slug.is_empty() {
        slug = format!("change-{}", now_ms());
    }
    let original = slug.clone();
    let mut n = 2;
    while changes_dir(cwd).join(&slug).exists() {
        slug = format!("{original}-{n}");
        n += 1;
    }
    slug
}

fn proposal_template(title: &str, description: &str) -> String {
    format!(
        "# {title}\n\n{description}\n\n## Problem\n\n- State the user-visible problem and the current behavior.\n\n## Proposed Change\n\n- Describe the intended behavior and affected surfaces.\n\n## Impact\n\n- List risks, migrations, and compatibility notes.\n"
    )
}

fn design_template(title: &str) -> String {
    format!(
        "# Design: {title}\n\n## Architecture\n\n- Record the chosen implementation shape.\n\n## Interfaces\n\n- REST, tool, UI, storage, and workflow contract changes.\n\n## Alternatives Considered\n\n- Note rejected options when they matter.\n"
    )
}

fn tasks_template() -> String {
    "- [ ] Stabilize baseline and capture failing tests before behavior changes.\n- [ ] Implement the smallest spec-compliant runtime change.\n- [ ] Update agent prompts/tools and UI contract surfaces.\n- [ ] Verify tests/build and record rollback or release notes.\n".to_string()
}

fn readiness_template() -> String {
    "# Production Readiness\n\n- [ ] TDD/regression tests: failing or protective tests cover the change.\n- [ ] Auth/authz: permissions, identities, and secret boundaries are explicit.\n- [ ] Error handling: failure modes return clear operator-safe errors.\n- [ ] Migrations/data risks: persistence, rollback, and compatibility are documented.\n- [ ] Security: input validation and trust boundaries were reviewed.\n- [ ] Hosting/deploy: packaging, update, and environment assumptions are known.\n- [ ] Observability: logs, UI status, or metrics expose success/failure.\n- [ ] VCS/release: branch, atomic commit, PR, and rollback plan are ready.\n".to_string()
}

fn spec_template(id: &str, title: &str) -> String {
    format!(
        "# {title}\n\n## ADDED Requirements\n\n### Requirement: {title}\n\nThe system SHALL implement the behavior described by change `{id}`.\n\n#### Scenario: Default path\n\n- **WHEN** the workflow runs\n- **THEN** the spec artifacts and readiness gates guide implementation before code changes\n"
    )
}

pub fn create_change(cwd: &FsPath, req: CreateSpecChange) -> Result<SpecChange, String> {
    let title = req.title.trim();
    if title.is_empty() {
        return Err("title is required".to_string());
    }
    let description = req.description.trim();
    let slug = req
        .slug
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| unique_slug(cwd, title));
    let id = unique_slug(cwd, &slug);
    let dir = changes_dir(cwd).join(&id);
    let spec_dir = dir.join("specs");
    std::fs::create_dir_all(&spec_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(specs_dir(cwd)).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("proposal.md"),
        proposal_template(title, description),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(dir.join("design.md"), design_template(title)).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("tasks.md"), tasks_template()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("readiness.md"), readiness_template()).map_err(|e| e.to_string())?;
    std::fs::write(spec_dir.join(format!("{id}.md")), spec_template(&id, title))
        .map_err(|e| e.to_string())?;
    let ts = now_ms();
    write_meta(
        &dir,
        &SpecMeta {
            status: SpecStatus::Draft,
            created_at: ts,
            updated_at: ts,
            archived_at: None,
        },
    )?;
    materialize_change(cwd, &dir, false).ok_or_else(|| "failed to load created change".to_string())
}

pub fn get_change(cwd: &FsPath, id: &str) -> Result<SpecChange, String> {
    let dir = change_dir(cwd, id)?;
    if dir.exists() {
        return materialize_change(cwd, &dir, false)
            .ok_or_else(|| "failed to load change".to_string());
    }
    let archive = changes_dir(cwd).join("archive");
    if let Ok(entries) = std::fs::read_dir(archive) {
        for ent in entries.flatten() {
            let path = ent.path();
            if path.is_dir()
                && path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(id))
                    .unwrap_or(false)
            {
                return materialize_change(cwd, &path, true)
                    .ok_or_else(|| "failed to load archived change".to_string());
            }
        }
    }
    Err("no such spec change".to_string())
}

fn set_status(dir: &FsPath, status: SpecStatus) -> Result<(), String> {
    let mut meta = read_meta(dir);
    meta.status = status;
    meta.updated_at = now_ms();
    write_meta(dir, &meta)
}

fn update_change(cwd: &FsPath, id: &str, patch: PatchSpecChange) -> Result<SpecChange, String> {
    let dir = change_dir(cwd, id)?;
    if !dir.exists() {
        return Err("no such spec change".to_string());
    }
    if patch.title.is_some() || patch.description.is_some() {
        let (old_title, old_description) = proposal_title_and_description(&dir);
        let title = patch.title.as_deref().unwrap_or(&old_title);
        let desc = patch.description.as_deref().unwrap_or(&old_description);
        std::fs::write(dir.join("proposal.md"), proposal_template(title, desc))
            .map_err(|e| e.to_string())?;
    }
    if let Some(content) = patch.proposal {
        std::fs::write(dir.join("proposal.md"), content).map_err(|e| e.to_string())?;
    }
    if let Some(content) = patch.design {
        std::fs::write(dir.join("design.md"), content).map_err(|e| e.to_string())?;
    }
    if let Some(content) = patch.tasks {
        std::fs::write(dir.join("tasks.md"), content).map_err(|e| e.to_string())?;
    }
    if let Some(content) = patch.readiness {
        std::fs::write(dir.join("readiness.md"), content).map_err(|e| e.to_string())?;
    }
    if let Some(content) = patch.spec {
        let spec_file = dir.join("specs").join(format!("{id}.md"));
        if let Some(parent) = spec_file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(spec_file, content).map_err(|e| e.to_string())?;
    }
    let mut meta = read_meta(&dir);
    meta.updated_at = now_ms();
    write_meta(&dir, &meta)?;
    get_change(cwd, id)
}

pub fn apply_change(cwd: &FsPath, id: &str) -> Result<Value, String> {
    let dir = change_dir(cwd, id)?;
    if !dir.exists() {
        return Err("no such spec change".to_string());
    }
    set_status(&dir, SpecStatus::Applying)?;
    Ok(json!({
        "change": get_change(cwd, id)?,
        "tasks": read_file(&dir, "tasks.md"),
        "message": "Spec is marked applying. Execute tasks one logical item at a time and verify readiness before commit.",
    }))
}

pub fn verify_change(cwd: &FsPath, id: &str) -> Result<Value, String> {
    let dir = change_dir(cwd, id)?;
    if !dir.exists() {
        return Err("no such spec change".to_string());
    }
    let missing_artifacts: Vec<SpecArtifact> = artifacts(cwd, &dir, id)
        .into_iter()
        .filter(|a| !a.exists)
        .collect();
    let readiness = readiness_findings(&dir);
    let pending = readiness.iter().filter(|r| r.status != "complete").count();
    let incomplete_tasks = task_incomplete_count(&dir);
    let ok = missing_artifacts.is_empty() && pending == 0 && incomplete_tasks == 0;
    set_status(
        &dir,
        if ok {
            SpecStatus::Verified
        } else {
            SpecStatus::Blocked
        },
    )?;
    Ok(json!({
        "ok": ok,
        "change": get_change(cwd, id)?,
        "missingArtifacts": missing_artifacts,
        "pendingReadiness": pending,
        "incompleteTasks": incomplete_tasks,
    }))
}

pub fn sync_change(cwd: &FsPath, id: &str) -> Result<Value, String> {
    let dir = change_dir(cwd, id)?;
    if !dir.exists() {
        return Err("no such spec change".to_string());
    }
    let src = dir.join("specs");
    let dst = specs_dir(cwd);
    std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
    let mut copied = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&src) {
        for ent in entries.flatten() {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let target = dst.join(ent.file_name());
                std::fs::copy(&path, &target).map_err(|e| e.to_string())?;
                copied.push(target.to_string_lossy().to_string());
            }
        }
    }
    set_status(&dir, SpecStatus::Ready)?;
    Ok(json!({ "ok": true, "copied": copied, "change": get_change(cwd, id)? }))
}

pub fn archive_change(cwd: &FsPath, id: &str) -> Result<Value, String> {
    let src = change_dir(cwd, id)?;
    if !src.exists() {
        return Err("no such spec change".to_string());
    }
    let archive_root = changes_dir(cwd).join("archive");
    std::fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;
    let dest = archive_root.join(format!("{}-{id}", now_ms()));
    let mut meta = read_meta(&src);
    meta.status = SpecStatus::Archived;
    meta.updated_at = now_ms();
    meta.archived_at = Some(meta.updated_at);
    write_meta(&src, &meta)?;
    std::fs::rename(&src, &dest).map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "archivedPath": dest.to_string_lossy(),
        "change": materialize_change(cwd, &dest, true),
    }))
}

fn error_response(e: String) -> (StatusCode, Json<Value>) {
    let status = if e.contains("no such") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(json!({ "error": e })))
}

async fn status_handler(
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    Ok(Json(status_for_cwd(&cwd)))
}

async fn list_handler(
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    Ok(Json(json!({ "changes": list_changes(&cwd) })))
}

async fn create_handler(
    body: Option<Json<CreateSpecChange>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or(CreateSpecChange {
        project_id: None,
        title: String::new(),
        description: String::new(),
        slug: None,
    });
    let cwd = resolve_cwd(b.project_id.as_deref())?;
    create_change(&cwd, b)
        .map(|change| Json(json!({ "change": change })))
        .map_err(error_response)
}

async fn get_handler(
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    get_change(&cwd, &id)
        .map(|change| Json(json!({ "change": change })))
        .map_err(error_response)
}

async fn patch_handler(
    Path(id): Path<String>,
    body: Option<Json<PatchSpecChange>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_default();
    let cwd = resolve_cwd(b.project_id.as_deref())?;
    update_change(&cwd, &id, b)
        .map(|change| Json(json!({ "change": change })))
        .map_err(error_response)
}

async fn action_handler(
    Path((id, action)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let project_id = body
        .as_ref()
        .and_then(|Json(v)| v.get("projectId"))
        .and_then(|v| v.as_str());
    let cwd = resolve_cwd(project_id)?;
    let result = match action.as_str() {
        "apply" => apply_change(&cwd, &id),
        "verify" => verify_change(&cwd, &id),
        "sync" => sync_change(&cwd, &id),
        "archive" => archive_change(&cwd, &id),
        _ => Err(format!("unknown spec action: {action}")),
    };
    result.map(Json).map_err(error_response)
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/specs/status", get(status_handler))
        .route("/api/specs/changes", get(list_handler).post(create_handler))
        .route(
            "/api/specs/changes/{id}",
            get(get_handler).patch(patch_handler),
        )
        .route("/api/specs/changes/{id}/{action}", post(action_handler))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dotz-specs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_change_writes_openspec_layout_and_readiness() {
        let dir = tmp_dir();
        let change = create_change(
            &dir,
            CreateSpecChange {
                project_id: None,
                title: "Spec Driven Flow".into(),
                description: "Make changes spec-first.".into(),
                slug: None,
            },
        )
        .unwrap();

        assert_eq!(change.id, "spec-driven-flow");
        let root = dir.join("openspec").join("changes").join(&change.id);
        assert!(root.join("proposal.md").exists());
        assert!(root.join("design.md").exists());
        assert!(root.join("tasks.md").exists());
        assert!(root.join("readiness.md").exists());
        assert!(root.join("specs").join("spec-driven-flow.md").exists());
        assert!(std::fs::read_to_string(root.join("readiness.md"))
            .unwrap()
            .contains("Observability"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn verify_blocks_until_tasks_and_readiness_are_complete() {
        let dir = tmp_dir();
        let change = create_change(
            &dir,
            CreateSpecChange {
                project_id: None,
                title: "Ready Gate".into(),
                description: "".into(),
                slug: None,
            },
        )
        .unwrap();
        let blocked = verify_change(&dir, &change.id).unwrap();
        assert_eq!(blocked["ok"], false);
        assert_eq!(blocked["change"]["status"], "blocked");

        let root = dir.join("openspec").join("changes").join(&change.id);
        let tasks = std::fs::read_to_string(root.join("tasks.md"))
            .unwrap()
            .replace("- [ ]", "- [x]");
        std::fs::write(root.join("tasks.md"), tasks).unwrap();
        let readiness = std::fs::read_to_string(root.join("readiness.md"))
            .unwrap()
            .replace("- [ ]", "- [x]");
        std::fs::write(root.join("readiness.md"), readiness).unwrap();

        let ok = verify_change(&dir, &change.id).unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["change"]["status"], "verified");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sync_copies_change_specs_to_project_specs() {
        let dir = tmp_dir();
        let change = create_change(
            &dir,
            CreateSpecChange {
                project_id: None,
                title: "Sync Specs".into(),
                description: "".into(),
                slug: None,
            },
        )
        .unwrap();
        let result = sync_change(&dir, &change.id).unwrap();
        assert_eq!(result["ok"], true);
        assert!(dir
            .join("openspec")
            .join("specs")
            .join("sync-specs.md")
            .exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archive_moves_change_under_archive() {
        let dir = tmp_dir();
        let change = create_change(
            &dir,
            CreateSpecChange {
                project_id: None,
                title: "Archive Me".into(),
                description: "".into(),
                slug: None,
            },
        )
        .unwrap();
        let result = archive_change(&dir, &change.id).unwrap();
        assert_eq!(result["ok"], true);
        assert!(!dir
            .join("openspec")
            .join("changes")
            .join("archive-me")
            .exists());
        assert_eq!(
            list_changes(&dir)
                .iter()
                .find(|c| c.id.ends_with("archive-me"))
                .unwrap()
                .status,
            SpecStatus::Archived
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_markdown_falls_back_to_slug_title() {
        let dir = tmp_dir();
        let root = dir.join("openspec").join("changes").join("bad-md");
        std::fs::create_dir_all(root.join("specs")).unwrap();
        std::fs::write(root.join("proposal.md"), "---\n:not yaml\n").unwrap();
        std::fs::write(root.join("design.md"), "").unwrap();
        std::fs::write(root.join("tasks.md"), "").unwrap();
        std::fs::write(root.join("readiness.md"), "").unwrap();
        std::fs::write(root.join("specs").join("bad-md.md"), "").unwrap();

        let change = get_change(&dir, "bad-md").unwrap();
        assert_eq!(change.title, "bad-md");
        let _ = std::fs::remove_dir_all(dir);
    }
}
