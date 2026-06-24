//! Agent session: state + the turn loop. A session owns its conversation history, model selection,
//! thinking level, active tools, and a broadcast channel that fans agent events to WS subscribers.
//!
//! The turn loop (`run_turn`) is the heart of the runtime — "call LLM → parse tool calls → run tools
//! → repeat" — wrapped in the exact event sequence `web/app.js` expects:
//!   agent_start → turn_start → (message_start → message_update×N → message_end)×rounds → turn_end → agent_end
//! with `message_update.assistantMessageEvent.partial` carrying the full assistant snapshot each tick.
use super::event::*;
use super::provider::{self, ChatRequest, StreamDelta};
use super::tools::{ToolCtx, ToolRegistry};
use crate::{config, memory, profiles, projects, skills, types};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A live agent session. Mutable runtime state behind the store's Mutex; the turn loop snapshots
/// what it needs and streams without holding the lock across awaits.
pub struct AgentSession {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub profile_id: String,
    pub project_id: Option<String>,
    pub cwd: PathBuf,
    /// System prompt assembled at session-build time (doctrine + memory seed + project + skills + low-cost).
    pub system_prompt: String,
    /// Conversation history as OpenAI-shape messages ({role, content, ...}) AND as rich Messages for
    /// agent_end. We keep the rich Message list for the contract's `agent_end.messages`.
    pub history: Vec<Message>,
    pub tools: ToolRegistry,
    pub tx: broadcast::Sender<Value>,
    pub cancel: CancellationToken,
    /// Guards against concurrent turns on the same session. `run_turn` swaps this on and the
    /// `TurnGuard` clears it when the turn finishes or panics, preventing interleaved history
    /// and cancellation-token replacement.
    pub turn_active: AtomicBool,
}

impl AgentSession {
    /// Public control snapshot (sessionSummary shape).
    pub fn summary(&self) -> Value {
        json!({
            "sessionId": self.id,
            "profileId": self.profile_id,
            "projectId": self.project_id,
            "model": {
                "provider": self.provider,
                "modelId": self.model_id,
                "name": self.model_id,
                "reasoning": true,
            },
            "thinkingLevel": self.thinking_level,
            "supportsThinking": true,
            "availableThinkingLevels": types::THINKING_LEVELS,
            "tools": self.tools.active_names(),
        })
    }

    /// getSessionStats shape: {tokens, cost, contextUsage:{percent, contextWindow}}.
    pub fn stats(&self) -> Value {
        // app.js refreshStats reads tokens.input/output → tokens MUST be an object, not a scalar.
        let last = self.history.iter().filter_map(|m| m.usage.as_ref()).last();
        let (input, output, total) = last
            .map(|u| (u.input, u.output, u.total_tokens))
            .unwrap_or((0, 0, 0));
        let cost: f64 = self
            .history
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| u.cost.total)
            .sum();
        let window = provider::resolve(&self.provider, &self.model_id)
            .map(|m| m.context_window)
            .unwrap_or(256_000);
        let percent = if window > 0 {
            (total as f64 / window as f64) * 100.0
        } else {
            0.0
        };
        json!({
            "tokens": { "input": input, "output": output, "totalTokens": total },
            "cost": cost,
            "contextUsage": { "percent": percent, "contextWindow": window },
        })
    }
}

// ---- session store (module-owned, no AppState) ----
type Store = HashMap<String, std::sync::Arc<Mutex<AgentSession>>>;
static SESSIONS: OnceLock<Mutex<Store>> = OnceLock::new();
fn store() -> &'static Mutex<Store> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Options for creating a session (mirrors CreateOpts).
#[derive(Default)]
pub struct CreateOpts {
    pub cwd: Option<String>,
    pub model: Option<types::ModelRef>,
    pub thinking_level: Option<String>,
    pub tools: Option<Vec<String>>,
    pub profile_id: Option<String>,
    pub project_id: Option<String>,
}

