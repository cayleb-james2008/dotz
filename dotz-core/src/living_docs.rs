//! Living project/global context documents for dotz.
//!
//! These files are doctrine-adjacent knowledge, not replacements for AGENTS.md. They live in
//! `<cwd>/.ai-agents/` for projects and `~/.dotz/ai-agents/` globally, then get compactly injected
//! into the agent prompt beside memory recall.
use crate::types::LivingDocKind;
use axum::{
    Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SUGGESTIONS_FILE: &str = "living-doc-suggestions.json";

#[derive(Clone, Debug, Serialize)]
pub struct LivingDoc {
    pub kind: LivingDocKind,
    pub path: String,
    pub content: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LivingDocSuggestion {
    pub id: String,
    pub scope: String,
    pub kind: LivingDocKind,
    pub text: String,
    pub confidence: f32,
    pub source: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct PatchBody {
    kind: LivingDocKind,
    content: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn global_dir() -> PathBuf {
    crate::config::dotz_dir().join("ai-agents")
}

fn project_dir(cwd: &FsPath) -> PathBuf {
    cwd.join(".ai-agents")
}

fn doc_dir(scope: &str, cwd: Option<&FsPath>) -> PathBuf {
    if scope == "global" {
        global_dir()
    } else {
        project_dir(cwd.unwrap_or_else(|| FsPath::new(".")))
    }
}

fn resolve_cwd(project_id: Option<&str>) -> Result<Option<PathBuf>, (StatusCode, Json<Value>)> {
    if let Some(id) = project_id.map(str::trim).filter(|s| !s.is_empty()) {
        return crate::projects::cwd_for_project(Some(id))
            .map(PathBuf::from)
            .map(Some)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "no such project" })),
                )
            });
    }
    Ok(std::env::current_dir().ok())
}

fn doc_path(dir: &FsPath, kind: &LivingDocKind) -> PathBuf {
    dir.join(kind.file_name())
}

fn updated_at(path: &FsPath) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_doc(dir: &FsPath, kind: &LivingDocKind) -> LivingDoc {
    let path = doc_path(dir, kind);
    LivingDoc {
        kind: *kind,
        path: path.to_string_lossy().to_string(),
        content: std::fs::read_to_string(&path).unwrap_or_default(),
        updated_at: updated_at(&path),
    }
}

pub fn list_docs(scope: &str, cwd: Option<&FsPath>) -> Vec<LivingDoc> {
    let dir = doc_dir(scope, cwd);
    LivingDocKind::all()
        .iter()
        .map(|kind| read_doc(&dir, kind))
        .collect()
}

pub fn write_doc(
    scope: &str,
    cwd: Option<&FsPath>,
    kind: &LivingDocKind,
    content: &str,
) -> Result<LivingDoc, String> {
    let dir = doc_dir(scope, cwd);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = doc_path(&dir, kind);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(read_doc(&dir, kind))
}

fn ensure_doc_header(path: &FsPath, kind: &LivingDocKind) -> Result<String, String> {
    if path.exists() {
        return std::fs::read_to_string(path).map_err(|e| e.to_string());
    }
    Ok(format!("# {}\n\n", kind.heading()))
}

fn append_fact(
    scope: &str,
    cwd: Option<&FsPath>,
    kind: &LivingDocKind,
    text: &str,
) -> Result<bool, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(false);
    }
    let dir = doc_dir(scope, cwd);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = doc_path(&dir, kind);
    let mut content = ensure_doc_header(&path, kind)?;
    if content.contains(text) {
        return Ok(false);
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("- {text}\n"));
    std::fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(true)
}

fn suggestions_path(dir: &FsPath) -> PathBuf {
    dir.join(SUGGESTIONS_FILE)
}

fn read_suggestions(dir: &FsPath) -> Vec<LivingDocSuggestion> {
    std::fs::read_to_string(suggestions_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<LivingDocSuggestion>>(&raw).ok())
        .unwrap_or_default()
}

fn write_suggestions(dir: &FsPath, suggestions: &[LivingDocSuggestion]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let raw = serde_json::to_string_pretty(suggestions).map_err(|e| e.to_string())?;
    std::fs::write(suggestions_path(dir), raw).map_err(|e| e.to_string())
}

