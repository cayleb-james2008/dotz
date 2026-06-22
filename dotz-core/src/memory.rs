//! dotz memory store — Rust port of src/memory.ts (the store + REST endpoints; the LLM autonomy
//! capture/extraction is Phase 4). Vector store = rusqlite; embeddings = crate::embed (ort/ONNX,
//! float-identical to the JS transformers.js embedder). MEMORY.md mirror is the git-committable
//! source of truth; the sqlite index is a derived cache.
//!
//! Self-contained: owns a sqlite Connection + a lazily-loaded Embedder via module statics
//! (mirrors the Node module-singleton `memoryStore`). No AppState, stateless Router<()>.
use crate::embed::Embedder;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const GLOBAL_USER: &str = "__global__";
const RECENCY_HALFLIFE_MS: f64 = 1000.0 * 60.0 * 60.0 * 24.0 * 30.0; // 30 days

/// A memory as surfaced to REST/UI (mirrors types.ts MemoryView). camelCase; optionals omitted.
#[derive(Clone, Serialize)]
pub struct MemoryView {
    pub id: String,
    pub memory: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Scope → mem0 userId. global / no-cwd → "__global__"; project → "proj:<norm cwd>"
/// (backslashes → "/", lowercased on Windows). Mirrors memory.ts scopeUser.
fn scope_user(scope: &str, cwd: Option<&str>) -> String {
    match (scope, cwd) {
        ("global", _) | (_, None) => GLOBAL_USER.to_string(),
        (_, Some(c)) => {
            let norm = c.replace('\\', "/");
            let norm = if cfg!(windows) { norm.to_lowercase() } else { norm };
            format!("proj:{norm}")
        }
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..n {
        d += a[i] as f64 * b[i] as f64;
        na += a[i] as f64 * a[i] as f64;
        nb += b[i] as f64 * b[i] as f64;
    }
    let den = na.sqrt() * nb.sqrt();
    if den == 0.0 {
        d
    } else {
        d / den
    }
}

fn norm_folder(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    if let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix('/') {
        s = rest.to_string();
    }
    s.to_lowercase()
}

// ---- store + embedder (module statics) ----
static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
fn db() -> &'static Mutex<Connection> {
    DB.get_or_init(|| {
        let dir = crate::config::dotz_dir().join("ai-agents");
        let _ = std::fs::create_dir_all(&dir);
        let conn = Connection::open(dir.join("memory.db")).expect("open memory.db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL, scope TEXT NOT NULL,
                memory TEXT NOT NULL, category TEXT, folder TEXT,
                embedding BLOB NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER);
             CREATE INDEX IF NOT EXISTS idx_mem_user ON memories(user_id);",
        )
        .expect("init memory schema");
        Mutex::new(conn)
    })
}

static EMBEDDER: OnceLock<Mutex<Option<Embedder>>> = OnceLock::new();
fn embed_text(text: &str) -> Result<Vec<f32>, String> {
    let m = EMBEDDER.get_or_init(|| Mutex::new(None));
    let mut g = m.lock().unwrap();
    if g.is_none() {
        *g = Some(Embedder::load().map_err(|e| format!("embedder load: {e}"))?);
    }
    g.as_mut().unwrap().embed(text).map_err(|e| format!("embed: {e}"))
}