/// Assemble the system prompt: profile doctrine + memory recall seed + project block + skill index +
/// low-cost directive. Mirrors profiles.ts buildResourceLoader's appendSystemPrompt assembly.
fn build_system_prompt(
    cwd: &str,
    profile_id: &str,
    project_id: Option<&str>,
    app_url: Option<&str>,
) -> String {
    let pi = skills_pi_design_dir();
    let mut parts: Vec<String> = vec![profiles::doctrine(profile_id, &pi)];

    // Memory seed: project-relevant (when bound) + global. Recall a recent slice as the always-on
    // baseline (the per-turn query-relevant recall happens in run_turn).
    let seed_cwd = if project_id.is_some() {
        Some(cwd)
    } else {
        None
    };
    let seed = memory::list_public(seed_cwd);
    if !seed.is_empty() {
        let mut block = String::from("# Durable memory (seed)\n");
        for m in seed.iter().take(30) {
            block.push_str(&format!("- {}\n", m.memory));
        }
        parts.push(block);
    }

    // Project block — the working directory the tools already operate in.
    let mut proj = format!(
        "# Project (dotz)\nYour working directory (the selected project's root) is: {cwd}\nThe shell, git, and file tools already operate HERE — do NOT `cd` to a guessed path; run commands relative to this root."
    );
    if let Some(url) = app_url {
        proj.push_str(&format!("\nThis project's running app is served at: {url} — for any VISUAL, E2E, or BUG-BOUNTY task, drive THAT url with the in-app browser; do NOT target any other dev server."));
    }
    parts.push(proj);

    // Skill index.
    let idx = skills::render_index();
    if !idx.is_empty() {
        parts.push(idx);
    }

    // Low-cost subagent-model directive.
    parts.push(types::render_low_cost_models());

    parts.join("\n\n")
}

/// The bundled design-systems dir, for the DESIGN doctrine interpolation (DOTZ_PI/.pi/design-systems).
fn skills_pi_design_dir() -> String {
    let pi = std::env::var("DOTZ_PI")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".pi")
        });
    pi.join("design-systems").to_string_lossy().to_string()
}

/// Create a session, resolving project binding + defaults exactly like pi.create.
pub fn create(opts: CreateOpts) -> Result<Value, String> {
    let cfg = config::load();
    let mut cwd = opts.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into())
    });
    let mut profile_id = opts.profile_id.unwrap_or_else(|| "workflow".into());
    let mut model = opts.model;
    let mut thinking = opts.thinking_level;
    let mut project_id: Option<String> = None;
    let mut app_url: Option<String> = None;

    // A bound project supplies cwd/profile and the default model/thinking (explicit opts still win).
    if let Some(pid) = &opts.project_id {
        if let Some(p) = projects::find(pid) {
            project_id = Some(p.id.clone());
            cwd = p.cwd.clone();
            profile_id = p.profile_id.clone();
            model = model.or(Some(p.model.clone()));
            thinking = thinking.or(Some(p.thinking_level.clone()));
            app_url = p.app_url.clone();
        }
    }

    let profile = profiles::get(Some(&profile_id));
    let model = model.unwrap_or_else(|| types::ModelRef {
        provider: cfg.provider.clone(),
        model_id: cfg.executive_model.clone(),
    });
    let thinking = thinking.unwrap_or_else(|| profile.thinking_level.to_string());

    let system_prompt =
        build_system_prompt(&cwd, &profile_id, project_id.as_deref(), app_url.as_deref());

    let mut tools = ToolRegistry::new();
    if let Some(t) = &opts.tools {
        tools.set_active(t);
    } else if let Some(profile_tools) = profile.tools {
        let names: Vec<String> = profile_tools.iter().map(|s| s.to_string()).collect();
        tools.set_active(&names);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let (tx, _rx) = broadcast::channel::<Value>(1024);
    let session = AgentSession {
        id: id.clone(),
        provider: model.provider.clone(),
        model_id: model.model_id.clone(),
        thinking_level: thinking,
        profile_id: profile.id.to_string(),
        project_id,
        cwd: PathBuf::from(&cwd),
        system_prompt,
        history: Vec::new(),
        tools,
        tx,
        cancel: CancellationToken::new(),
        turn_active: AtomicBool::new(false),
    };
    let summary = session.summary();
    store()
        .lock()
        .unwrap()
        .insert(id.clone(), std::sync::Arc::new(Mutex::new(session)));
    Ok(summary)
}

pub fn get(id: &str) -> Option<std::sync::Arc<Mutex<AgentSession>>> {
    store().lock().unwrap().get(id).cloned()
}

pub fn list_summaries() -> Vec<Value> {
    store()
        .lock()
        .unwrap()
        .values()
        .map(|s| s.lock().unwrap().summary())
        .collect()
}

