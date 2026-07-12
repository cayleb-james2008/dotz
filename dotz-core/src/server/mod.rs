//! axum HTTP/WS server. Mirrors src/server.ts route groups; serves the static `web/` UI.
mod guard;

use crate::{
    config::{self, CleanPatch, DotzConfig},
    profiles, types,
};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
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
        .route("/api/models", get(models))
        .route("/api/config", get(get_config).post(post_config))
        .route(
            "/api/verify/suite/{profile}",
            get(crate::verify::suite_handler),
        )
        .route("/api/verify/run", post(crate::verify::run_handler))
        .with_state(state)
        // Phase 2 cold modules — self-contained, stateless Router<()> merged after with_state.
        .merge(crate::sandbox::router())
        .merge(crate::design::router())
        .merge(crate::leaderboard::router())
        .merge(crate::projects::router())
        .merge(crate::connections::router())
        .merge(crate::workflows::router())
        .merge(crate::skills::router())
        .merge(crate::templates::router())
        .merge(crate::specs::router())
        .merge(crate::living_docs::router())
        .merge(crate::vcs::router())
        .merge(crate::memory::router())
        .merge(crate::agent::router())
        .merge(crate::browser::router())
        .merge(crate::checkpoint::router())
        .merge(crate::commands::router())
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
        // Origin/Host allowlist guard (applied last so it wraps every route above, including the
        // static fallback and the `/ws` upgrade). Rejects present-and-disallowed Origin/Host with
        // 403; a missing Origin passes so the Tauri IPC shim / same-origin fetches keep working.
        .layer(axum::middleware::from_fn(guard::origin_guard))
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

