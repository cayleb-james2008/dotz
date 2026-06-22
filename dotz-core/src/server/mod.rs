//! axum HTTP/WS server. Mirrors src/server.ts route groups; serves the static `web/` UI.
use crate::{
    config::{self, CleanPatch, DotzConfig},
    profiles, types,
};
use axum::{
    extract::State,
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tower_http::services::ServeDir;

/// Server-wide state. Grows as modules land (sessions, sandbox, memory, ...).
pub struct AppState {
    pub config: Mutex<DotzConfig>,
}
pub type Shared = Arc<AppState>;

/// Build the app: REST API + static `web/` UI fallback. Unmatched paths fall through to the
/// unchanged `web/` SPA, which talks to this backend over `location.host`.
pub fn app(web_dir: PathBuf, state: Shared) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/providers", get(providers))
        .route("/api/profiles", get(profiles_list))
        .route("/api/config", get(get_config).post(post_config))
        .with_state(state)
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
}

async fn health(State(_s): State<Shared>) -> Json<Value> {
    // ponytail: sessions/sandboxRuns are 0 until those stores land (Phase 3/4).
    Json(json!({ "ok": true, "sessions": 0, "sandboxRuns": 0 }))
}

async fn providers() -> Json<Value> {
    Json(json!({ "providers": types::providers() }))
}

async fn profiles_list() -> Json<Value> {
    Json(json!({ "profiles": profiles::summaries(), "default": "workflow" }))
}

async fn get_config(State(s): State<Shared>) -> Json<Value> {
    let c = s.config.lock().unwrap().clone();
    Json(json!({
        "config": c,
        "providerDefaults": types::provider_defaults_json(),
        "providers": types::providers(),
    }))
}

/// POST /api/config — validate (400 on bad value), persist, return `{config}`. Mirrors server.ts.
async fn post_config(
    State(s): State<Shared>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let patch = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let mut clean = CleanPatch::default();

    if let Some(t) = patch.get("thinkingLevel") {
        let ts = t.as_str().unwrap_or("");
        if !types::is_valid_thinking(ts) {
            return Err(bad(format!(
                "thinkingLevel must be one of: {}",
                types::THINKING_LEVELS.join(", ")
            )));
        }
        clean.thinking_level = Some(ts.to_string());
    }
    if let Some(p) = patch.get("provider") {
        let ps = p.as_str().unwrap_or("").trim().to_lowercase();
        if !types::is_known_provider(&ps) {
            return Err(bad(format!(
                "provider must be one of: {}",
                types::provider_ids().join(", ")
            )));
        }
        clean.provider = Some(ps);
    }
    for key in ["executiveModel", "subagentModel"] {
        if let Some(v) = patch.get(key) {
            let t = v.as_str().unwrap_or("").trim().to_string();
            if t.is_empty() {
                return Err(bad(format!("{key} must be a non-empty string")));
            }
            if key == "executiveModel" {
                clean.executive_model = Some(t);
            } else {
                clean.subagent_model = Some(t);
            }
        }
    }

    let next = {
        let mut guard = s.config.lock().unwrap();
        let n = config::update(&guard, &clean);
        *guard = n.clone();
        n
    };
    Ok(Json(json!({ "config": next })))
}

fn bad(msg: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

pub async fn serve(addr: SocketAddr, web_dir: PathBuf) -> std::io::Result<()> {
    let state = Arc::new(AppState { config: Mutex::new(config::load()) });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("dotz-core listening on http://{addr}  (web: {})", web_dir.display());
    axum::serve(listener, app(web_dir, state)).await
}