pub fn add_suggestion(
    scope: &str,
    cwd: Option<&FsPath>,
    kind: LivingDocKind,
    text: String,
    confidence: f32,
    source: String,
) -> Result<LivingDocSuggestion, String> {
    let dir = doc_dir(scope, cwd);
    let mut all = read_suggestions(&dir);
    let suggestion = LivingDocSuggestion {
        id: uuid::Uuid::new_v4().to_string(),
        scope: scope.to_string(),
        kind,
        text,
        confidence,
        source,
        created_at: now_ms(),
    };
    all.push(suggestion.clone());
    write_suggestions(&dir, &all)?;
    Ok(suggestion)
}

fn all_suggestions(scope: &str, cwd: Option<&FsPath>) -> Vec<LivingDocSuggestion> {
    read_suggestions(&doc_dir(scope, cwd))
}

pub fn accept_suggestion(
    scope: &str,
    cwd: Option<&FsPath>,
    id: &str,
) -> Result<LivingDocSuggestion, String> {
    let dir = doc_dir(scope, cwd);
    let mut all = read_suggestions(&dir);
    let idx = all
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| "no such living-doc suggestion".to_string())?;
    let suggestion = all.remove(idx);
    write_suggestions(&dir, &all)?;
    append_fact(scope, cwd, &suggestion.kind, &suggestion.text)?;
    Ok(suggestion)
}

pub fn reject_suggestion(
    scope: &str,
    cwd: Option<&FsPath>,
    id: &str,
) -> Result<LivingDocSuggestion, String> {
    let dir = doc_dir(scope, cwd);
    let mut all = read_suggestions(&dir);
    let idx = all
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| "no such living-doc suggestion".to_string())?;
    let suggestion = all.remove(idx);
    write_suggestions(&dir, &all)?;
    Ok(suggestion)
}

fn fact_kind_from_prefix(line: &str) -> Option<(LivingDocKind, &str, bool)> {
    let trimmed = line.trim().trim_start_matches("- ").trim();
    let lower = trimmed.to_ascii_lowercase();
    let pairs = [
        ("possible anti-pattern:", LivingDocKind::AntiPatterns, false),
        ("maybe non-inferable:", LivingDocKind::NonInferables, false),
        ("maybe context-scope:", LivingDocKind::ContextScope, false),
        ("maybe living-doc:", LivingDocKind::LivingDocs, false),
        ("anti-pattern:", LivingDocKind::AntiPatterns, true),
        ("anti pattern:", LivingDocKind::AntiPatterns, true),
        ("non-inferable:", LivingDocKind::NonInferables, true),
        ("non inferable:", LivingDocKind::NonInferables, true),
        ("context-scope:", LivingDocKind::ContextScope, true),
        ("context scope:", LivingDocKind::ContextScope, true),
        ("living-doc:", LivingDocKind::LivingDocs, true),
        ("living doc:", LivingDocKind::LivingDocs, true),
    ];
    for (prefix, kind, automatic) in pairs {
        if lower.starts_with(prefix) {
            return Some((kind, trimmed[prefix.len()..].trim(), automatic));
        }
    }
    None
}

/// Conservative lifecycle capture: only explicit labeled facts are persisted automatically.
/// Ambiguous `maybe ...` labels go to the review queue.
pub fn capture_exchange(cwd: Option<&str>, user: &str, assistant: &str) -> Result<Value, String> {
    let cwd_path = cwd.map(FsPath::new);
    let mut appended = 0usize;
    let mut suggested = 0usize;
    for line in user.lines().chain(assistant.lines()) {
        if let Some((kind, text, automatic)) = fact_kind_from_prefix(line) {
            if automatic {
                if append_fact("project", cwd_path, &kind, text)? {
                    appended += 1;
                }
            } else {
                add_suggestion(
                    "project",
                    cwd_path,
                    kind,
                    text.to_string(),
                    0.55,
                    "agent_end".to_string(),
                )?;
                suggested += 1;
            }
        }
    }
    Ok(json!({ "appended": appended, "suggested": suggested }))
}

