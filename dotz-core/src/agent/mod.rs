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
pub mod provider_health;
pub mod session;
pub mod subagent;
pub mod tools;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, Query,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::{profiles, projects, skills, types};

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

/// Extract the bare slash-command NAME from a user-typed prompt, if any.
///
/// Returns `Some("/implement")` for `"/implement the thing"`, and `None` for
/// plain prose or a token that isn't a clean slash command. Mirrors the PII-free
/// contract pinned in `telemetry::tests::test_no_pii_in_event_payload`: the
/// result is the command name only — never its arguments, never file paths —
/// so it's safe to hand to `telemetry::record_command_run`. A token with an
/// embedded slash after position 0 (e.g. `/a/b`) is rejected as a path, and
/// any arguments after the command are stripped.
fn slash_command_name(prompt: &str) -> Option<&str> {
    // `split_whitespace` already skips leading/trailing whitespace, so no `trim()`
    // is needed (clippy::trim_split_whitespace). It also yields `None` for an
    // empty or all-whitespace string, which is the desired "no command" result.
    let token = prompt.split_whitespace().next()?;
    let name = token.strip_prefix('/')?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(token)
}

#[cfg(test)]
mod slash_command_name_tests {
    use super::slash_command_name;

    #[test]
    fn extracts_bare_command_name() {
        assert_eq!(
            slash_command_name("/implement the thing"),
            Some("/implement")
        );
        assert_eq!(
            slash_command_name("/ultra-code-review"),
            Some("/ultra-code-review")
        );
    }

    #[test]
    fn ignores_plain_prose() {
        assert_eq!(slash_command_name("just a question"), None);
        assert_eq!(slash_command_name(""), None);
        assert_eq!(slash_command_name("   "), None);
    }

    #[test]
    fn rejects_path_like_and_bare_slash() {
        assert_eq!(slash_command_name("/"), None);
        assert_eq!(slash_command_name("/a/b"), None);
        assert_eq!(slash_command_name("/implement/some/path"), None);
    }

    #[test]
    fn trims_leading_whitespace() {
        assert_eq!(
            slash_command_name("   /scout-and-plan\n"),
            Some("/scout-and-plan")
        );
    }
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

    // Validate projectId: a present (non-null) id must reference an existing project so the
    // session actually inherits the project's cwd/profile/model/thinking. Without this, the UI
    // could bind a session to a deleted project and silently fall back to defaults.
    let project_id = match b.get("projectId") {
        None | Some(Value::Null) => None,
        Some(p) => {
            let pid = p.as_str().unwrap_or("").trim();
            if pid.is_empty() {
                return Err(bad("projectId must be a non-empty string"));
            }
            if projects::find(pid).is_none() {
                return Err(bad(format!("no such project: {pid}")));
            }
            Some(pid.to_string())
        }
    };