pub fn count() -> usize {
    store().lock().unwrap().len()
}

pub fn dispose(id: &str) -> bool {
    if let Some(s) = store().lock().unwrap().remove(id) {
        s.lock().unwrap().cancel.cancel();
        true
    } else {
        false
    }
}

/// Subscribe to a session's event stream (broadcast). The WS handler relays each Value frame.
pub fn subscribe(id: &str) -> Option<broadcast::Receiver<Value>> {
    get(id).map(|s| s.lock().unwrap().tx.subscribe())
}

fn emit(sess: &AgentSession, ev: &AgentEvent) {
    let _ = sess.tx.send(ws_frame(&sess.id, ev));
}

/// Convert the rich history into OpenAI-shape messages for the request.
fn to_openai_messages(system_prompt: &str, history: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system_prompt })];
    for m in history {
        match m.role.as_str() {
            "user" => {
                let text = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({ "role": "user", "content": text }));
            }
            "assistant" => {
                let text = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let tool_calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id, "type": "function",
                            "function": { "name": name, "arguments": arguments.to_string() }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({ "role": "assistant", "content": text });
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            "tool" => {
                // Tool result messages carry the tool_call_id in response_id (reused) + text content.
                let text = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": m.response_id.clone().unwrap_or_default(),
                    "content": text,
                }));
            }
            _ => {}
        }
    }
    out
}

/// Accumulator for the streaming assistant message — tracks content blocks + the active index.
struct Accumulator {
    msg: Message,
    /// Active block index for thinking / text (so deltas append to the right block).
    thinking_idx: Option<usize>,
    text_idx: Option<usize>,
    /// Streamed tool calls by provider index → (block index in content, id, name, args-json-buffer).
    tool_calls: HashMap<usize, (usize, String, String, String)>,
}

impl Accumulator {
    fn new(provider: &str, model: &str, ts: i64) -> Self {
        Accumulator {
            msg: Message::assistant_shell(provider, model, ts),
            thinking_idx: None,
            text_idx: None,
            tool_calls: HashMap::new(),
        }
    }
}

/// RAII guard that clears `AgentSession::turn_active` when the turn finishes, even on panic.
struct TurnGuard {
    session: Arc<Mutex<AgentSession>>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.session
            .lock()
            .unwrap()
            .turn_active
            .store(false, Ordering::SeqCst);
    }
}

