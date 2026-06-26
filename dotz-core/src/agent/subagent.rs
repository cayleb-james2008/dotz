//! Subagent orchestration — the `subagent` tool's runtime (Phase 4).
//!
//! Port of `.pi/extensions/subagent/index.ts`. The Node original spawns a fresh `pi` child PROCESS
//! per subagent (isolated context window). The Rust runtime is in-process, so a subagent is instead
//! a fresh, self-contained agent loop: a child "session" with the agent's system prompt + the
//! configured subagent model (DOTZ_SUBAGENT_MODEL, default `ollama/minimax-m3`), run to completion
//! with NO memory/recall autonomy (just the agent's own prompt) and a restricted tool set.
//!
//! Three modes, mirroring the oracle:
//!   - single:   one {agent, task}
//!   - parallel: tasks[] — tokio JoinSet + Semaphore(4 concurrency), max 8
//!   - chain:    chain[] — sequential, `{previous}` feed-forward, max 16
//!
//! `dispatch` returns a `SubagentDetails` the workflow bridge reads (mode / agentScope /
//! projectAgentsDir / results[]); the `subagent` tool (registered in tools.rs) serializes it into
//! the ToolResult's `details` so the existing DAG-population path works unchanged.
use super::event::{ContentBlock, Message};
use super::provider::{self, ChatRequest, StreamDelta};
use super::tools::{ToolCtx, ToolRegistry};
use crate::context_bus::ContextBus;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Semaphore};

const MAX_PARALLEL_TASKS: usize = 8;
const MAX_CHAIN_STEPS: usize = 16;
const MAX_CONCURRENCY: usize = 4;
/// Cap the {previous} feed-forward so a large prior output can't explode the next task prompt
/// (mirrors CHAIN_PREVIOUS_CAP in the oracle — 24 KiB).
const CHAIN_PREVIOUS_CAP: usize = 24 * 1024;
/// Bound a subagent's own tool-rounds (matches the executive loop's MAX_ROUNDS in session.rs).
const MAX_ROUNDS: usize = 12;
/// Default wall-clock timeout for one subagent run. Long enough for real work, short enough that
/// a hung provider/tool cannot stall the executive turn forever. Override with
/// `DOTZ_SUBAGENT_TIMEOUT_MS` (e.g. for fast tests).
const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 1000 * 60 * 5; // 5 minutes

fn subagent_timeout() -> Duration {
    const MIN_MS: u64 = 1_000; // 1 second — zero would time out before any stream arrives.
    const MAX_MS: u64 = 3_600_000; // 1 hour — anything larger defeats the purpose of the cap.
    std::env::var("DOTZ_SUBAGENT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_SUBAGENT_TIMEOUT_MS))
}

fn timeout_result(agent_name: &str, task: &str, step: Option<usize>) -> SingleResult {
    SingleResult {
        agent: agent_name.to_string(),
        agent_source: "timeout".into(),
        task: task.to_string(),
        exit_code: 1,
        messages: Vec::new(),
        usage: SubUsage::default(),
        model: None,
        stop_reason: Some("timeout".into()),
        error_message: Some(format!(
            "subagent '{agent_name}' timed out after {:?}",
            subagent_timeout()
        )),
        step,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---- agent discovery (port of agents.ts) ----

/// A discovered agent definition (frontmatter + body system prompt).
#[derive(Clone, Debug)]
struct AgentConfig {
    name: String,
    #[allow(dead_code)]
    description: String,
    tools: Option<Vec<String>>,
    model: Option<String>,
    system_prompt: String,
    /// "user" (bundled / ~/.pi) or "project" (.pi/agents under cwd).
    source: &'static str,
}

/// Resolve the bundled `.pi` dir the way `skills.rs` / `design.rs` does: a `DOTZ_PI` override
/// (pointing at the `.pi` dir), else `<cwd>/.pi`.
fn pi_dir() -> PathBuf {
    std::env::var("DOTZ_PI")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".pi")
        })
}

/// Parse a `--- frontmatter ---` markdown file into (key→value map, body). Mirrors the subset of
/// pi's `parseFrontmatter` the discovery path needs: simple `key: value` lines until a closing `---`.
/// Tolerant of CRLF (normalizes `\r`) — the bundled agents may be checked out with either ending.
fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut fm = HashMap::new();
    let norm = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n");
    // Frontmatter must start at the very top with a `---` line.
    let Some(after_open) = norm.strip_prefix("---\n") else {
        return (fm, content.to_string());
    };
    // Find the closing `---` line: either `\n---\n<body>` or a trailing `\n---`.
    let (fm_block, body) = if let Some(idx) = after_open.find("\n---\n") {
        (&after_open[..idx], after_open[idx + 5..].to_string())
    } else if let Some(stripped) = after_open.strip_suffix("\n---") {
        (stripped, String::new())
    } else {
        // No closing fence — treat the whole thing as body (no frontmatter).
        return (fm, content.to_string());
    };

    for line in fm_block.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let mut val = v.trim().to_string();
            // Strip surrounding quotes (frontmatter values are often JSON-quoted, per agents.ts).
            if val.len() >= 2
                && ((val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\'')))
            {
                val = val[1..val.len() - 1].to_string();
            }
            if !key.is_empty() {
                fm.insert(key, val);
            }
        }
    }
    (fm, body)
}