fn enc_emb(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
fn dec_emb(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

struct Row {
    id: String,
    scope: String,
    memory: String,
    category: Option<String>,
    folder: Option<String>,
    embedding: Vec<f32>,
    created_at: i64,
    updated_at: Option<i64>,
}

fn rows_for_user(conn: &Connection, user_id: &str, category: Option<&str>) -> Vec<Row> {
    let mapper = |r: &rusqlite::Row| -> rusqlite::Result<Row> {
        let blob: Vec<u8> = r.get(5)?;
        Ok(Row {
            id: r.get(0)?,
            scope: r.get(1)?,
            memory: r.get(2)?,
            category: r.get(3)?,
            folder: r.get(4)?,
            embedding: dec_emb(&blob),
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
        })
    };
    let sql = if category.is_some() {
        "SELECT id,scope,memory,category,folder,embedding,created_at,updated_at FROM memories WHERE user_id=?1 AND category=?2"
    } else {
        "SELECT id,scope,memory,category,folder,embedding,created_at,updated_at FROM memories WHERE user_id=?1"
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let result = if let Some(cat) = category {
        stmt.query_map(rusqlite::params![user_id, cat], mapper)
    } else {
        stmt.query_map(rusqlite::params![user_id], mapper)
    };
    match result {
        Ok(it) => it.filter_map(|x| x.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

fn row_to_view(r: &Row, score: Option<f64>) -> MemoryView {
    MemoryView {
        id: r.id.clone(),
        memory: r.memory.clone(),
        scope: r.scope.clone(),
        category: r.category.clone(),
        folder: r.folder.clone(),
        score,
        created_at: Some(r.created_at),
        updated_at: r.updated_at,
    }
}

/// Add a memory verbatim (manual add — infer:false). Returns the created view; writes the mirror.
fn add(text: &str, scope: &str, category: Option<&str>, folder: Option<&str>, cwd: Option<&str>) -> Result<MemoryView, String> {
    let user_id = scope_user(scope, cwd);
    let emb = embed_text(text)?;
    let id = uuid::Uuid::new_v4().to_string();
    let ts = now_ms();
    {
        let conn = db().lock().unwrap();
        conn.execute(
            "INSERT INTO memories (id,user_id,scope,memory,category,folder,embedding,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
            rusqlite::params![id, user_id, scope, text, category, folder, enc_emb(&emb), ts],
        )
        .map_err(|e| e.to_string())?;
    }
    write_mirror(scope, cwd);
    Ok(MemoryView {
        id,
        memory: text.to_string(),
        scope: scope.to_string(),
        category: category.map(|s| s.to_string()),
        folder: folder.map(|s| s.to_string()),
        score: None,
        created_at: Some(ts),
        updated_at: None,
    })
}

fn list(cwd: Option<&str>) -> Vec<MemoryView> {
    let conn = db().lock().unwrap();
    let mut out: Vec<MemoryView> = rows_for_user(&conn, GLOBAL_USER, None).iter().map(|r| row_to_view(r, None)).collect();
    if let Some(c) = cwd {
        let u = scope_user("project", Some(c));
        for r in rows_for_user(&conn, &u, None) {
            out.push(row_to_view(&r, None));
        }
    }
    out
}

fn search(query: &str, cwd: Option<&str>, scope: Option<&str>, threshold: Option<f64>, top_k: Option<usize>, folder: Option<&str>, category: Option<&str>) -> Result<Vec<MemoryView>, String> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }
    let threshold = threshold.unwrap_or(0.3);
    let top_k = top_k.unwrap_or(8);
    let scopes: Vec<&str> = match scope {
        Some(s) => vec![s],
        None => {
            if cwd.is_some() {
                vec!["project", "global"]
            } else {
                vec!["global"]
            }
        }
    };
    let q = embed_text(query)?;
    let mut collected: Vec<MemoryView> = Vec::new();
    {
        let conn = db().lock().unwrap();
        for sc in scopes {
            let u = scope_user(sc, cwd);
            // top (topK*2) by cosine above threshold per scope, mirroring mem.search(topK*2).
            let mut hits: Vec<(f64, Row)> = Vec::new();
            for r in rows_for_user(&conn, &u, category) {
                let s = cosine(&q, &r.embedding);
                if s >= threshold {
                    hits.push((s, r));
                }
            }
            hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            for (s, r) in hits.into_iter().take(top_k * 2) {
                collected.push(row_to_view(&r, Some(s)));
            }
        }
    }
    Ok(rerank(collected, folder).into_iter().take(top_k).collect())
}

/// Combine semantic score with recency decay + folder-match (mirror memory.ts rerank).
fn rerank(items: Vec<MemoryView>, folder: Option<&str>) -> Vec<MemoryView> {
    let now = now_ms() as f64;
    let f = folder.map(norm_folder);
    let mut scored: Vec<(f64, MemoryView)> = items
        .into_iter()
        .map(|it| {
            let base = it.score.unwrap_or(0.0);
            let age = now - it.created_at.unwrap_or(now as i64) as f64;
            let recency = (-(age.max(0.0)) / RECENCY_HALFLIFE_MS).exp();
            let folder_boost = match (&f, &it.folder) {
                (Some(ff), Some(itf)) => {
                    let nf = norm_folder(itf);
                    if &nf == ff || ff.starts_with(&nf) || nf.starts_with(ff.as_str()) {
                        0.1
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };
            (base * 0.8 + recency * 0.2 + folder_boost, it)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|x| x.1).collect()
}

/// Merge/prune near-duplicate memories per scope (keep newest of each >0.97 cluster). Returns (removed, kept).
fn consolidate(cwd: Option<&str>) -> (i64, i64) {
    let scopes: Vec<(&str, String)> = match cwd {
        Some(c) => vec![("project", scope_user("project", Some(c))), ("global", GLOBAL_USER.to_string())],
        None => vec![("global", GLOBAL_USER.to_string())],
    };
    let (mut removed, mut kept) = (0i64, 0i64);
    for (sc, user_id) in &scopes {
        let rows = {
            let conn = db().lock().unwrap();
            rows_for_user(&conn, user_id, None)
        };
        if rows.len() > 1 {
            let mut dropped: HashSet<String> = HashSet::new();
            for i in 0..rows.len() {
                if dropped.contains(&rows[i].id) {
                    continue;
                }
                for j in (i + 1)..rows.len() {
                    if dropped.contains(&rows[j].id) {
                        continue;
                    }
                    if cosine(&rows[i].embedding, &rows[j].embedding) > 0.97 {
                        // keep the newer; if all[i] (anchor) is dropped, stop using it as anchor.
                        let drop_i = rows[j].created_at >= rows[i].created_at;
                        dropped.insert(if drop_i { rows[i].id.clone() } else { rows[j].id.clone() });
                        if drop_i {
                            break;
                        }
                    }
                }
            }
            {
                let conn = db().lock().unwrap();
                for id in &dropped {
                    if conn.execute("DELETE FROM memories WHERE id=?1", rusqlite::params![id]).is_ok() {
                        removed += 1;
                    }
                }
            }
            kept += rows.len() as i64 - dropped.len() as i64;
        } else {
            kept += rows.len() as i64;
        }
        write_mirror(sc, cwd);
    }
    (removed, kept)
}

fn get_row(conn: &Connection, id: &str) -> Option<Row> {
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Row> {
        let blob: Vec<u8> = r.get(5)?;
        Ok(Row {
            id: r.get(0)?,
            scope: r.get(1)?,
            memory: r.get(2)?,
            category: r.get(3)?,
            folder: r.get(4)?,
            embedding: dec_emb(&blob),
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
        })
    };
    conn.query_row(
        "SELECT id,scope,memory,category,folder,embedding,created_at,updated_at FROM memories WHERE id=?1",
        rusqlite::params![id],
        map,
    )
    .ok()
}

fn update(id: &str, text: &str, cwd: Option<&str>) -> Option<MemoryView> {
    let emb = embed_text(text).ok()?;
    let ts = now_ms();
    let row = {
        let conn = db().lock().unwrap();
        let n = conn
            .execute(
                "UPDATE memories SET memory=?1, embedding=?2, updated_at=?3 WHERE id=?4",
                rusqlite::params![text, enc_emb(&emb), ts, id],
            )
            .unwrap_or(0);
        if n == 0 {
            return None;
        }
        get_row(&conn, id)
    };
    write_mirror_all(cwd);
    row.map(|r| row_to_view(&r, None))
}

fn remove(id: &str, cwd: Option<&str>) -> bool {
    // Mirror memory.ts: delete is best-effort; success unless the statement errors (missing id => still ok).
    let ok = {
        let conn = db().lock().unwrap();
        conn.execute("DELETE FROM memories WHERE id=?1", rusqlite::params![id]).is_ok()
    };
    if ok {
        write_mirror_all(cwd);
    }
    ok
}

// ---- MEMORY.md mirror (git-committable source of truth) ----
fn mirror_items(user_id: &str) -> Vec<MemoryView> {
    let conn = db().lock().unwrap();
    rows_for_user(&conn, user_id, None).iter().map(|r| row_to_view(r, None)).collect()
}

fn write_mirror(scope: &str, cwd: Option<&str>) {
    if scope == "global" {
        let items = mirror_items(GLOBAL_USER);
        let file = crate::config::dotz_dir().join("ai-agents").join("MEMORY.md");
        render_mirror_file(&file, "dotz global memory", &items);
    } else if let Some(c) = cwd {
        let items = mirror_items(&scope_user("project", Some(c)));
        let file = std::path::Path::new(c).join(".ai-agents").join("MEMORY.md");
        render_mirror_file(&file, "dotz project memory", &items);
    }
}

fn write_mirror_all(cwd: Option<&str>) {
    write_mirror("global", None);
    if let Some(c) = cwd {
        write_mirror("project", Some(c));
    }
}

fn render_mirror_file(file: &std::path::Path, title: &str, items: &[MemoryView]) {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = format!(
        "# {title}\n\n> Generated by dotz from mem0 — human-readable, git-committable mirror. The vector index\n> under ~/.dotz/ai-agents/mem0 is a derived cache, rebuildable from this file.\n"
    );
    if items.is_empty() {
        out.push_str("\n_(no memories yet)_\n");
        let _ = std::fs::write(file, out);
        return;
    }
    let mut by_cat: BTreeMap<String, Vec<&MemoryView>> = BTreeMap::new();
    for it in items {
        let c = it.category.clone().unwrap_or_else(|| "general".to_string());
        by_cat.entry(c).or_default().push(it);
    }
    for (cat, list) in &by_cat {
        out.push_str(&format!("\n## {cat}\n"));
        for it in list {
            let folder = it.folder.as_ref().map(|f| format!("  _(folder: {f})_")).unwrap_or_default();
            out.push_str(&format!("- {}{}\n", it.memory, folder));
        }
    }
    let _ = std::fs::write(file, out);
}

// ---- public surface for the agent runtime (pre-turn recall + the memory_* tools) ----

/// Query-relevant recall for pre-turn system-prompt injection. Searches project+global scope
/// (or global-only when no cwd), best-effort: any embedder/db error yields an empty list rather
/// than failing the turn. Mirrors the dotz-tools before_agent_start recall hook.
pub fn recall(query: &str, cwd: Option<&str>) -> Vec<MemoryView> {
    search(query, cwd, None, None, Some(6), None, None).unwrap_or_default()
}

/// Render a recalled-memory block for the system prompt (empty string when no hits).
pub fn render_recall(items: &[MemoryView]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Relevant memory (recalled for this turn)\n");
    for it in items {
        out.push_str(&format!("- {}\n", it.memory));
    }
    out
}

/// Public list (global + project) for the memory_list tool.
pub fn list_public(cwd: Option<&str>) -> Vec<MemoryView> {
    list(cwd)
}

/// Public search for the memory_search tool (defaulted threshold/topK).
pub fn search_public(query: &str, cwd: Option<&str>) -> Vec<MemoryView> {
    search(query, cwd, None, None, Some(8), None, None).unwrap_or_default()
}

/// Public verbatim add for the memory_add tool.
pub fn add_public(text: &str, scope: &str, cwd: Option<&str>) -> Result<MemoryView, String> {
    add(text, scope, None, None, cwd)
}

// ---- handlers ----
fn cwd_of(q: &HashMap<String, String>) -> Option<String> {
    crate::projects::cwd_for_project(q.get("projectId").map(|s| s.as_str()))
}
fn bad(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

async fn get_memory(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let cwd = cwd_of(&q);
    Json(json!({ "entries": list(cwd.as_deref()) }))
}

async fn post_memory(body: Option<Json<Value>>) -> Result<Json<MemoryView>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let raw = b.get("text").or_else(|| b.get("value"));
    let text = match raw.and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(bad("text (or value) is required")),
    };
    for k in ["category", "folder"] {
        if let Some(v) = b.get(k) {
            if !v.is_null() && !v.is_string() {
                return Err(bad(&format!("{k} must be a string")));
            }
        }
    }
    if let Some(s) = b.get("scope") {
        let sv = s.as_str().unwrap_or("");
        if sv != "project" && sv != "global" {
            return Err(bad("scope must be 'project' or 'global'"));
        }
    }
    let project_id = b.get("projectId").and_then(|v| v.as_str());
    let cwd = crate::projects::cwd_for_project(project_id);
    let scope = b
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| if project_id.is_some() { "project".into() } else { "global".into() });
    let category = b.get("category").and_then(|v| v.as_str());
    let folder = b.get("folder").and_then(|v| v.as_str());
    match add(&text, &scope, category, folder, cwd.as_deref()) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e })))),
    }
}

