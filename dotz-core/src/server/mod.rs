//! axum HTTP/WS server. Mirrors src/server.ts route groups; serves the static `web/` UI.
use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf};
use tower_http::services::ServeDir;

/// Build the app: REST API + static `web/` UI fallback.
/// Phase 1: only `/api/health` exists; every other path falls through to the static UI,
/// so the unchanged `web/` SPA loads and talks to this Rust backend over `location.host`.
pub fn app(web_dir: PathBuf) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
}

async fn health() -> Json<Value> {
    // ponytail: sessions/sandboxRuns are 0 until those stores land (Phase 3/4).
    Json(json!({ "ok": true, "sessions": 0, "sandboxRuns": 0 }))
}

pub async fn serve(addr: SocketAddr, web_dir: PathBuf) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("dotz-core listening on http://{addr}  (web: {})", web_dir.display());
    axum::serve(listener, app(web_dir)).await
}