/// Load agent definitions from a directory (`*.md` with name+description frontmatter).
fn load_agents_from_dir(dir: &PathBuf, source: &'static str) -> Vec<AgentConfig> {
    let mut agents = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return agents,
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (fm, body) = parse_frontmatter(&content);
        let (Some(name), Some(description)) = (fm.get("name"), fm.get("description")) else {
            continue;
        };
        let tools = fm.get("tools").map(|t| {
            t.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        });
        agents.push(AgentConfig {
            name: name.clone(),
            description: description.clone(),
            tools: tools.filter(|t| !t.is_empty()),
            model: fm.get("model").cloned().filter(|s| !s.is_empty()),
            system_prompt: body,
            source,
        });
    }
    agents
}

/// Walk up from `cwd` to find the nearest `.pi/agents` dir (project-local agents).
fn find_project_agents_dir(cwd: &str) -> Option<PathBuf> {
    let mut current = PathBuf::from(cwd);
    loop {
        let candidate = current.join(".pi").join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        match current.parent() {
            Some(p) if p != current => current = p.to_path_buf(),
            _ => return None,
        }
    }
}

/// Result of discovery: the merged agent list + the resolved project agents dir (for SubagentDetails).
struct Discovery {
    agents: Vec<AgentConfig>,
    project_agents_dir: Option<String>,
}

/// Discover agents for the given cwd + scope. Precedence (low→high): bundled (.pi/agents) →
/// user (~/.pi/agent/agents) → project-local override. Bundled agents are ALWAYS available so the
/// flagship workflows work against any cwd (mirrors agents.ts).
fn discover_agents(cwd: &str, scope: &str) -> Discovery {
    let bundled_dir = pi_dir().join("agents");
    let user_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pi")
        .join("agent")
        .join("agents");
    let project_dir = find_project_agents_dir(cwd);

    let bundled = load_agents_from_dir(&bundled_dir, "user");
    let user = if scope == "project" {
        Vec::new()
    } else {
        load_agents_from_dir(&user_dir, "user")
    };
    let project = match (&project_dir, scope) {
        (Some(d), s) if s != "user" => load_agents_from_dir(d, "project"),
        _ => Vec::new(),
    };

    // Map by name, applying precedence.
    let mut map: HashMap<String, AgentConfig> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let put = |a: AgentConfig, map: &mut HashMap<String, AgentConfig>, order: &mut Vec<String>| {
        if !map.contains_key(&a.name) {
            order.push(a.name.clone());
        }
        map.insert(a.name.clone(), a);
    };
    for a in bundled {
        put(a, &mut map, &mut order);
    }
    if scope != "project" {
        for a in user {
            put(a, &mut map, &mut order);
        }
    }
    if scope != "user" {
        for a in project {
            put(a, &mut map, &mut order);
        }
    }
    let agents = order.into_iter().filter_map(|n| map.remove(&n)).collect();
    Discovery {
        agents,
        project_agents_dir: project_dir.map(|p| p.to_string_lossy().to_string()),
    }
}

// ---- result + details shapes (SubagentDetails the workflow bridge reads) ----

/// Aggregated usage for one subagent run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SubUsage {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    pub cost: f64,
    #[serde(rename = "contextTokens")]
    pub context_tokens: u64,
    pub turns: u64,
}

/// One subagent's run result (SingleResult in the oracle). `messages` is the rich message list as
/// JSON (the workflow bridge reads JSON; event::Message is Serialize-only).
#[derive(Clone, Debug, Serialize)]
pub struct SingleResult {
    pub agent: String,
    #[serde(rename = "agentSource")]
    pub agent_source: String,
    pub task: String,
    #[serde(rename = "exitCode")]
    pub exit_code: i64,
    pub messages: Vec<Value>,
    pub usage: SubUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
}

/// The tool-result `details` payload the workflow bridge consumes.
#[derive(Clone, Debug, Serialize)]
pub struct SubagentDetails {
    pub mode: String,
    #[serde(rename = "agentScope")]
    pub agent_scope: String,
    #[serde(rename = "projectAgentsDir")]
    pub project_agents_dir: Option<String>,
    pub results: Vec<SingleResult>,
}

impl SingleResult {
    pub fn is_failed(&self) -> bool {
        self.exit_code != 0
            || self.stop_reason.as_deref() == Some("error")
            || self.stop_reason.as_deref() == Some("aborted")
            || self.stop_reason.as_deref() == Some("timeout")
    }

    /// The final assistant text output (last assistant message's text blocks).
    pub fn final_output(&self) -> String {
        for m in self.messages.iter().rev() {
            if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                    let text: String = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        return text;
                    }
                }
            }
        }
        String::new()
    }

    /// Output for assembly: error detail when failed, else the final assistant text.
    pub fn result_output(&self) -> String {
        if self.is_failed() {
            return self
                .error_message
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let f = self.final_output();
                    (!f.is_empty()).then_some(f)
                })
                .unwrap_or_else(|| "(no output)".into());
        }
        let f = self.final_output();
        if f.is_empty() {
            "(no output)".into()
        } else {
            f
        }
    }
}

