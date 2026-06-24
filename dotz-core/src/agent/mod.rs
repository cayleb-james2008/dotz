//! dotz agent runtime — the Rust replacement for the third-party pi SDK.
//!
//! A WORKING single-agent chat turn streamed over WebSocket, emitting the EXACT event shapes the
//! unchanged `web/app.js` consumes. Ollama Cloud (OpenAI-compatible) is the primary provider and
//! works end-to-end. Other OpenAI-compatible providers (openrouter/openai/groq/mistral/xai/deepseek/
//! cohere/local) share the same adapter; anthropic + google have their own native adapters.
//!
//! Wiring: `agent::router()` returns a `Router<()>` with the `/api/sessions*` REST endpoints AND the
//! `GET /ws` upgrade. Merge it into `server::app()` like the other Phase-2 cold modules.
pub mod event;
pub mod extra_tools;
pub mod provider;
pub mod provider_anthropic;
pub mod provider_google;
pub mod session;
pub mod subagent;
pub mod tools;

use axum::{
    body::Bytes,
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::{skills, types};

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}
fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such session" })),
    )
}

// ---- REST handlers (server.ts 546-719) ----

async fn create_session(
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));

    // Validate model shape (must be {provider, modelId} when present).
    let model = match b.get("model") {
        None | Some(Value::Null) => None,
        Some(m) => {
            let prov = m
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let mid = m
                .get("modelId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if prov.is_empty() || mid.is_empty() {
                return Err(bad("model must be { provider, modelId }"));
            }
            if !types::is_known_provider(prov) {
                return Err(bad(format!(
                    "provider must be one of: {}",
                    types::provider_ids().join(", ")
                )));
            }
            Some(types::ModelRef {
                provider: prov.to_string(),
                model_id: mid.to_string(),
            })
        }
    };
    // Validate thinkingLevel.
    let thinking = match b.get("thinkingLevel") {
        None | Some(Value::Null) => None,
        Some(t) => {
            let tv = t.as_str().unwrap_or("");
            if !types::is_valid_thinking(tv) {
                return Err(bad(format!(
                    "thinkingLevel must be one of: {}",
                    types::THINKING_LEVELS.join(", ")
                )));
            }
            Some(tv.to_string())
        }
    };
    let tools = b.get("tools").and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect::<Vec<_>>()
    });

    let opts = session::CreateOpts {
        cwd: b.get("cwd").and_then(|v| v.as_str()).map(String::from),
        model,
        thinking_level: thinking,
        tools,
        profile_id: b
            .get("profileId")
            .and_then(|v| v.as_str())
            .map(String::from),
        project_id: b
            .get("projectId")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    match session::create(opts) {
        Ok(summary) => Ok(Json(summary)),
        Err(e) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "session creation failed", "detail": e })),
        )),
    }
}

async fn list_sessions() -> Json<Value> {
    Json(json!(session::list_summaries()))
}

async fn get_session(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    session::summary_with_stats(&id)
        .map(Json)
        .ok_or_else(not_found)
}

async fn delete_session(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if session::dispose(&id) {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(not_found())
    }
}

async fn get_models(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    session::models(&id).map(Json).ok_or_else(not_found)
}

async fn post_model(
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let prov = b
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mid = b
        .get("modelId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if prov.is_empty() || mid.is_empty() {
        return Err(bad("provider and modelId are required"));
    }
    if session::get(&id).is_none() {
        return Err(not_found());
    }
    // Free-form providers + OpenAI-compatible providers accept any id; anthropic/google resolve to
    // None in provider::resolve, mirroring the catalog-only restriction.
    if provider::resolve(&prov, &mid).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("model not found: {prov}/{mid}") })),
        ));
    }
    session::set_model(&id, &prov, &mid)
        .map(Json)
        .map_err(|_| not_found())
}