/// Run one prompt to completion: assemble, append user msg, loop (stream → maybe tools → repeat).
/// This is the public entry the WS `prompt` handler calls. It blocks until the turn finishes.
pub async fn run_turn(session: Arc<Mutex<AgentSession>>, prompt: String) {
    // Reject concurrent turns on the same session to keep history and the cancellation token sane.
    let _guard = {
        let s = session.lock().unwrap();
        if s.turn_active.swap(true, Ordering::SeqCst) {
            return;
        }
        TurnGuard {
            session: session.clone(),
        }
    };

    // Snapshot the immutable bits + append the user message under the lock; release before awaiting.
    let (sess_id, system_prompt, provider_id, model_id, thinking, cwd, tools_specs) = {
        let mut s = session.lock().unwrap();
        s.cancel = CancellationToken::new();
        let ts = now_ms();
        let user_msg = Message::user(&prompt, ts);
        emit(&s, &AgentEvent::AgentStart);
        emit(&s, &AgentEvent::TurnStart);
        emit(
            &s,
            &AgentEvent::MessageStart {
                message: user_msg.clone(),
            },
        );
        emit(
            &s,
            &AgentEvent::MessageEnd {
                message: user_msg.clone(),
            },
        );
        s.history.push(user_msg);
        (
            s.id.clone(),
            s.system_prompt.clone(),
            s.provider.clone(),
            s.model_id.clone(),
            s.thinking_level.clone(),
            s.cwd.clone(),
            s.tools.active_specs(),
        )
    };

    // Per-turn query-relevant memory recall, appended to the system prompt (best-effort).
    let recall = memory::recall(&prompt, Some(&cwd.to_string_lossy()));
    let recall_block = memory::render_recall(&recall);
    let effective_system = if recall_block.is_empty() {
        system_prompt
    } else {
        format!("{system_prompt}\n\n{recall_block}")
    };

    let ctx = ToolCtx {
        cwd: cwd.clone(),
        tx: Some(session.lock().unwrap().tx.clone()),
    };
    let cancel = session.lock().unwrap().cancel.clone();

    // Accumulate every tool result from this turn so the final turn_end event can surface them
    // to the UI (workflow step links, error markers, etc.).
    let mut tool_results: Vec<ToolResult> = Vec::new();

    // The agent loop: up to a bounded number of tool-rounds.
    const MAX_ROUNDS: usize = 12;
    for _round in 0..MAX_ROUNDS {
        // Build the request from current history.
        let messages = {
            let s = session.lock().unwrap();
            to_openai_messages(&effective_system, &s.history)
        };
        let resolved = match provider::resolve(&provider_id, &model_id) {
            Some(r) => r,
            None => {
                finish_error(
                    &session,
                    &format!(
                        "provider '{provider_id}' not resolvable (anthropic/google are Phase 3b)"
                    ),
                );
                return;
            }
        };
        let req = ChatRequest {
            model: resolved,
            messages,
            tools: tools_specs.clone(),
            reasoning_effort: provider::reasoning_effort(&thinking),
        };

        let (delta_tx, mut delta_rx) = mpsc::channel::<StreamDelta>(256);
        let adapter = provider::adapter_for(&provider_id);
        let stream_task = tokio::spawn(async move { adapter.stream(req, delta_tx).await });

        // message_start (assistant shell).
        let start_ts = now_ms();
        {
            let s = session.lock().unwrap();
            emit(
                &s,
                &AgentEvent::MessageStart {
                    message: Message::assistant_shell(&provider_id, &model_id, start_ts),
                },
            );
        }

        let mut acc = Accumulator::new(&provider_id, &model_id, start_ts);
        let mut stop_reason = "stop".to_string();

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    stop_reason = "aborted".into();
                    break;
                }
                d = delta_rx.recv() => {
                    match d {
                        Some(delta) => apply_delta(&session, &sess_id, &mut acc, delta, &mut stop_reason),
                        None => break, // stream channel closed
                    }
                }
            }
        }

        // Join the stream task to surface a transport error as an error message.
        match stream_task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                finish_error(&session, &e);
                return;
            }
            Err(e) => {
                finish_error(&session, &format!("stream task panicked: {e}"));
                return;
            }
        }

        acc.msg.stop_reason = Some(stop_reason.clone());
        let assistant_msg = acc.msg.clone();

        // message_end for the assistant message.
        {
            let mut s = session.lock().unwrap();
            emit(
                &s,
                &AgentEvent::MessageEnd {
                    message: assistant_msg.clone(),
                },
            );
            s.history.push(assistant_msg.clone());
        }

        // Collect tool calls from this assistant message.
        let calls: Vec<(String, String, Value)> = assistant_msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();

        if calls.is_empty() || stop_reason == "aborted" {
            // No tools → turn is done (tool_results is empty unless tools ran earlier).
            finish_turn(&session, assistant_msg, tool_results);
            return;
        }

        // Run each tool, emit tool_execution_start/end, append a tool-result message to history.
        for (call_id, name, args) in calls {
            {
                let s = session.lock().unwrap();
                emit(
                    &s,
                    &AgentEvent::ToolExecutionStart {
                        tool_call_id: call_id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                    },
                );
            }
            // Execute without holding the session lock (the registry is stateless).
            // subagent is special-cased so its SubagentDetails reach result.details (the workflow bridge reads it).
            let (result_value, is_error, result_text) = if name == "subagent" {
                let cwd = ctx.cwd.to_string_lossy().to_string();
                let d = super::subagent::dispatch(&args, &cwd).await;
                let rv = json!({
                    "content": [{ "type": "text", "text": d.text.clone() }],
                    "details": d.details_json(),
                    "isError": d.is_error,
                });
                (rv, d.is_error, d.text)
            } else {
                match run_tool(&session, &name, &args, &ctx).await {
                    Ok(text) => (
                        json!({ "content": [{ "type": "text", "text": text }] }),
                        false,
                        text,
                    ),
                    Err(e) => (
                        json!({ "content": [{ "type": "text", "text": e.clone() }], "isError": true }),
                        true,
                        e,
                    ),
                }
            };
            {
                let mut s = session.lock().unwrap();
                emit(
                    &s,
                    &AgentEvent::ToolExecutionEnd {
                        tool_call_id: call_id.clone(),
                        tool_name: name.clone(),
                        is_error,
                        result: result_value.clone(),
                    },
                );
                // Append the tool result as a `tool` message (response_id carries the tool_call_id).
                s.history.push(Message {
                    role: "tool".into(),
                    content: vec![ContentBlock::Text {
                        text: result_text.clone(),
                    }],
                    api: None,
                    provider: None,
                    model: None,
                    usage: None,
                    stop_reason: None,
                    error_message: None,
                    timestamp: now_ms(),
                    response_id: Some(call_id.clone()),
                });
            }
            tool_results.push(ToolResult {
                tool_call_id: call_id,
                tool_name: name,
                is_error,
                result: result_value,
            });
        }
        // Loop again so the model can consume the tool results.
    }

    // Hit the round cap — finish with whatever the last assistant message was.
    let last = session
        .lock()
        .unwrap()
        .history
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .cloned();
    if let Some(msg) = last {
        finish_turn(&session, msg, tool_results);
    }
}

