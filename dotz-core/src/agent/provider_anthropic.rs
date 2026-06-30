//! Anthropic Messages API streaming adapter (Phase 4). Implements the same `Provider` trait as the
//! OpenAI Chat adapter and emits the SAME `StreamDelta` variants, so the session is provider-agnostic.
//!
//! The session assembles OpenAI-shape messages + tool specs (see `session::to_openai_messages` and
//! `tools::active_specs`). This adapter translates that shape into the native Anthropic Messages API:
//!   - the leading `{role:"system"}` message → the top-level `system` string
//!   - user/assistant text → content blocks; assistant `tool_calls` → `tool_use` blocks
//!   - `{role:"tool"}` messages → a user turn carrying a `tool_result` block
//!   - OpenAI `{type:"function", function:{name,description,parameters}}` tool specs →
//!     Anthropic `{name, description, input_schema}`
//!
//! Streaming maps Anthropic SSE events → StreamDelta:
//!   content_block_start(text|thinking|tool_use), content_block_delta(text_delta|thinking_delta|
//!   input_json_delta), message_delta(usage + stop_reason), message_stop.
//!
//! Thinking: when the session sends a reasoning_effort (any non-off thinking level), we request
//! adaptive thinking (`thinking:{type:"adaptive"}`) and map the effort onto `output_config.effort`
//! (clamped to Anthropic's low|medium|high|max set) per the claude-api skill. Auth resolves the
//! `$ANTHROPIC_API_KEY` reference via the shared `resolve_api_key`.
use super::event::{Cost, Usage};
use super::provider::{request_timeout, resolve_api_key, ChatRequest, Provider, StreamDelta};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicMessages {
    client: reqwest::Client,
}

