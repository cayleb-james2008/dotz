//! dotz sandbox — code-execution run store + language list. Port of src/sandbox.ts +
//! server.ts sandbox routes (lines 510-545).
//!
//! Phase 2 scope: READ + store-shape endpoints backed by an in-memory runs store. The store is a
//! module-level singleton (OnceLock<Mutex<Vec<SandboxRun>>>) mirroring the Node `sandbox` singleton.
//! Real process execution (POST runs / kill, port detection) is deferred to Phase 4.
use axum::{
    extract::Path,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

/// Language list, in the exact order of LANGUAGES in sandbox.ts (Object.keys order).
/// Matches fixtures/sandbox.languages.json exactly.
const SANDBOX_LANGUAGES: [&str; 6] =
    ["javascript", "typescript", "python", "bash", "powershell", "shell"];

/// A sandbox run record — mirrors the SandboxRun interface in src/types.ts.
#[derive(Clone, Debug, Serialize)]
pub struct SandboxRun {
    pub id: String,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    pub language: String,
    pub code: String,
    pub status: String,
    pub output: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i64>,
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    #[serde(rename = "endedAt")]
    pub ended_at: Option<i64>,
}

/// In-memory runs store (module singleton, mirrors the Node `sandbox` module-level instance).
fn runs() -> &'static Mutex<Vec<SandboxRun>> {
    static RUNS: OnceLock<Mutex<Vec<SandboxRun>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn router() -> Router<()> {
    Router::new()
        .route("/api/sandbox/languages", get(languages))
        .route("/api/sandbox/runs", get(list_runs).post(create_run))
        .route("/api/sandbox/runs/{id}", get(get_run))
        .route("/api/sandbox/runs/{id}/port", get(run_port))
        .route("/api/sandbox/runs/{id}/kill", post(kill_run))
}

async fn languages() -> Json<Value> {
    Json(json!({ "languages": SANDBOX_LANGUAGES }))
}

async fn list_runs() -> Json<Value> {
    let store = runs().lock().unwrap();
    Json(json!({ "runs": &*store }))
}

async fn get_run(Path(id): Path<String>) -> Result<Json<SandboxRun>, (StatusCode, Json<Value>)> {
    let store = runs().lock().unwrap();
    match store.iter().find(|r| r.id == id) {
        Some(r) => Ok(Json(r.clone())),
        None => Err(not_found("no such sandbox run")),
    }
}

/// Phase 2: no port detection yet — always 404, matching the Node "no web port" 404 path.
async fn run_port(Path(_id): Path<String>) -> (StatusCode, Json<Value>) {
    not_found("no web port detected (terminal run or not yet listening)")
}

// ponytail: Phase 4 — real process exec (spawn temp dir, stream stdout/stderr, port detect) lands later.
async fn create_run() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "sandbox execution not yet implemented (Phase 4)" })),
    )
}

// ponytail: Phase 4 — kill a live run (killTree) once exec exists.
async fn kill_run(Path(_id): Path<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "sandbox execution not yet implemented (Phase 4)" })),
    )
}

fn not_found(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
}