/// Execute a tool against the session's registry (no session lock held across the await).
async fn run_tool(
    session: &std::sync::Arc<Mutex<AgentSession>>,
    name: &str,
    args: &Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    // The registry is owned by the session; we take a short snapshot reference by running through a
    // dedicated method. Since ToolRegistry isn't Clone, build a fresh registry for execution — the
    // built-ins are stateless, so a fresh registry behaves identically. (Active-set filtering already
    // happened when assembling the request specs.)
    let _ = session;
    let registry = ToolRegistry::new();
    registry.run(name, args, ctx).await
}

/// Apply one StreamDelta to the accumulator, emitting the matching message_update event.
fn apply_delta(
    session: &std::sync::Arc<Mutex<AgentSession>>,
    sess_id: &str,
    acc: &mut Accumulator,
    delta: StreamDelta,
    stop_reason: &mut String,
) {
    let mut ame_kind: Option<&str> = None;
    let mut delta_text: Option<String> = None;
    let mut content_index = 0usize;

    match delta {
        StreamDelta::Thinking(t) => {
            let idx = match acc.thinking_idx {
                Some(i) => {
                    if let ContentBlock::Thinking { thinking, .. } = &mut acc.msg.content[i] {
                        thinking.push_str(&t);
                    }
                    ame_kind = Some("thinking_delta");
                    i
                }
                None => {
                    acc.msg.content.push(ContentBlock::Thinking {
                        thinking: t.clone(),
                        thinking_signature: "reasoning".into(),
                    });
                    let i = acc.msg.content.len() - 1;
                    acc.thinking_idx = Some(i);
                    ame_kind = Some("thinking_start");
                    i
                }
            };
            content_index = idx;
            delta_text = Some(t);
        }
        StreamDelta::Text(t) => {
            let idx = match acc.text_idx {
                Some(i) => {
                    if let ContentBlock::Text { text } = &mut acc.msg.content[i] {
                        text.push_str(&t);
                    }
                    ame_kind = Some("text_delta");
                    i
                }
                None => {
                    acc.msg.content.push(ContentBlock::Text { text: t.clone() });
                    let i = acc.msg.content.len() - 1;
                    acc.text_idx = Some(i);
                    ame_kind = Some("text_start");
                    i
                }
            };
            content_index = idx;
            delta_text = Some(t);
        }
        StreamDelta::ToolCallStart { index, id, name } => {
            acc.msg.content.push(ContentBlock::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: json!({}),
            });
            let block_idx = acc.msg.content.len() - 1;
            acc.tool_calls
                .insert(index, (block_idx, id, name, String::new()));
            ame_kind = Some("toolcall_start");
            content_index = block_idx;
        }
        StreamDelta::ToolCallArgs { index, json: frag } => {
            if let Some(entry) = acc.tool_calls.get_mut(&index) {
                entry.3.push_str(&frag);
                // Try to parse the accumulated buffer; update the block's arguments when valid.
                if let Ok(parsed) = serde_json::from_str::<Value>(&entry.3) {
                    if let ContentBlock::ToolCall { arguments, .. } = &mut acc.msg.content[entry.0]
                    {
                        *arguments = parsed;
                    }
                }
                ame_kind = Some("toolcall_delta");
                content_index = entry.0;
                delta_text = Some(frag);
            }
        }
        StreamDelta::Usage(u) => {
            acc.msg.usage = Some(u);
            return; // usage isn't a content event — no message_update
        }
        StreamDelta::Stop(reason) => {
            *stop_reason = reason;
            return;
        }
    }

    if let Some(kind) = ame_kind {
        let s = session.lock().unwrap();
        let ame = AssistantMessageEvent {
            kind: kind.to_string(),
            content_index,
            delta: delta_text,
            content: None,
            partial: acc.msg.clone(),
        };
        let _ = s.tx.send(ws_frame(
            sess_id,
            &AgentEvent::MessageUpdate {
                assistant_message_event: ame,
                message: acc.msg.clone(),
            },
        ));
    }
}

