//! Google Gemini `streamGenerateContent` adapter (Phase 4). Implements the same `Provider` trait as
//! the OpenAI Chat adapter and emits the SAME `StreamDelta` variants, so the session is provider-
//! agnostic.
//!
//! The session assembles OpenAI-shape messages + tool specs. This adapter translates that into the
//! native Gemini `generateContent` shape:
//!   - the leading `{role:"system"}` message → top-level `systemInstruction`
//!   - user text → `{role:"user", parts:[{text}]}`; assistant text → `{role:"model", parts:[{text}]}`
//!   - assistant `tool_calls` → `{role:"model", parts:[{functionCall:{name,args}}]}`
//!   - `{role:"tool"}` messages → `{role:"user", parts:[{functionResponse:{name,response}}]}` (the
//!     function name is recovered from the preceding assistant `tool_calls` by id)
//!   - OpenAI `{type:"function", function:{name,description,parameters}}` tool specs →
//!     Gemini `tools:[{functionDeclarations:[{name,description,parameters}]}]`
//!
//! Streaming (alt=sse) yields JSON frames with `candidates[].content.parts[]` — plain text → Text,
//! parts flagged `thought:true` → Thinking, `functionCall` → ToolCall, plus `usageMetadata` → Usage.
//! Auth resolves the `$GEMINI_API_KEY` (or `$GOOGLE_API_KEY`) reference via the shared resolver and
//! is sent as the `key` query parameter.
use super::event::{Cost, Usage};
use super::provider::{resolve_api_key, ChatRequest, Provider, StreamDelta};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub struct GoogleGemini {
    client: reqwest::Client,
}

impl GoogleGemini {
    pub fn new() -> Self {
        GoogleGemini {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for GoogleGemini {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GoogleGemini {
    async fn stream(&self, req: ChatRequest, tx: mpsc::Sender<StreamDelta>) -> Result<(), String> {
        let key = resolve_api_key(&req.model.api_key_ref);
        // base_url points at the Gemini API root (https://generativelanguage.googleapis.com/v1beta).
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            req.model.base_url.trim_end_matches('/'),
            req.model.model_id,
            key,
        );

        let (system_instruction, contents) = convert_messages(&req.messages);

        let mut body = json!({ "contents": contents });
        if let Some(sys) = system_instruction {
            body["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
        }
        if !req.tools.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": convert_tools(&req.tools) }]);
        }
        // Thinking: enable Gemini's thinking + include thought summaries so we can map them.
        if req.reasoning_effort.is_some() {
            body["generationConfig"] = json!({
                "thinkingConfig": { "includeThoughts": true }
            });
        }

        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request to gemini failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!(
                "google returned {status}: {}",
                truncate(&detail, 400)
            ));
        }

        let mut stream = resp.bytes_stream().eventsource();

        let mut usage_seen: Option<Usage> = None;
        let mut stop: Option<String> = None;
        // Gemini emits whole functionCall parts (not streamed-arg fragments); we synthesize a
        // ToolCallStart + ToolCallArgs pair per call, keyed by a monotonically increasing index.
        let mut tool_index: usize = 0;

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
                Err(_) => continue,
            };

            if let Some(u) = v.get("usageMetadata") {
                usage_seen = Some(parse_usage(u));
            }

            let candidate = match v.get("candidates").and_then(|c| c.get(0)) {
                Some(c) => c,
                None => continue,
            };
            if let Some(fr) = candidate.get("finishReason").and_then(|f| f.as_str()) {
                stop = Some(map_finish_reason(fr));
            }

            let parts = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array());
            let parts = match parts {
                Some(p) => p,
                None => continue,
            };

            for part in parts {
                // functionCall part → a tool call.
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let index = tool_index;
                    tool_index += 1;
                    let id = format!("call_{index}");
                    let _ = tx
                        .send(StreamDelta::ToolCallStart { index, id, name })
                        .await;
                    let _ = tx
                        .send(StreamDelta::ToolCallArgs {
                            index,
                            json: args.to_string(),
                        })
                        .await;
                    continue;
                }
                // text part → Thinking when flagged thought:true, else Text.
                if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                    if t.is_empty() {
                        continue;
                    }
                    let is_thought = part
                        .get("thought")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    if is_thought {
                        let _ = tx.send(StreamDelta::Thinking(t.to_string())).await;
                    } else {
                        let _ = tx.send(StreamDelta::Text(t.to_string())).await;
                    }
                }
            }
        }

        if let Some(u) = usage_seen {
            let _ = tx.send(StreamDelta::Usage(u)).await;
        }
        let _ = tx
            .send(StreamDelta::Stop(stop.unwrap_or_else(|| "stop".into())))
            .await;
        Ok(())
    }
}

/// Translate OpenAI-shape messages → (systemInstruction, Gemini contents[]). The leading system
/// message is hoisted out; `tool` messages become `functionResponse` parts whose `name` is recovered
/// from the preceding assistant `tool_calls` (Gemini keys results by function name, not call id).
fn convert_messages(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();
    // tool_call_id → function name, populated as we see assistant tool_calls.
    let mut id_to_name: HashMap<String, String> = HashMap::new();

    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                    system = Some(c.to_string());
                }
            }
            "user" => {
                let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                out.push(json!({ "role": "user", "parts": [{ "text": text }] }));
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = m.get("content").and_then(|c| c.as_str()) {
                    if !t.is_empty() {
                        parts.push(json!({ "text": t }));
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
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}");
                        let args: Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        if !id.is_empty() {
                            id_to_name.insert(id.to_string(), name.to_string());
                        }
                        parts.push(json!({ "functionCall": { "name": name, "args": args } }));
                    }
                }
                if parts.is_empty() {
                    parts.push(json!({ "text": "" }));
                }
                out.push(json!({ "role": "model", "parts": parts }));
            }
            "tool" => {
                let id = m.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("");
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                // Recover the function name; fall back to the id if unknown.
                let name = id_to_name
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_string());
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "result": content },
                        }
                    }],
                }));
            }
            _ => {}
        }
    }

    (system, out)
}

/// OpenAI `{type:"function", function:{name,description,parameters}}` → Gemini
/// `{name, description, parameters}` (a functionDeclaration).
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
            let params = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            Some(json!({
                "name": name,
                "description": description,
                "parameters": params,
            }))
        })
        .collect()
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let input = g("promptTokenCount");
    let output = g("candidatesTokenCount");
    let cache_read = g("cachedContentTokenCount");
    let total = u
        .get("totalTokenCount")
        .and_then(|x| x.as_u64())
        .unwrap_or(input + output);
    Usage {
        input,
        output,
        cache_read,
        cache_write: 0,
        total_tokens: total,
        cost: Cost::default(),
    }
}

/// Gemini finishReason → dotz stopReason. STOP→stop, MAX_TOKENS→max_tokens; a functionCall turn
/// reports STOP too — the session derives tool_use from the presence of tool-call deltas, so STOP
/// is safe here.
fn map_finish_reason(fr: &str) -> String {
    match fr {
        "MAX_TOKENS" => "max_tokens".into(),
        "STOP" => "stop".into(),
        other => other.to_lowercase(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