fn compact_doc(doc: &LivingDoc, max_lines: usize) -> Option<String> {
    let lines: Vec<&str> = doc
        .content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .take(max_lines)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("{}:\n{}", doc.kind.heading(), lines.join("\n")))
}

pub fn render_prompt_block(cwd: Option<&str>) -> String {
    let cwd_path = cwd.map(FsPath::new);
    let mut sections = Vec::new();
    for doc in list_docs("global", None) {
        if let Some(summary) = compact_doc(&doc, 8) {
            sections.push(format!("Global {summary}"));
        }
    }
    if let Some(cwd) = cwd_path {
        for doc in list_docs("project", Some(cwd)) {
            if let Some(summary) = compact_doc(&doc, 12) {
                sections.push(format!("Project {summary}"));
            }
        }
    }
    if sections.is_empty() {
        return String::new();
    }
    let mut rendered = sections.join("\n\n");
    if rendered.len() > 5000 {
        // Back up to the nearest UTF-8 char boundary at or before byte 5000 so the slice
        // doesn't panic when a multi-byte character straddles the cut point — the common
        // case for real living-docs content (emoji, non-ASCII prose, i18n code comments).
        // `String::truncate` panics on a non-boundary index, and this fn runs inside agent
        // system-prompt construction, so a panic here would crash the whole turn.
        let mut end = 5000;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        rendered.truncate(end);
        rendered.push_str("\n[truncated]");
    }
    format!(
        "\n# dotz living docs\nUse these project/global anti-patterns, non-inferables, context scope, and living docs as durable context. They supplement AGENTS.md; they do not override it.\n{rendered}\n"
    )
}

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn not_found(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg.into() })))
}

async fn get_handler(
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let scope = q.get("scope").map(String::as_str).unwrap_or("project");
    if scope != "project" && scope != "global" {
        return Err(bad("scope must be project or global"));
    }
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    let docs = list_docs(scope, cwd.as_deref());
    let suggestions = all_suggestions(scope, cwd.as_deref());
    Ok(Json(json!({
        "scope": scope,
        "docs": docs,
        "suggestions": suggestions,
    })))
}

async fn patch_handler(
    Query(q): Query<HashMap<String, String>>,
    body: Option<Json<PatchBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let scope = q.get("scope").map(String::as_str).unwrap_or("project");
    if scope != "project" && scope != "global" {
        return Err(bad("scope must be project or global"));
    }
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    let body = body
        .map(|Json(v)| v)
        .ok_or_else(|| bad("kind and content are required"))?;
    write_doc(scope, cwd.as_deref(), &body.kind, &body.content)
        .map(|doc| Json(json!({ "doc": doc, "docs": list_docs(scope, cwd.as_deref()) })))
        .map_err(bad)
}

