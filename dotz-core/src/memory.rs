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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const GLOBAL_USER: &str = "__global__";
const RECENCY_HALFLIFE_MS: f64 = 1000.0 * 60.0 * 60.0 * 24.0 * 30.0; // 30 days
const AUTO_CONSOLIDATE_EVERY: i64 = 25; // captures between automatic consolidation passes
const CAPTURE_DEDUP_THRESHOLD: f64 = 0.95; // a new fact >= this cosine to a same-scope neighbor is a near-dup

/// Coding-tuned fact extraction instruction — ported verbatim from memory.ts CODING_INSTRUCTIONS.
/// Keeps durable engineering facts, drops transient task chatter; this is what makes auto-capture
/// useful instead of noisy.
const CODING_INSTRUCTIONS: &str = "You are the durable memory of a coding agent working on software projects. Extract ONLY durable, reusable facts worth remembering across future sessions: - project conventions & code style, architecture/design decisions and their rationale, - build / test / lint / deploy commands, important file or module locations, - gotchas, workarounds, and non-obvious constraints, tooling/library choices, - explicit, stable USER preferences and standing instructions. IGNORE transient task state, one-off answers, ephemeral file contents, and pleasantries. Write each memory as a single concise, self-contained fact. If nothing is durable, extract nothing.";

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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Scope → mem0 userId. global / no-cwd → "__global__"; project → "proj:<norm cwd>"
/// (backslashes → "/", lowercased on Windows). Mirrors memory.ts scopeUser.
fn scope_user(scope: &str, cwd: Option<&str>) -> String {
    match (scope, cwd) {
        ("global", _) | (_, None) => GLOBAL_USER.to_string(),
        (_, Some(c)) => {
            let norm = c.replace('\\', "/");
            let norm = if cfg!(windows) {
                norm.to_lowercase()
            } else {
                norm
            };
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

/// Lock the DB mutex, recovering from a poisoned lock. A panic while holding the DB lock (e.g.
/// inside a memory tool callback or autonomous capture) must not permanently brick the memory
/// store.
fn db_guard() -> std::sync::MutexGuard<'static, Connection> {
    db().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

static EMBEDDER: OnceLock<Mutex<Option<Embedder>>> = OnceLock::new();
fn embedder() -> &'static Mutex<Option<Embedder>> {
    EMBEDDER.get_or_init(|| Mutex::new(None))
}
/// Lock the embedder mutex, recovering from a poisoned lock. A panic during embedder load or
/// inference must not permanently brick all future memory operations.
fn embedder_guard() -> std::sync::MutexGuard<'static, Option<Embedder>> {
    embedder().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn embed_text(text: &str) -> Result<Vec<f32>, String> {
    let mut g = embedder_guard();
    if g.is_none() {
        *g = Some(Embedder::load().map_err(|e| format!("embedder load: {e}"))?);
    }
    g.as_mut()
        .unwrap()
        .embed(text)
        .map_err(|e| format!("embed: {e}"))
}

fn enc_emb(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
fn dec_emb(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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
fn add(
    text: &str,
    scope: &str,
    category: Option<&str>,
    folder: Option<&str>,
    cwd: Option<&str>,
) -> Result<MemoryView, String> {
    let user_id = scope_user(scope, cwd);
    let emb = embed_text(text)?;
    let id = uuid::Uuid::new_v4().to_string();
    let ts = now_ms();
    {
        let conn = db_guard();
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
    let conn = db_guard();
    let mut out: Vec<MemoryView> = rows_for_user(&conn, GLOBAL_USER, None)
        .iter()
        .map(|r| row_to_view(r, None))
        .collect();
    if let Some(c) = cwd {
        let u = scope_user("project", Some(c));
        for r in rows_for_user(&conn, &u, None) {
            out.push(row_to_view(&r, None));
        }
    }
    out
}

fn search(
    query: &str,
    cwd: Option<&str>,
    scope: Option<&str>,
    threshold: Option<f64>,
    top_k: Option<usize>,
    folder: Option<&str>,
    category: Option<&str>,
) -> Result<Vec<MemoryView>, String> {
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
        let conn = db_guard();
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
        Some(c) => vec![
            ("project", scope_user("project", Some(c))),
            ("global", GLOBAL_USER.to_string()),
        ],
        None => vec![("global", GLOBAL_USER.to_string())],
    };
    let (mut removed, mut kept) = (0i64, 0i64);
    for (sc, user_id) in &scopes {
        let rows = {
            let conn = db_guard();
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
                        dropped.insert(if drop_i {
                            rows[i].id.clone()
                        } else {
                            rows[j].id.clone()
                        });
                        if drop_i {
                            break;
                        }
                    }
                }
            }
            {
                let conn = db_guard();
                for id in &dropped {
                    if conn
                        .execute("DELETE FROM memories WHERE id=?1", rusqlite::params![id])
                        .is_ok()
                    {
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
        let conn = db_guard();
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
        let conn = db_guard();
        conn.execute("DELETE FROM memories WHERE id=?1", rusqlite::params![id])
            .is_ok()
    };
    if ok {
        write_mirror_all(cwd);
    }
    ok
}

// ---- MEMORY.md mirror (git-committable source of truth) ----
fn mirror_items(user_id: &str) -> Vec<MemoryView> {
    let conn = db_guard();
    rows_for_user(&conn, user_id, None)
        .iter()
        .map(|r| row_to_view(r, None))
        .collect()
}

fn write_mirror(scope: &str, cwd: Option<&str>) {
    if scope == "global" {
        let items = mirror_items(GLOBAL_USER);
        let file = crate::config::dotz_dir()
            .join("ai-agents")
            .join("MEMORY.md");
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
            let folder = it
                .folder
                .as_ref()
                .map(|f| format!("  _(folder: {f})_"))
                .unwrap_or_default();
            out.push_str(&format!("- {}{}\n", it.memory, folder));
        }
    }
    let _ = std::fs::write(file, out);
}

// ---- LLM autonomy: capture (infer:true) + auto-consolidation ----
// Mirrors memory.ts captureExchange / maybeAutoConsolidate. The Node version delegated the fact
// extraction + dedup to mem0 (infer:true); this port does it explicitly: one LLM chat completion to
// extract durable facts, then per-fact cosine dedup vs same-scope neighbors before a verbatim add().

/// ONLY the main server process enables capture — spawned subagents (separate processes) must never
/// capture. Mirrors memory.ts autonomyEnabled / isMemoryAutonomyEnabled.
static AUTONOMY: AtomicBool = AtomicBool::new(false);
/// Facts captured since the last consolidation pass (counts stored facts, not exchanges).
static CAPTURES_SINCE_CONSOLIDATE: AtomicI64 = AtomicI64::new(0);

/// Enable autonomous memory capture for this process. Call once at server startup.
pub fn enable_autonomy() {
    AUTONOMY.store(true, Ordering::Relaxed);
}

/// True when this process is the main session and should auto-capture memory.
pub fn is_autonomy_enabled() -> bool {
    AUTONOMY.load(Ordering::Relaxed)
}

/// Resolve the memory-LLM endpoint (Ollama OpenAI-compatible) + model + key. Mirrors memory.ts
/// engine(): DOTZ_MEMORY_BASE_URL || https://ollama.com/v1, DOTZ_MEMORY_MODEL || executiveModel ||
/// glm-5.2, and OLLAMA_API_KEY via the $-env indirection the providers use.
fn memory_llm() -> (String, String, String) {
    let base_url = std::env::var("DOTZ_MEMORY_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://ollama.com/v1".to_string());
    let model = std::env::var("DOTZ_MEMORY_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let exec = crate::config::load().executive_model;
            if exec.trim().is_empty() {
                "glm-5.2".to_string()
            } else {
                exec
            }
        });
    // Same auth rule as the providers: prefer DOTZ_MEMORY_API_KEY, else OLLAMA_API_KEY (the $-form is
    // what triggers env indirection — we resolve it directly here).
    let key = std::env::var("DOTZ_MEMORY_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::agent::provider::resolve_api_key("$OLLAMA_API_KEY"));
    (base_url, model, key)
}

/// Ask the configured LLM to extract durable facts from one user↔assistant exchange. Returns a list
/// of self-contained fact strings (possibly empty). Best-effort: any transport/parse error → empty.
async fn extract_facts(user_text: &str, assistant_text: &str) -> Vec<String> {
    let (base_url, model, key) = memory_llm();
    if key.is_empty() {
        return Vec::new(); // no key → silent no-op (mirrors the provider's empty-reply 401 behavior)
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let system = format!(
        "{CODING_INSTRUCTIONS}\n\n\
         OUTPUT FORMAT — CRITICAL: respond with ONLY a raw JSON array of strings and NOTHING else. \
         No prose, no greeting, no acknowledgement, no markdown code fences. Each array element is one \
         durable, self-contained fact in the third person. If nothing is durable, output exactly [].\n\
         Example of a valid response: [\"The deploy command is `make ship-prod`, run from the repo root on the release branch.\"]"
    );
    let exchange = format!(
        "User:\n{user_text}\n\nAssistant:\n{assistant_text}\n\n\
         Now output the durable facts from this exchange as a raw JSON array of strings (just the array)."
    );
    let body = json!({
        "model": model,
        "temperature": 0.1,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": exchange },
        ],
    });
    let resp = match reqwest::Client::new()
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return Vec::new(),
    };
    let v: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    parse_facts(content)
}

/// Parse the LLM reply into a fact list. Accepts a bare JSON array, or one fenced/embedded in prose
/// (we slice the outermost [...]); falls back to non-empty trimmed lines. Always returns clean facts.
fn parse_facts(content: &str) -> Vec<String> {
    let slice = match (content.find('['), content.rfind(']')) {
        (Some(a), Some(b)) if b > a => &content[a..=b],
        _ => content.trim(),
    };
    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(slice) {
        return arr
            .into_iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

/// Auto-capture a completed user↔assistant exchange. LLM-extracts durable facts, dedups each against
/// same-scope neighbors (cosine >= CAPTURE_DEDUP_THRESHOLD → skip), verbatim-adds the rest (which
/// writes the mirror), bumps the capture counter, and consolidates every AUTO_CONSOLIDATE_EVERY.
/// Best-effort throughout — never panics, mirrors memory.ts captureExchange. Returns the kept facts.
pub async fn capture_exchange(
    user_text: &str,
    assistant_text: &str,
    cwd: Option<&str>,
) -> Vec<MemoryView> {
    // Defense-in-depth: only the main server process is allowed to auto-capture memory. Subagents
    // and any future callers get a silent no-op instead of polluting the memory store.
    if !is_autonomy_enabled() {
        return Vec::new();
    }
    let u = user_text.trim();
    let a = assistant_text.trim();
    if u.len() < 8 && a.len() < 40 {
        return Vec::new(); // skip trivial exchanges
    }
    let scope = if cwd.is_some() { "project" } else { "global" };
    let facts = extract_facts(u, a).await;
    if facts.is_empty() {
        return Vec::new();
    }

    // Same-scope neighbors for dedup (embeddings already in the row).
    let user_id = scope_user(scope, cwd);
    let neighbors: Vec<Row> = {
        let conn = db_guard();
        rows_for_user(&conn, &user_id, None)
    };

    let mut kept: Vec<MemoryView> = Vec::new();
    let mut added_embs: Vec<Vec<f32>> = Vec::new();
    for fact in facts {
        let emb = match embed_text(&fact) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Near-dup vs existing neighbors OR vs a fact we just added this turn → skip.
        let dup = neighbors
            .iter()
            .any(|r| cosine(&emb, &r.embedding) >= CAPTURE_DEDUP_THRESHOLD)
            || added_embs
                .iter()
                .any(|e| cosine(&emb, e) >= CAPTURE_DEDUP_THRESHOLD);
        if dup {
            continue;
        }
        if let Ok(v) = add(&fact, scope, None, None, cwd) {
            kept.push(v);
            added_embs.push(emb);
        }
    }

    if !kept.is_empty() {
        CAPTURES_SINCE_CONSOLIDATE.fetch_add(kept.len() as i64, Ordering::Relaxed);
        maybe_auto_consolidate(cwd);
    }
    kept
}

/// Run consolidation if enough new captures have accumulated since the last pass. Resets the counter
/// when it fires. Mirrors memory.ts maybeAutoConsolidate.
pub fn maybe_auto_consolidate(cwd: Option<&str>) {
    if CAPTURES_SINCE_CONSOLIDATE.load(Ordering::Relaxed) < AUTO_CONSOLIDATE_EVERY {
        return;
    }
    CAPTURES_SINCE_CONSOLIDATE.store(0, Ordering::Relaxed);
    let _ = consolidate(cwd);
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

async fn post_memory(
    body: Option<Json<Value>>,
) -> Result<Json<MemoryView>, (StatusCode, Json<Value>)> {
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
        .unwrap_or_else(|| {
            if project_id.is_some() {
                "project".into()
            } else {
                "global".into()
            }
        });
    let category = b.get("category").and_then(|v| v.as_str());
    let folder = b.get("folder").and_then(|v| v.as_str());
    match add(&text, &scope, category, folder, cwd.as_deref()) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )),
    }
}

async fn patch_memory(
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<MemoryView>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let raw = b.get("text").or_else(|| b.get("value"));
    let text = match raw.and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(bad("text (or value) is required")),
    };
    let cwd = crate::projects::cwd_for_project(b.get("projectId").and_then(|v| v.as_str()));
    match update(&id, &text, cwd.as_deref()) {
        Some(v) => Ok(Json(v)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such memory entry" })),
        )),
    }
}

async fn delete_memory(
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let cwd = cwd_of(&q);
    Json(json!({ "ok": remove(&id, cwd.as_deref()) }))
}

async fn search_memory(
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
    match search(
        query,
        cwd.as_deref(),
        scope,
        threshold,
        top_k,
        folder,
        category,
    ) {
        Ok(results) => Ok(Json(json!({ "results": results }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )),
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
        .route(
            "/api/memory/{id}",
            patch(patch_memory).delete(delete_memory),
        )
        .route("/api/memory/search", post(search_memory))
        .route("/api/memory/consolidate", post(consolidate_memory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-memory-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let result = f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
        result
    }

    #[test]
    fn scope_user_global_ignores_cwd() {
        assert_eq!(scope_user("global", Some("/any/path")), "__global__");
        assert_eq!(scope_user("global", None), "__global__");
    }

    #[test]
    fn scope_user_project_normalizes_path() {
        assert_eq!(
            scope_user("project", Some("C:\\Projects\\Dotz")),
            "proj:c:/projects/dotz"
        );
        assert_eq!(
            scope_user("project", Some("/home/user/dotz")),
            "proj:/home/user/dotz"
        );
    }

    #[test]
    fn norm_folder_strips_leading_separators_and_lower_cases() {
        assert_eq!(norm_folder("./src/Memory"), "src/memory");
        assert_eq!(norm_folder("/src/Memory"), "src/memory");
        assert_eq!(norm_folder("src\\Memory"), "src/memory");
    }

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector_is_zero() {
        let a = vec![0.0f32; 4];
        let b = vec![1.0f32, 0.0, 0.0, 0.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn parse_facts_bare_json_array() {
        let content = r#"["Fact one", "Fact two"]"#;
        let facts = parse_facts(content);
        assert_eq!(facts, vec!["Fact one", "Fact two"]);
    }

    #[test]
    fn parse_facts_extracts_array_from_prose() {
        let content = "Here are the facts: [\"Fact A\", \"Fact B\"] Done.";
        let facts = parse_facts(content);
        assert_eq!(facts, vec!["Fact A", "Fact B"]);
    }

    #[test]
    fn parse_facts_returns_empty_for_invalid_json() {
        let content = "Just some prose without an array.";
        assert!(parse_facts(content).is_empty());
    }

    #[test]
    fn parse_facts_skips_empty_strings() {
        let content = r#"["", "Real fact", "   "]"#;
        let facts = parse_facts(content);
        assert_eq!(facts, vec!["Real fact"]);
    }

    #[test]
    fn render_recall_empty_returns_empty() {
        assert_eq!(render_recall(&[]), "");
    }

    #[test]
    fn render_recall_renders_items() {
        let items = vec![
            MemoryView {
                id: "a".into(),
                memory: "Use cargo test -p dotz-core for the gate.".into(),
                scope: "global".into(),
                category: None,
                folder: None,
                score: None,
                created_at: None,
                updated_at: None,
            },
            MemoryView {
                id: "b".into(),
                memory: "Project root is /home/user/dotz.".into(),
                scope: "project".into(),
                category: None,
                folder: None,
                score: None,
                created_at: None,
                updated_at: None,
            },
        ];
        let rendered = render_recall(&items);
        assert!(rendered.starts_with("# Relevant memory (recalled for this turn)\n"));
        assert!(rendered.contains("Use cargo test -p dotz-core for the gate."));
        assert!(rendered.contains("Project root is /home/user/dotz."));
    }

    #[test]
    fn write_mirror_file_renders_empty_placeholder() {
        with_tmp_dir(|dir| {
            let file = dir.join("MEMORY.md");
            render_mirror_file(&file, "test memory", &[]);
            let raw = std::fs::read_to_string(&file).unwrap();
            assert!(raw.starts_with("# test memory\n"));
            assert!(raw.contains("_(no memories yet)_"));
        });
    }

    #[test]
    fn write_mirror_file_groups_by_category_sorted() {
        with_tmp_dir(|dir| {
            let file = dir.join("MEMORY.md");
            let items = vec![
                MemoryView {
                    id: "z".into(),
                    memory: "Zebra convention.".into(),
                    scope: "global".into(),
                    category: Some("zoo".into()),
                    folder: None,
                    score: None,
                    created_at: Some(1),
                    updated_at: None,
                },
                MemoryView {
                    id: "a".into(),
                    memory: "Alpha fact.".into(),
                    scope: "global".into(),
                    category: Some("abc".into()),
                    folder: None,
                    score: None,
                    created_at: Some(2),
                    updated_at: None,
                },
                MemoryView {
                    id: "g".into(),
                    memory: "General note.".into(),
                    scope: "global".into(),
                    category: None,
                    folder: None,
                    score: None,
                    created_at: Some(3),
                    updated_at: None,
                },
            ];
            render_mirror_file(&file, "grouped memory", &items);
            let raw = std::fs::read_to_string(&file).unwrap();
            // Categories are sorted alphabetically; "general" is the default for missing category.
            let abc_pos = raw.find("## abc").expect("abc category present");
            let general_pos = raw.find("## general").expect("general category present");
            let zoo_pos = raw.find("## zoo").expect("zoo category present");
            assert!(abc_pos < general_pos && general_pos < zoo_pos);
            assert!(raw.contains("- Alpha fact."));
            assert!(raw.contains("- Zebra convention."));
            assert!(raw.contains("- General note."));
        });
    }

    #[tokio::test]
    async fn capture_exchange_respects_autonomy_gate() {
        // Toggle the autonomy flag under the serialized test lock, but never hold the
        // std::sync::MutexGuard across an await point — that would block the tokio executor
        // and can deadlock other async tasks.
        let prev = {
            let guard = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prev = AUTONOMY.load(Ordering::Relaxed);
            // Disable autonomy to simulate a subagent/non-main process.
            AUTONOMY.store(false, Ordering::Relaxed);
            drop(guard);
            prev
        };

        // Use text long enough to bypass the trivial-exchange filter so the only reason we get an
        // empty result is the autonomy gate, not the length heuristic.
        let kept = capture_exchange(
            "The user asked a substantive multi-sentence question about the codebase architecture.",
            "The assistant replied with a detailed explanation that is well over forty characters long and contains durable facts worth remembering.",
            None,
        )
        .await;

        // Restore the previous autonomy flag so other tests see their expected state.
        {
            let guard = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            AUTONOMY.store(prev, Ordering::Relaxed);
            drop(guard);
        }

        assert!(
            kept.is_empty(),
            "capture_exchange must not run when memory autonomy is disabled"
        );
    }

    /// A panic while holding the DB mutex must not permanently brick the memory store. With
    /// poison recovery, subsequent DB reads/writes keep working after a previous lock owner
    /// panicked mid-operation.
    #[test]
    fn db_guard_recovers_from_poisoned_mutex() {
        with_tmp_dir(|dir| {
            let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
            std::env::set_var("DOTZ_CONFIG_DIR", dir);
            // Force initialization if not already done, so this test runs in an isolated
            // location when it is the first caller.
            drop(db().lock().unwrap());

            let db_ref = db();
            let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = db_ref.lock().unwrap();
                panic!("intentional db mutex poison");
            }));
            assert!(poisoned.is_err(), "db mutex should be poisoned");

            // db_guard must recover and return a usable connection. The table may already
            // contain rows if another test or a previous run initialized the process-global DB
            // before this test; the invariant here is that the connection remains usable.
            let guard = db_guard();
            let _: i64 = guard
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .unwrap();

            match prev {
                Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
                None => std::env::remove_var("DOTZ_CONFIG_DIR"),
            }
        });
    }

    /// A panic while holding the embedder mutex must not permanently brick future memory
    /// operations. With poison recovery, the guard returns the underlying Option<Embedder>
    /// instead of panicking.
    #[test]
    fn embedder_guard_recovers_from_poisoned_mutex() {
        // Ensure the embedder mutex is initialized.
        drop(embedder().lock().unwrap());

        let m = embedder();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("intentional embedder mutex poison");
        }));
        assert!(poisoned.is_err(), "embedder mutex should be poisoned");

        let guard = embedder_guard();
        assert!(
            guard.is_none(),
            "embedder should be lazily unloaded in tests"
        );
    }
}