fn unknown_agent_result(
    agent_name: &str,
    task: &str,
    available: &[AgentConfig],
    step: Option<usize>,
) -> SingleResult {
    let list = if available.is_empty() {
        "none".to_string()
    } else {
        available
            .iter()
            .map(|a| format!("\"{}\"", a.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    SingleResult {
        agent: agent_name.to_string(),
        agent_source: "unknown".into(),
        task: task.to_string(),
        exit_code: 1,
        messages: Vec::new(),
        usage: SubUsage::default(),
        model: None,
        stop_reason: Some("error".into()),
        error_message: Some(format!(
            "Unknown agent: \"{agent_name}\". Available agents: {list}."
        )),
        step,
    }
}

// ---- the in-process child agent loop ----

/// Build the tool registry for a subagent: honor the agent's declared tool list, otherwise use the
/// full registry minus the `subagent` tool so a subagent does not recurse by default.
fn build_subagent_registry(agent: &AgentConfig) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    if let Some(t) = &agent.tools {
        r.set_active(t);
    } else {
        let default: Vec<String> = r
            .all_names()
            .into_iter()
            .filter(|n| n != "subagent")
            .collect();
        r.set_active(&default);
    }
    r
}

/// Parse a subagent model string into (provider, model_id). Accepts a bare id (falls back to
/// ollama) or "provider/model-id". Strips a redundant provider prefix so the upstream API receives
/// the bare model id, matching the executive session + config normalization behavior.
fn parse_subagent_model(effective_model: &str) -> (String, String) {
    match effective_model.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => {
            let provider_id = p.to_ascii_lowercase();
            let model_id = crate::types::strip_matching_provider_prefix(&provider_id, m);
            (provider_id, model_id)
        }
        _ => ("ollama".to_string(), effective_model.to_string()),
    }
}

/// Run one subagent to completion: a fresh agent loop with the agent's system prompt + the resolved
/// model, NO memory autonomy (the agent's own prompt only), and the agent's tool set (or the default
/// active set). Captures the message list + usage as a SingleResult. A wall-clock timeout prevents a
/// hung provider or long tool chain from stalling the executive turn indefinitely.
async fn run_single_agent(
    agents: &[AgentConfig],
    agent_name: &str,
    task: &str,
    model_override: Option<&str>,
    cwd: &str,
    step: Option<usize>,
    bus: Option<&ContextBus>,
) -> SingleResult {
    run_single_agent_inner(agents, agent_name, task, model_override, cwd, step, bus).await
}

/// The actual subagent loop. The provider stream task is aborted on the wall-clock timeout so a
/// hung provider cannot keep holding a connection (and a tokio task) after the subagent returns.
async fn run_single_agent_inner(
    agents: &[AgentConfig],
    agent_name: &str,
    task: &str,
    model_override: Option<&str>,
    cwd: &str,
    step: Option<usize>,
    bus: Option<&ContextBus>,
) -> SingleResult {
    let Some(agent) = agents.iter().find(|a| a.name == agent_name) else {
        return unknown_agent_result(agent_name, task, agents, step);
    };

    // Model resolution: explicit override → agent's own default → DOTZ_SUBAGENT_MODEL → ollama/minimax-m3.
    let effective_model = model_override
        .map(|s| s.to_string())
        .or_else(|| agent.model.clone())
        .or_else(|| {
            std::env::var("DOTZ_SUBAGENT_MODEL")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "ollama/minimax-m3".to_string());
    // Split "provider/model-id" → (provider, model_id). A bare id falls back to ollama.
    // Normalize a redundant "provider/" prefix so the upstream API receives a bare model id,
    // matching the executive session + config behavior.
    let (provider_id, model_id) = parse_subagent_model(&effective_model);

    let mut result = SingleResult {
        agent: agent.name.clone(),
        agent_source: agent.source.to_string(),
        task: task.to_string(),
        exit_code: 0,
        messages: Vec::new(),
        usage: SubUsage::default(),
        model: Some(effective_model.clone()),
        stop_reason: None,
        error_message: None,
        step,
    };

    // Restricted tool set: the agent's declared tools (validated against the registry) or the default
    // active set with subagent recursion removed. An explicit agent.tools listing `subagent` is
    // still honored — the oracle places no special guard beyond the bounded round/task/chain caps.
    let registry = build_subagent_registry(agent);
    let tool_specs = registry.active_specs();

    let resolved = match provider::resolve(&provider_id, &model_id) {
        Some(r) => r,
        None => {
            result.exit_code = 1;
            result.stop_reason = Some("error".into());
            result.error_message = Some(format!(
                "provider '{provider_id}' not resolvable for subagent model '{effective_model}'"
            ));
            return result;
        }
    };

    // System prompt: the agent's own prompt only (no doctrine/memory seed/recall — restricted context).
    let system_prompt = if agent.system_prompt.trim().is_empty() {
        format!("You are the \"{}\" subagent.", agent.name)
    } else {
        agent.system_prompt.clone()
    };

    let ctx = ToolCtx {
        cwd: PathBuf::from(cwd),
        tx: None,
    };

    // Conversation history (rich Messages, like the executive session).
    // Inject shared context from the inter-agent bus into the task prompt. When the
    // bus is present and non-empty, the task is prepended with a compact JSON block of
    // prior agent outputs so this subagent inherits scout findings / planner plans /
    // reviewer gap-lists without re-reading the repo or parsing raw text.
    let effective_task = bus.map(|b| crate::context_bus::inject_context_into_task(task, b))
        .unwrap_or_else(|| task.to_string());
    let mut history: Vec<Message> = vec![Message::user(&format!("Task: {effective_task}"), now_ms())];

    for _round in 0..MAX_ROUNDS {
        let messages = to_openai_messages(&system_prompt, &history);
        let req = ChatRequest {
            model: resolved.clone(),
            messages,
            tools: tool_specs.clone(),
            // Subagents run at the model's default reasoning; "low" keeps the worker cheap+fast.
            reasoning_effort: provider::reasoning_effort("low"),
        };

        let (delta_tx, mut delta_rx) = mpsc::channel::<StreamDelta>(256);
        let adapter = provider::adapter_for(&provider_id);
        let stream_task = tokio::spawn(async move { adapter.stream(req, delta_tx).await });
        let deadline = tokio::time::Instant::now() + subagent_timeout();

        let mut acc = Acc::new(&provider_id, &model_id, now_ms());
        let mut stop_reason = "stop".to_string();
        loop {
            match tokio::time::timeout_at(deadline, delta_rx.recv()).await {
                Ok(Some(delta)) => apply_delta(&mut acc, delta, &mut stop_reason),
                Ok(None) => break,
                Err(_) => {
                    stream_task.abort();
                    return timeout_result(agent_name, task, step);
                }
            }
        }

        match stream_task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                result.exit_code = 1;
                result.stop_reason = Some("error".into());
                result.error_message = Some(e);
                return result;
            }
            Err(e) => {
                result.exit_code = 1;
                result.stop_reason = Some("error".into());
                result.error_message = Some(format!("stream task panicked: {e}"));
                return result;
            }
        }

        acc.msg.stop_reason = Some(stop_reason.clone());
        let assistant_msg = acc.msg.clone();

        // Tally usage from this assistant message.
        result.usage.turns += 1;
        if let Some(u) = &assistant_msg.usage {
            result.usage.input += u.input;
            result.usage.output += u.output;
            result.usage.cache_read += u.cache_read;
            result.usage.cache_write += u.cache_write;
            result.usage.cost += u.cost.total;
            result.usage.context_tokens = u.total_tokens;
        }
        if assistant_msg.stop_reason.is_some() {
            result.stop_reason = assistant_msg.stop_reason.clone();
        }

        result.messages.push(message_to_json(&assistant_msg));
        history.push(assistant_msg.clone());

        // Collect tool calls.
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

        if calls.is_empty() {
            // Done — no tools requested.
            return result;
        }

        // Run each tool; append a `tool` message + a JSON tool-result message to history/messages.
        for (call_id, name, args) in calls {
            let (result_json, text) = match registry.run(&name, &args, &ctx).await {
                Ok(t) => (
                    json!({ "role": "tool", "content": [{ "type": "text", "text": t }], "toolCallId": call_id }),
                    t,
                ),
                Err(e) => (
                    json!({ "role": "tool", "content": [{ "type": "text", "text": e }], "isError": true, "toolCallId": call_id }),
                    e,
                ),
            };
            result.messages.push(result_json);
            history.push(Message {
                role: "tool".into(),
                content: vec![ContentBlock::Text { text }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                error_message: None,
                timestamp: now_ms(),
                response_id: Some(call_id),
            });
        }
        // Loop so the subagent can consume the tool results.
    }

    // Hit the round cap — return whatever we have (stopReason stays as the last assistant's).
    result
}

/// Public single-agent entry (the name the workstream contract names). Discovers agents for `cwd`
/// under the default "user" scope and runs one to completion.
pub async fn run_single_agent_public(
    agent_name: &str,
    task: &str,
    model_override: Option<&str>,
    cwd: &str,
) -> SingleResult {
    let discovery = discover_agents(cwd, "user");
    run_single_agent(
        &discovery.agents,
        agent_name,
        task,
        model_override,
        cwd,
        None,
        None,
    )
    .await
}

/// Public single-agent entry WITH a context bus. The workflow executor uses this so each
/// subagent inherits shared context (scout findings, planner plans, reviewer gap-lists)
/// and can read/write structured data on the bus.
pub async fn run_single_agent_with_bus(
    agent_name: &str,
    task: &str,
    model_override: Option<&str>,
    cwd: &str,
    bus: Option<&ContextBus>,
) -> SingleResult {
    let discovery = discover_agents(cwd, "user");
    run_single_agent(
        &discovery.agents,
        agent_name,
        task,
        model_override,
        cwd,
        None,
        bus,
    )
    .await
}

// ---- streaming accumulator (a no-event subset of session::Accumulator) ----

struct Acc {
    msg: Message,
    thinking_idx: Option<usize>,
    text_idx: Option<usize>,
    /// provider tool-call index → (block index, arg-json buffer).
    tool_calls: HashMap<usize, (usize, String)>,
    /// Indices for which a ToolCallStart has already been emitted. Some providers stream the name
    /// across multiple chunks; without this guard the same call gets multiple content blocks.
    tool_call_started: std::collections::HashSet<usize>,
}

impl Acc {
    fn new(provider: &str, model: &str, ts: i64) -> Self {
        Acc {
            msg: Message::assistant_shell(provider, model, ts),
            thinking_idx: None,
            text_idx: None,
            tool_calls: HashMap::new(),
            tool_call_started: std::collections::HashSet::new(),
        }
    }
}

fn apply_delta(acc: &mut Acc, delta: StreamDelta, stop_reason: &mut String) {
    match delta {
        StreamDelta::Thinking(t) => match acc.thinking_idx {
            Some(i) => {
                if let ContentBlock::Thinking { thinking, .. } = &mut acc.msg.content[i] {
                    thinking.push_str(&t);
                }
            }
            None => {
                acc.msg.content.push(ContentBlock::Thinking {
                    thinking: t,
                    thinking_signature: "reasoning".into(),
                });
                acc.thinking_idx = Some(acc.msg.content.len() - 1);
            }
        },
        StreamDelta::Text(t) => match acc.text_idx {
            Some(i) => {
                if let ContentBlock::Text { text } = &mut acc.msg.content[i] {
                    text.push_str(&t);
                }
            }
            None => {
                acc.msg.content.push(ContentBlock::Text { text: t });
                acc.text_idx = Some(acc.msg.content.len() - 1);
            }
        },
        StreamDelta::ToolCallStart { index, id, name } => {
            // Only create a content block the first time we see a given provider index.
            // The provider may repeat the name in later argument chunks; re-using the same index
            // must append arguments to the existing block, not spawn a duplicate tool call.
            if acc.tool_call_started.insert(index) {
                acc.msg.content.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments: json!({}),
                });
                let block_idx = acc.msg.content.len() - 1;
                acc.tool_calls.insert(index, (block_idx, String::new()));
            }
        }
        StreamDelta::ToolCallArgs { index, json: frag } => {
            if let Some(entry) = acc.tool_calls.get_mut(&index) {
                entry.1.push_str(&frag);
                if let Ok(parsed) = serde_json::from_str::<Value>(&entry.1) {
                    if let ContentBlock::ToolCall { arguments, .. } = &mut acc.msg.content[entry.0]
                    {
                        *arguments = parsed;
                    }
                }
            }
        }
        StreamDelta::Usage(u) => acc.msg.usage = Some(u),
        StreamDelta::Stop(reason) => *stop_reason = reason,
    }
}