async fn post_thinking(
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let level = b.get("level").and_then(|v| v.as_str()).unwrap_or("");
    if !types::is_valid_thinking(level) {
        return Err(bad(format!(
            "level must be one of: {}",
            types::THINKING_LEVELS.join(", ")
        )));
    }
    session::set_thinking(&id, level)
        .map(Json)
        .map_err(|_| not_found())
}

async fn get_tools(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    session::get_tools(&id).map(Json).ok_or_else(not_found)
}

async fn post_tools(
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let tools = b.get("tools").and_then(|v| v.as_array());
    let names: Vec<String> = match tools {
        Some(a) if a.iter().all(|x| x.is_string()) => {
            a.iter().map(|x| x.as_str().unwrap().to_string()).collect()
        }
        Some(_) => return Err(bad("tools must be an array of strings")),
        None => Vec::new(),
    };
    session::set_tools(&id, &names)
        .map(Json)
        .map_err(|_| not_found())
}

async fn get_commands(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if session::get(&id).is_none() {
        return Err(not_found());
    }
    Ok(Json(json!({ "commands": commands() })))
}

/// Build the composer slash-palette: bundled workflow presets (`.pi/prompts/*.md`) plus discovered
/// skills. Presets come first so workflow presets win on name collisions in the UI deduper.
pub fn commands() -> Vec<Value> {
    let mut out = prompt_commands_from_dir(&skills::pi_dir().join("prompts"));
    out.extend(skills::command_views());
    out
}

/// Load workflow-preset commands from a prompts directory. Each `*.md` file becomes a command
/// named by its filename stem, described by the `description:` frontmatter line.
fn prompt_commands_from_dir(dir: &std::path::Path) -> Vec<Value> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    let mut items: Vec<(String, String)> = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let desc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| parse_prompt_description(&raw))
            .unwrap_or_default();
        items.push((name, desc));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, desc) in items {
        out.push(json!({
            "name": name,
            "description": desc,
            "kind": "preset",
        }));
    }
    out
}

/// Extract the `description:` value from a leading `--- ... ---` frontmatter block. Mirrors the
/// simple `key: value` subset used by the bundled `.pi/prompts/*.md` files.
fn parse_prompt_description(raw: &str) -> Option<String> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let after_open = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let close_idx = after_open.find("\n---")?;
    let fm = &after_open[..close_idx];
    for line in fm.lines() {
        let mut parts = line.splitn(2, ':');
        let key = parts.next()?.trim();
        if key == "description" {
            let val = parts.next()?.trim();
            let val = val.trim_matches('"').trim_matches('\'').to_string();
            return Some(val);
        }
    }
    None
}

async fn post_abort(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if session::abort(&id) {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(not_found())
    }
}

/// reload-context: rebuild the session in place (new system prompt, cleared history) keeping the
/// same session id so existing WebSocket subscribers stay connected.
async fn post_reload(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match session::reload(&id) {
        Ok(summary) => Ok(Json(summary)),
        Err(e) if e.contains("busy") => Err((StatusCode::CONFLICT, Json(json!({ "error": e })))),
        Err(e) => Err((StatusCode::NOT_FOUND, Json(json!({ "error": e })))),
    }
}

// ---- WebSocket: GET /ws?sessionId=... ----

fn require_ws_session(q: &HashMap<String, String>) -> Result<String, (StatusCode, Json<Value>)> {
    let session_id = q.get("sessionId").cloned().unwrap_or_default();
    if session_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sessionId is required" })),
        ));
    }
    if session::get(&session_id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such session" })),
        ));
    }
    Ok(session_id)
}

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<HashMap<String, String>>) -> Response {
    match require_ws_session(&q) {
        Ok(session_id) => ws.on_upgrade(move |socket| ws_loop(socket, session_id)),
        Err(resp) => resp.into_response(),
    }
}