async fn patch_memory(Path(id): Path<String>, body: Option<Json<Value>>) -> Result<Json<MemoryView>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let raw = b.get("text").or_else(|| b.get("value"));
    let text = match raw.and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(bad("text (or value) is required")),
    };
    let cwd = crate::projects::cwd_for_project(b.get("projectId").and_then(|v| v.as_str()));
    match update(&id, &text, cwd.as_deref()) {
        Some(v) => Ok(Json(v)),
        None => Err((StatusCode::NOT_FOUND, Json(json!({ "error": "no such memory entry" })))),
    }
}

async fn delete_memory(Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let cwd = cwd_of(&q);
    Json(json!({ "ok": remove(&id, cwd.as_deref()) }))
}

async fn search_memory(body: Option<Json<Value>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    if let Some(qv) = b.get("query") {
        if !qv.is_null() && !qv.is_string() {
            return Err(bad("query must be a string"));
        }
    }
    let num = |key: &str| -> Result<Option<f64>, ()> {
        match b.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_f64() {
                Some(n) if n.is_finite() => Ok(Some(n)),
                _ => Err(()),
            },
        }
    };
    let threshold = match num("threshold") {
        Ok(t) => t,
        Err(_) => return Err(bad("threshold must be a finite number")),
    };
    let top_k = match num("topK") {
        Ok(t) => t.map(|n| n as usize),
        Err(_) => return Err(bad("topK must be a finite number")),
    };
    if let Some(s) = b.get("scope") {
        let sv = s.as_str().unwrap_or("");
        if sv != "project" && sv != "global" {
            return Err(bad("scope must be 'project' or 'global'"));
        }
    }
    let query = b.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = crate::projects::cwd_for_project(b.get("projectId").and_then(|v| v.as_str()));
    let scope = b.get("scope").and_then(|v| v.as_str());
    let folder = b.get("folder").and_then(|v| v.as_str());
    let category = b.get("category").and_then(|v| v.as_str());
    match search(query, cwd.as_deref(), scope, threshold, top_k, folder, category) {
        Ok(results) => Ok(Json(json!({ "results": results }))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e })))),
    }
}

async fn consolidate_memory(body: Option<Json<Value>>) -> Json<Value> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let cwd = crate::projects::cwd_for_project(b.get("projectId").and_then(|v| v.as_str()));
    let (removed, kept) = consolidate(cwd.as_deref());
    Json(json!({ "removed": removed, "kept": kept }))
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/memory", get(get_memory).post(post_memory))
        .route("/api/memory/{id}", patch(patch_memory).delete(delete_memory))
        .route("/api/memory/search", post(search_memory))
        .route("/api/memory/consolidate", post(consolidate_memory))
}