/// Build OpenAI-shape messages from the subagent history (a self-contained copy of the executive
/// session's `to_openai_messages`, so this module doesn't depend on session internals).
fn to_openai_messages(system_prompt: &str, history: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system_prompt })];
    for m in history {
        match m.role.as_str() {
            "user" => {
                let text = collect_text(&m.content);
                out.push(json!({ "role": "user", "content": text }));
            }
            "assistant" => {
                let text = collect_text(&m.content);
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
                let text = collect_text(&m.content);
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

fn collect_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Serialize a rich Message to its contract JSON (event::Message is Serialize-only).
fn message_to_json(m: &Message) -> Value {
    serde_json::to_value(m).unwrap_or_else(|_| json!({}))
}

fn truncate_bytes(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = s.len() - end;
    format!(
        "{}\n\n[Output truncated: {omitted} bytes omitted.]",
        &s[..end]
    )
}

// ---- dispatch: parse params → run the right mode → assemble (text, is_error, details) ----

/// The tool-facing return: the text summary, the error flag, and the SubagentDetails payload.
pub struct Dispatch {
    pub text: String,
    pub is_error: bool,
    pub details: SubagentDetails,
}

impl Dispatch {
    /// The SubagentDetails as JSON, for attaching to the ToolResult's `details` field.
    pub fn details_json(&self) -> Value {
        serde_json::to_value(&self.details).unwrap_or_else(|_| json!({}))
    }
}

/// The JSON-schema for the `subagent` tool's `parameters` (single / parallel / chain). Returned here
/// so the tool registration in tools.rs stays a one-liner (`subagent::parameters_schema()`).
pub fn parameters_schema() -> Value {
    let task_item = json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "description": "Name of the agent to invoke" },
            "task": { "type": "string", "description": "Task to delegate to the agent" },
            "cwd": { "type": "string", "description": "Working directory for the agent" },
            "model": { "type": "string", "description": "Model override (provider/model-id). Pick a low-cost model from the injected list." }
        },
        "required": ["agent", "task"]
    });
    json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "description": "Name of the agent to invoke (single mode)" },
            "task": { "type": "string", "description": "Task to delegate (single mode)" },
            "tasks": { "type": "array", "items": task_item, "description": "Array of {agent, task} for parallel execution (max 8)" },
            "chain": { "type": "array", "items": task_item, "description": "Array of {agent, task} for sequential execution; use {previous} in a task for the prior step's output (max 16)" },
            "agentScope": { "type": "string", "enum": ["user", "project", "both"], "description": "Which agent dirs to use (default user; both to include project-local .pi/agents)" },
            "cwd": { "type": "string", "description": "Working directory (single mode)" },
            "model": { "type": "string", "description": "Model override for single mode (provider/model-id)" },
            "runId": { "type": "string", "description": "Workflow run id — when provided, the subagent inherits the shared context bus for that run (prior agent findings, plans, gap-lists)" }
        }
    })
}