/// Emit turn_end + agent_end, then (main session only) fire-and-forget autonomous memory capture.
fn finish_turn(
    session: &std::sync::Arc<Mutex<AgentSession>>,
    final_msg: Message,
    tool_results: Vec<ToolResult>,
) {
    let join_text = |m: &Message| {
        m.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let (cwd, user_text, assistant_text) = {
        let s = session.lock().unwrap();
        let user_text = s
            .history
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(&join_text)
            .unwrap_or_default();
        let assistant_text = join_text(&final_msg);
        emit(
            &s,
            &AgentEvent::TurnEnd {
                message: final_msg.clone(),
                tool_results,
            },
        );
        emit(
            &s,
            &AgentEvent::AgentEnd {
                messages: s.history.clone(),
                will_retry: false,
            },
        );
        // Capture to PROJECT scope only when the session is project-bound; otherwise global (matches Node).
        let cwd = if s.project_id.is_some() {
            s.cwd.to_string_lossy().to_string()
        } else {
            String::new()
        };
        (cwd, user_text, assistant_text)
    };
    if crate::memory::is_autonomy_enabled() && tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move {
            let cwd_opt = if cwd.is_empty() {
                None
            } else {
                Some(cwd.as_str())
            };
            crate::memory::capture_exchange(&user_text, &assistant_text, cwd_opt).await;
        });
    }
}

/// Finish the turn with an error assistant message (stopReason "error").
fn finish_error(session: &std::sync::Arc<Mutex<AgentSession>>, detail: &str) {
    let mut s = session.lock().unwrap();
    let mut msg = Message::assistant_shell(&s.provider, &s.model_id, now_ms());
    msg.stop_reason = Some("error".into());
    msg.error_message = Some(detail.to_string());
    // The UI contract expects a message_start before every message_end; the error path was
    // skipping it and leaving the bubble renderer with no shell to attach the error to.
    emit(
        &s,
        &AgentEvent::MessageStart {
            message: Message::assistant_shell(&s.provider, &s.model_id, msg.timestamp),
        },
    );
    emit(
        &s,
        &AgentEvent::MessageEnd {
            message: msg.clone(),
        },
    );
    s.history.push(msg.clone());
    emit(
        &s,
        &AgentEvent::TurnEnd {
            message: msg,
            tool_results: Vec::new(),
        },
    );
    emit(
        &s,
        &AgentEvent::AgentEnd {
            messages: s.history.clone(),
            will_retry: false,
        },
    );
}

/// Abort the running turn (CancellationToken). The select! in run_turn observes it.
pub fn abort(id: &str) -> bool {
    if let Some(s) = get(id) {
        s.lock().unwrap().cancel.cancel();
        true
    } else {
        false
    }
}

// ---- control mutations used by the REST handlers ----

pub fn set_model(id: &str, provider_id: &str, model_id: &str) -> Result<Value, String> {
    let s = get(id).ok_or("no such session")?;
    let mut g = s.lock().unwrap();
    g.provider = provider_id.to_string();
    g.model_id = model_id.to_string();
    Ok(g.summary())
}

pub fn set_thinking(id: &str, level: &str) -> Result<Value, String> {
    let s = get(id).ok_or("no such session")?;
    let mut g = s.lock().unwrap();
    g.thinking_level = level.to_string();
    Ok(json!({
        "thinkingLevel": g.thinking_level,
        "supportsThinking": true,
        "availableThinkingLevels": types::THINKING_LEVELS,
    }))
}

pub fn set_tools(id: &str, names: &[String]) -> Result<Value, String> {
    let s = get(id).ok_or("no such session")?;
    let mut g = s.lock().unwrap();
    g.tools.set_active(names);
    Ok(json!({ "active": g.tools.active_names(), "all": g.tools.all_names() }))
}

