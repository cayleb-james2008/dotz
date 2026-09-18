//! Git/VCS operations for spec-driven production workflows.
//!
//! dotz shells out to the user's installed `git` and optionally `gh`; it never reads or stores
//! provider tokens. This mirrors the checkpoint module and keeps VCS behavior native/self-contained.
use crate::types::{AtomicCommitRequest, RollbackTarget, VcsStatus};
use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Deserialize)]
struct BranchRequest {
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    slug: Option<String>,
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PrRequest {
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub draft: Option<bool>,
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

fn run(cwd: &FsPath, program: &str, args: &[&str]) -> Result<(String, String, i32), String> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    let output = crate::util::no_window(&mut cmd)
        .output()
        .map_err(|e| format!("{program} {}: {e}", args.join(" ")))?;
    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    ))
}

fn git(cwd: &FsPath, args: &[&str]) -> Result<(String, String, i32), String> {
    run(cwd, "git", args)
}

fn gh(cwd: &FsPath, args: &[&str]) -> Result<(String, String, i32), String> {
    run(cwd, "gh", args)
}

fn git_ok(cwd: &FsPath, args: &[&str]) -> bool {
    matches!(git(cwd, args), Ok((_, _, 0)))
}

fn trim_opt(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn parse_ahead_behind(raw: &str) -> (u32, u32) {
    let mut parts = raw.split_whitespace();
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}

fn gh_installed() -> bool {
    let mut cmd = Command::new("gh");
    cmd.arg("--version");
    crate::util::no_window(&mut cmd).output().is_ok()
}

fn gh_logged_in(cwd: &FsPath) -> bool {
    matches!(gh(cwd, &["auth", "status"]), Ok((_, _, 0)))
}

pub fn status_for_cwd(cwd: &FsPath) -> VcsStatus {
    let inside = git_ok(cwd, &["rev-parse", "--is-inside-work-tree"]);
    if !inside {
        return VcsStatus {
            cwd: cwd.to_string_lossy().to_string(),
            inside_worktree: false,
            branch: None,
            head_sha: None,
            dirty: false,
            staged: false,
            untracked: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            gh_installed: gh_installed(),
            gh_logged_in: false,
        };
    }
    let branch = git(cwd, &["branch", "--show-current"])
        .ok()
        .and_then(|(out, _, _)| trim_opt(out));
    let head_sha = git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .and_then(|(out, _, code)| if code == 0 { trim_opt(out) } else { None });
    let porcelain = git(cwd, &["status", "--porcelain"])
        .ok()
        .map(|(out, _, _)| out)
        .unwrap_or_default();
    let dirty = !porcelain.trim().is_empty();
    let staged = porcelain.lines().any(|line| {
        let bytes = line.as_bytes();
        bytes.len() >= 2 && bytes[0] != b' ' && bytes[0] != b'?'
    });
    let untracked = porcelain.lines().any(|line| line.starts_with("??"));
    let upstream = git(
        cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .and_then(|(out, _, code)| if code == 0 { trim_opt(out) } else { None });
    let (ahead, behind) = if upstream.is_some() {
        git(cwd, &["rev-list", "--left-right", "--count", "HEAD...@{u}"])
            .ok()
            .map(|(out, _, _)| parse_ahead_behind(&out))
            .unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let gh_installed = gh_installed();
    VcsStatus {
        cwd: cwd.to_string_lossy().to_string(),
        inside_worktree: true,
        branch,
        head_sha,
        dirty,
        staged,
        untracked,
        upstream,
        ahead,
        behind,
        gh_installed,
        gh_logged_in: gh_installed && gh_logged_in(cwd),
    }
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

fn safe_branch_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.starts_with('-')
        || trimmed.ends_with('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("invalid branch name".to_string());
    }
    Ok(trimmed.to_string())
}

pub fn create_or_checkout_branch(cwd: &FsPath, input: Option<&str>) -> Result<Value, String> {
    if !status_for_cwd(cwd).inside_worktree {
        return Err("project is not inside a git worktree".to_string());
    }
    let raw = input.unwrap_or("spec-change");
    let branch = if raw.starts_with("dotz/") {
        safe_branch_name(raw)?
    } else {
        let slug = slugify(raw);
        safe_branch_name(&format!(
            "dotz/{}",
            if slug.is_empty() { "change" } else { &slug }
        ))?
    };
    let exists = git(cwd, &["rev-parse", "--verify", &branch])
        .map(|(_, _, code)| code == 0)
        .unwrap_or(false);
    let args = if exists {
        vec!["checkout", &branch]
    } else {
        vec!["checkout", "-b", &branch]
    };
    let (_, stderr, code) = git(cwd, &args)?;
    if code != 0 {
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }
    Ok(json!({
        "ok": true,
        "branch": branch,
        "reused": exists,
        "status": status_for_cwd(cwd),
    }))
}

fn add_files(cwd: &FsPath, files: Option<&[String]>) -> Result<(), String> {
    if let Some(files) = files {
        if files.is_empty() {
            return Err("files must not be empty when provided".to_string());
        }
        for file in files {
            if file.trim().is_empty() || file.contains("..") {
                return Err(format!("invalid file path for commit: {file}"));
            }
            let (_, stderr, code) = git(cwd, &["add", "--", file])?;
            if code != 0 {
                return Err(format!("git add {file} failed: {stderr}"));
            }
        }
    } else {
        let (_, stderr, code) = git(cwd, &["add", "-A"])?;
        if code != 0 {
            return Err(format!("git add -A failed: {stderr}"));
        }
    }
    Ok(())
}

pub fn atomic_commit(cwd: &FsPath, req: AtomicCommitRequest) -> Result<Value, String> {
    if req.message.trim().is_empty() {
        return Err("commit message is required".to_string());
    }
    if !status_for_cwd(cwd).inside_worktree {
        return Err("project is not inside a git worktree".to_string());
    }
    add_files(cwd, req.files.as_deref())?;
    let staged = git(cwd, &["diff", "--cached", "--quiet"])
        .map(|(_, _, code)| code != 0)
        .unwrap_or(false);
    if !staged {
        return Err("nothing staged for commit".to_string());
    }
    let mut args = vec!["commit", "-m", req.message.as_str()];
    if let Some(body) = req.body.as_deref().filter(|s| !s.trim().is_empty()) {
        args.push("-m");
        args.push(body);
    }
    let (_, stderr, code) = git(cwd, &args)?;
    if code != 0 {
        return Err(format!("git commit failed: {stderr}"));
    }
    let sha = git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .and_then(|(out, _, code)| if code == 0 { trim_opt(out) } else { None });
    Ok(json!({
        "ok": true,
        "commitId": sha,
        "status": status_for_cwd(cwd),
    }))
}

pub fn create_pr(cwd: &FsPath, req: PrRequest) -> Result<Value, String> {
    if !gh_installed() {
        return Err("GitHub CLI is not installed".to_string());
    }
    if !gh_logged_in(cwd) {
        return Err("GitHub CLI is not logged in; run gh auth login outside dotz".to_string());
    }
    let mut owned: Vec<String> = vec!["pr".into(), "create".into()];
    if req.title.is_none() && req.body.is_none() {
        owned.push("--fill".into());
    }
    if let Some(title) = req.title {
        owned.push("--title".into());
        owned.push(title);
    }
    if let Some(body) = req.body {
        owned.push("--body".into());
        owned.push(body);
    }
    if let Some(base) = req.base {
        owned.push("--base".into());
        owned.push(base);
    }
    if req.draft.unwrap_or(false) {
        owned.push("--draft".into());
    }
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (stdout, stderr, code) = gh(cwd, &args)?;
    if code != 0 {
        return Err(format!("gh pr create failed: {stderr}"));
    }
    Ok(json!({ "ok": true, "url": stdout.trim() }))
}

pub fn rollback(cwd: &FsPath, target: RollbackTarget) -> Result<Value, String> {
    if target.target.trim().is_empty() {
        return Err("rollback target is required".to_string());
    }
    match target.mode.as_str() {
        "checkpoint" => {
            crate::checkpoint::restore_checkpoint(&target.target, &cwd.to_string_lossy())
                .map_err(|e| e.message())?;
            Ok(json!({ "ok": true, "mode": "checkpoint", "target": target.target }))
        }
        "revert" => {
            let (_, stderr, code) = git(cwd, &["revert", "--no-edit", &target.target])?;
            if code != 0 {
                return Err(format!("git revert failed: {stderr}"));
            }
            Ok(
                json!({ "ok": true, "mode": "revert", "target": target.target, "status": status_for_cwd(cwd) }),
            )
        }
        "reset" => {
            if !target.confirm {
                return Err("reset rollback requires confirm:true because it discards commits and worktree changes".to_string());
            }
            let (_, stderr, code) = git(cwd, &["reset", "--hard", &target.target])?;
            if code != 0 {
                return Err(format!("git reset --hard failed: {stderr}"));
            }
            Ok(
                json!({ "ok": true, "mode": "reset", "target": target.target, "status": status_for_cwd(cwd) }),
            )
        }
        other => Err(format!("unknown rollback mode: {other}")),
    }
}

fn bad(e: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": e })))
}

async fn status_handler(
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    Ok(Json(json!({ "status": status_for_cwd(&cwd) })))
}

async fn branch_handler(
    body: Option<Json<BranchRequest>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let req = body.map(|Json(v)| v).unwrap_or(BranchRequest {
        project_id: None,
        slug: None,
        name: None,
    });
    let cwd = resolve_cwd(req.project_id.as_deref())?;
    let branch = req.name.as_deref().or(req.slug.as_deref());
    create_or_checkout_branch(&cwd, branch)
        .map(Json)
        .map_err(bad)
}

async fn commit_handler(
    body: Option<Json<AtomicCommitRequest>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let req = body.map(|Json(v)| v).unwrap_or_default();
    let cwd = resolve_cwd(req.project_id.as_deref())?;
    atomic_commit(&cwd, req).map(Json).map_err(bad)
}

async fn pr_handler(
    body: Option<Json<PrRequest>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let req = body.map(|Json(v)| v).unwrap_or(PrRequest {
        project_id: None,
        title: None,
        body: None,
        base: None,
        draft: None,
    });
    let cwd = resolve_cwd(req.project_id.as_deref())?;
    create_pr(&cwd, req).map(Json).map_err(bad)
}

async fn rollback_handler(
    body: Option<Json<RollbackTarget>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let target = body.map(|Json(v)| v).unwrap_or_default();
    let cwd = resolve_cwd(target.project_id.as_deref())?;
    rollback(&cwd, target).map(Json).map_err(bad)
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/vcs/status", get(status_handler))
        .route("/api/vcs/branch", post(branch_handler))
        .route("/api/vcs/commit", post(commit_handler))
        .route("/api/vcs/pr", post(pr_handler))
        .route("/api/vcs/rollback", post(rollback_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn tmp_git_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dotz-vcs-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init"]).unwrap();
        git(&dir, &["config", "core.autocrlf", "false"]).unwrap();
        git(&dir, &["config", "user.email", "test@dotz"]).unwrap();
        git(&dir, &["config", "user.name", "dotz test"]).unwrap();
        std::fs::write(dir.join("README.md"), "# dotz\n").unwrap();
        git(&dir, &["add", "README.md"]).unwrap();
        git(&dir, &["commit", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn status_detects_dirty_staged_and_untracked() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = tmp_git_repo();
        std::fs::write(dir.join("README.md"), "# changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new").unwrap();
        git(&dir, &["add", "README.md"]).unwrap();
        let status = status_for_cwd(&dir);
        assert!(status.inside_worktree);
        assert!(status.dirty);
        assert!(status.staged);
        assert!(status.untracked);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_creates_dotz_prefixed_branch() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = tmp_git_repo();
        let result = create_or_checkout_branch(&dir, Some("spec driven")).unwrap();
        assert_eq!(result["branch"], "dotz/spec-driven");
        assert_eq!(
            status_for_cwd(&dir).branch.as_deref(),
            Some("dotz/spec-driven")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_commit_commits_staged_logical_change() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = tmp_git_repo();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        let result = atomic_commit(
            &dir,
            AtomicCommitRequest {
                project_id: None,
                message: "add file".into(),
                body: None,
                files: Some(vec!["file.txt".into()]),
            },
        )
        .unwrap();
        assert_eq!(result["ok"], true);
        assert!(result["commitId"].as_str().unwrap_or("").len() >= 40);
        assert!(!status_for_cwd(&dir).dirty);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_rollback_requires_confirmation() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = tmp_git_repo();
        let head = status_for_cwd(&dir).head_sha.unwrap();
        let err = rollback(
            &dir,
            RollbackTarget {
                project_id: None,
                target: head,
                mode: "reset".into(),
                confirm: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("confirm:true"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