    let opts = session::CreateOpts {
        cwd: b.get("cwd").and_then(|v| v.as_str()).map(String::from),
        model,
        thinking_level: thinking,
        tools,
        profile_id,
        project_id,
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
    // Free-form providers + OpenAI-compatible providers accept any id; anthropic/google resolve
    // through their native adapters and also accept any id the upstream API validates.
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

/// GET /api/provider-health — live health snapshot + failover pairs for the conn-chip.
async fn get_provider_health() -> Json<Value> {
    Json(provider_health::health_snapshot_json().await)
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
/// simple `key: value` subset used by the bundled `.pi/prompts/*.md` files. Recognizes both LF
/// and CRLF line endings so Windows-checked-out or user-edited prompt files keep their descriptions.
fn parse_prompt_description(raw: &str) -> Option<String> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let after_open = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;

    // Closing fence: a line that is exactly `---` followed by a newline or EOF. We must validate
    // that the `---` is a complete fence line — a bare `\n---` match inside content like
    // `some-text---more-text` would otherwise be mistaken for the closing fence and truncate the
    // frontmatter. This mirrors the validation in skills::split_frontmatter.
    let close_idx = if after_open.starts_with("---\n")
        || after_open.starts_with("---\r\n")
        || after_open == "---"
    {
        0
    } else {
        // Find a `\n---` that is followed by a newline, CR, or EOF (a complete fence line).
        let mut search_from = 0usize;

        loop {
            let rel = after_open[search_from..].find("\n---")?;
            let pos = search_from + rel; // position of the `\n` before the `---`
            let after_dashes = pos + 4; // just past `\n---`
            let tail = &after_open[after_dashes..];
            if tail.is_empty() || tail.starts_with('\n') || tail.starts_with('\r') {
                break pos;
            }
            search_from = pos + 1;
        }
    };

    // Normalize CRLF in the frontmatter content so key:value parsing sees clean LF lines.
    let fm = &after_open[..close_idx];
    let fm = fm.replace("\r\n", "\n");
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

/// Receive the next WORKFLOW broadcast frame, accounting for lag. Identical to
/// [`recv_broadcast`] except a `Lagged(n)` result increments the workflow event-lag counter
/// (surfaced in `/api/health` as `eventLagCount`) and logs a warn, so a slow WebSocket client
/// dropping workflow frames is observable instead of silent. The session and provider-health
/// broadcasts use the plain [`recv_broadcast`] — only the workflow channel has a lag counter
/// today. `Lagged` is recoverable: the receiver resyncs to the newest frame and we keep
/// streaming; `Closed` ends the fan.
async fn recv_workflow_broadcast(rx: &mut broadcast::Receiver<Value>) -> Option<Value> {
    loop {
        match rx.recv().await {
            Ok(frame) => return Some(frame),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                crate::workflows::record_event_lag(n);
                continue;
            }
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

/// The grace window added to the run's timeout when computing the poller deadline. After the
/// timeout watchdog fires (or the process exits normally) the executor still needs time to kill
/// the child tree, drain the output pipes, and call `finish()` — the terminal status lands a few
/// seconds after the timeout itself. 10 s is generous for process teardown on any platform.
const SANDBOX_POLL_GRACE_MS: u64 = 10_000;

/// The minimum poller deadline, used when the run's timeout is very short or zero. Keeps a
/// reasonable observation window even for instant-exit runs whose `finish()` may still lag the
/// `sandbox_start` broadcast by a scheduling tick.
const SANDBOX_POLL_MIN_DEADLINE_MS: u64 = 30_000;

/// Compute the poller deadline for a sandbox run given its configured timeout. The deadline must
/// exceed the run's timeout (otherwise a long run finishes after the poller gives up and the UI
/// never sees `sandbox_end`). We add a grace window for process teardown/output drain and floor at
/// the minimum so short/zero-timeout runs are still observed. Exposed for unit testing.
fn sandbox_poll_deadline_ms(timeout_ms: i64) -> u64 {
    let base = if timeout_ms > 0 {
        timeout_ms as u64 + SANDBOX_POLL_GRACE_MS
    } else {
        SANDBOX_POLL_MIN_DEADLINE_MS
    };
    base.max(SANDBOX_POLL_MIN_DEADLINE_MS)
}

/// Poll a sandbox run until it reaches a terminal status, then broadcast `sandbox_end`. This gives
/// the UI the run lifecycle events it expects without requiring the executor task to know about
/// sessions or WebSockets. The deadline scales with the run's `timeout_ms` (plus a grace window)
/// so a long-timeout run is still observed to completion; a stuck run can't leak the poller past
/// `timeout_ms + grace`.
fn spawn_sandbox_end_poller(
    session_id: String,
    run_id: String,
    tx: broadcast::Sender<Value>,
    timeout_ms: i64,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(sandbox_poll_deadline_ms(timeout_ms));
        while tokio::time::Instant::now() < deadline {
            if let Some(run) = crate::sandbox::lookup(&run_id) {
                if run.status != "running" {
                    // Dedup against the sandbox.kill handler: only the first caller to claim
                    // the end emission should emit, so a killed run delivers exactly one
                    // sandbox_end to the UI instead of two.
                    if crate::sandbox::try_mark_end_emitted(&run_id) {
                        emit_sandbox_event(
                            &tx,
                            &session_id,
                            json!({ "type": "sandbox_end", "runId": run_id, "run": run }),
                        );
                    }
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

    // Workflow events are broadcast globally (every socket receives every workflow update). The
    // UI filters by runId/sessionId/projectId, so a session-specific subscription is not needed.
    let mut wf_rx = crate::workflows::subscribe_events();

    // Provider-health state-change events are broadcast globally so every client's conn-chip
    // updates when a provider degrades or recovers. The UI filters by provider id.
    let mut health_rx = crate::agent::provider_health::subscribe_health_events();
    // The health receiver is always available (the channel never closes); the subscribe fn
    // returns a Receiver directly now, so no Option handling is needed.

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
                frame = recv_workflow_broadcast(&mut wf_rx) => {
                    match frame {
                        Some(f) => {
                            if sink.send(WsMessage::Text(f.to_string().into())).await.is_err() {
                                break Some(None);
                            }
                        }
                        // The global workflow channel never closes; a lag-only path just means
                        // this socket skipped some frames and will resume from the newest one.
                        None => break Some(None),
                    }
                }
                frame = recv_broadcast(&mut health_rx) => {
                    match frame {
                        Some(f) => {
                            if sink.send(WsMessage::Text(f.to_string().into())).await.is_err() {
                                break Some(None);
                            }
                        }
                        // The global health channel never closes; a lag-only path just means
                        // this socket skipped some frames and will resume from the newest one.
                        None => break Some(None),
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

    // Subscribe to the server's graceful-shutdown signal so this WebSocket closes
    // promptly when the server is draining, instead of hanging axum's shutdown
    // phase indefinitely. Without this, an active WS read loop blocks the server
    // from completing graceful shutdown — axum waits for all connection tasks.
    let mut shutdown_rx = crate::server::subscribe_shutdown();

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
                        // Fire-and-forget command-run telemetry when the user typed a
                        // slash command (e.g. "/implement …"). Only the bare command
                        // NAME is recorded — never the arguments — and telemetry is a
                        // no-op until the operator opts in via settings.
                        if let Some(cmd) = slash_command_name(&body) {
                            let cmd = cmd.to_string();
                            tokio::spawn(async move {
                                crate::telemetry::record_command_run(cmd).await;
                            });
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
                        // Mode-aware default: web-mode previews host a dev server the user is
                        // actively looking at, so they get the long preview default instead of
                        // the 30 s terminal default (which killed every preview mid-view). An
                        // explicit positive timeoutMs from the client still wins in either mode.
                        let timeout_ms = crate::sandbox::resolve_timeout_ms(
                            v.get("timeoutMs").and_then(|x| x.as_i64()),
                            &mode,
                        );
                        if let Some(tx) = session::tx(&session_id) {
                            let sid = session_id.clone();
                            tokio::spawn(async move {
                                // start_run's streamed frames (sandbox_output / sandbox_port) are
                                // raw {type,...} values, but the api-contract requires every
                                // sandbox event on the session socket to be wrapped in
                                // {kind:"sandbox", sessionId, event} — app.js drops bare frames.
                                // Relay through a private channel and wrap each frame.
                                let (raw_tx, mut raw_rx) = broadcast::channel::<Value>(256);
                                {
                                    let tx = tx.clone();
                                    let sid = sid.clone();
                                    tokio::spawn(async move {
                                        loop {
                                            match raw_rx.recv().await {
                                                Ok(ev) => emit_sandbox_event(&tx, &sid, ev),
                                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                                Err(broadcast::error::RecvError::Closed) => break,
                                            }
                                        }
                                    });
                                }
                                match crate::sandbox::start_run(
                                    &language,
                                    &code,
                                    &mode,
                                    project_id.as_deref(),
                                    timeout_ms,
                                    Some(raw_tx),
                                    None,
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
                                        spawn_sandbox_end_poller(sid, run_id, tx, timeout_ms);
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
                            if let Some(tx) = session::tx(&session_id) {
                                let sid = session_id.clone();
                                let rid = run_id.to_string();
                                tokio::spawn(async move {
                                    // Give the kill a moment to update status, then emit end.
                                    if killed {
                                        tokio::time::sleep(std::time::Duration::from_millis(200))
                                            .await;
                                    }
                                    // Dedup against the sandbox-start poller: only the first
                                    // caller to claim the end emission should emit, so a killed
                                    // run delivers exactly one sandbox_end instead of two.
                                    if crate::sandbox::try_mark_end_emitted(&rid) {
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
                                    }
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
                            if let Some(tx) = session::tx(&session_id) {
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
                    // Live-editable workflow steering: rerun a step with feedback.
                    "workflow.rerun" => {
                        let run_id = v.get("runId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let step_id = v.get("stepId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let feedback = v.get("feedback").and_then(|x| x.as_str()).map(|s| s.to_string());
                        if let (Some(rid), Some(sid)) = (run_id, step_id) {
                            // Spawn the executor so the HTTP response returns immediately.
                            tokio::spawn(async move {
                                crate::workflows::rerun_step_and_dispatch(&rid, &sid, feedback).await;
                            });
                        }
                    }
                    // Live-editable workflow steering: patch a step's parents.
                    "workflow.patchParents" => {
                        let run_id = v.get("runId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let step_id = v.get("stepId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let parents = v.get("parents").and_then(|x| x.as_array()).cloned();
                        if let (Some(rid), Some(sid)) = (run_id, step_id) {
                            let parents = parents.unwrap_or_default();
                            let _ = crate::workflows::patch_parents(&rid, &sid, parents);
                        }
                    }
                    // Artifact review: "reject" feeds a repair rerun with a rejection note.
                    // ("approve" is UI-only — the artifact already exists — and never sent.)
                    "workflow.actOnStep" => {
                        let run_id = v.get("runId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let step_id = v.get("stepId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
                        if action == "reject" {
                            if let (Some(rid), Some(sid)) = (run_id, step_id) {
                                let feedback = v
                                    .get("feedback")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("The operator reviewed this step's artifact and requested changes.")
                                    .to_string();
                                tokio::spawn(async move {
                                    crate::workflows::rerun_step_and_dispatch(&rid, &sid, Some(feedback))
                                        .await;
                                });
                            }
                        }
                    }
                    // Live-editable workflow steering: patch a step's model.
                    "workflow.patchModel" => {
                        let run_id = v.get("runId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let step_id = v.get("stepId").and_then(|x| x.as_str()).map(|s| s.to_string());
                        let model = v.get("model").and_then(|x| x.as_str()).map(|s| s.to_string());
                        if let (Some(rid), Some(sid)) = (run_id, step_id) {
                            let _ = crate::workflows::patch_model(&rid, &sid, model.as_deref());
                        }
                    }
                    _ => {}
                }
            }
            _ = done.cancelled() => break,
            // Server is shutting down — cancel the done token so the fan also breaks,
            // then exit the reader loop to let the close handshake run.
            _ = shutdown_rx.changed() => {
                done.cancel();
                break;
            }
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
        .route("/api/provider-health", get(get_provider_health))
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

    /// CRLF line endings in a prompt file must not prevent description extraction. Before the fix,
    /// `parse_prompt_description` searched for a literal `\n---` closing fence and treated CRLF
    /// frontmatter as having no frontmatter at all.
    #[test]
    fn prompt_commands_from_dir_reads_crlf_frontmatter_description() {
        let dir =
            std::env::temp_dir().join(format!("dotz-prompts-crlf-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("windows-preset.md"),
            "---\r\ndescription: A Windows-style preset\r\n---\r\nBody.\r\n",
        )
        .unwrap();

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
        assert_eq!(
            by_name.get("windows-preset").cloned().unwrap_or_default(),
            "A Windows-style preset",
            "CRLF frontmatter description must be extracted"
        );
    }

    /// A `---` sequence inside frontmatter content (not a closing fence) must not truncate the
    /// frontmatter. Before the fix, `parse_prompt_description` matched any `\n---` as the
    /// closing fence, so content like `note: see --- for details` would cut the frontmatter
    /// short and miss the description field.
    #[test]
    fn parse_prompt_description_ignores_bare_dashes_in_content() {
        let dir =
            std::env::temp_dir().join(format!("dotz-prompts-dashes-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dashed-preset.md"),
            "---\ndescription: Real description\nnote: see --- for details\n---\nBody.\n",
        )
        .unwrap();

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
        assert_eq!(
            by_name.get("dashed-preset").cloned().unwrap_or_default(),
            "Real description",
            "a bare `---` in content must not be mistaken for the closing fence"
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
        let msg = err.1.0["error"].as_str().unwrap_or("");
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
        let create_resp = create_session(Some(Json(json!({})))).await.unwrap();
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

    /// Anthropic and Google are routed to their native adapters, so `post_model` must accept them
    /// instead of returning 404. Before the `provider::resolve` fix these providers resolved to
    /// None and the model-update endpoint rejected them even though the adapters existed.
    #[tokio::test]
    async fn post_model_accepts_anthropic_and_google() {
        let create_resp = create_session(Some(Json(json!({})))).await.unwrap();
        let sid = create_resp.0["sessionId"].as_str().unwrap().to_string();

        let body = Json(json!({
            "provider": "anthropic",
            "modelId": "claude-sonnet-4"
        }));
        let resp = post_model(axum::extract::Path(sid.clone()), Some(body))
            .await
            .unwrap();
        assert_eq!(resp.0["model"]["provider"], "anthropic");
        assert_eq!(resp.0["model"]["modelId"], "claude-sonnet-4");

        let body = Json(json!({
            "provider": "google",
            "modelId": "gemini-2.5-pro"
        }));
        let resp = post_model(axum::extract::Path(sid), Some(body))
            .await
            .unwrap();
        assert_eq!(resp.0["model"]["provider"], "google");
        assert_eq!(resp.0["model"]["modelId"], "gemini-2.5-pro");
    }

    #[tokio::test]
    async fn create_session_rejects_unknown_profile() {
        let body = Json(json!({ "profileId": "not-a-profile" }));
        let err = create_session(Some(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1.0["error"].as_str().unwrap_or("");
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

    #[tokio::test]
    async fn create_session_rejects_unknown_project_id() {
        let id = uuid::Uuid::new_v4().to_string();
        let body = Json(json!({ "projectId": id }));
        let err = create_session(Some(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1.0["error"].as_str().unwrap_or("");
        assert!(msg.contains("no such project"), "unexpected error: {msg}");
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

    /// `sandbox_poll_deadline_ms` must scale with the run's timeout so a long-timeout run is
    /// observed to completion instead of the poller giving up at a fixed 30 s and the UI never
    /// seeing `sandbox_end`. Before the fix the deadline was hardcoded at 30 s, so a run with
    /// `timeoutMs: 60_000` would finish *after* the poller exited — the sandbox panel hung on
    /// "running" forever.
    #[test]
    fn sandbox_poll_deadline_scales_with_timeout() {
        // Short timeout: the grace window lifts the deadline above the 30 s floor.
        assert_eq!(
            sandbox_poll_deadline_ms(5_000),
            30_000,
            "5 s timeout + 10 s grace (15 s) is below the 30 s floor"
        );
        // Exactly at the floor boundary: 20 s + 10 s grace = 30 s.
        assert_eq!(
            sandbox_poll_deadline_ms(20_000),
            30_000,
            "20 s timeout + 10 s grace hits the 30 s floor exactly"
        );
        // Long timeout: deadline must exceed the timeout so the poller is still alive when the
        // run finishes. This is the regression case — the old fixed 30 s deadline would expire
        // 30 s before a 60 s run completes.
        let d60 = sandbox_poll_deadline_ms(60_000);
        assert_eq!(
            d60, 70_000,
            "60 s timeout + 10 s grace = 70 s deadline (must exceed the run timeout)"
        );
        assert!(d60 > 60_000, "deadline must outlive the run timeout");
        // Very long timeout: scales linearly, no hidden cap that would re-introduce the bug.
        let d_h = sandbox_poll_deadline_ms(3_600_000);
        assert_eq!(d_h, 3_610_000, "1 h timeout + 10 s grace");
        // Zero / negative (defensive — the WS path always sends > 0): floor at the minimum.
        assert_eq!(
            sandbox_poll_deadline_ms(0),
            SANDBOX_POLL_MIN_DEADLINE_MS,
            "zero timeout falls back to the minimum deadline"
        );
        assert_eq!(
            sandbox_poll_deadline_ms(-1),
            SANDBOX_POLL_MIN_DEADLINE_MS,
            "negative timeout falls back to the minimum deadline"
        );
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
        // Generous timeoutMs: it only bounds a hang, and a powershell cold start can exceed 5 s
        // under heavy load — a timeout kill would race the lifecycle this test observes.
        ws.send(Message::Text(
            json!({
                "kind": "sandbox.start",
                "language": language,
                "code": code,
                "mode": "terminal",
                "projectId": null,
                "timeoutMs": 60000,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let mut run_id: Option<String> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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

    /// A killed sandbox run must deliver exactly one `sandbox_end` event, not two. Before the
    /// `try_mark_end_emitted` dedup, both the sandbox-start poller and the sandbox-kill handler
    /// emitted `sandbox_end` for the same run, so the UI received a redundant terminal event on
    /// every manual kill. This test starts a long-lived run, kills it via the WebSocket, and
    /// counts the `sandbox_end` frames for that run id.
    #[tokio::test]
    async fn websocket_sandbox_kill_emits_exactly_one_sandbox_end() {
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

        // Start a long-lived run so it is still running when we kill it.
        let (language, code) = if cfg!(windows) {
            ("powershell", "Start-Sleep -Seconds 30")
        } else {
            ("bash", "sleep 30")
        };
        ws.send(Message::Text(
            json!({
                "kind": "sandbox.start",
                "language": language,
                "code": code,
                "mode": "terminal",
                "projectId": null,
                "timeoutMs": 60000,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        // Collect the sandbox_start to get the run id. Generous deadline: under heavy load the
        // server task and WS round-trip can be scheduled several seconds late.
        let mut run_id: Option<String> = None;
        let start_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < start_deadline {
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("sandbox") {
                        let event = frame.get("event").cloned().unwrap_or_default();
                        if event.get("type").and_then(|t| t.as_str()) == Some("sandbox_start") {
                            run_id = event
                                .get("runId")
                                .and_then(|r| r.as_str())
                                .map(String::from);
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

        // Wait for execute_run to spawn the child and record its pid. The sandbox_start event
        // fires as soon as the run is created, before execute_run has necessarily spawned the
        // child — if we kill before the pid is set, kill_run_by_id returns false (no-op) and
        // the poller never sees a terminal status, so the dedup path isn't exercised. A fixed
        // sleep is not enough: a powershell cold start can take well over 5 s under heavy
        // load, so poll for the pid with a generous deadline instead.
        let pid_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while crate::sandbox::test_run_pid(&rid).is_none()
            && tokio::time::Instant::now() < pid_deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            crate::sandbox::test_run_pid(&rid).is_some(),
            "sandbox child should register a pid before we kill it"
        );

        // Kill the run via the WebSocket.
        ws.send(Message::Text(
            json!({ "kind": "sandbox.kill", "runId": rid })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

        // Count sandbox_end events for this run id. Wait generously for the FIRST one (taskkill
        // plus the status poller can take many seconds under heavy load), then keep listening a
        // further fixed window so a duplicate — the regression this test guards against — would
        // still be caught. The assertion is unchanged: exactly one sandbox_end.
        let mut end_count = 0usize;
        let first_end_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut dup_deadline: Option<tokio::time::Instant> = None;
        loop {
            let deadline = dup_deadline.unwrap_or(first_end_deadline);
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("sandbox") {
                        let event = frame.get("event").cloned().unwrap_or_default();
                        if event.get("type").and_then(|t| t.as_str()) == Some("sandbox_end") {
                            let eid = event.get("runId").and_then(|r| r.as_str()).unwrap_or("");
                            if eid == rid {
                                end_count += 1;
                                if dup_deadline.is_none() {
                                    // First terminal event seen: give a would-be duplicate a
                                    // dedicated window (the buggy double-emit fired back-to-back).
                                    dup_deadline =
                                        Some(tokio::time::Instant::now() + Duration::from_secs(3));
                                }
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }

        assert_eq!(
            end_count, 1,
            "killed run should deliver exactly one sandbox_end, got {end_count}"
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

    /// Workflow events are broadcast globally to every WebSocket. Creating a workflow via REST
    /// must deliver a `workflow_start` frame, and a step update must deliver `step_state`, so the
    /// UI graph panel stays live without polling.
    #[tokio::test]
    async fn websocket_receives_global_workflow_events() {
        use futures_util::StreamExt;
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message;

        let _guard = WS_TEST_LOCK.lock().await;

        // Isolate the workflow history file so this test does not pollute the real store.
        let workflows_file = std::env::temp_dir().join(format!(
            "dotz-ws-workflows-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        let prev_workflows_file = std::env::var("DOTZ_WORKFLOWS_FILE").ok();
        std::env::set_var(
            "DOTZ_WORKFLOWS_FILE",
            workflows_file.to_string_lossy().to_string(),
        );

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

        // Create a workflow via REST. The create handler calls start(), which emits workflow_start.
        let wf_resp = client
            .post(format!("http://127.0.0.1:{port}/api/workflows"))
            .json(&json!({
                "label": "ws-test",
                "steps": [
                    { "agent": "a", "task": "A" },
                    { "agent": "b", "task": "B", "parents": [0] }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert!(wf_resp.status().is_success());
        let wf = wf_resp.json::<Value>().await.unwrap();
        let run_id = wf["id"].as_str().unwrap().to_string();
        let step_id = wf["steps"][0]["id"].as_str().unwrap().to_string();

        let mut seen_start = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("workflow") {
                        // The workflow event bus is GLOBAL: another test's concurrently
                        // running workflow can interleave its events onto this socket.
                        // Filter to this run's events instead of asserting on whatever
                        // frame happens to arrive first.
                        if frame["runId"].as_str() != Some(run_id.as_str()) {
                            continue;
                        }
                        let event = frame.get("event").cloned().unwrap_or_default();
                        if event.get("type").and_then(|t| t.as_str()) == Some("workflow_start") {
                            assert_eq!(event["run"]["status"], "running");
                            seen_start = true;
                            break;
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(
            seen_start,
            "websocket should receive workflow_start for the new run"
        );

        // Update the first step to done.
        let step_resp = client
            .post(format!(
                "http://127.0.0.1:{port}/api/workflows/{run_id}/step"
            ))
            .json(&json!({ "stepId": step_id, "status": "done", "output": "ok" }))
            .send()
            .await
            .unwrap();
        assert!(step_resp.status().is_success());

        let mut seen_step_state = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if frame.get("kind").and_then(|k| k.as_str()) == Some("workflow") {
                        // Same global-bus caveat as above: skip foreign runs' events.
                        if frame["runId"].as_str() != Some(run_id.as_str()) {
                            continue;
                        }
                        let event = frame.get("event").cloned().unwrap_or_default();
                        if event.get("type").and_then(|t| t.as_str()) == Some("step_state") {
                            assert_eq!(event["stepId"], step_id);
                            assert_eq!(event["status"], "done");
                            assert_eq!(event["output"], "ok");
                            seen_step_state = true;
                            break;
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(
            seen_step_state,
            "websocket should receive step_state after a step update"
        );

        session::dispose(&sid);
        match prev_workflows_file {
            Some(p) => std::env::set_var("DOTZ_WORKFLOWS_FILE", p),
            None => std::env::remove_var("DOTZ_WORKFLOWS_FILE"),
        }
        let _ = std::fs::remove_file(&workflows_file);
        let _ = tx.send(());
        assert!(
            handle.await.unwrap().is_ok(),
            "server should shut down cleanly"
        );
    }
}