/// Entry the `subagent` tool calls. Parses the tool args (single / parallel / chain), discovers
/// agents for `cwd`, runs the selected mode, and assembles the result the workflow bridge reads.
/// `run_id` is the workflow run this dispatch belongs to; when present, the shared context
/// bus for that run is passed to each subagent so they inherit prior findings.
pub async fn dispatch(args: &Value, cwd: &str) -> Dispatch {
    let scope = args
        .get("agentScope")
        .and_then(|v| v.as_str())
        .unwrap_or("user")
        .to_string();
    let run_id = args.get("runId").and_then(|v| v.as_str());
    let bus = run_id.map(|id| ContextBus { run_id: id.to_string() });
    let discovery = discover_agents(cwd, &scope);
    let agents = discovery.agents;
    let project_agents_dir = discovery.project_agents_dir.clone();

    let make_details = |mode: &str, results: Vec<SingleResult>| SubagentDetails {
        mode: mode.to_string(),
        agent_scope: scope.clone(),
        project_agents_dir: project_agents_dir.clone(),
        results,
    };

    let chain = args.get("chain").and_then(|v| v.as_array());
    let tasks = args.get("tasks").and_then(|v| v.as_array());
    let single_agent = args.get("agent").and_then(|v| v.as_str());
    let single_task = args.get("task").and_then(|v| v.as_str());

    let has_chain = chain.map(|c| !c.is_empty()).unwrap_or(false);
    let has_tasks = tasks.map(|t| !t.is_empty()).unwrap_or(false);
    let has_single = single_agent.is_some() && single_task.is_some();
    let mode_count = has_chain as u8 + has_tasks as u8 + has_single as u8;

    if mode_count != 1 {
        let available = agents
            .iter()
            .map(|a| format!("{} ({})", a.name, a.source))
            .collect::<Vec<_>>()
            .join(", ");
        let available = if available.is_empty() {
            "none".into()
        } else {
            available
        };
        return Dispatch {
            text: format!(
                "Invalid parameters. Provide exactly one mode.\nAvailable agents: {available}"
            ),
            is_error: true,
            details: make_details("single", Vec::new()),
        };
    }

    // ---- chain ----
    if has_chain {
        let chain = chain.unwrap();
        if chain.len() > MAX_CHAIN_STEPS {
            return Dispatch {
                text: format!(
                    "Chain too long ({}). Max is {MAX_CHAIN_STEPS}.",
                    chain.len()
                ),
                is_error: true,
                details: make_details("chain", Vec::new()),
            };
        }
        let mut results: Vec<SingleResult> = Vec::new();
        let mut previous = String::new();
        for (i, step) in chain.iter().enumerate() {
            let agent_name = step.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            let task_tmpl = step.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let step_cwd = step.get("cwd").and_then(|v| v.as_str()).unwrap_or(cwd);
            let model = step.get("model").and_then(|v| v.as_str());
            // Substitute {previous} (literal replacement — no regex specials).
            let task = task_tmpl.replace("{previous}", &previous);
            let r =
                run_single_agent(&agents, agent_name, &task, model, step_cwd, Some(i + 1), bus.as_ref()).await;
            let failed = r.is_failed();
            results.push(r);
            if failed {
                let errmsg = results.last().unwrap().result_output();
                return Dispatch {
                    text: format!("Chain stopped at step {} ({agent_name}): {errmsg}", i + 1),
                    is_error: true,
                    details: make_details("chain", results),
                };
            }
            previous = truncate_bytes(&results.last().unwrap().final_output(), CHAIN_PREVIOUS_CAP);
        }
        let last_out = results.last().map(|r| r.final_output()).unwrap_or_default();
        let text = if last_out.is_empty() {
            "(no output)".into()
        } else {
            last_out
        };
        return Dispatch {
            text,
            is_error: false,
            details: make_details("chain", results),
        };
    }

    // ---- parallel ----
    if has_tasks {
        let tasks = tasks.unwrap();
        if tasks.len() > MAX_PARALLEL_TASKS {
            return Dispatch {
                text: format!(
                    "Too many parallel tasks ({}). Max is {MAX_PARALLEL_TASKS}.",
                    tasks.len()
                ),
                is_error: true,
                details: make_details("parallel", Vec::new()),
            };
        }
        let agents = std::sync::Arc::new(agents);
        let sem = std::sync::Arc::new(Semaphore::new(MAX_CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for (idx, t) in tasks.iter().enumerate() {
            let agent_name = t
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task = t
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task_cwd = t
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or(cwd)
                .to_string();
            let model = t
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let agents = agents.clone();
            let sem = sem.clone();
            let bus = bus.clone();
            set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore not closed");
                let r = run_single_agent(
                    &agents[..],
                    &agent_name,
                    &task,
                    model.as_deref(),
                    &task_cwd,
                    None,
                    bus.as_ref(),
                )
                .await;
                (idx, r)
            });
        }
        // Collect, preserving input order.
        let mut indexed: Vec<(usize, SingleResult)> = Vec::with_capacity(tasks.len());
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(pair) => indexed.push(pair),
                Err(_) => { /* a panicked task is dropped; the summary count reflects it */ }
            }
        }
        indexed.sort_by_key(|(i, _)| *i);
        let results: Vec<SingleResult> = indexed.into_iter().map(|(_, r)| r).collect();

        let success = results.iter().filter(|r| !r.is_failed()).count();
        let summaries: Vec<String> = results
            .iter()
            .map(|r| {
                let status = if r.is_failed() {
                    match r.stop_reason.as_deref() {
                        Some(sr) if sr != "end" => format!("failed ({sr})"),
                        _ => "failed".into(),
                    }
                } else {
                    "completed".into()
                };
                let output = truncate_bytes(&r.result_output(), 50 * 1024);
                format!("### [{}] {status}\n\n{output}", r.agent)
            })
            .collect();
        let text = format!(
            "Parallel: {success}/{} succeeded\n\n{}",
            results.len(),
            summaries.join("\n\n---\n\n")
        );
        return Dispatch {
            text,
            is_error: success != results.len(),
            details: make_details("parallel", results),
        };
    }

    // ---- single ----
    let agent_name = single_agent.unwrap();
    let task = single_task.unwrap();
    let model = args.get("model").and_then(|v| v.as_str());
    let single_cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or(cwd);
    let r = run_single_agent(&agents, agent_name, task, model, single_cwd, None, bus.as_ref()).await;
    if r.is_failed() {
        let errmsg = r.result_output();
        let label = r.stop_reason.clone().unwrap_or_else(|| "failed".into());
        return Dispatch {
            text: format!("Agent {label}: {errmsg}"),
            is_error: true,
            details: make_details("single", vec![r]),
        };
    }
    let out = r.final_output();
    let text = if out.is_empty() {
        "(no output)".into()
    } else {
        out
    };
    Dispatch {
        text,
        is_error: false,
        details: make_details("single", vec![r]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_accumulator_dedupes_repeated_tool_call_start() {
        let mut acc = Acc::new("local", "test", 0);
        // Some providers stream the tool-call name across multiple chunks. The first chunk
        // establishes the call; the second chunk repeats the same index and must NOT create a
        // second ToolCall content block.
        apply_delta(
            &mut acc,
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_abc".into(),
                name: "bash".into(),
            },
            &mut String::new(),
        );
        apply_delta(
            &mut acc,
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_abc".into(),
                name: "bash".into(),
            },
            &mut String::new(),
        );
        apply_delta(
            &mut acc,
            StreamDelta::ToolCallArgs {
                index: 0,
                json: "{\"command\":\"echo hi\"}".into(),
            },
            &mut String::new(),
        );

        let tool_blocks: Vec<_> = acc
            .msg
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
        assert_eq!(
            tool_blocks.len(),
            1,
            "repeated ToolCallStart for the same index must not create duplicate blocks"
        );
        let (id, name, args) = &tool_blocks[0];
        assert_eq!(id, "call_abc");
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "echo hi");
    }

    #[test]
    fn parse_subagent_model_handles_bare_and_prefixed_ids() {
        // Bare model id falls back to the default ollama provider.
        assert_eq!(
            parse_subagent_model("glm-5.2"),
            ("ollama".to_string(), "glm-5.2".to_string())
        );
        // Clean provider/model pair passes through unchanged.
        assert_eq!(
            parse_subagent_model("ollama/minimax-m3"),
            ("ollama".to_string(), "minimax-m3".to_string())
        );
        assert_eq!(
            parse_subagent_model("openrouter/nex-agi/nex-n2-pro:free"),
            (
                "openrouter".to_string(),
                "nex-agi/nex-n2-pro:free".to_string()
            )
        );
        // Redundant provider prefix (a common copy/paste mistake) is stripped so the upstream
        // API receives the bare model id instead of failing on "ollama/ollama/glm-5.2".
        assert_eq!(
            parse_subagent_model("ollama/ollama/glm-5.2"),
            ("ollama".to_string(), "glm-5.2".to_string())
        );
        // A mismatched provider prefix must be preserved so cross-provider namespaces are not
        // corrupted (e.g. an OpenRouter id that happens to start with "ollama/").
        assert_eq!(
            parse_subagent_model("openrouter/ollama/glm-5.2"),
            ("openrouter".to_string(), "ollama/glm-5.2".to_string())
        );
    }

    /// A subagent `model` override written by the lead agent may use a mixed-case provider
    /// segment (e.g. "Ollama/glm-5.2"). Without normalization, provider resolution is
    /// case-sensitive and would reject the model; lowering the provider segment fixes the
    /// fan-out while still stripping a redundant matching prefix.
    #[test]
    fn parse_subagent_model_lowercases_provider_segment() {
        assert_eq!(
            parse_subagent_model("Ollama/glm-5.2"),
            ("ollama".to_string(), "glm-5.2".to_string())
        );
        assert_eq!(
            parse_subagent_model("OpenRouter/nex-agi/nex-n2-pro:free"),
            ("openrouter".to_string(), "nex-agi/nex-n2-pro:free".to_string())
        );
        assert_eq!(
            parse_subagent_model("OPENROUTER/openrouter/nex-agi/nex-n2-pro:free"),
            ("openrouter".to_string(), "nex-agi/nex-n2-pro:free".to_string()),
            "uppercase provider segment with a redundant lowercase prefix must still be normalized"
        );
    }

    #[test]
    fn subagent_registry_excludes_recursion_by_default() {
        let agent = AgentConfig {
            name: "test".into(),
            description: "test".into(),
            tools: None,
            model: None,
            system_prompt: "sys".into(),
            source: "user",
        };
        // The baseline executive registry includes subagent; the subagent default must not.
        let full = ToolRegistry::new();
        assert!(
            full.active_names().contains(&"subagent".to_string()),
            "the full registry default should include subagent for executive sessions"
        );

        let r = build_subagent_registry(&agent);
        let names = r.active_names();
        assert!(
            !names.contains(&"subagent".to_string()),
            "default subagent tool set must not include subagent recursion"
        );
        assert!(
            names.contains(&"bash".to_string()),
            "default subagent tool set should still include bash"
        );
    }

    #[test]
    fn subagent_registry_honors_explicit_tools_including_subagent() {
        let agent = AgentConfig {
            name: "test".into(),
            description: "test".into(),
            tools: Some(vec!["subagent".to_string(), "bash".to_string()]),
            model: None,
            system_prompt: "sys".into(),
            source: "user",
        };
        let r = build_subagent_registry(&agent);
        let names = r.active_names();
        assert!(
            names.contains(&"subagent".to_string()),
            "explicit agent.tools listing subagent should be honored"
        );
        assert!(
            names.contains(&"bash".to_string()),
            "explicit agent.tools listing bash should be honored"
        );
    }

    /// A parallel subagent fan-out must report `is_error: true` when any of its tasks fail.
    /// Single and chain modes already do this; parallel was returning success even when every
    /// subagent errored, which misled the executive loop and workflow graph.
    #[tokio::test]
    async fn parallel_dispatch_marks_error_when_tasks_fail() {
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        let args = json!({
            "tasks": [
                { "agent": "dotz-parallel-unknown-1", "task": "task" },
                { "agent": "dotz-parallel-unknown-2", "task": "task" }
            ]
        });
        let d = dispatch(&args, &cwd).await;
        assert!(
            d.is_error,
            "parallel dispatch must report error when tasks fail: {}",
            d.text
        );
        assert!(
            d.text.contains("0/2 succeeded"),
            "summary should report zero successes: {}",
            d.text
        );
        assert_eq!(d.details.mode, "parallel");
        assert_eq!(d.details.results.len(), 2);
    }

    /// The configurable subagent timeout must clamp to sane bounds. A zero or extremely small
    /// value would time out before the provider stream starts; an enormous value defeats the
    /// purpose of the wall-clock cap.
    #[tokio::test]
    async fn subagent_timeout_is_configurable_and_clamped() {
        // Serialize with other subagent tests that mutate this process-global env var.
        let _guard = crate::agent::session::SSE_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_SUBAGENT_TIMEOUT_MS").ok();

        std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS");
        assert_eq!(
            subagent_timeout().as_secs(),
            300,
            "default subagent timeout is 5 minutes"
        );

        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "5000");
        assert_eq!(
            subagent_timeout().as_millis(),
            5000,
            "valid override is preserved"
        );

        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "50");
        assert_eq!(
            subagent_timeout().as_millis(),
            1000,
            "below-minimum value clamps to 1 second"
        );

        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "100000000");
        assert_eq!(
            subagent_timeout().as_millis(),
            3_600_000,
            "above-maximum value clamps to 1 hour"
        );

        match prev {
            Some(p) => std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS"),
        }
    }

    /// A subagent whose provider stream hangs must not stall the executive turn forever, and the
    /// provider stream task must be aborted so the underlying connection is released. Before the
    /// timeout fix, `run_single_agent` would await the stream indefinitely; before the abort fix,
    /// the timed-out task detached and kept the connection open.
    #[tokio::test]
    async fn run_single_agent_times_out_on_hung_provider() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Serialize with other tests that mutate the process-global local-provider URL.
        let _guard = crate::agent::session::SSE_TEST_LOCK.lock().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
        let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::with_capacity(8192);
            loop {
                let mut tmp = [0u8; 1024];
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
            let _ = stream.write_all(response).await;
            let _ = headers_tx.send(());
            let _ = server_rx.recv().await;
        });

        let prev_url = std::env::var("DOTZ_LOCAL_BASE_URL").ok();
        let prev_timeout = std::env::var("DOTZ_SUBAGENT_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_LOCAL_BASE_URL", format!("http://127.0.0.1:{port}/v1"));
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "500");

        let agent = AgentConfig {
            name: "test".into(),
            description: "test".into(),
            tools: None,
            model: None,
            system_prompt: "sys".into(),
            source: "test",
        };
        let cwd = std::env::temp_dir().to_string_lossy().to_string();

        // Start the subagent first so the fake server gets a connection; once headers are on the
        // wire we know the provider stream is hung and the timeout is actually being exercised.
        let mut run = tokio::spawn(async move {
            let agents = [agent];
            run_single_agent(&agents, "test", "task", Some("local/test"), &cwd, None, None).await
        });

        tokio::select! {
            _ = headers_rx => {}
            r = &mut run => panic!("run_single_agent finished before provider stream started: {r:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();

        match prev_url {
            Some(p) => std::env::set_var("DOTZ_LOCAL_BASE_URL", p),
            None => std::env::remove_var("DOTZ_LOCAL_BASE_URL"),
        }
        match prev_timeout {
            Some(p) => std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS"),
        }
        let _ = server_tx.send(()).await;

        assert_eq!(
            result.exit_code, 1,
            "timed-out subagent must report failure"
        );
        assert_eq!(
            result.stop_reason.as_deref(),
            Some("timeout"),
            "stop_reason must be 'timeout', got {:?}",
            result.stop_reason
        );
        assert!(
            result
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("timed out"),
            "error message should mention timeout: {:?}",
            result.error_message
        );
    }

    /// A timed-out subagent must abort the provider stream task, releasing the underlying TCP
    /// connection. We verify this by having the fake server detect EOF after the client aborts.
    #[tokio::test]
    async fn run_single_agent_aborts_provider_stream_task_on_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = crate::agent::session::SSE_TEST_LOCK.lock().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read_half, mut write_half) = stream.into_split();
            let mut buf = Vec::with_capacity(8192);
            // Read the full HTTP request (headers + small JSON body) before responding so the
            // client can finish sending and later close cleanly when the subagent aborts.
            loop {
                let mut tmp = [0u8; 1024];
                let n = read_half.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            // Drain the rest of the request body (small JSON); stop if the client has nothing
            // more to send within a short window.
            let body_deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            while tokio::time::Instant::now() < body_deadline {
                let mut tmp = [0u8; 1024];
                match tokio::time::timeout(Duration::from_millis(50), read_half.read(&mut tmp))
                    .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
            let _ = write_half.write_all(response).await;
            let _ = headers_tx.send(());
            // The client should close the connection once `stream_task.abort()` runs.
            tokio::spawn(async move {
                let mut after = [0u8; 1];
                match tokio::time::timeout(Duration::from_secs(3), read_half.read(&mut after)).await
                {
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        // `closed_tx` is a bounded mpsc sender; `send` is async and must be
                        // awaited or the message is dropped without being delivered.
                        let _ = closed_tx.send(()).await;
                    }
                    _ => {}
                }
            });
            let _ = done_rx.recv().await;
        });

        let prev_url = std::env::var("DOTZ_LOCAL_BASE_URL").ok();
        let prev_timeout = std::env::var("DOTZ_SUBAGENT_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_LOCAL_BASE_URL", format!("http://127.0.0.1:{port}/v1"));
        std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", "500");

        let agent = AgentConfig {
            name: "test".into(),
            description: "test".into(),
            tools: None,
            model: None,
            system_prompt: "sys".into(),
            source: "test",
        };
        let cwd = std::env::temp_dir().to_string_lossy().to_string();

        let mut run = tokio::spawn(async move {
            let agents = [agent];
            run_single_agent(&agents, "test", "task", Some("local/test"), &cwd, None, None).await
        });

        tokio::select! {
            _ = headers_rx => {}
            r = &mut run => panic!("run_single_agent finished before provider stream started: {r:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.stop_reason.as_deref(), Some("timeout"));

        match prev_url {
            Some(p) => std::env::set_var("DOTZ_LOCAL_BASE_URL", p),
            None => std::env::remove_var("DOTZ_LOCAL_BASE_URL"),
        }
        match prev_timeout {
            Some(p) => std::env::set_var("DOTZ_SUBAGENT_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_SUBAGENT_TIMEOUT_MS"),
        }

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), closed_rx.recv()).await,
            Ok(Some(())),
            "timed-out subagent must abort the provider stream task and close the connection"
        );
        let _ = done_tx.send(()).await;
    }
}
