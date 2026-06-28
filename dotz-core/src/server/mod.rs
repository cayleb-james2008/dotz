//! axum HTTP/WS server. Mirrors src/server.ts route groups; serves the static `web/` UI.
use crate::{
    config::{self, CleanPatch, DotzConfig},
    profiles, types,
};
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
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

/// Lock the shared config mutex, recovering from a poisoned lock. A panic while holding the
/// config lock (e.g. inside a validation callback) must not permanently brick the config REST
/// endpoints.
fn state_config(s: &AppState) -> std::sync::MutexGuard<'_, DotzConfig> {
    s.config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build the app: REST API + static `web/` UI fallback. Unmatched paths fall through to the
/// unchanged `web/` SPA, which talks to this backend over `location.host`.
pub fn app(web_dir: PathBuf, state: Shared) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/providers", get(providers))
        .route("/api/profiles", get(profiles_list))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/verify/suite/{profile}", get(crate::verify::suite_handler))
        .route("/api/verify/run", post(crate::verify::run_handler))
        .with_state(state)
        // Phase 2 cold modules — self-contained, stateless Router<()> merged after with_state.
        .merge(crate::sandbox::router())
        .merge(crate::design::router())
        .merge(crate::projects::router())
        .merge(crate::connections::router())
        .merge(crate::workflows::router())
        .merge(crate::skills::router())
        .merge(crate::templates::router())
        .merge(crate::memory::router())
        .merge(crate::agent::router())
        .merge(crate::browser::router())
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
}

async fn health(State(_s): State<Shared>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "sessions": crate::agent::session_count(),
        "sandboxRuns": crate::sandbox::run_count(),
        "workflowRuns": crate::workflows::active_count(),
        "browserSessions": crate::browser::session_count(),
        "embedderReady": crate::embed::model_files_present(),
    }))
}

async fn providers() -> Json<Value> {
    Json(json!({ "providers": types::providers() }))
}

async fn profiles_list() -> Json<Value> {
    Json(json!({ "profiles": profiles::summaries(), "default": "workflow" }))
}

async fn get_config(State(s): State<Shared>) -> Json<Value> {
    let c = state_config(&s).clone();
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

    // Hold one lock across read → persist → write-back so concurrent POSTs cannot
    // interleave: a later request must see the persisted state of an earlier one.
    let mut guard = state_config(&s);
    let next = config::update(&guard, &clean).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to persist config: {e}") })),
        )
    })?;
    *guard = next.clone();
    Ok(Json(json!({ "config": next })))
}