impl AnthropicMessages {
    pub fn new() -> Self {
        AnthropicMessages {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for AnthropicMessages {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AnthropicMessages {
    async fn stream(&self, req: ChatRequest, tx: mpsc::Sender<StreamDelta>) -> Result<(), String> {
        let key = resolve_api_key(&req.model.api_key_ref);
        // base_url already points at the Anthropic root (https://api.anthropic.com/v1).
        let url = format!("{}/messages", req.model.base_url.trim_end_matches('/'));

        let (system, messages) = convert_messages(&req.messages);

        let mut body = json!({
            "model": req.model.model_id,
            // Anthropic requires max_tokens; mirror the streaming default the rest of dotz uses.
            "max_tokens": 64000,
            "messages": messages,
            "stream": true,
        });
        if let Some(sys) = system {
            body["system"] = json!(sys);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(convert_tools(&req.tools));
        }
        // Thinking: adaptive on, effort mapped from the dotz thinking level.
        if let Some(effort) = &req.reasoning_effort {
            body["thinking"] = json!({ "type": "adaptive" });
            body["output_config"] = json!({ "effort": map_effort(effort) });
        }

        let resp = self
            .client
            .post(&url)
            .timeout(request_timeout())
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!(
                "anthropic returned {status}: {}",
                truncate(&detail, 400)
            ));
        }

        let mut stream = resp.bytes_stream().eventsource();

        // Track input tokens (from message_start) + output tokens (from message_delta).
        let mut input_tokens: u64 = 0;
        let mut cache_read: u64 = 0;
        let mut cache_write: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut stop: Option<String> = None;
        // content_block index → tool-call provider index (only for tool_use blocks).
        // We forward the Anthropic block index as the StreamDelta index — the session keys tool
        // calls by index, so it just needs a stable per-call integer, which the block index is.

        while let Some(ev) = stream.next().await {
            let ev = match ev {
                Ok(e) => e,
                Err(e) => return Err(format!("stream error: {e}")),
            };
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue, // skip ping / malformed frames
            };
            let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match kind {
                "message_start" => {
                    if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                        input_tokens = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        cache_read = u
                            .get("cache_read_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                        cache_write = u
                            .get("cache_creation_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                    }
                }
                "content_block_start" => {
                    let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let block = v.get("content_block");
                    let btype = block
                        .and_then(|b| b.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    match btype {
                        "tool_use" => {
                            let id = block
                                .and_then(|b| b.get("id"))
                                .and_then(|i| i.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("call_{index}"));
                            let name = block
                                .and_then(|b| b.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx
                                .send(StreamDelta::ToolCallStart { index, id, name })
                                .await;
                        }
                        // text / thinking blocks may carry initial content; forward it if present.
                        "text" => {
                            if let Some(t) =
                                block.and_then(|b| b.get("text")).and_then(|x| x.as_str())
                            {
                                if !t.is_empty() {
                                    let _ = tx.send(StreamDelta::Text(t.to_string())).await;
                                }
                            }
                        }
                        "thinking" => {
                            if let Some(t) = block
                                .and_then(|b| b.get("thinking"))
                                .and_then(|x| x.as_str())
                            {
                                if !t.is_empty() {
                                    let _ = tx.send(StreamDelta::Thinking(t.to_string())).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let delta = v.get("delta");
                    let dtype = delta
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    match dtype {
                        "text_delta" => {
                            if let Some(t) =
                                delta.and_then(|d| d.get("text")).and_then(|x| x.as_str())
                            {
                                if !t.is_empty() {
                                    let _ = tx.send(StreamDelta::Text(t.to_string())).await;
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta
                                .and_then(|d| d.get("thinking"))
                                .and_then(|x| x.as_str())
                            {
                                if !t.is_empty() {
                                    let _ = tx.send(StreamDelta::Thinking(t.to_string())).await;
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(j) = delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(|x| x.as_str())
                            {
                                if !j.is_empty() {
                                    let _ = tx
                                        .send(StreamDelta::ToolCallArgs {
                                            index,
                                            json: j.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(u) = v.get("usage") {
                        if let Some(o) = u.get("output_tokens").and_then(|x| x.as_u64()) {
                            output_tokens = o;
                        }
                    }
                    if let Some(sr) = v
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                    {
                        stop = Some(map_stop_reason(sr));
                    }
                }
                "message_stop" => break,
                // error event mid-stream: surface it.
                "error" => {
                    let msg = v
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("anthropic stream error");
                    return Err(msg.to_string());
                }
                _ => {}
            }
        }

        let usage = Usage {
            input: input_tokens,
            output: output_tokens,
            cache_read,
            cache_write,
            total_tokens: input_tokens + output_tokens + cache_read + cache_write,
            cost: Cost::default(),
        };
        let _ = tx.send(StreamDelta::Usage(usage)).await;
        let _ = tx
            .send(StreamDelta::Stop(stop.unwrap_or_else(|| "stop".into())))
            .await;
        Ok(())
    }
}

/// Translate OpenAI-shape messages → (system, Anthropic messages[]). The leading system message is
/// hoisted to the top-level `system`; `tool` messages become a user turn with a `tool_result` block;
/// assistant `tool_calls` become `tool_use` blocks.
///
/// Anthropic requires user/assistant roles to strictly alternate. OpenAI history can contain
/// multiple consecutive `tool` results after one assistant `tool_calls` message, so this function
/// merges consecutive turns that map to the same Anthropic role (user/tool → "user").
fn convert_messages(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();

    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "system" {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                system = Some(c.to_string());
            }
            continue;
        }

        let anthropic_role = match role {
            "user" | "tool" => "user",
            "assistant" => "assistant",
            _ => continue,
        };

        let mut blocks: Vec<Value> = Vec::new();
        match role {
            "user" => {
                let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                blocks.push(json!({ "type": "text", "text": text }));
            }
            "assistant" => {
                if let Some(t) = m.get("content").and_then(|c| c.as_str()) {
                    if !t.is_empty() {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                        let func = tc.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        // OpenAI stores arguments as a JSON string; Anthropic wants a parsed object.
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
            }
            "tool" => {
                let tool_use_id = m.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("");
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                }));
            }
            _ => {}
        }

        // Merge consecutive turns that map to the same Anthropic role. Without this, a single
        // assistant tool_calls turn followed by N tool results would produce N consecutive user
        // turns, which Anthropic rejects.
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(anthropic_role) {
                if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                    content.extend(blocks);
                    continue;
                }
            }
        }
        out.push(json!({ "role": anthropic_role, "content": blocks }));
    }

    (system, out)
}

/// OpenAI `{type:"function", function:{name,description,parameters}}` → Anthropic
/// `{name, description, input_schema}`.
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name")?.as_str()?;
            let description = func
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": schema,
            }))
        })
        .collect()
}

/// dotz thinking level → Anthropic effort. Anthropic accepts low|medium|high|max (xhigh is rolled
/// into high here); unknown values clamp to "high".
fn map_effort(effort: &str) -> &str {
    match effort {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "max" => "max",
        _ => "high",
    }
}

/// Anthropic stop_reason → dotz stopReason. end_turn→stop, max_tokens stays, tool_use stays,
/// stop_sequence→stop.
fn map_stop_reason(sr: &str) -> String {
    match sr {
        "end_turn" | "stop_sequence" => "stop".into(),
        "max_tokens" => "max_tokens".into(),
        "tool_use" => "tool_use".into(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Back up to the nearest UTF-8 char boundary at or before byte `n` so the slice
        // doesn't panic when a multi-byte character straddles the cut point.
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_messages_hoists_system_to_top_level() {
        let messages = vec![
            json!({ "role": "system", "content": "sys" }),
            json!({ "role": "user", "content": "hello" }),
        ];
        let (system, anthropic) = convert_messages(&messages);
        assert_eq!(system, Some("sys".to_string()));
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["role"], "user");
        let content = anthropic[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
    }

    #[test]
    fn convert_messages_maps_assistant_tool_calls_to_tool_use() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "bash", "arguments": "{\"command\":\"echo hi\"}" }
            }]
        })];
        let (system, anthropic) = convert_messages(&messages);
        assert_eq!(system, None);
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["role"], "assistant");
        let content = anthropic[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "call_1");
        assert_eq!(content[0]["name"], "bash");
        assert_eq!(content[0]["input"]["command"], "echo hi");
    }

    /// Anthropic rejects consecutive user turns. OpenAI history has one assistant `tool_calls`
    /// turn followed by N `tool` result turns; those must collapse into a single user turn
    /// with N `tool_result` blocks.
    #[test]
    fn convert_messages_groups_consecutive_tool_results_into_one_user_turn() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "bash", "arguments": "{}" } },
                    { "id": "call_2", "type": "function", "function": { "name": "read", "arguments": "{}" } }
                ]
            }),
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "out1" }),
            json!({ "role": "tool", "tool_call_id": "call_2", "content": "out2" }),
        ];
        let (_, anthropic) = convert_messages(&messages);
        assert_eq!(anthropic.len(), 2);
        assert_eq!(anthropic[0]["role"], "assistant");
        assert_eq!(anthropic[1]["role"], "user");
        let tool_results = anthropic[1]["content"].as_array().unwrap();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[0]["type"], "tool_result");
        assert_eq!(tool_results[0]["tool_use_id"], "call_1");
        assert_eq!(tool_results[0]["content"], "out1");
        assert_eq!(tool_results[1]["type"], "tool_result");
        assert_eq!(tool_results[1]["tool_use_id"], "call_2");
        assert_eq!(tool_results[1]["content"], "out2");
    }

    #[test]
    fn convert_messages_keeps_alternating_roles_when_no_tool_results() {
        let messages = vec![
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "assistant", "content": "hello" }),
            json!({ "role": "user", "content": "bye" }),
        ];
        let (_, anthropic) = convert_messages(&messages);
        assert_eq!(anthropic.len(), 3);
        assert_eq!(anthropic[0]["role"], "user");
        assert_eq!(anthropic[1]["role"], "assistant");
        assert_eq!(anthropic[2]["role"], "user");
    }

    #[test]
    fn convert_messages_merges_consecutive_user_turns() {
        let messages = vec![
            json!({ "role": "user", "content": "part one" }),
            json!({ "role": "user", "content": "part two" }),
        ];
        let (_, anthropic) = convert_messages(&messages);
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["role"], "user");
        let content = anthropic[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "part one");
        assert_eq!(content[1]["text"], "part two");
    }

    #[test]
    fn convert_tools_maps_openai_function_to_anthropic_tool() {
        let tools = vec![json!({
            "type": "function",
            "function": { "name": "bash", "description": "run shell", "parameters": { "type": "object" } }
        })];
        let anthropic = convert_tools(&tools);
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["name"], "bash");
        assert_eq!(anthropic[0]["description"], "run shell");
        assert_eq!(anthropic[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn map_effort_clamps_xhigh_to_max() {
        assert_eq!(map_effort("xhigh"), "max");
        assert_eq!(map_effort("max"), "max");
        assert_eq!(map_effort("minimal"), "low");
        assert_eq!(map_effort("unknown"), "high");
    }

    #[test]
    fn map_stop_reason_maps_anthropic_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "max_tokens");
        assert_eq!(map_stop_reason("tool_use"), "tool_use");
        // Unknown stop reasons are passed through verbatim (case preserved).
        assert_eq!(map_stop_reason("OTHER"), "OTHER");
    }
}
