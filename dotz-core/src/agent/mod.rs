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
use tokio::sync::broadcast;

use crate::types;

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
    // Commands are sourced from skills (skill:<name>) — extensions/prompt templates are Phase 4.
    Ok(Json(json!({ "commands": [] })))
}

async fn post_abort(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if session::abort(&id) {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(not_found())
    }
}

/// reload-context: rebuild the session fresh (new system prompt) keeping project/profile/model/thinking.
async fn post_reload(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let s = match session::get(&id) {
        Some(s) => s,
        None => return Err(not_found()),
    };
    let opts = {
        let g = s.lock().unwrap();
        session::CreateOpts {
            cwd: Some(g.cwd.to_string_lossy().to_string()),
            model: Some(types::ModelRef {
                provider: g.provider.clone(),
                model_id: g.model_id.clone(),
            }),
            thinking_level: Some(g.thinking_level.clone()),
            tools: Some(g.tools.active_names()),
            profile_id: Some(g.profile_id.clone()),
            project_id: g.project_id.clone(),
        }
    };
    match session::create(opts) {
        Ok(fresh) => {
            session::dispose(&id);
            Ok(Json(fresh))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "reload failed", "detail": e })),
        )),
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

async fn ws_loop(socket: WebSocket, session_id: String) {
    let (mut sink, mut stream) = socket.split();

    // Session existence was validated before the HTTP upgrade, so subscribe cannot fail here.
    let mut rx =
        session::subscribe(&session_id).expect("session validated before WebSocket upgrade");

    // ready frame.
    let _ = sink
        .send(WsMessage::Text(
            json!({ "kind": "ready", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await;

    // Fan agent events (broadcast) → socket.
    let fan = tokio::spawn(async move {
        while let Some(frame) = recv_broadcast(&mut rx).await {
            if sink
                .send(WsMessage::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Read client messages (prompt/steer/followUp/abort).
    while let Some(Ok(msg)) = stream.next().await {
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
}