pub fn get_tools(id: &str) -> Option<Value> {
    let s = get(id)?;
    let g = s.lock().unwrap();
    Some(json!({ "active": g.tools.active_names(), "all": g.tools.all_names() }))
}

pub fn summary_with_stats(id: &str) -> Option<Value> {
    let s = get(id)?;
    let g = s.lock().unwrap();
    let mut sum = g.summary();
    sum["stats"] = g.stats();
    Some(sum)
}

pub fn summary(id: &str) -> Option<Value> {
    get(id).map(|s| s.lock().unwrap().summary())
}

/// Models list for GET /api/sessions/:id/models — current + default + providers + providerMeta.
pub fn models(id: &str) -> Option<Value> {
    let s = get(id)?;
    let g = s.lock().unwrap();
    let default = types::default_model();
    Some(json!({
        "current": { "provider": g.provider, "modelId": g.model_id, "name": g.model_id, "reasoning": true },
        "default": default,
        "providers": types::provider_ids(),
        "available": [],
        "providerMeta": types::providers(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_guard_clears_flag_on_drop() {
        let summary = create(CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        let sess = get(&sid).unwrap();
        sess.lock().unwrap().turn_active.store(true, Ordering::SeqCst);
        {
            let _guard = TurnGuard { session: sess.clone() };
        }
        assert!(
            !sess.lock().unwrap().turn_active.load(Ordering::SeqCst),
            "TurnGuard must clear turn_active on drop"
        );
        dispose(&sid);
    }

    #[tokio::test]
    async fn run_turn_rejects_concurrent_prompt() {
        let summary = create(CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        let sess = get(&sid).unwrap();
        // Simulate an in-flight turn.
        sess.lock().unwrap().turn_active.store(true, Ordering::SeqCst);

        run_turn(sess.clone(), "second prompt while busy".into()).await;

        let g = sess.lock().unwrap();
        assert!(
            g.history.is_empty(),
            "concurrent run_turn should not append to history"
        );
        assert!(
            g.turn_active.load(Ordering::SeqCst),
            "active flag should remain set (the rejected turn did not create a guard)"
        );
        drop(g);
        dispose(&sid);
    }

    #[test]
    fn finish_error_emits_message_start_before_message_end() {
        let summary = create(CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        let sess = get(&sid).unwrap();
        let mut rx = sess.lock().unwrap().tx.subscribe();

        finish_error(&sess, "provider unreachable");

        let mut kinds: Vec<String> = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let Some(kind) = frame
                .get("event")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
            {
                kinds.push(kind.to_string());
            }
        }

        dispose(&sid);

        let start_pos = kinds.iter().position(|k| k == "message_start");
        let end_pos = kinds.iter().position(|k| k == "message_end");
        assert!(
            start_pos.is_some(),
            "message_start missing in error events: {kinds:?}"
        );
        assert!(
            end_pos.is_some(),
            "message_end missing in error events: {kinds:?}"
        );
        assert!(
            start_pos.unwrap() < end_pos.unwrap(),
            "message_start must precede message_end in error path: {kinds:?}"
        );
    }

    #[test]
    fn finish_turn_emits_tool_results_in_turn_end() {
        let summary = create(CreateOpts::default()).unwrap();
        let sid = summary["sessionId"].as_str().unwrap().to_string();
        let sess = get(&sid).unwrap();
        let mut rx = sess.lock().unwrap().tx.subscribe();

        let final_msg = Message::assistant_shell("ollama", "glm-5.2", now_ms());
        let result = ToolResult {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            is_error: true,
            result: json!({ "output": "hello" }),
        };
        finish_turn(&sess, final_msg, vec![result]);

        let mut found = None;
        while let Ok(frame) = rx.try_recv() {
            if let Some("turn_end") = frame
                .get("event")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
            {
                found = Some(frame);
            }
        }

        dispose(&sid);

        let event = found
            .expect("turn_end event should be emitted")
            .get("event")
            .cloned()
            .unwrap();
        let results = event
            .get("toolResults")
            .and_then(|v| v.as_array())
            .expect("turn_end should include toolResults");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["toolCallId"], "tc-1");
        assert_eq!(results[0]["toolName"], "bash");
        assert_eq!(results[0]["isError"], true);
        assert_eq!(results[0]["result"]["output"], "hello");
    }
}
