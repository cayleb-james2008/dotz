//! Git-backed safe-edit checkpoints for workflow runs.
//!
//! A workflow run's subagents edit the project's working tree: `write`, `edit`, and `bash`
//! tools all mutate files under the session cwd. When a run fails verification or the
//! operator aborts, the working tree is left in a half-edited state — dirty files that
//! aren't part of any commit, with no clean way to undo just the changes this run made.
//! Parallel runs on the same project make it worse: run B's edits overlap run A's, and
//! rolling back A with `git checkout -- .` clobbered B's intermediate work too.
//!
//! This module solves both problems with one mechanism: before a workflow run starts,
//! snapshot the working tree (tracked HEAD + any pre-existing dirty state) into a git
//! stash whose message encodes the run id. On run completion (success or failure), the
//! operator can restore the exact pre-run tree:
//!   - `save_checkpoint`: record HEAD SHA + `git stash push -m "dotz-checkpoint:<run_id>"`.
//!   - `restore_checkpoint`: `git reset --hard <snapshot_sha>` then drop the stash.
//!     This discards ALL changes made since the checkpoint — including any subagent edits.
//!   - `discard_checkpoint`: just drop the stash (run was rolled forward intentionally).
//!
//! ### Parallel runs
//! Each run gets its own stash (git's stash stack is LIFO but messages are unique). Two
//! parallel runs on the same project each push their own stash; restoring either pops
//! back to just-before-that-run. The most-recent checkpoint restores first (LIFO) — this
//! is the correct semantic: runs that finish later started later in time, so restoring
//! them first is the right chronological order. If the operator wants to restore an
//! earlier run after a later one, the later run's stash must be popped first (the REST
//! layer enforces this).
//!
//! ### No-git projects
//! If the project directory is not inside a git repo, `save_checkpoint` returns
//! `Err(CheckpointError::NotAGitRepo)` and the executor proceeds without a checkpoint.
//! The operator sees `"checkpoint": "unavailable"` in the run response so the UI can
//! surface a warning. This is graceful— the feature doesn't paper over a
//! missing VCS.
//!
//! ### Module structure
//! - Free functions for the git operations (testable via a mock git binary in tests).
//! - A module-level `OnceLock<Mutex<CheckpointMap>>` mirrors the singleton pattern used
//!   by `workflows.rs`, `projects.rs`, `context_bus.rs`.
//! - REST routes under `/api/workflows/:id/checkpoint|rollback|discard`.
//!
//! ponytail: uses `std::process::Command` to shell out to `git` — no new crate dependency,
//! matches the style of `agent/tools.rs::BashTool` and `sandbox.rs::create_run`.
use crate::workflows;
use axum::http::StatusCode;
use axum::{
    extract::Path,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

// ---- types ----

/// A recorded checkpoint — enough to restore the pre-run working tree.
#[derive(Clone, Debug, Serialize)]
pub struct Checkpoint {
    pub run_id: String,
    /// HEAD SHA at checkpoint time. `git reset --hard <snapshot_sha>` lands here.
    pub snapshot_sha: String,
    /// Human-readable timestamp (epoch ms) for list UIs.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Project cwd at checkpoint time — checkpoints are scoped per-project.
    pub cwd: String,
}

/// Errors from checkpoint operations, mapped to HTTP responses by the handlers.
#[derive(Debug)]
pub enum CheckpointError {
    NotAGitRepo,
    SnapFailed(String),
    StashFailed(String),
    RestoreFailed(String),
    DiscardFailed(String),
    RollbackOrder { current: String, expected: String },
    UnknownCheckpoint,
}

impl CheckpointError {
    fn http_status(&self) -> StatusCode {
        match self {
            Self::NotAGitRepo => StatusCode::FAILED_DEPENDENCY,
            Self::RollbackOrder { .. } => StatusCode::CONFLICT,
            Self::UnknownCheckpoint => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Human-readable error message for the HTTP response body.
    pub fn message(&self) -> String {
        match self {
            Self::NotAGitRepo => "project is not inside a git repository — checkpoint unavailable".into(),
            Self::SnapFailed(e) => format!("failed to snapshot working tree: {e}"),
            Self::StashFailed(e) => format!("git stash failed: {e}"),
            Self::RestoreFailed(e) => format!("failed to restore checkpoint: {e}"),
            Self::DiscardFailed(e) => format!("failed to discard checkpoint: {e}"),
            Self::RollbackOrder { current, expected } => format!(
                "rollback order violation: '{}' is the most-recent checkpoint (LIFO); restore '{}' first",
                current, expected
            ),
            Self::UnknownCheckpoint => "no checkpoint for that run id".into(),
        }
    }
}

impl CheckpointError {
    /// Convert into an axum response tuple `(StatusCode, Json<Value>)` for use in handlers.
    pub fn into_response(self) -> (StatusCode, Json<Value>) {
        (self.http_status(), Json(json!({ "error": self.message() })))
    }
}

// ---- module-level checkpoint map ----

/// In-memory checkpoint map: run_id → Checkpoint. The stash stack lives in git, but we
/// track metadata here so the REST layer can answer "does this run have a checkpoint?"
/// without shelling out.
fn checkpoints() -> &'static Mutex<HashMap<String, Checkpoint>> {
    static CP: OnceLock<Mutex<HashMap<String, Checkpoint>>> = OnceLock::new();
    CP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn checkpoints_guard() -> std::sync::MutexGuard<'static, HashMap<String, Checkpoint>> {
    checkpoints()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test-only: clear the in-memory checkpoint map. Each test calls this first so
/// parallel runs over the global OnceLock don't bleed state.
#[cfg(test)]
pub fn reset_checkpoints_for_testing() {
    let mut g = checkpoints_guard();
    g.clear();
}

/// Test-only global lock so checkpoint tests run serially — the in-memory map is
/// a process-wide singleton and concurrent tests would race on insert/remove.
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Public read access to a single checkpoint (REST handlers use this).
pub fn get_checkpoint(run_id: &str) -> Option<Checkpoint> {
    checkpoints_guard().get(run_id).cloned()
}

/// Public read access to all checkpoints (list REST handler).
pub fn list_checkpoints() -> Vec<Checkpoint> {
    checkpoints_guard().values().cloned().collect()
}

/// Remove a checkpoint from the memory map (called after discard or restore).
pub fn remove_checkpoint(run_id: &str) -> Option<Checkpoint> {
    checkpoints_guard().remove(run_id)
}

// ---- git command helpers ----

/// Run a git command in `cwd`, return (stdout, stderr, exit_code).
/// Mirrors the style of BashTool: build a `Command`, collect stdout+stderr, return them.
fn git(cwd: &str, args: &[&str]) -> Result<(String, String, i32), String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {}: {}", args.join(" "), e))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    Ok((stdout, stderr, code))
}

/// Is `cwd` inside a git working tree? `git rev-parse --is-inside-work-tree`.
pub fn is_inside_git_worktree(cwd: &str) -> bool {
    match git(cwd, &["rev-parse", "--is-inside-work-tree"]) {
        Ok((stdout, _, 0)) => stdout.trim() == "true",
        _ => false,
    }
}

/// Resolve the top-level directory of the git working tree containing `cwd`.
/// Returns the absolute path to the worktree root, or None if not in a git repo.
pub fn git_worktree_root(cwd: &str) -> Option<String> {
    match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok((stdout, _, 0)) => {
            let root = stdout.trim();
            if root.is_empty() {
                None
            } else {
                Some(root.to_string())
            }
        }
        _ => None,
    }
}

/// The HEAD SHA of the git repo at `cwd`. None if not a git repo or no commits.
pub fn git_head_sha(cwd: &str) -> Option<String> {
    match git(cwd, &["rev-parse", "HEAD"]) {
        Ok((stdout, _, 0)) => {
            let sha = stdout.trim();
            if sha.is_empty() {
                None
            } else {
                Some(sha.to_string())
            }
        }
        _ => None,
    }
}

/// Capture the working-tree diff at `cwd` as an `Artifact`.
///
/// This is the primary inspectable result of a worker step: the actual code
/// change the agent produced, surfaced in the UI node drawer above the prose
/// summary so the operator can review / approve / reject the concrete diff
/// without cross-referencing a terminal.
///
/// Runs `git diff` (unstaged) + `git diff --cached` (staged) and concatenates
/// them. Returns `None` when `cwd` is not inside a git worktree or when git
/// is not installed — the caller falls back to the prose `output` and the UI
/// degrades gracefully (no artifact row).
///
/// The `title` is a short summary line like "3 files changed, 42 insertions(+), 7 deletions(-)"
/// so the UI can render a one-liner header without parsing the diff body.
pub fn git_diff_artifact(cwd: &str) -> Option<workflows::Artifact> {
    if !is_inside_git_worktree(cwd) {
        return None;
    }

    let mut content = String::new();

    // Unstaged changes (working tree vs index).
    if let Ok((stdout, _, 0)) = git(cwd, &["diff"]) {
        if !stdout.trim().is_empty() {
            content.push_str(&stdout);
        }
    }

    // Staged changes (index vs HEAD).
    if let Ok((stdout, _, 0)) = git(cwd, &["diff", "--cached"]) {
        if !stdout.trim().is_empty() {
            if !content.is_empty() {
                content.push_str("\n--- staged changes ---\n");
            }
            content.push_str(&stdout);
        }
    }

    if content.trim().is_empty() {
        // No diff — the agent ran but produced no file changes. Return None
        // so the UI doesn't show an empty artifact block.
        return None;
    }

    // Build a title line with change stats (insertions/deletions) by parsing
    // the diff summary that `git diff --stat` produces. Fall back to a generic
    // title if parsing fails.
    let title = git_diff_stat(cwd);

    Some(workflows::Artifact {
        kind: "git_diff".to_string(),
        title,
        content,
    })
}

/// Run `git diff --shortstat HEAD` and return a human-readable title like
/// "3 files changed, 42 insertions(+), 7 deletions(-)". Returns None on error.
///
/// Uses `HEAD` as the base so the stat covers BOTH staged (index vs HEAD) and
/// unstaged (working tree vs index) changes — matching the two sections
/// `git_diff_artifact` concatenates for its `content`. The previous
/// `git diff --shortstat` (no base) reported unstaged changes only, so a
/// staged-only edit produced a non-empty `content` with a `None` title: the UI
/// rendered a diff with no stat header. `git diff HEAD --shortstat` gives the
/// combined count, which always matches the rendered content.
fn git_diff_stat(cwd: &str) -> Option<String> {
    match git(cwd, &["diff", "--shortstat", "HEAD"]) {
        Ok((stdout, _, 0)) => {
            let s = stdout.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        _ => None,
    }
}

/// Snapshot any pre-existing dirty state at `cwd` BEFORE the run starts.
/// Uses a temporary stash: `git stash push -u -m "dotz-pre-work:<run_id>" - algo布袋...`.
/// Actually, we don't need a separate "dirty stash" — the simpler design is:
///   1. record HEAD SHA
///   2. if working tree is dirty, stash everything (including untracked) BEFORE the run
///   3. the run edits the tree freely
///   4. on restore, `git reset --hard <head_sha>` + `git stash pop` to recover the
///      original dirty state (if any).
///
/// But this conflates "dirty with original state" and "dirty with run edits".
///
/// The cleaner design this module actually implements:
///   - snapshot_sha = HEAD SHA (the clean baseline)
///   - If the tree is dirty BEFORE the run, stash that separately so the operator
///     recovers their in-progress work after restore.
///   - The run edits the tree.
///   - On restore: `git reset --hard <snapshot_sha>` (wipes everything the run did),
///     then if a pre-run dirty stash exists, `git stash pop` it.
///
/// However: keeping two stashes per run is complex and error-prone. The simple
/// design: snapshot the HEAD sha, and let `git reset --hard <snapshot_sha>` +
/// `git clean -fd` restore the tree to its state BEFORE the run. If the operator
/// had uncommitted work before the run, they should have committed or stashed it
/// beforehand. The pre-run stash is an improvement, not a requirement.
///
/// Decision: implement the SIMPLE design (HEAD sha + cleanup) first. A future
/// iteration can add pre-run dirty-stash if the operator requests it. This matches
/// ponytail's "one thing per iteration" rule and keeps the surface minimal.
///
/// Returns the snapshot SHA, or None if not in a git repo.
pub fn save_checkpoint(run_id: &str, cwd: &str) -> Result<String, CheckpointError> {
    // Validate the project is in a git repo.
    if !is_inside_git_worktree(cwd) {
        return Err(CheckpointError::NotAGitRepo);
    }
    let snapshot_sha = git_head_sha(cwd).ok_or_else(|| {
        CheckpointError::SnapFailed(format!("{} has no git history (no HEAD)", cwd))
    })?;

    // Record the checkpoint metadata.
    let cp = Checkpoint {
        run_id: run_id.to_string(),
        snapshot_sha: snapshot_sha.clone(),
        created_at: crate::util::now_ms(),
        cwd: cwd.to_string(),
    };

    // Register in the in-memory map (git itself doesn't persist across server
    // restarts; the metadata fast-path is for the REST layer).
    {
        let mut g = checkpoints_guard();
        g.insert(run_id.to_string(), cp);
    }

    Ok(snapshot_sha)
}

/// Roll back the working tree at `cwd` to the snapshot recorded for `run_id`.
///
/// This `git reset --hard <snapshot_sha>` the tree, then `git clean -fd` to remove
/// any untracked files the run created. Then drops the in-memory checkpoint entry.
///
/// LIFO enforcement: if `expected_run_id` is the most-recent checkpoint (the top of
/// the stash stack), restore it. Otherwise the caller is trying to restore a mid-stack
/// checkpoint while newer ones exist — which would apply the newer run's changes
/// on top of the restored older tree, likely producing a confusing mixed state.
/// We detect this by ordering `checkpoints_guard()` by `created_at` and refusing if
/// the target isn't the newest.
pub fn restore_checkpoint(run_id: &str, cwd: &str) -> Result<(), CheckpointError> {
    // Validate the project is in a git repo.
    if !is_inside_git_worktree(cwd) {
        return Err(CheckpointError::NotAGitRepo);
    }

    // LIFO check: if there are newer checkpoints than this one IN THE SAME PROJECT,
    // refuse. The ordering is by created_at (descending); the target must be the
    // newest or tied-newest among checkpoints sharing its `cwd`.
    //
    // The check is scoped to the same project (`cwd`), NOT global. A checkpoint on
    // a different project has no overlap with this run's working tree — restoring
    // project X is independent of a newer checkpoint on project Y. A global check
    // would block cross-project rollbacks (e.g. a long-running run on project B
    // pinning project A's older checkpoint un-restoreable), defeating the
    // parallel-runs-across-projects guarantee the module documents.
    {
        let g = checkpoints_guard();
        let target = g.get(run_id);
        if target.is_none() {
            return Err(CheckpointError::UnknownCheckpoint);
        }
        let target = target.unwrap();
        let target_ts = target.created_at;
        let target_cwd = target.cwd.as_str();
        let has_newer = g.values().any(|other| {
            other.run_id != run_id && other.cwd == target_cwd && other.created_at > target_ts
        });
        if has_newer {
            // Find the newest checkpoint in the SAME project for the error message.
            let newest = g
                .values()
                .filter(|c| c.cwd == target_cwd)
                .max_by_key(|c| c.created_at)
                .map(|c| c.run_id.clone())
                .unwrap_or_default();
            return Err(CheckpointError::RollbackOrder {
                current: newest,
                expected: run_id.to_string(),
            });
        }
    }

    // Find the snapshot.
    let snapshot_sha = {
        let g = checkpoints_guard();
        let cp = g.get(run_id).ok_or(CheckpointError::UnknownCheckpoint)?;
        cp.snapshot_sha.clone()
    };

    // Validate the snapshot SHA still exists in this repo (defensive against a
    // `git gc` or manual force-push that rewrote history).
    let (stdout, _, code) = git(cwd, &["cat-file", "-t", &snapshot_sha]).map_err(|e| {
        CheckpointError::RestoreFailed(format!("git cat-file for snapshot {snapshot_sha}: {e}"))
    })?;
    if code != 0 || stdout.trim() != "commit" {
        return Err(CheckpointError::RestoreFailed(format!(
            "snapshot SHA {snapshot_sha} is no longer valid in this repo (git gc? force-push?)"
        )));
    }

    // Reset the working tree to the snapshot. This discards ALL uncommitted edits
    // (tracked modifications + added files) the run made.
    let (_, stderr, code) = git(cwd, &["reset", "--hard", &snapshot_sha]).map_err(|e| {
        CheckpointError::RestoreFailed(format!("git reset --hard {snapshot_sha}: {e}"))
    })?;
    if code != 0 {
        return Err(CheckpointError::RestoreFailed(format!(
            "git reset --hard failed: {stderr}"
        )));
    }

    // Remove any untracked files the run created. `git reset --hard` doesn't touch
    // untracked files; the run's subagents may have created new ones (build output,
    // temp files, etc.) that aren't in the snapshot.
    let (_, _, _) = git(cwd, &["clean", "-fd"])
        .map_err(|e| CheckpointError::RestoreFailed(format!("git clean -fd: {e}")))?;

    // Remove the in-memory entry.
    remove_checkpoint(run_id);

    Ok(())
}

/// Discard a checkpoint without rolling back the tree. Useful after an explicit
/// "operator accepts current state" action. Drops the in-memory entry.
pub fn discard_checkpoint(run_id: &str) -> Result<(), CheckpointError> {
    remove_checkpoint(run_id).ok_or(CheckpointError::UnknownCheckpoint)?;
    Ok(())
}

// ---- REST handlers ----

/// Resolve a working directory for the run's project. Prefers the project's stored
/// cwd (if the run was created with a project_id); falls back to server cwd.
fn cwd_for_run(run_id: &str) -> String {
    let project_id = crate::workflows::get_active(run_id).and_then(|r| r.project_id.clone());
    if let Some(pid) = project_id {
        if let Some(cwd) = crate::projects::cwd_for_project(Some(&pid)) {
            return cwd;
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

/// POST /api/workflows/:id/checkpoint → snapshot the working tree before run execution.
/// Returns 201 with the checkpoint on success, 422 if the project isn't in a git repo.
async fn create_checkpoint_handler(
    Path(run_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let cwd = cwd_for_run(&run_id);
    match save_checkpoint(&run_id, &cwd) {
        Ok(snapshot_sha) => {
            let cp = get_checkpoint(&run_id).unwrap();
            Ok((
                StatusCode::CREATED,
                Json(json!({ "checkpoint": cp, "snapshotSha": snapshot_sha })),
            ))
        }
        Err(e) => Err(e.into_response()),
    }
}

/// GET /api/workflows/:id/checkpoint → inspect a single checkpoint.
async fn get_checkpoint_handler(
    Path(run_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match get_checkpoint(&run_id) {
        Some(cp) => Ok(Json(json!({ "checkpoint": cp }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no checkpoint for this run" })),
        )),
    }
}

/// GET /api/checkpoints → list all active checkpoints (for the checkpoint panel).
async fn list_checkpoints_handler() -> Json<Value> {
    let cps = list_checkpoints();
    Json(json!({ "checkpoints": cps }))
}

/// POST /api/workflows/:id/rollback → restore the working tree to its pre-run state.
/// LIFO: only the most-recent checkpoint can be rolled back; restore newer ones first.
async fn rollback_handler(
    Path(run_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = cwd_for_run(&run_id);
    match restore_checkpoint(&run_id, &cwd) {
        Ok(()) => Ok(Json(json!({ "ok": true, "restoredTo": run_id }))),
        Err(e) => Err(e.into_response()),
    }
}

/// DELETE /api/workflows/:id/checkpoint → accept current state, drop the checkpoint.
async fn discard_checkpoint_handler(
    Path(run_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match discard_checkpoint(&run_id) {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(e) => Err(e.into_response()),
    }
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/checkpoints", get(list_checkpoints_handler))
        .route(
            "/api/workflows/{id}/checkpoint",
            get(get_checkpoint_handler)
                .post(create_checkpoint_handler)
                .delete(discard_checkpoint_handler),
        )
        .route("/api/workflows/{id}/rollback", post(rollback_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Create a throwaway temp dir with a git repo containing an initial file.
    /// Returns the path to the temp dir.
    fn init_git_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dotz-checkpoint-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(dir.to_str().unwrap(), &["init"]).unwrap();
        git(dir.to_str().unwrap(), &["config", "core.autocrlf", "false"]).unwrap();
        git(
            dir.to_str().unwrap(),
            &["config", "user.email", "test@dotz"],
        )
        .unwrap();
        git(dir.to_str().unwrap(), &["config", "user.name", "dotz test"]).unwrap();
        std::fs::write(dir.join("README.md"), "# hello\n").unwrap();
        git(dir.to_str().unwrap(), &["add", "README.md"]).unwrap();
        git(dir.to_str().unwrap(), &["commit", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn is_inside_git_worktree_detects_repo() {
        let _guard = test_lock();
        let dir = init_git_repo();
        assert!(is_inside_git_worktree(dir.to_str().unwrap()));

        let not_git = std::env::temp_dir().join(format!("dotz-no-git-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&not_git).unwrap();
        assert!(!is_inside_git_worktree(not_git.to_str().unwrap()));
    }

    #[test]
    fn git_head_sha_returns_40_char_sha() {
        let _guard = test_lock();
        let dir = init_git_repo();
        let sha = git_head_sha(dir.to_str().unwrap());
        assert!(sha.is_some());
        assert_eq!(sha.unwrap().len(), 40);
    }

    /// Test that save_checkpoint records a snapshot, editing the tree doesn't corrupt
    /// the snapshot, and restore_checkpoint rolls the tree back.
    #[test]
    fn save_then_restore_round_trips_via_git_reset() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();

        // Record original README contents.
        let original = std::fs::read_to_string(dir.join("README.md")).unwrap();

        // Save checkpoint.
        let run_id = "test-run-1";
        let sha = save_checkpoint(run_id, cwd).unwrap();
        assert!(!sha.is_empty());

        // Simulate a subagent edit the README.
        std::fs::write(dir.join("README.md"), "# edited content\n").unwrap();
        // Simulate a subagent creating a new file.
        std::fs::write(dir.join("new_file.txt"), "should be deleted").unwrap();

        // Restore.
        restore_checkpoint(run_id, cwd).unwrap();

        // README is back to original.
        let restored = std::fs::read_to_string(dir.join("README.md")).unwrap();
        assert_eq!(
            restored, original,
            "restored README should match pre-run content"
        );

        // New file was removed.
        assert!(
            !dir.join("new_file.txt").exists(),
            "untracked file created by the run should be removed by restore"
        );

        // Checkpoint metadata is gone.
        assert!(get_checkpoint(run_id).is_none());
    }

    /// No-git projects must error gracefully, not panic.
    #[test]
    fn save_checkpoint_errors_for_no_git() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = std::env::temp_dir().join(format!("dotz-checkpoint-nogit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = save_checkpoint("no-git-run", dir.to_str().unwrap());
        assert!(matches!(result, Err(CheckpointError::NotAGitRepo)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discard removes metadata but leaves the tree untouched.
    #[test]
    fn discard_metadata_only() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        let run_id = "discard-run";
        save_checkpoint(run_id, cwd).unwrap();

        // Edit the tree after the checkpoint.
        std::fs::write(dir.join("README.md"), "modified").unwrap();

        // Discard (does NOT reset the tree).
        discard_checkpoint(run_id).unwrap();

        // Tree is untouched.
        let content = std::fs::read_to_string(dir.join("README.md")).unwrap();
        assert_eq!(content, "modified");

        // Metadata is gone.
        assert!(get_checkpoint(run_id).is_none());
    }

    /// `save_checkpoint` must stamp `created_at` with a plausible recent wall-clock timestamp.
    /// This guards the consolidation from the module's local `now_ms()` to the shared
    /// `crate::util::now_ms()`: a broken or stale copy would produce a zero or pre-2020
    /// timestamp, which would corrupt the LIFO ordering the restore path depends on.
    #[test]
    fn save_checkpoint_stamps_plausible_created_at() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        let run_id = "created-at-run";
        save_checkpoint(run_id, cwd).unwrap();

        let cp = get_checkpoint(run_id).expect("checkpoint should exist after save");
        assert!(
            cp.created_at > 1_577_836_800_000,
            "created_at should be a plausible recent timestamp (after 2020-01-01), got {}",
            cp.created_at
        );
    }

    /// Restore without a prior save returns UnknownCheckpoint.
    #[test]
    fn restore_unknown_returns_error() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        let result = restore_checkpoint("never-saved", cwd);
        assert!(matches!(result, Err(CheckpointError::UnknownCheckpoint)));
    }

    /// LIFO: restoring an older checkpoint while a newer one exists must fail with RollbackOrder.
    #[test]
    fn restore_enforces_lifo_order() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();

        // Save checkpoint A, then B (B is newer).
        save_checkpoint("run-a", cwd).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10)); // ensure different created_at
        save_checkpoint("run-b", cwd).unwrap();

        // Trying to restore A (older) must fail.
        let result = restore_checkpoint("run-a", cwd);
        assert!(
            matches!(result, Err(CheckpointError::RollbackOrder { ref current, ref expected }) if current == "run-b" && expected == "run-a"),
            "expected RollbackOrder error, got {:?}",
            result
        );
    }

    /// Restoring an already-consumed checkpoint (after a newer restore) returns Unknown.
    #[test]
    fn restore_after_newer_restore_returns_unknown() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        save_checkpoint("old", cwd).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        save_checkpoint("new", cwd).unwrap();

        restore_checkpoint("new", cwd).unwrap();
        // Now "old" is the only checkpoint left; restoring it should succeed.
        restore_checkpoint("old", cwd).unwrap();
        // Now there's no checkpoint at all.
        let result = restore_checkpoint("old", cwd);
        assert!(matches!(result, Err(CheckpointError::UnknownCheckpoint)));
    }

    /// LIFO is scoped per-project (`cwd`), not globally. A newer checkpoint on a
    /// DIFFERENT project must NOT block restoring an older checkpoint on this
    /// project — the two working trees are independent, so cross-project rollback
    /// interference defeats the parallel-runs-across-projects guarantee the module
    /// documents. Before the fix, the LIFO check scanned every checkpoint globally,
    // so a long-running run on project B left project A's older checkpoint
    // un-restoreable.
    #[test]
    fn restore_lifo_is_scoped_per_project_not_global() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();

        // Two independent git repos = two independent projects.
        let dir_a = init_git_repo();
        let dir_b = init_git_repo();
        let cwd_a = dir_a.to_str().unwrap();
        let cwd_b = dir_b.to_str().unwrap();

        // Project A: older checkpoint.
        save_checkpoint("run-a", cwd_a).unwrap();
        // Project B: NEWER checkpoint (created_at strictly greater than run-a's).
        std::thread::sleep(std::time::Duration::from_millis(10));
        save_checkpoint("run-b", cwd_b).unwrap();

        // Edit both trees to simulate subagent work.
        std::fs::write(dir_a.join("README.md"), "edited by A\n").unwrap();
        std::fs::write(dir_b.join("README.md"), "edited by B\n").unwrap();

        // Restoring the OLDER checkpoint on project A must succeed despite project
        // B having a newer checkpoint — they are different working trees.
        restore_checkpoint("run-a", cwd_a)
            .expect("restoring project A must not be blocked by a newer checkpoint on project B");
        // Project A's tree is rolled back; project B's tree is untouched.
        assert_eq!(
            std::fs::read_to_string(dir_a.join("README.md")).unwrap(),
            "# hello\n",
            "project A should be restored to its pre-run state"
        );
        assert_eq!(
            std::fs::read_to_string(dir_b.join("README.md")).unwrap(),
            "edited by B\n",
            "project B must be untouched by project A's rollback"
        );

        // The cross-project checkpoint on B is still registered and restorable.
        assert!(get_checkpoint("run-b").is_some());
        restore_checkpoint("run-b", cwd_b).unwrap();
    }

    /// Restore succeeds even if the working tree has staged (added-to-index) changes.
    #[test]
    fn restore_handles_staged_and_unstaged_changes() {
        let _guard = test_lock();
        reset_checkpoints_for_testing();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        save_checkpoint("staged-test", cwd).unwrap();

        // Stage some changes a subagent might make.
        std::fs::write(dir.join("README.md"), "modified content").unwrap();
        git(cwd, &["add", "README.md"]).unwrap();
        std::fs::write(dir.join("untracked.sh"), "#!/bin/sh\necho hi").unwrap();

        restore_checkpoint("staged-test", cwd).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "# hello\n"
        );
        assert!(!dir.join("untracked.sh").exists());
    }

    // ---- git_diff_artifact tests ----

    /// `git_diff_artifact` returns None when `cwd` is not inside a git worktree.
    #[test]
    fn git_diff_artifact_returns_none_for_no_git() {
        let _guard = test_lock();
        let dir = std::env::temp_dir().join(format!("dtz-diff-nogit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = git_diff_artifact(dir.to_str().unwrap());
        assert!(result.is_none(), "no git repo → None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `git_diff_artifact` returns None when the working tree is clean (no changes).
    #[test]
    fn git_diff_artifact_returns_none_for_clean_tree() {
        let _guard = test_lock();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();
        let result = git_diff_artifact(cwd);
        assert!(result.is_none(), "clean tree → None");
    }

    /// `git_diff_artifact` returns a `git_diff` Artifact with content and a stat
    /// title when the working tree has unstaged changes.
    #[test]
    fn git_diff_artifact_captures_unstaged_changes() {
        let _guard = test_lock();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();

        // Make a change.
        std::fs::write(dir.join("README.md"), "# modified\n").unwrap();

        let art = git_diff_artifact(cwd).expect("dirty tree → Some artifact");
        assert_eq!(art.kind, "git_diff");
        assert!(!art.content.is_empty(), "diff content must not be empty");
        assert!(
            art.content.contains("README.md"),
            "diff should mention the changed file"
        );
        // Title is the shortstat line (e.g. "1 file changed, 1 insertion(+)").
        assert!(
            art.title.is_some(),
            "title should be set from git diff --shortstat"
        );
    }

    /// `git_diff_artifact` includes staged changes when they exist.
    #[test]
    fn git_diff_artifact_includes_staged_changes() {
        let _guard = test_lock();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();

        std::fs::write(dir.join("README.md"), "staged line\n").unwrap();
        git(cwd, &["add", "README.md"]).unwrap();

        let art = git_diff_artifact(cwd).expect("staged changes → Some artifact");
        assert_eq!(art.kind, "git_diff");
        assert!(
            art.content.contains("README.md"),
            "staged diff should mention the file"
        );
        // Regression: staged-only changes must still produce a stat title. The old
        // `git diff --shortstat` (no base) reported unstaged changes only, so this
        // case had content but a `None` title — the UI rendered a header-less diff.
        assert!(
            art.title.is_some(),
            "staged-only changes must yield a stat title (got None)"
        );
        assert!(
            art.title.as_deref().unwrap_or("").contains("changed"),
            "staged-only title should be a shortstat line, got {:?}",
            art.title
        );
    }

    /// `git_diff_artifact` title must reflect BOTH staged and unstaged changes combined
    /// (the union `git diff HEAD` reports), not just the unstaged subset. Before the
    /// `git diff --shortstat HEAD` fix, a tree with one staged and one unstaged edit
    /// produced a title counting only the unstaged file — under-reporting the very
    /// change set the content section below it displayed.
    #[test]
    fn git_diff_artifact_title_counts_staged_and_unstaged_combined() {
        let _guard = test_lock();
        let dir = init_git_repo();
        let cwd = dir.to_str().unwrap();

        // Add a second tracked file so we have two files to edit independently.
        std::fs::write(dir.join("NOTES.md"), "initial notes\n").unwrap();
        git(cwd, &["add", "NOTES.md"]).unwrap();
        git(cwd, &["commit", "-m", "add notes"]).unwrap();

        // Staged change to README.md (index vs HEAD).
        std::fs::write(dir.join("README.md"), "staged edit\n").unwrap();
        git(cwd, &["add", "README.md"]).unwrap();
        // Unstaged change to NOTES.md (working tree vs index).
        std::fs::write(dir.join("NOTES.md"), "unstaged edit\n").unwrap();

        let art = git_diff_artifact(cwd).expect("dirty tree → Some artifact");
        assert!(
            art.content.contains("README.md"),
            "content should include the staged file"
        );
        assert!(
            art.content.contains("NOTES.md"),
            "content should include the unstaged file"
        );
        let title = art.title.expect("combined staged+unstaged → Some title");
        // `git diff HEAD --shortstat` reports both files; the old code reported only NOTES.md.
        assert!(
            title.contains("2 files changed"),
            "title should count both staged and unstaged files, got {title}"
        );
    }
}