fn bad(msg: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

/// Wait for SIGINT (all platforms) or SIGTERM (Unix) so the axum server can drain open
/// connections instead of leaving them hanging on a hard kill. Exposed from `dotz-core::server`
/// so both the headless `serve` bin and any future caller share the same shutdown behavior.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
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

    // Resume interrupted workflow runs from the on-disk history. Without this, a
    // run that was `running` at shutdown would be lost forever — the in-memory
    // active map is empty after a restart. This scans `workflows.json`, marks
    // in-flight steps `interrupted`, and re-spawns the executor for each.
    let resumed = crate::workflows::startup_resume();
    if resumed > 0 {
        eprintln!("dotz-core resumed {resumed} interrupted workflow run(s)");
    }

    eprintln!(
        "dotz-core listening on http://{}  (web: {})",
        listener.local_addr()?,
        web_dir.display()
    );
    axum::serve(listener, app(web_dir, state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Convenience: bind `addr`, then run the server until `shutdown` resolves. Mirrors the common
/// "bind + serve with shutdown" pattern used by the headless bin and lets callers avoid
/// duplicating bind logic.
pub async fn serve_with_shutdown_addr(
    addr: SocketAddr,
    web_dir: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_with_shutdown(listener, web_dir, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Two concurrent POST /api/config patches on different fields must both survive: the
    /// second request has to observe the first request's persisted state, not the original
    /// in-memory snapshot. Before the single-lock fix the save/write-back window let one patch
    /// overwrite the other in memory and on disk. We use OS threads + a barrier to force real
    /// contention; the tokio task scheduler alone does not interleave the synchronous bodies.
    #[test]
    fn post_config_serializes_concurrent_updates() {
        use std::sync::Barrier;
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap();

        let dir =
            std::env::temp_dir().join(format!("dotz-server-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev_dir = std::env::var("DOTZ_CONFIG_DIR").ok();
        let prev_subagent = std::env::var("DOTZ_SUBAGENT_MODEL").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);

        let state = Arc::new(AppState {
            config: Mutex::new(config::load()),
        });
        let barrier = Arc::new(Barrier::new(2));

        let spawn = |state: Arc<AppState>, barrier: Arc<Barrier>, body: Value| {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                barrier.wait();
                rt.block_on(post_config(State(state), Some(Json(body))))
                    .unwrap()
            })
        };

        let t1 = spawn(
            state.clone(),
            barrier.clone(),
            json!({ "provider": "openrouter" }),
        );
        let t2 = spawn(
            state.clone(),
            barrier.clone(),
            json!({ "thinkingLevel": "xhigh" }),
        );
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let in_memory = state.config.lock().unwrap().clone();
        assert_eq!(in_memory.provider, "openrouter");
        assert_eq!(in_memory.thinking_level, "xhigh");

        // Persisted file must also reflect both mutations (not just whichever save happened last).
        let persisted = config::load();
        assert_eq!(persisted.provider, "openrouter");
        assert_eq!(persisted.thinking_level, "xhigh");

        // Returned JSON matches the final in-memory config.
        assert_eq!(r1.0["config"]["provider"], "openrouter");
        assert_eq!(r2.0["config"]["thinkingLevel"], "xhigh");

        match prev_dir {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        match prev_subagent {
            Some(p) => std::env::set_var("DOTZ_SUBAGENT_MODEL", p),
            None => std::env::remove_var("DOTZ_SUBAGENT_MODEL"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
    }

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
        assert!(
            text.contains("\"workflowRuns\""),
            "health body missing workflowRuns field: {text}"
        );
        assert!(
            text.contains("\"browserSessions\""),
            "health body missing browserSessions field: {text}"
        );
        assert!(
            text.contains("\"embedderReady\""),
            "health body missing embedderReady field: {text}"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server exited with error: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn get_config_recovers_from_poisoned_mutex() {
        let state = Arc::new(AppState {
            config: Mutex::new(DotzConfig::default()),
        });
        let state2 = state.clone();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = state2.config.lock().unwrap();
            panic!("intentional poison to test recovery");
        }));
        assert!(poisoned.is_err(), "mutex should be poisoned");

        let Json(resp) = get_config(State(state)).await;
        assert!(
            resp.get("config").is_some(),
            "get_config should recover from a poisoned mutex and return config"
        );
    }

    /// `serve_with_shutdown_addr` must bind a free ephemeral port, serve the REST surface, and
    /// shut down cleanly when its future resolves. This covers the new convenience wrapper that
    /// both the headless `serve` bin and any future caller (e.g. the Tauri shell) can use.
    #[tokio::test]
    async fn serve_with_shutdown_addr_binds_and_serves() {
        // Bind a dummy listener on port 0, read the assigned port, then immediately drop it so
        // `serve_with_shutdown_addr` can reuse the same port deterministically.
        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        drop(dummy);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async {
            let _: () = rx.await.unwrap_or(());
        };
        let handle = tokio::spawn(serve_with_shutdown_addr(
            addr,
            PathBuf::from("web"),
            shutdown,
        ));

        // Wait for the server to start accepting on the known port.
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

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "serve_with_shutdown_addr exited with error: {:?}",
            result.err()
        );
    }

    /// `serve_with_shutdown_addr` must drain and exit when the shutdown future is driven by a
    /// `tokio::sync::watch` channel — the exact pattern the Tauri shell uses for graceful shutdown.
    #[tokio::test]
    async fn serve_with_shutdown_addr_drain_on_watch_shutdown() {
        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        drop(dummy);

        let (tx, mut rx) = tokio::sync::watch::channel(());
        let shutdown = async move {
            let _ = rx.changed().await;
        };
        let handle = tokio::spawn(serve_with_shutdown_addr(
            addr,
            PathBuf::from("web"),
            shutdown,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "health should be reachable before shutdown"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "serve_with_shutdown_addr should exit cleanly on watch shutdown: {:?}",
            result.err()
        );
    }

    /// `serve_with_shutdown_addr` must fail fast when the requested address is already bound,
    /// surfacing the bind error to the caller instead of panicking or swallowing it.
    #[tokio::test]
    async fn serve_with_shutdown_addr_fails_when_port_in_use() {
        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        let result =
            serve_with_shutdown_addr(addr, PathBuf::from("web"), std::future::pending()).await;
        assert!(
            result.is_err(),
            "binding to an in-use port should return an error: {result:?}"
        );
    }
}
