//! Agent event + message types. These serialize to EXACTLY the JSON shapes captured in
//! `~/.claude/dotz-rust/fixtures/ws_events.json` so the unchanged `web/app.js` renders them.
//!
//! The web UI (app.js `handleEvent`) consumes:
//!   agent_start → turn_start → message_start → message_update×N → message_end → turn_end → agent_end
//! and replaces the assistant bubble from `assistantMessageEvent.partial.content` on each update.
use serde::Serialize;
use serde_json::Value;

/// Cost breakdown — every field always serialized (the UI reads `usage.cost.total`).
#[derive(Clone, Debug, Serialize, Default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

/// Token usage + cost — matches the contract `{input, output, cacheRead, cacheWrite, totalTokens, cost}`.
#[derive(Clone, Debug, Serialize, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    pub cost: Cost,
}

/// A content block inside a message. Tagged by `type`: thinking | text | toolCall.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(rename = "thinkingSignature")]
        thinking_signature: String,
    },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// A chat message. A user message carries only `role/content/timestamp`; an assistant message also
/// carries `api/provider/model/usage/stopReason/responseId`. The `#[serde(skip_serializing_if)]`
/// guards keep a user message's JSON to exactly the captured `{role, content, timestamp}` shape.
#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

impl Message {
    pub fn user(text: &str, timestamp: i64) -> Self {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            api: None,
            provider: None,
            model: None,
            usage: None,
            stop_reason: None,
            error_message: None,
            timestamp,
            response_id: None,
        }
    }

    /// A fresh assistant message shell (empty content) for the start of a turn.
    pub fn assistant_shell(provider: &str, model: &str, timestamp: i64) -> Self {
        Message {
            role: "assistant".into(),
            content: Vec::new(),
            api: Some("openai-completions".into()),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            usage: Some(Usage::default()),
            stop_reason: Some("stop".into()),
            error_message: None,
            timestamp,
            response_id: None,
        }
    }
}

/// The `assistantMessageEvent` envelope carried by `message_update`. `type` is the streaming sub-kind
/// (thinking_start, thinking_delta, text_delta, …); `partial` is the FULL current assistant snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct AssistantMessageEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "contentIndex")]
    pub content_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub partial: Message,
}

/// A tool result attached to `turn_end.toolResults`. Shape mirrors pi's ToolResult subset the UI reads.
#[derive(Clone, Debug, Serialize)]
pub struct ToolResult {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    pub result: Value,
}

/// The agent event union — serializes with an internal `type` tag, matching the contract exactly.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "message_start")]
    MessageStart { message: Message },
    #[serde(rename = "message_update")]
    MessageUpdate {
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: AssistantMessageEvent,
        message: Message,
    },
    #[serde(rename = "message_end")]
    MessageEnd { message: Message },
    #[serde(rename = "turn_end")]
    TurnEnd {
        message: Message,
        #[serde(rename = "toolResults")]
        tool_results: Vec<ToolResult>,
    },
    #[serde(rename = "agent_end")]
    AgentEnd {
        messages: Vec<Message>,
        #[serde(rename = "willRetry")]
        will_retry: bool,
    },
    // Tool-turn events the UI consumes (handleEvent: tool_execution_start/update/end).
    #[serde(rename = "tool_execution_start")]
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: Value,
    },
    #[serde(rename = "tool_execution_end")]
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "isError")]
        is_error: bool,
        result: Value,
    },
    #[serde(rename = "tool_execution_update")]
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "partialResult")]
        partial_result: Value,
    },
    /// A subagent's streaming reasoning/findings, forwarded live to the lead's WebSocket so the
    /// operator (and the orchestrator) can see a drifting scout/planner's thinking as it happens
    /// rather than only after the subagent finishes.
    #[serde(rename = "subagent_progress")]
    SubagentProgress {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        agent: String,
        task: String,
        /// The assistant message the subagent is building, mid-stream.
        partial: Message,
    },
}

/// Wrap an agent event as the WS frame the server pushes: `{kind:"event", sessionId, event}`.
pub fn ws_frame(session_id: &str, event: &AgentEvent) -> Value {
    serde_json::json!({ "kind": "event", "sessionId": session_id, "event": event })
}