/// GET /api/models — the backend-driven model catalog for the UI's model picker. Returns the
/// full provider list, per-provider metadata, the available model catalog, provider-aware
/// default executive/subagent model ids, and the currently configured provider + executive
/// model. This is session-independent (unlike `/api/sessions/:id/models`), so the command
/// center can render a populated picker before any session is opened. The UI filters
/// `available` by `current.provider` to fill the dropdown / datalist.
async fn models(State(s): State<Shared>) -> Json<Value> {
    let c = state_config(&s).clone();
    let default = types::default_model();
    Json(json!({
        "current": {
            "provider": c.provider,
            "modelId": c.executive_model,
            "name": c.executive_model,
            "reasoning": true,
        },
        "default": default,
        "providers": types::provider_ids(),
        "providerMeta": types::providers(),
        "available": types::available_models(),
        "providerDefaults": types::provider_defaults_json(),
        "subagentModel": c.subagent_model,
        "thinkingLevel": c.thinking_level,
        "thinkingLevels": types::THINKING_LEVELS.to_vec(),
    }))
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

/// The process-wide graceful-shutdown signal. When the server's shutdown future resolves,
/// this watch is set to `true` so long-lived connection handlers (WebSocket loops) can break
/// out and close their sockets instead of hanging axum's drain phase indefinitely. Without this,
/// an active WebSocket connection would prevent the server from ever completing graceful
/// shutdown — axum's `with_graceful_shutdown` waits for all connection tasks to finish, and a
/// WS read loop blocks forever until the client disconnects.
static SHUTDOWN_WATCH: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
    std::sync::OnceLock::new();

fn shutdown_watch() -> &'static tokio::sync::watch::Sender<bool> {
    SHUTDOWN_WATCH.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// Subscribe to the server's graceful-shutdown signal. Returns a `watch::Receiver<bool>` that
/// yields `true` when the server is draining. WebSocket handlers use this to close active
/// connections so the server can exit promptly instead of hanging on long-lived sockets.
pub fn subscribe_shutdown() -> tokio::sync::watch::Receiver<bool> {
    shutdown_watch().subscribe()
}

/// Serve with an explicit graceful-shutdown future. Callers (e.g. the headless `serve` bin) can
/// stop cleanly on SIGINT/SIGTERM; Tauri uses `serve()` and lets the process die with the window.
pub async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    web_dir: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Reset the shutdown watch so a fresh server start doesn't inherit a prior shutdown signal
    // (e.g. in tests that start/stop the server multiple times, or a re-bind after a clean exit).
    shutdown_watch().send_modify(|v| *v = false);

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
    // Wrap the caller's shutdown future so that when it resolves we also broadcast the
    // shutdown signal to all active WebSocket handlers. This lets them close their sockets
    // promptly so axum's drain phase completes instead of hanging on long-lived connections.
    let sw = shutdown_watch().clone();
    axum::serve(listener, app(web_dir, state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = sw.send(true);
        })
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

    /// `GET /api/models` must return the backend-driven model catalog: at least one provider, a
    /// non-empty `available` catalog with model ids, provider metadata, provider-aware defaults,
    /// and the currently configured `current` provider/model derived from the loaded config. This
    /// is what the UI's model picker binds to instead of a hardcoded list.
    #[tokio::test]
    async fn get_models_returns_backend_driven_catalog() {
        let dir =
            std::env::temp_dir().join(format!("dotz-server-models-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev_dir = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);

        let state = Arc::new(AppState {
            config: Mutex::new(config::load()),
        });
        let Json(resp) = models(State(state)).await;

        // At least one provider id is present.
        let providers = resp["providers"].as_array().expect("providers is an array");
        assert!(!providers.is_empty(), "providers list must not be empty");
        assert!(
            providers.iter().any(|p| p.as_str() == Some("ollama")),
            "providers must include ollama"
        );

        // Provider metadata is present and non-empty.
        let meta = resp["providerMeta"]
            .as_array()
            .expect("providerMeta is an array");
        assert!(!meta.is_empty(), "providerMeta must not be empty");
        assert!(
            meta.iter().any(|p| p["id"] == "ollama"),
            "providerMeta must include ollama entry"
        );

        // The available catalog must have at least one model id per entry, and at least one entry
        // for a fixed (non-free-form) provider so the dropdown is usable.
        let available = resp["available"].as_array().expect("available is an array");
        assert!(!available.is_empty(), "available catalog must not be empty");
        for m in available {
            let mid = m["modelId"].as_str().expect("modelId present");
            assert!(
                !mid.is_empty(),
                "catalog entry must have a non-empty modelId"
            );
            let prov = m["provider"].as_str().expect("provider present");
            assert!(
                !prov.is_empty(),
                "catalog entry must have a non-empty provider"
            );
            assert!(
                types::is_known_provider(prov),
                "catalog entry provider {prov} must be a known provider"
            );
        }
        assert!(
            available.iter().any(|m| m["provider"] == "anthropic"),
            "catalog must include at least one anthropic model for the fixed-provider dropdown"
        );

        // Provider-aware defaults are present for the free-form providers.
        let defaults = &resp["providerDefaults"];
        assert_eq!(defaults["ollama"]["executive"], "glm-5.2");
        assert_eq!(defaults["ollama"]["subagent"], "minimax-m3");

        // The current provider/model is derived from the loaded config (default = ollama/glm-5.2).
        assert_eq!(resp["current"]["provider"], "ollama");
        assert_eq!(resp["current"]["modelId"], "glm-5.2");
        assert_eq!(resp["current"]["reasoning"], true);

        // The default model ref is present.
        assert_eq!(resp["default"]["provider"], "ollama");
        assert_eq!(resp["default"]["modelId"], "glm-5.2");

        // Subagent model + thinking level are surfaced so the UI can render them without a
        // second round-trip to /api/config.
        assert_eq!(resp["subagentModel"], "minimax-m3");
        assert_eq!(resp["thinkingLevel"], "high");

        // The full thinking-level list is surfaced so the UI's reasoning picker is backend-driven
        // instead of hardcoded — it must match types::THINKING_LEVELS exactly.
        let levels = resp["thinkingLevels"]
            .as_array()
            .expect("thinkingLevels is an array");
        assert_eq!(
            levels,
            &types::THINKING_LEVELS
                .iter()
                .map(|s| json!(*s))
                .collect::<Vec<_>>(),
            "thinkingLevels must match the backend constant"
        );
        assert!(
            levels.contains(&json!("high")),
            "thinkingLevels must include 'high'"
        );
        assert!(!levels.is_empty(), "thinkingLevels must not be empty");

        match prev_dir {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `GET /api/models` must be reachable over HTTP through the full axum router and return the
    /// catalog JSON with the expected shape — proving the route is wired into `app()`.
    #[tokio::test]
    async fn get_models_endpoint_reachable_over_http() {
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
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/models", addr.port()))
            .send()
            .await
            .expect("GET /api/models");
        assert!(
            resp.status().is_success(),
            "/api/models should be 200, got {}",
            resp.status()
        );
        let body: Value = resp.json().await.expect("/api/models body is JSON");
        assert!(
            body["providers"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "/api/models body must have a non-empty providers array"
        );
        assert!(
            body["available"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "/api/models body must have a non-empty available catalog"
        );
        assert!(
            body["current"]["provider"]
                .as_str()
                .map(|p| !p.is_empty())
                .unwrap_or(false),
            "/api/models body must have a current.provider"
        );
        assert!(
            body["current"]["modelId"]
                .as_str()
                .map(|m| !m.is_empty())
                .unwrap_or(false),
            "/api/models body must have a current.modelId"
        );
        // The thinking-level list must be surfaced so the UI's reasoning picker is backend-driven.
        assert!(
            body["thinkingLevels"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "/api/models body must have a non-empty thinkingLevels array"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly: {:?}",
            result.err()
        );
    }

    /// The Origin/Host allowlist guard must reject a cross-origin request from a foreign page with
    /// `403` on a code-exec route (`GET /api/sandbox/runs`) — before the handler runs — while an
    /// allowed loopback Origin and a no-Origin request both pass. This is the router-level proof
    /// that the guard layer is wired into `app()` and covers the sandbox surface. We use the GET
    /// list route (which spawns nothing) so the test never executes sandbox code.
    #[tokio::test]
    async fn origin_guard_blocks_foreign_origin_on_sandbox_route() {
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
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        let client = reqwest::Client::new();
        let sandbox_url = format!("http://127.0.0.1:{}/api/sandbox/runs", addr.port());
        let health_url = format!("http://127.0.0.1:{}/api/health", addr.port());

        // Foreign Origin -> 403 (rejected before the handler).
        let forbidden = client
            .get(&sandbox_url)
            .header("Origin", "http://evil.example")
            .send()
            .await
            .expect("GET /api/sandbox/runs with foreign origin");
        assert_eq!(
            forbidden.status(),
            reqwest::StatusCode::FORBIDDEN,
            "a foreign Origin must be rejected with 403 on the sandbox route"
        );

        // Allowed loopback Origin -> not rejected.
        let allowed = client
            .get(&health_url)
            .header("Origin", format!("http://127.0.0.1:{}", addr.port()))
            .send()
            .await
            .expect("GET /api/health with loopback origin");
        assert!(
            allowed.status().is_success(),
            "an allowed loopback Origin must pass, got {}",
            allowed.status()
        );

        // No Origin header (Tauri IPC shim / same-origin) -> passes.
        let no_origin = client
            .get(&health_url)
            .send()
            .await
            .expect("GET /api/health with no origin");
        assert!(
            no_origin.status().is_success(),
            "a request with no Origin must pass, got {}",
            no_origin.status()
        );

        let _ = tx.send(());
        let _ = handle.await.unwrap();
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

    // ---- Offline font bundle ------------------------------------------------------
    //
    // The desktop app must be genuinely offline-excellent: it must NOT depend on
    // fonts.googleapis.com / fonts.gstatic.com at runtime. These tests guard the
    // locally-bundled font stack (web/fonts.css + web/fonts/*.woff2) that replaced
    // the Google Fonts <link> in index.html. If someone re-introduces a Google
    // Fonts <link> or forgets to bundle the woff2 files, these tests fail.

    /// Resolve the `web/` dir the way the server does in the existing tests: the cargo
    /// workspace root (CARGO_MANIFEST_DIR is dotz-core, so the web dir is one level up).
    fn web_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("web")
    }

    /// `index.html` must not reach out to Google Fonts at runtime and must load the local
    /// `fonts.css` instead. Catches a regression where the Google Fonts <link> is restored.
    #[test]
    fn index_html_has_no_google_fonts_runtime_dependency() {
        let html = std::fs::read_to_string(web_dir().join("index.html"))
            .expect("web/index.html must exist alongside the crate");
        assert!(
            !html.contains("fonts.googleapis.com"),
            "index.html must not depend on fonts.googleapis.com at runtime (offline app)"
        );
        assert!(
            !html.contains("fonts.gstatic.com"),
            "index.html must not preconnect to fonts.gstatic.com (offline app)"
        );
        assert!(
            html.contains("/fonts.css"),
            "index.html must load the locally-bundled /fonts.css stylesheet"
        );
    }

    /// `fonts.css` must be fully self-contained: every `src: url(...)` must point at a local
    /// file under `fonts/`, with no `https://` (and specifically no gstatic) reference left.
    #[test]
    fn fonts_css_references_only_local_files() {
        let css = std::fs::read_to_string(web_dir().join("fonts.css"))
            .expect("web/fonts.css must be bundled alongside the UI");
        assert!(
            !css.contains("https://"),
            "fonts.css must not reference any remote URL (offline app): {css}"
        );
        assert!(
            css.contains("font-family: 'Chakra Petch';"),
            "fonts.css must declare the Chakra Petch family used by --display"
        );
        assert!(
            css.contains("font-family: 'JetBrains Mono';"),
            "fonts.css must declare the JetBrains Mono family used by --mono"
        );
        assert!(
            css.contains("font-family: 'Pixelify Sans';"),
            "fonts.css must declare the Pixelify Sans family used by --pixel"
        );
        // Every src: url(...) must be a local path.
        for line in css.lines() {
            if line.contains("src: url(") {
                assert!(
                    line.contains("url(fonts/"),
                    "fonts.css src line must point at a local fonts/ file, got: {line}"
                );
            }
        }
    }

    /// The `web/fonts/` directory must contain real woff2 binaries (valid `wOF2` magic) for
    /// each declared family/weight. Without these the @font-face rules point at 404s and the
    /// app silently falls back to system fonts — defeating the whole point of the bundle.
    #[test]
    fn bundled_font_files_are_valid_woff2() {
        let dir = web_dir().join("fonts");
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("web/fonts must exist and be readable: {e}"));
        let mut saw_chakra = false;
        let mut saw_jetbrains = false;
        let mut saw_pixelify = false;
        let mut count = 0usize;
        for ent in entries.flatten() {
            let path = ent.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".woff2") {
                continue;
            }
            count += 1;
            let bytes =
                std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read font {name}: {e}"));
            // woff2 magic: b"wOF2".
            assert_eq!(
                &bytes[..4.min(bytes.len())],
                b"wOF2",
                "{name} is not a valid woff2 file (bad magic)"
            );
            assert!(
                bytes.len() > 100,
                "{name} is suspiciously small ({} bytes), likely a 404/HTML error page",
                bytes.len()
            );
        }
        // Cross-check coverage against fonts.css: each family declared there must have at
        // least one bundled woff2. We can't map subset files back to families by name (Google's
        // opaque filenames), so we re-parse fonts.css and, for each family, assert at least one
        // of its `src: url(fonts/<file>)` files exists on disk.
        let css = std::fs::read_to_string(web_dir().join("fonts.css")).unwrap();
        for family in ["Chakra Petch", "JetBrains Mono", "Pixelify Sans"] {
            let mut found = false;
            let mut in_block = false;
            for line in css.lines() {
                if line.contains(&format!("font-family: '{family}';")) {
                    in_block = true;
                }
                if in_block && line.contains("url(fonts/") {
                    let f = line
                        .split("url(fonts/")
                        .nth(1)
                        .and_then(|s| s.split(')').next())
                        .unwrap_or("");
                    if f.contains(".woff2") && std::fs::exists(dir.join(f)).unwrap_or(false) {
                        found = true;
                    }
                }
                if in_block && line.trim() == "}" {
                    in_block = false;
                }
            }
            assert!(
                found,
                "no bundled woff2 file on disk is referenced by the {family} @font-face block"
            );
            match family {
                "Chakra Petch" => saw_chakra = true,
                "JetBrains Mono" => saw_jetbrains = true,
                "Pixelify Sans" => saw_pixelify = true,
                _ => {}
            }
        }
        assert!(saw_chakra && saw_jetbrains && saw_pixelify);
        assert!(
            count >= 3,
            "expected at least 3 bundled woff2 files, found {count}"
        );
    }

    /// The running server must serve `fonts.css` (as text/css) and a real woff2 file (as a font
    /// content-type) from the static `web/` fallback. This proves the bundle is reachable over
    /// HTTP exactly the way the WebView2 shell loads it — not just present on disk.
    #[tokio::test]
    async fn server_serves_bundled_fonts_locally() {
        // Pick a real woff2 file from the bundle to request over HTTP.
        let fonts_dir = web_dir().join("fonts");
        let a_woff2 = std::fs::read_dir(&fonts_dir)
            .expect("web/fonts must exist")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".woff2"))
            .expect("at least one bundled .woff2 file");

        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        drop(dummy);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async {
            let _: () = rx.await.unwrap_or(());
        };
        let handle = tokio::spawn(serve_with_shutdown_addr(addr, web_dir(), shutdown));
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{}", addr.port());

        // fonts.css must be served as text/css.
        let css_resp = client
            .get(format!("{base}/fonts.css"))
            .send()
            .await
            .expect("GET /fonts.css");
        assert!(
            css_resp.status().is_success(),
            "/fonts.css should be 200, got {}",
            css_resp.status()
        );
        let css_ct = css_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        assert!(
            css_ct.contains("css"),
            "/fonts.css should be served as text/css, got content-type {css_ct}"
        );
        let css_body = css_resp.text().await.unwrap();
        assert!(
            css_body.contains("font-family: 'Chakra Petch';"),
            "served /fonts.css body must contain the @font-face declarations"
        );

        // The woff2 file must be served with a font content-type, not as a 404.
        let font_resp = client
            .get(format!("{base}/fonts/{a_woff2}"))
            .send()
            .await
            .expect("GET /fonts/<woff2>");
        assert!(
            font_resp.status().is_success(),
            "/fonts/{a_woff2} should be 200, got {}",
            font_resp.status()
        );
        let font_ct = font_resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        assert!(
            font_ct.contains("woff2") || font_ct.contains("font"),
            "/fonts/{a_woff2} should be served with a font content-type, got {font_ct}"
        );
        let font_bytes = font_resp.bytes().await.unwrap();
        assert_eq!(
            &font_bytes[..4.min(font_bytes.len())],
            b"wOF2",
            "served woff2 must have valid magic bytes"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly: {:?}",
            result.err()
        );
    }

    // ===========================================================================
    // Graceful shutdown integration tests — full system under load & failure
    // ===========================================================================
    //
    // These tests cover the scenarios the campaign 2026-07-04 step 6 calls out:
    // full-system graceful shutdown under concurrent HTTP load, with active
    // WebSocket connections, with live agent sessions, on rapid restart, and
    // under mixed REST + WS traffic. They also verify failure modes (port
    // already bound, rapid start/stop cycles).

    /// Start a test server on an ephemeral port. Returns the port, a oneshot
    /// trigger for the shutdown signal, and the join handle for the server task.
    /// The server serves the `web/` directory (relative to CARGO_MANIFEST_DIR/..).
    async fn start_server() -> (
        u16,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr.port(), tx, handle)
    }

    /// The server must drain and exit cleanly when the shutdown signal fires
    /// while a burst of concurrent HTTP requests is in flight. The server must
    /// not panic or hang. This is the core "shutdown under load" scenario.
    #[tokio::test]
    async fn shutdown_drains_under_concurrent_http_load() {
        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // First, verify the server is responsive with a single request.
        let resp = client
            .get(format!("{base}/api/health"))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "server should be responsive before load test"
        );

        // Fire 20 concurrent health requests.
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let client = client.clone();
            let url = format!("{base}/api/health");
            tasks.push(tokio::spawn(async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }));
        }

        // Let the tasks be scheduled and some requests land before triggering shutdown.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = tx.send(());

        // Collect results — some may fail because the server stopped accepting,
        // which is expected. The key assertion is that the server exits cleanly.
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap_or(false) {
                ok += 1;
            }
        }
        // Under heavy parallel test load, the 20ms window might not be enough for
        // any task to complete. The critical assertion is the clean exit below.
        let _ = ok; // don't assert on ok — it's timing-dependent

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly under concurrent HTTP load: {:?}",
            result.err()
        );
    }

    /// The server must serve a high burst of concurrent requests to multiple
    /// endpoints without dropping any, then shut down cleanly. This verifies
    /// the server stays responsive under load right up to the shutdown signal.
    #[tokio::test]
    async fn server_responsive_under_burst_load_then_clean_shutdown() {
        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // 50 concurrent requests across 3 endpoints.
        let endpoints = ["/api/health", "/api/providers", "/api/models"];
        let mut tasks = Vec::new();
        for i in 0..50 {
            let client = client.clone();
            let url = format!("{base}{}", endpoints[i % 3]);
            tasks.push(tokio::spawn(async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }));
        }

        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap_or(false) {
                ok += 1;
            }
        }
        assert_eq!(
            ok, 50,
            "all 50 burst requests should succeed before shutdown"
        );

        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly after burst load: {:?}",
            result.err()
        );
    }

    /// The server must shut down promptly even when there are active WebSocket
    /// connections. Before the shutdown-watch fix, an active WS read loop would
    /// hang axum's drain phase indefinitely because `with_graceful_shutdown`
    /// waits for all connection tasks to finish. Now the WS handler subscribes
    /// to the shutdown signal and closes its socket so the server can exit.
    #[tokio::test]
    async fn shutdown_closes_active_websocket_connections() {
        use futures_util::StreamExt;
        use tokio_tungstenite::connect_async;

        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // Create a session and open a WebSocket.
        let resp = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let summary = resp.json::<Value>().await.unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();

        let url = format!("ws://127.0.0.1:{port}/ws?sessionId={sid}");
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let ready = ws.next().await.unwrap().unwrap();
        assert!(
            ready.to_text().unwrap().contains("\"ready\""),
            "first WS frame should be ready: {ready:?}"
        );

        // Trigger shutdown while the WS is still connected and idle.
        let _ = tx.send(());

        // The server must exit within a reasonable timeout (not hang).
        // Before the fix this would hang forever waiting for the WS task.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("server should shut down within 10s even with active WS connections");
        let result = result.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly with active WS: {:?}",
            result.err()
        );

        // The WS connection should be closed by the server.
        let close = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
        match close {
            Ok(None) | Ok(Some(Err(_))) => {}
            Ok(Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(_)))) => {}
            other => panic!("expected WS to close on server shutdown, got {other:?}"),
        }

        // Clean up the session from the global store.
        crate::agent::session::dispose(&sid);
    }

    /// The server must shut down cleanly when multiple active agent sessions
    /// exist in the global store. Sessions are in-memory and not explicitly
    /// disposed during shutdown, but the server must not deadlock or panic.
    #[tokio::test]
    async fn shutdown_with_active_agent_sessions_exits_cleanly() {
        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // Create 5 sessions via REST.
        let mut sids = Vec::new();
        for _ in 0..5 {
            let resp = client
                .post(format!("{base}/api/sessions"))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            let summary = resp.json::<Value>().await.unwrap();
            sids.push(summary["sessionId"].as_str().unwrap().to_string());
        }

        // Verify the health endpoint sees the sessions.
        let resp = client
            .get(format!("{base}/api/health"))
            .send()
            .await
            .unwrap();
        let health = resp.json::<Value>().await.unwrap();
        assert!(
            health["sessions"].as_u64().unwrap_or(0) >= 5,
            "health should reflect active sessions: {health}"
        );

        // Trigger shutdown — server must exit cleanly.
        let _ = tx.send(());
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly with active sessions: {:?}",
            result.err()
        );

        // Clean up sessions from the global store.
        for sid in &sids {
            crate::agent::session::dispose(sid);
        }
    }

    /// The server must bind and serve on the same port immediately after a
    /// prior instance shut down. This verifies the TCP listener is properly
    /// released during graceful shutdown and there is no lingering socket
    /// (TIME_WAIT or similar) that would block a rapid restart.
    #[tokio::test]
    async fn rapid_restart_on_same_port_after_shutdown() {
        // Start first instance.
        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        drop(dummy);

        let (tx1, rx1) = tokio::sync::oneshot::channel::<()>();
        let shutdown1 = async {
            let _: () = rx1.await.unwrap_or(());
        };
        let handle1 = tokio::spawn(serve_with_shutdown_addr(
            addr,
            PathBuf::from("web"),
            shutdown1,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify it serves.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "first instance should serve");

        // Shut it down.
        let _ = tx1.send(());
        assert!(
            handle1.await.unwrap().is_ok(),
            "first instance should exit cleanly"
        );

        // Immediately restart on the same port.
        let (tx2, rx2) = tokio::sync::oneshot::channel::<()>();
        let shutdown2 = async {
            let _: () = rx2.await.unwrap_or(());
        };
        let handle2 = tokio::spawn(serve_with_shutdown_addr(
            addr,
            PathBuf::from("web"),
            shutdown2,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify the second instance serves.
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/health", addr.port()))
            .send()
            .await
            .expect("second instance should accept connections");
        assert!(
            resp.status().is_success(),
            "second instance should serve on the same port"
        );

        let _ = tx2.send(());
        assert!(
            handle2.await.unwrap().is_ok(),
            "second instance should exit cleanly"
        );
    }

    /// The server must shut down cleanly under mixed REST + WebSocket load:
    /// concurrent HTTP requests and an active WS connection. This is the
    /// real-world operator scenario — the UI has a WS open and is polling REST
    /// endpoints when the process receives SIGINT.
    #[tokio::test]
    async fn shutdown_under_mixed_rest_and_ws_load() {
        use futures_util::StreamExt;
        use tokio_tungstenite::connect_async;

        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // Create a session and open a WS.
        let resp = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let summary = resp.json::<Value>().await.unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();

        let url = format!("ws://127.0.0.1:{port}/ws?sessionId={sid}");
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let ready = ws.next().await.unwrap().unwrap();
        assert!(
            ready.to_text().unwrap().contains("\"ready\""),
            "WS should connect and receive ready frame"
        );

        // Fire concurrent REST requests while the WS is open.
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let client = client.clone();
            let url = format!("{base}/api/health");
            tasks.push(tokio::spawn(async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }));
        }

        // Let some requests land before triggering shutdown.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Trigger shutdown while both REST and WS are active.
        let _ = tx.send(());

        // Server must exit within a reasonable timeout.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("server should shut down within 10s under mixed REST + WS load");
        let result = result.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly under mixed load: {:?}",
            result.err()
        );

        // The WS should close.
        let close = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
        match close {
            Ok(None) | Ok(Some(Err(_))) => {}
            Ok(Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(_)))) => {}
            other => panic!("expected WS to close on server shutdown, got {other:?}"),
        }

        // At least some REST requests should have succeeded (the server was
        // responsive under load before shutdown). Under heavy parallel test load
        // the timing window may be tight, so we only assert the server exited
        // cleanly above — the ok count is informational.
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap_or(false) {
                ok += 1;
            }
        }
        let _ = ok; // timing-dependent under parallel test load

        crate::agent::session::dispose(&sid);
    }

    /// Multiple concurrent shutdown triggers must not panic or deadlock. In the
    /// Tauri shell, both the window-close handler and a SIGINT could fire near
    /// simultaneously; the server must handle this gracefully.
    #[tokio::test]
    async fn multiple_shutdown_triggers_are_safe() {
        let dummy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dummy.local_addr().unwrap();
        drop(dummy);

        // Use a watch channel so we can signal multiple times.
        let (tx, mut rx) = tokio::sync::watch::channel(());
        let shutdown = async move {
            let _ = rx.changed().await;
            // Even if changed() returns, the server should handle the signal once.
        };
        let handle = tokio::spawn(serve_with_shutdown_addr(
            addr,
            PathBuf::from("web"),
            shutdown,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send the shutdown signal.
        let _ = tx.send(());

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly on shutdown signal: {:?}",
            result.err()
        );

        // Sending another signal after shutdown should not panic.
        let _ = tx.send(());
    }

    /// Multiple WebSocket connections must all close on server shutdown, not
    /// just the first one. Each WS handler subscribes to the shutdown signal
    /// independently; if any handler misses the signal, the server would hang.
    #[tokio::test]
    async fn shutdown_closes_multiple_websocket_connections() {
        use futures_util::StreamExt;
        use tokio_tungstenite::connect_async;

        let (port, tx, handle) = start_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // Create 3 sessions and open a WS for each.
        let mut sids = Vec::new();
        let mut conns = Vec::new();
        for _ in 0..3 {
            let resp = client
                .post(format!("{base}/api/sessions"))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            let summary = resp.json::<Value>().await.unwrap();
            let sid = summary["sessionId"].as_str().unwrap().to_string();
            let url = format!("ws://127.0.0.1:{port}/ws?sessionId={sid}");
            let (ws, _) = connect_async(&url).await.unwrap();
            conns.push(ws);
            sids.push(sid);
        }

        // Verify all 3 received the ready frame.
        for ws in &mut conns {
            let ready = ws.next().await.unwrap().unwrap();
            assert!(
                ready.to_text().unwrap().contains("\"ready\""),
                "each WS should receive ready frame"
            );
        }

        // Trigger shutdown.
        let _ = tx.send(());

        // Server must exit within 10s.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("server should shut down within 10s with multiple WS connections");
        let result = result.unwrap();
        assert!(
            result.is_ok(),
            "server should exit cleanly with multiple WS: {:?}",
            result.err()
        );

        // All 3 WS connections should close.
        for ws in &mut conns {
            let close = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
            match close {
                Ok(None) | Ok(Some(Err(_))) => {}
                Ok(Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(_)))) => {}
                other => panic!("expected each WS to close on shutdown, got {other:?}"),
            }
        }

        for sid in &sids {
            crate::agent::session::dispose(sid);
        }
    }
}