async fn suggestion_action_handler(
    Path((id, action)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let scope = q.get("scope").map(String::as_str).unwrap_or("project");
    if scope != "project" && scope != "global" {
        return Err(bad("scope must be project or global"));
    }
    let cwd = resolve_cwd(q.get("projectId").map(|s| s.as_str()))?;
    let result = match action.as_str() {
        "accept" => accept_suggestion(scope, cwd.as_deref(), &id),
        "reject" => reject_suggestion(scope, cwd.as_deref(), &id),
        _ => Err(format!("unknown living-doc suggestion action: {action}")),
    };
    result
        .map(|suggestion| {
            Json(json!({
                "ok": true,
                "suggestion": suggestion,
                "docs": list_docs(scope, cwd.as_deref()),
                "suggestions": all_suggestions(scope, cwd.as_deref()),
            }))
        })
        .map_err(|e| {
            if e.contains("no such") {
                not_found(e)
            } else {
                bad(e)
            }
        })
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/living-docs", get(get_handler).patch(patch_handler))
        .route(
            "/api/living-docs/suggestions/{id}/{action}",
            post(suggestion_action_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dotz-living-docs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn project_docs_write_and_read_from_ai_agents() {
        let dir = tmp_dir();
        let doc = write_doc(
            "project",
            Some(&dir),
            &LivingDocKind::AntiPatterns,
            "# Anti-Patterns\n\n- Do not hand edit generated indexes.\n",
        )
        .unwrap();
        assert!(doc.path.contains(".ai-agents"));
        let docs = list_docs("project", Some(&dir));
        assert!(
            docs.iter().any(|d| d.kind == LivingDocKind::AntiPatterns
                && d.content.contains("generated indexes"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_exchange_appends_only_explicit_high_confidence_facts() {
        let dir = tmp_dir();
        let result = capture_exchange(
            Some(dir.to_str().unwrap()),
            "please remember",
            "anti-pattern: Do not bypass OpenSpec readiness.\nmaybe living-doc: Review this later.",
        )
        .unwrap();
        assert_eq!(result["appended"], 1);
        assert_eq!(result["suggested"], 1);
        let anti =
            std::fs::read_to_string(dir.join(".ai-agents").join("anti-patterns.md")).unwrap();
        assert!(anti.contains("Do not bypass OpenSpec readiness"));
        let suggestions = all_suggestions("project", Some(&dir));
        assert_eq!(suggestions.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accepting_suggestion_moves_it_into_doc() {
        let dir = tmp_dir();
        let suggestion = add_suggestion(
            "project",
            Some(&dir),
            LivingDocKind::ContextScope,
            "Only scan src and web for UI work.".into(),
            0.5,
            "test".into(),
        )
        .unwrap();
        let accepted = accept_suggestion("project", Some(&dir), &suggestion.id).unwrap();
        assert_eq!(accepted.id, suggestion.id);
        assert!(all_suggestions("project", Some(&dir)).is_empty());
        let doc = std::fs::read_to_string(dir.join(".ai-agents").join("context-scope.md")).unwrap();
        assert!(doc.contains("Only scan src and web"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn render_prompt_block_includes_global_and_project_summaries() {
        let dir = tmp_dir();
        write_doc(
            "project",
            Some(&dir),
            &LivingDocKind::NonInferables,
            "# Non-Inferables\n\n- Do not infer credentials.\n",
        )
        .unwrap();
        let block = render_prompt_block(Some(dir.to_str().unwrap()));
        assert!(block.contains("dotz living docs"));
        assert!(block.contains("Do not infer credentials"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `render_prompt_block` truncates the joined doc summaries to 5 KB. The old code used
    /// `String::truncate(5000)`, which panics when byte 5000 falls inside a multi-byte UTF-8
    /// character — the common case for real living-docs content (emoji, non-ASCII prose, i18n
    /// code comments). Because this fn runs inside agent system-prompt construction, a panic
    /// here would crash the whole turn. This test feeds a doc whose compacted summary is a
    /// single long run of `é` (2 bytes/char); the project section prefix
    /// `"Project Anti-Patterns:\n"` is 23 bytes (odd), so byte 5000 lands at offset 4977
    /// into the `é` run — an odd offset, i.e. 1 byte into a 2-byte char — which is exactly
    /// the cut that panicked before the char-boundary fix. It must truncate without panicking
    /// and still emit the `[truncated]` marker.
    #[test]
    fn render_prompt_block_truncates_multibyte_content_without_panicking() {
        let dir = tmp_dir();
        // 2500 × `é` = 5000 bytes on one non-`#` line; after `compact_doc(.., 12)` the
        // rendered section is `"Project Anti-Patterns:\n"` (23 bytes) + this line, so total
        // is 5023 bytes > 5000 and byte 5000 sits at offset 4977 inside the `é` run — an odd
        // offset, which is mid-char for a 2-byte codepoint.
        let big = "é".repeat(2500);
        write_doc(
            "project",
            Some(&dir),
            &LivingDocKind::AntiPatterns,
            &format!("# Anti-Patterns\n\n{big}\n"),
        )
        .unwrap();
        let block = render_prompt_block(Some(dir.to_str().unwrap()));
        assert!(
            block.contains("[truncated]"),
            "truncated multibyte block must carry the [truncated] marker"
        );
        assert!(block.contains("dotz living docs"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