/// Receive the next broadcast frame, treating a lagged receiver as a recoverable skip instead
/// of a connection-fatal error. A slow WebSocket client will resume from the newest message.
async fn recv_broadcast(rx: &mut broadcast::Receiver<Value>) -> Option<Value> {
    loop {
        match rx.recv().await {
            Ok(frame) => return Some(frame),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// Configurable WebSocket keep-alive interval. Browsers and most clients auto-respond to ping
/// frames with pong, which keeps idle connections alive through proxies/firewalls. Defaults to
/// 30s; override with `DOTZ_WS_PING_INTERVAL_MS` (e.g. for fast tests).
fn ws_ping_interval() -> Duration {
    std::env::var("DOTZ_WS_PING_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(30))
}

async fn ws_loop(socket: WebSocket, session_id: String) {
    let (mut sink, mut stream) = socket.split();

    // Session existence was validated before the HTTP upgrade, but a concurrent dispose can race
    // and remove the session before we subscribe. Close the socket gracefully instead of panicking.
    let mut rx = match session::subscribe(&session_id) {
        Some(rx) => rx,
        None => {
            let _ = sink
                .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
                    code: axum::extract::ws::close_code::NORMAL,
                    reason: "session disposed before websocket subscription".into(),
                })))
                .await;
            return;
        }
    };

    // ready frame.
    let _ = sink
        .send(WsMessage::Text(
            json!({ "kind": "ready", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await;

    // Shared cancellation so a disposed session closes the WebSocket promptly from the server
    // side, instead of leaving the read half blocked until the client sends something.
    let done = tokio_util::sync::CancellationToken::new();
    let done2 = done.clone();

    // Fan agent events (broadcast) → socket, plus periodic keep-alive pings. The ping keeps the
    // connection alive through proxies that drop idle sockets; browsers auto-pong in response.
    let ping_interval = ws_ping_interval();
    let fan = tokio::spawn(async move {
        let start = tokio::time::Instant::now() + ping_interval;
        let mut ping_tick = tokio::time::interval_at(start, ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = done2.cancelled() => break,
                _ = ping_tick.tick() => {
                    if sink.send(WsMessage::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                frame = recv_broadcast(&mut rx) => {
                    match frame {
                        Some(f) => {
                            if sink.send(WsMessage::Text(f.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        // Broadcast closed means the session was disposed, or the socket send failed. Wake the
        // reader so the socket drops promptly.
        done2.cancel();
    });

    // Read client messages (prompt/steer/followUp/abort).
    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(_)) => break,
                    None => break,
                };
                let text = match msg {
                    WsMessage::Text(t) => t.to_string(),
                    WsMessage::Close(_) => break,
                    _ => continue,
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                let body = v
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                match kind {
                    // prompt/steer/followUp all drive a turn. (steer/followUp queueing collapses to a turn
                    // here — single-agent path; the richer queueing semantics are Phase 4.)
                    "prompt" | "steer" | "followUp" => {
                        if body.trim().is_empty() {
                            continue;
                        }
                        if let Some(sess) = session::get(&session_id) {
                            let sid = session_id.clone();
                            let _ = sid;
                            tokio::spawn(async move {
                                session::run_turn(sess, body).await;
                            });
                        }
                    }
                    "abort" => {
                        session::abort(&session_id);
                    }
                    // Resolve a pending human_gate (app.js sends {kind:"gate.approve"|"gate.reject", gateId, feedback?}).
                    "gate.approve" | "gate.reject" => {
                        if let Some(gid) = v.get("gateId").and_then(|g| g.as_str()) {
                            let fb = v
                                .get("feedback")
                                .and_then(|f| f.as_str())
                                .map(|s| s.to_string());
                            extra_tools::resolve_gate(gid, kind == "gate.approve", fb);
                        }
                    }
                    _ => {}
                }
            }
            _ = done.cancelled() => break,
        }
    }

    fan.abort();
}

/// The sessions Router<()> + /ws handler, to merge into server::app().
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/models", get(get_models))
        .route("/api/sessions/{id}/model", post(post_model))
        .route("/api/sessions/{id}/thinking", post(post_thinking))
        .route("/api/sessions/{id}/tools", get(get_tools).post(post_tools))
        .route("/api/sessions/{id}/commands", get(get_commands))
        .route("/api/sessions/{id}/abort", post(post_abort))
        .route("/api/sessions/{id}/reload-context", post(post_reload))
        .route("/ws", get(ws_handler))
}

/// Live session count, for the /api/health merge.
pub fn session_count() -> usize {
    session::count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize WebSocket integration tests that mutate the process-global ping-interval env var
    /// so they do not interfere with each other when run concurrently.
    static WS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn recv_broadcast_returns_none_when_closed() {
        let (tx, mut rx) = broadcast::channel::<Value>(2);
        drop(tx);
        assert!(recv_broadcast(&mut rx).await.is_none());
    }

    #[tokio::test]
    async fn recv_broadcast_resumes_after_lag() {
        let (tx, mut rx) = broadcast::channel::<Value>(2);
        // Overflow the buffer so the receiver lags.
        for i in 0..5 {
            let _ = tx.send(json!({ "n": i }));
        }
        // The fan must stay alive across the lag error.
        let first = recv_broadcast(&mut rx).await;
        assert!(
            first.is_some(),
            "recv_broadcast should resume after lag, not close the connection"
        );

        // Drain whatever buffered tail remains so the receiver is caught up.
        let mut seen_last = false;
        while let Some(v) = recv_broadcast(&mut rx).await {
            if v.get("n").and_then(|n| n.as_i64()) == Some(4) {
                seen_last = true;
                break;
            }
        }
        assert!(seen_last, "receiver should catch up to the buffered tail");

        // After recovery, new messages are delivered normally.
        let _ = tx.send(json!({ "n": 99 }));
        let next = recv_broadcast(&mut rx).await;
        assert_eq!(
            next.and_then(|v| v.get("n").and_then(|n| n.as_i64())),
            Some(99)
        );
    }

    #[test]
    fn ws_validation_rejects_missing_session_id() {
        let err = require_ws_session(&HashMap::new()).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!((err.1).0["error"], "sessionId is required");
    }

    #[test]
    fn ws_validation_rejects_unknown_session() {
        let mut q = HashMap::new();
        q.insert("sessionId".to_string(), "no-such-id".to_string());
        let err = require_ws_session(&q).unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!((err.1).0["error"], "no such session");
    }

    #[test]
    fn ws_validation_accepts_valid_session() {
        let summary = session::create(session::CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        let mut q = HashMap::new();
        q.insert("sessionId".to_string(), sid.clone());
        assert_eq!(require_ws_session(&q).unwrap(), sid);
        session::dispose(&sid);
    }

    #[test]
    fn subscribe_is_none_after_session_disposed() {
        let summary = session::create(session::CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        assert!(
            session::subscribe(&sid).is_some(),
            "active session must be subscribable"
        );
        assert!(session::dispose(&sid), "dispose should succeed");
        assert!(
            session::subscribe(&sid).is_none(),
            "disposed session must not be subscribable"
        );
    }

    #[test]
    fn prompt_commands_from_dir_reads_frontmatter_description() {
        let dir = std::env::temp_dir().join(format!("dotz-prompts-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("scout-and-plan.md"),
            "---\ndescription: Scout maps, planner plans\n---\nDo stuff.\n",
        )
        .unwrap();
        // Bare file (no frontmatter) yields empty description but still appears.
        std::fs::write(dir.join("plain.md"), "# Plain\n").unwrap();
        // Non-markdown file is ignored.
        std::fs::write(dir.join("ignored.txt"), "---\ndescription: Bad\n---\n").unwrap();

        let cmds = prompt_commands_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        let by_name: HashMap<String, String> = cmds
            .iter()
            .filter_map(|v| {
                Some((
                    v.get("name")?.as_str()?.to_string(),
                    v.get("description")?.as_str()?.to_string(),
                ))
            })
            .collect();
        assert!(by_name.contains_key("plain"), "bare markdown should appear");
        assert!(
            !by_name.contains_key("ignored"),
            "non-markdown should be ignored"
        );
        assert_eq!(
            by_name.get("scout-and-plan").cloned().unwrap_or_default(),
            "Scout maps, planner plans"
        );
    }

    #[test]
    fn commands_includes_bundled_workflow_presets() {
        let cmds = commands();
        let names: std::collections::HashSet<String> = cmds
            .iter()
            .filter_map(|v| v.get("name")?.as_str().map(String::from))
            .collect();
        for preset in [
            "scout-and-plan",
            "implement",
            "implement-and-review",
            "self-improve",
        ] {
            assert!(
                names.contains(preset),
                "commands() should include bundled preset /{preset}"
            );
        }
        // Every entry must have the expected shape.
        for v in &cmds {
            assert!(v.get("name").and_then(|x| x.as_str()).is_some());
            assert!(v.get("description").is_some());
            assert!(v.get("kind").and_then(|x| x.as_str()).is_some());
        }
    }

    #[tokio::test]
    async fn create_session_rejects_unknown_provider() {
        let body = Json(json!({
            "model": { "provider": "not-a-provider", "modelId": "anything" }
        }));
        let err = create_session(Some(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1 .0["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("provider must be one of"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("ollama"),
            "provider list should include ollama: {msg}"
        );
    }

    #[tokio::test]
    async fn create_session_accepts_known_provider() {
        let body = Json(json!({
            "model": { "provider": "ollama", "modelId": "glm-5.2" }
        }));
        let resp = create_session(Some(body)).await.unwrap();
        let sid = resp.0["sessionId"].as_str().unwrap().to_string();
        assert!(!sid.is_empty(), "create_session should return a session id");
        session::dispose(&sid);
    }

    /// A connected WebSocket must close promptly when its session is disposed server-side.
    /// Before the cancellation-token fix, the read half stayed blocked until the client sent
    /// something, leaving the UI connected to a dead session.
    #[tokio::test]
    async fn websocket_closes_when_session_disposed() {
        use futures_util::StreamExt;
        use std::time::Duration;
        use tokio_tungstenite::connect_async;

        let _guard = WS_TEST_LOCK.lock().await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async {
            let _: () = rx.await.unwrap_or(());
        };
        let handle = tokio::spawn(crate::server::serve_with_shutdown(
            listener,
            std::path::PathBuf::from("web"),
            shutdown,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/sessions"))
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

        let resp = client
            .delete(format!("http://127.0.0.1:{port}/api/sessions/{sid}"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let close = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        match close {
            Ok(None) | Ok(Some(Err(_))) => {}
            other => panic!("expected WS to close after session disposal, got {other:?}"),
        }

        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }

    /// A connected WebSocket should receive periodic keep-alive ping frames from the server.
    /// Browsers and compliant clients auto-respond with pong, keeping idle sessions alive through
    /// proxies and firewalls that drop silent connections.
    #[tokio::test]
    async fn websocket_sends_keepalive_pings() {
        use futures_util::StreamExt;
        use std::time::Duration;
        use tokio_tungstenite::connect_async;

        let _guard = WS_TEST_LOCK.lock().await;

        // Use a very short ping interval so the test finishes quickly.
        let prev_interval = std::env::var("DOTZ_WS_PING_INTERVAL_MS").ok();
        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "100");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async {
            let _: () = rx.await.unwrap_or(());
        };
        let handle = tokio::spawn(crate::server::serve_with_shutdown(
            listener,
            std::path::PathBuf::from("web"),
            shutdown,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/sessions"))
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

        let mut found_ping = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Ping(_)))) => {
                    found_ping = true;
                    break;
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }

        match prev_interval {
            Some(p) => std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", p),
            None => std::env::remove_var("DOTZ_WS_PING_INTERVAL_MS"),
        }

        // The server should also still be healthy after the ping exchange.
        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
        assert!(
            found_ping,
            "expected a WebSocket ping frame from the server"
        );
    }
}
