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

use crate::{profiles, skills, types};

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
    // Provider is normalized to lowercase so "Ollama" / "OPENROUTER" etc. are accepted.
    let model = match b.get("model") {
        None | Some(Value::Null) => None,
        Some(m) => {
            let prov = m
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let mid = m
                .get("modelId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if prov.is_empty() || mid.is_empty() {
                return Err(bad("model must be { provider, modelId }"));
            }
            if !types::is_known_provider(&prov) {
                return Err(bad(format!(
                    "provider must be one of: {}",
                    types::provider_ids().join(", ")
                )));
            }
            Some(types::ModelRef {
                provider: prov,
                model_id: mid,
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

    // Validate profileId.
    let profile_id = match b.get("profileId") {
        None | Some(Value::Null) => None,
        Some(p) => {
            let ps = p.as_str().unwrap_or("").trim();
            if !profiles::is_valid(ps) {
                return Err(bad(format!(
                    "profileId must be one of: {}",
                    profiles::summaries()
                        .iter()
                        .map(|s| s.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Some(ps.to_string())
        }
    };

    let opts = session::CreateOpts {
        cwd: b.get("cwd").and_then(|v| v.as_str()).map(String::from),
        model,
        thinking_level: thinking,
        tools,
        profile_id,
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
        .to_lowercase();
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
///
/// Clamped to [100ms, 1h]: a zero or missing value would panic `tokio::time::interval`, and a
/// sub-millisecond interval would spam pings; an enormous interval defeats the keep-alive purpose.
fn ws_ping_interval() -> Duration {
    const MIN_MS: u64 = 100;
    const MAX_MS: u64 = 3_600_000; // 1 hour
    std::env::var("DOTZ_WS_PING_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(30))
}

/// Build a `{kind:"sandbox", sessionId, event}` WS frame, matching the contract app.js consumes.
fn sandbox_event(session_id: &str, event: Value) -> Value {
    json!({ "kind": "sandbox", "sessionId": session_id, "event": event })
}

/// Best-effort broadcast of a sandbox event to the session's WebSocket subscribers.
fn emit_sandbox_event(tx: &broadcast::Sender<Value>, session_id: &str, event: Value) {
    let _ = tx.send(sandbox_event(session_id, event));
}

/// Poll a sandbox run until it reaches a terminal status, then broadcast `sandbox_end`. This gives
/// the UI the run lifecycle events it expects without requiring the executor task to know about
/// sessions or WebSockets. Bounded by a 30s deadline so a stuck run can't leak the poller.
fn spawn_sandbox_end_poller(session_id: String, run_id: String, tx: broadcast::Sender<Value>) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            if let Some(run) = crate::sandbox::lookup(&run_id) {
                if run.status != "running" {
                    emit_sandbox_event(
                        &tx,
                        &session_id,
                        json!({ "type": "sandbox_end", "runId": run_id, "run": run }),
                    );
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });
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

    // One-shot signal from the reader to the fan: "send this close frame and finish".
    // Used when the client initiates a close so the server echoes it cleanly instead of
    // dropping the socket mid-handshake.
    let (close_tx, mut close_rx) =
        tokio::sync::mpsc::unbounded_channel::<Option<axum::extract::ws::CloseFrame>>();

    // Reader → fan: respond to client Ping frames with a matching Pong. RFC 6455 requires
    // this; some proxies/load-balancers drop connections that don't answer pings.
    let (pong_tx, mut pong_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();

    // Fan agent events (broadcast) → socket, plus periodic keep-alive pings. The ping keeps the
    // connection alive through proxies that drop idle sockets; browsers auto-pong in response.
    let ping_interval = ws_ping_interval();
    let fan = tokio::spawn(async move {
        let start = tokio::time::Instant::now() + ping_interval;
        let mut ping_tick = tokio::time::interval_at(start, ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let close_frame = loop {
            tokio::select! {
                biased;
                _ = done2.cancelled() => break Some(None),
                frame = close_rx.recv() => break Some(frame.unwrap_or(None)),
                bytes = pong_rx.recv() => {
                    match bytes {
                        Some(b) => {
                            if sink.send(WsMessage::Pong(b)).await.is_err() {
                                break Some(None);
                            }
                        }
                        None => break Some(None),
                    }
                }
                _ = ping_tick.tick() => {
                    if sink.send(WsMessage::Ping(Bytes::new())).await.is_err() {
                        break Some(None);
                    }
                }
                frame = recv_broadcast(&mut rx) => {
                    match frame {
                        Some(f) => {
                            if sink.send(WsMessage::Text(f.to_string().into())).await.is_err() {
                                break Some(None);
                            }
                        }
                        None => break Some(Some(axum::extract::ws::CloseFrame {
                            code: axum::extract::ws::close_code::AWAY,
                            reason: "session disposed".into(),
                        })),
                    }
                }
            }
        };
        // Write a close frame whenever we can. A clean handshake lets proxies/CDNs and the
        // tungstenite client finish the close sequence instead of treating the connection as
        // unexpectedly dropped.
        if let Some(frame) = close_frame {
            let _ = sink.send(WsMessage::Close(frame)).await;
        }
        // Wake the reader so the socket drops promptly after the close frame is sent (or failed).
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
                    WsMessage::Ping(bytes) => {
                        // RFC 6455: respond to client pings with a matching pong so the
                        // connection stays alive through strict proxies/load-balancers.
                        // Hand off to the fan task, which owns the sink.
                        let _ = pong_tx.send(bytes);
                        continue;
                    }
                    WsMessage::Close(frame) => {
                        let _ = close_tx.send(frame);
                        break;
                    }
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
                            tokio::spawn(async move {
                                session::run_turn(sess, body).await;
                            });
                        }
                    }
                    "abort" => {
                        session::abort(&session_id);
                    }
                    // Sandbox controls — the UI sends these over WS instead of REST.
                    "sandbox.start" => {
                        let language = v
                            .get("language")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let code = v
                            .get("code")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let mode = v
                            .get("mode")
                            .and_then(|x| x.as_str())
                            .unwrap_or("terminal")
                            .to_string();
                        let project_id = v
                            .get("projectId")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        let timeout_ms = v
                            .get("timeoutMs")
                            .and_then(|x| x.as_i64())
                            .filter(|n| *n > 0)
                            .unwrap_or(30_000);
                        if let Some(sess) = session::get(&session_id) {
                            let tx = sess.lock().unwrap().tx.clone();
                            let sid = session_id.clone();
                            tokio::spawn(async move {
                                match crate::sandbox::start_run(
                                    &language, &code, &mode, project_id.as_deref(), timeout_ms,
                                )
                                .await
                                {
                                    Ok(run) => {
                                        let run_id = run.id.clone();
                                        emit_sandbox_event(
                                            &tx,
                                            &sid,
                                            json!({
                                                "type": "sandbox_start",
                                                "runId": run_id,
                                                "run": run,
                                            }),
                                        );
                                        spawn_sandbox_end_poller(sid, run_id, tx);
                                    }
                                    Err(e) => {
                                        emit_sandbox_event(
                                            &tx,
                                            &sid,
                                            json!({
                                                "type": "sandbox_end",
                                                "runId": Value::Null,
                                                "error": e,
                                            }),
                                        );
                                    }
                                }
                            });
                        }
                    }
                    "sandbox.kill" => {
                        if let Some(run_id) = v.get("runId").and_then(|x| x.as_str()) {
                            let killed = crate::sandbox::kill_run_by_id(run_id);
                            if let Some(sess) = session::get(&session_id) {
                                let tx = sess.lock().unwrap().tx.clone();
                                let sid = session_id.clone();
                                let rid = run_id.to_string();
                                tokio::spawn(async move {
                                    // Give the kill a moment to update status, then emit end.
                                    if killed {
                                        tokio::time::sleep(std::time::Duration::from_millis(200))
                                            .await;
                                    }
                                    let run = crate::sandbox::lookup(&rid);
                                    emit_sandbox_event(
                                        &tx,
                                        &sid,
                                        json!({
                                            "type": "sandbox_end",
                                            "runId": rid,
                                            "run": run,
                                        }),
                                    );
                                });
                            }
                        }
                    }
                    "sandbox.cursor" => {
                        let run_id = v.get("runId").and_then(|x| x.as_str()).unwrap_or("");
                        let x = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        let y = v.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        let action = v
                            .get("action")
                            .and_then(|x| x.as_str())
                            .unwrap_or("move")
                            .to_string();
                        let cursor_text = v
                            .get("cursorText")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        if !run_id.is_empty()
                            && ["move", "click", "type"].contains(&action.as_str())
                        {
                            if let Some(sess) = session::get(&session_id) {
                                let tx = sess.lock().unwrap().tx.clone();
                                emit_sandbox_event(
                                    &tx,
                                    &session_id,
                                    json!({
                                        "type": "sandbox_cursor",
                                        "runId": run_id,
                                        "x": x,
                                        "y": y,
                                        "action": action,
                                        "text": cursor_text,
                                    }),
                                );
                            }
                        }
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

    // Give the fan a bounded window to emit the close frame before the socket is dropped.
    // In the session-disposed path the fan already initiated the close; in the client-close path
    // we just asked it to via close_tx. A hard abort only happens if the close frame can't drain.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fan).await;

    // Drain any remaining inbound frames (e.g., the client's close-ack) for a short window so
    // tungstenite can complete the close handshake instead of seeing a TCP reset.
    let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while tokio::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(100), stream.next()).await {
            Ok(Some(Ok(WsMessage::Close(_)))) => break,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => continue,
        }
    }
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

    /// Provider strings are case-insensitive at session creation: a mixed-case provider like
    /// "Ollama" is accepted and stored as the canonical lowercase id.
    #[tokio::test]
    async fn create_session_accepts_uppercase_provider() {
        let body = Json(json!({
            "model": { "provider": "Ollama", "modelId": "glm-5.2" }
        }));
        let resp = create_session(Some(body)).await.unwrap();
        let sid = resp.0["sessionId"].as_str().unwrap().to_string();
        assert_eq!(resp.0["model"]["provider"], "ollama");
        assert_eq!(resp.0["model"]["modelId"], "glm-5.2");
        session::dispose(&sid);
    }

    /// A model id with a redundant provider prefix passed at session creation must be normalized
    /// to a bare id, matching the global config behavior.
    #[tokio::test]
    async fn create_session_normalizes_redundant_provider_prefix() {
        let body = Json(json!({
            "model": { "provider": "ollama", "modelId": "ollama/glm-5.2" }
        }));
        let resp = create_session(Some(body)).await.unwrap();
        let sid = resp.0["sessionId"].as_str().unwrap().to_string();
        assert_eq!(resp.0["model"]["provider"], "ollama");
        assert_eq!(resp.0["model"]["modelId"], "glm-5.2");
        session::dispose(&sid);
    }

    /// POST /api/sessions/:id/model must normalize a redundant provider prefix so the upstream
    /// API receives the bare model id.
    #[tokio::test]
    async fn post_model_normalizes_redundant_provider_prefix() {
        let create_resp = create_session(Some(Json(json!({
            "model": { "provider": "ollama", "modelId": "glm-5.2" }
        }))))
        .await
        .unwrap();
        let sid = create_resp.0["sessionId"].as_str().unwrap().to_string();

        let body = Json(json!({
            "provider": "ollama",
            "modelId": "ollama/glm-5.2"
        }));
        let resp = post_model(axum::extract::Path(sid.clone()), Some(body))
            .await
            .unwrap();
        assert_eq!(resp.0["model"]["provider"], "ollama");
        assert_eq!(resp.0["model"]["modelId"], "glm-5.2");

        session::dispose(&sid);
    }

    /// Provider strings are case-insensitive in the model-update path: "Ollama" is accepted and
    /// normalized to "ollama" so the UI/provider resolution does not fail on mixed-case input.
    #[tokio::test]
    async fn post_model_accepts_uppercase_provider() {
        let create_resp = create_session(Some(Json(json!({}))))
            .await
            .unwrap();
        let sid = create_resp.0["sessionId"].as_str().unwrap().to_string();

        let body = Json(json!({
            "provider": "Ollama",
            "modelId": "glm-5.2"
        }));
        let resp = post_model(axum::extract::Path(sid.clone()), Some(body))
            .await
            .unwrap();
        assert_eq!(resp.0["model"]["provider"], "ollama");
        assert_eq!(resp.0["model"]["modelId"], "glm-5.2");

        session::dispose(&sid);
    }

    #[tokio::test]
    async fn create_session_rejects_unknown_profile() {
        let body = Json(json!({ "profileId": "not-a-profile" }));
        let err = create_session(Some(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1 .0["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("profileId must be one of"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("workflow"),
            "profile list should include workflow: {msg}"
        );
    }

    #[tokio::test]
    async fn create_session_accepts_known_profile() {
        let body = Json(json!({ "profileId": "solo" }));
        let resp = create_session(Some(body)).await.unwrap();
        let sid = resp.0["sessionId"].as_str().unwrap().to_string();
        assert_eq!(resp.0["profileId"], "solo");
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
        use tokio_tungstenite::tungstenite::protocol::Message;

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
            Ok(Some(Ok(Message::Close(_)))) => {
                // Server initiated a clean close handshake after disposal; that's the new behavior.
            }
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

    /// A client Ping must be answered with a matching Pong (RFC 6455). Without this, strict
    /// proxies/load-balancers may terminate the connection, and custom clients that send their
    /// own keep-alive pings never get a response.
    #[tokio::test]
    async fn websocket_responds_to_client_ping_with_matching_pong() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message;

        let _guard = WS_TEST_LOCK.lock().await;

        // Disable server-side pings so the only frame after the ready is our pong echo.
        let prev_interval = std::env::var("DOTZ_WS_PING_INTERVAL_MS").ok();
        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "3600000");

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

        // Send a ping with recognizable payload; the server must echo it back in a pong.
        let ping_payload = Bytes::from_static(b"dotz-ping");
        ws.send(Message::Ping(ping_payload.clone())).await.unwrap();

        let response = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for pong")
            .expect("websocket stream ended without pong")
            .expect("websocket error while waiting for pong");
        match response {
            Message::Pong(payload) => assert_eq!(
                payload, ping_payload,
                "server pong must mirror the client ping payload"
            ),
            other => panic!("expected Pong in response to Ping, got: {other:?}"),
        }

        match prev_interval {
            Some(p) => std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", p),
            None => std::env::remove_var("DOTZ_WS_PING_INTERVAL_MS"),
        }

        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }

    /// When the client initiates the WebSocket close handshake, the server must echo a Close
    /// frame instead of just dropping the TCP connection. An unclean close causes proxies and
    /// the tungstenite client to treat the session as unexpectedly terminated.
    #[tokio::test]
    async fn websocket_echoes_close_frame_when_client_closes() {
        use futures_util::StreamExt;
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message;

        let _guard = WS_TEST_LOCK.lock().await;

        // Disable pings so the only frame we see after the ready is the close echo.
        let prev_interval = std::env::var("DOTZ_WS_PING_INTERVAL_MS").ok();
        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "3600000");

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

        // Initiate the close handshake from the client.
        ws.send(Message::Close(None)).await.unwrap();

        // The server must respond with a Close frame to complete the handshake cleanly.
        let server_close = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for server close frame")
            .expect("websocket stream ended without server close frame")
            .expect("websocket error while waiting for close frame");
        assert!(
            matches!(server_close, Message::Close(_)),
            "server should echo a Close frame, got: {server_close:?}"
        );

        match prev_interval {
            Some(p) => std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", p),
            None => std::env::remove_var("DOTZ_WS_PING_INTERVAL_MS"),
        }

        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }

    /// `ws_ping_interval` must reject values that would break tokio (zero) or defeat keep-alive
    /// (extreme values), falling back to the default when the env var is missing/invalid.
    #[tokio::test]
    async fn ws_ping_interval_clamps_invalid_values_and_honors_valid_override() {
        let _guard = WS_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_WS_PING_INTERVAL_MS").ok();

        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "0");
        assert_eq!(
            ws_ping_interval().as_millis(),
            100,
            "zero must clamp to min"
        );

        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "50");
        assert_eq!(
            ws_ping_interval().as_millis(),
            100,
            "below-minimum must clamp to min"
        );

        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "250");
        assert_eq!(ws_ping_interval().as_millis(), 250, "valid value preserved");

        std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", "10000000");
        assert_eq!(
            ws_ping_interval().as_millis(),
            3_600_000,
            "above-maximum must clamp to max"
        );

        std::env::remove_var("DOTZ_WS_PING_INTERVAL_MS");
        assert_eq!(
            ws_ping_interval().as_secs(),
            30,
            "missing env var falls back to 30s"
        );

        match prev {
            Some(p) => std::env::set_var("DOTZ_WS_PING_INTERVAL_MS", p),
            None => std::env::remove_var("DOTZ_WS_PING_INTERVAL_MS"),
        }
    }

    /// A `sandbox.start` WebSocket message must create a sandbox run and emit `sandbox_start`
    /// followed by `sandbox_end` when the run finishes. Before this wiring the UI's sandbox start
    /// button sent the message into a void and never showed the run.
    #[tokio::test]
    async fn websocket_routes_sandbox_start_to_run_and_emits_lifecycle_events() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message;

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

        let (language, code) = if cfg!(windows) {
            ("powershell", "echo dotz-sandbox-start-test")
        } else {
            ("bash", "echo dotz-sandbox-start-test")
        };
        ws.send(Message::Text(
            json!({
                "kind": "sandbox.start",
                "language": language,
                "code": code,
                "mode": "terminal",
                "projectId": null,
                "timeoutMs": 5000,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let mut run_id: Option<String> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("sandbox") {
                        let event = frame.get("event").cloned().unwrap_or_default();
                        let ty = event.get("type").and_then(|t| t.as_str());
                        if ty == Some("sandbox_start") {
                            run_id = event
                                .get("runId")
                                .and_then(|r| r.as_str())
                                .map(String::from);
                            assert!(
                                event.get("run").is_some(),
                                "sandbox_start must include the run record"
                            );
                        } else if ty == Some("sandbox_end") {
                            if let Some(id) = &run_id {
                                assert_eq!(
                                    event.get("runId").and_then(|r| r.as_str()),
                                    Some(id.as_str()),
                                    "sandbox_end runId must match sandbox_start"
                                );
                            }
                            break;
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }

        let rid = run_id.expect("sandbox_start event should set runId");
        let run = crate::sandbox::lookup(&rid);
        assert!(
            run.is_some(),
            "sandbox run should exist after websocket start"
        );
        assert_ne!(
            run.unwrap().status,
            "running",
            "run should reach a terminal status before sandbox_end"
        );

        // Clean up the process-global run record so later tests see a stable store.
        crate::sandbox::remove_test_run(&rid);
        session::dispose(&sid);

        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }

    /// A `sandbox.cursor` WebSocket message must be echoed back as a `sandbox_cursor` event so the
    /// agent cursor overlay renders in the web-preview panel.
    #[tokio::test]
    async fn websocket_echoes_sandbox_cursor_event() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message;

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

        ws.send(Message::Text(
            json!({
                "kind": "sandbox.cursor",
                "runId": "run-123",
                "x": 42.5,
                "y": 99.0,
                "action": "click",
                "cursorText": "submit",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let mut found = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("sandbox") {
                        let event = frame.get("event").cloned().unwrap_or_default();
                        if event.get("type").and_then(|t| t.as_str()) == Some("sandbox_cursor") {
                            assert_eq!(event["runId"], "run-123");
                            assert_eq!(event["x"], 42.5);
                            assert_eq!(event["y"], 99.0);
                            assert_eq!(event["action"], "click");
                            assert_eq!(event["text"], "submit");
                            found = true;
                            break;
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(found, "sandbox_cursor event should be echoed to the client");

        session::dispose(&sid);
        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }
}
