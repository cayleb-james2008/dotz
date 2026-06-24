//! axum HTTP/WS server. Mirrors src/server.ts route groups; serves the static `web/` UI.
use crate::{
    config::{self, CleanPatch, DotzConfig},
    profiles, types,
};
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
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
        // Phase 2 cold modules — self-contained, stateless Router<()> merged after with_state.
        .merge(crate::sandbox::router())
        .merge(crate::design::router())
        .merge(crate::projects::router())
        .merge(crate::connections::router())
        .merge(crate::workflows::router())
        .merge(crate::skills::router())
        .merge(crate::memory::router())
        .merge(crate::agent::router())
        .merge(crate::browser::router())
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
}

async fn health(State(_s): State<Shared>) -> Json<Value> {
    Json(
        json!({ "ok": true, "sessions": crate::agent::session_count(), "sandboxRuns": crate::sandbox::run_count() }),
    )
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
        let guard = s.config.lock().unwrap();
        let n = config::update(&guard, &clean).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to persist config: {e}") })),
            )
        })?;
        drop(guard);
        let mut guard = s.config.lock().unwrap();
        *guard = n.clone();
        n
    };
    Ok(Json(json!({ "config": next })))
}

fn bad(msg: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

/// Serve with an explicit graceful-shutdown future. Callers (e.g. the headless `serve` bin) can
/// stop cleanly on SIGINT/SIGTERM; Tauri uses `serve()` and lets the process die with the window.
pub async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    web_dir: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let state = Arc::new(AppState {
        config: Mutex::new(config::load()),
    });
    // Only the main server process captures memory autonomously (subagents never do).
    crate::memory::enable_autonomy();
    eprintln!(
        "dotz-core listening on http://{}  (web: {})",
        listener.local_addr()?,
        web_dir.display()
    );
    axum::serve(listener, app(web_dir, state))
        .with_graceful_shutdown(shutdown)
        .await
}

pub async fn serve(addr: SocketAddr, web_dir: PathBuf) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_with_shutdown(listener, web_dir, std::future::pending::<()>()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn health_endpoint_ok_and_graceful_shutdown_works() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async {
            let _: () = rx.await.unwrap_or(());
        };
        let handle = tokio::spawn(serve_with_shutdown(
            listener,
            PathBuf::from("web"),
            shutdown,
        ));
        // Give the server a tick to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("200 OK"), "health did not return 200: {text}");
        assert!(
            text.contains("\"ok\":true") || text.contains("\"ok\": true"),
            "health body missing ok:true: {text}"
        );
        assert!(
            text.contains("\"sandboxRuns\""),
            "health body missing sandboxRuns field: {text}"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server exited with error: {:?}",
            result.err()
        );
    }
}
