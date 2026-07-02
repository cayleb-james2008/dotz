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
//! Auth resolves `$GEMINI_API_KEY` first, falling back to `$GOOGLE_API_KEY`, and sends the key
//! as the `key` query parameter.
use super::event::{Cost, Usage};
use super::provider::{request_timeout, resolve_api_key, ChatRequest, Provider, StreamDelta};
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

/// Resolve the Gemini API key. The provider endpoint references `$GEMINI_API_KEY`; if that is
/// unset we fall back to `$GOOGLE_API_KEY` so users with the more standard Google env var can
/// still use the Google provider without duplicating credentials.
fn resolve_google_key(api_key_ref: &str) -> String {
    let key = resolve_api_key(api_key_ref);
    if !key.is_empty() {
        return key;
    }
    std::env::var("GOOGLE_API_KEY").unwrap_or_default()
}

#[async_trait]
impl Provider for GoogleGemini {
    async fn stream(&self, req: ChatRequest, tx: mpsc::Sender<StreamDelta>) -> Result<(), String> {
        let key = resolve_google_key(&req.model.api_key_ref);
        // base_url points at the Gemini API root (https://generativelanguage.googleapis.com/v1beta).
        // The key goes in the x-goog-api-key header, NEVER the URL: reqwest's error Display
        // prints the full URL (query included), and that string reaches the UI, session
        // history, and /api/provider-health — a query-param key leaks on any network error.
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            req.model.base_url.trim_end_matches('/'),
            req.model.model_id,
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
            .timeout(request_timeout())
            .header("content-type", "application/json")
            .header("x-goog-api-key", &key)
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
                let parts = vec![json!({ "text": text })];
                merge_or_push(&mut out, "user", parts);
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
                merge_or_push(&mut out, "model", parts);
            }
            "tool" => {
                let id = m.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("");
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                // Recover the function name; fall back to the id if unknown.
                let name = id_to_name
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_string());
                let parts = vec![json!({
                    "functionResponse": {
                        "name": name,
                        "response": { "result": content },
                    }
                })];
                // tool results map to the "user" role, so they merge with a preceding user turn.
                merge_or_push(&mut out, "user", parts);
            }
            _ => {}
        }
    }

    (system, out)
}

/// Push a `{role, parts}` turn onto `out`, OR merge `parts` into the last turn when it has the
/// same role. Gemini's `streamGenerateContent` API requires strictly alternating user/model
/// roles in `contents`; an assistant `tool_calls` turn followed by N `tool` results would
/// otherwise produce N consecutive "user" turns, which Gemini rejects. This mirrors the
/// consecutive-role merge the Anthropic adapter already does.
fn merge_or_push(out: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                existing.extend(parts);
                return;
            }
        }
    }
    out.push(json!({ "role": role, "parts": parts }));
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
    use std::sync::Mutex;

    /// Serializes env-var mutation so concurrent tests don't race on GEMINI_API_KEY / GOOGLE_API_KEY.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_google_key_prefers_gemini_then_falls_back_to_google() {
        let _guard = ENV_LOCK.lock().unwrap();

        // Neither set → empty.
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");
        assert!(resolve_google_key("$GEMINI_API_KEY").is_empty());

        // GEMINI_API_KEY wins when present.
        std::env::set_var("GEMINI_API_KEY", "gemini-key");
        assert_eq!(resolve_google_key("$GEMINI_API_KEY"), "gemini-key");

        // Fallback to GOOGLE_API_KEY when GEMINI_API_KEY is absent.
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("GOOGLE_API_KEY", "google-key");
        assert_eq!(resolve_google_key("$GEMINI_API_KEY"), "google-key");

        // Cleanup.
        std::env::remove_var("GOOGLE_API_KEY");
    }

    #[test]
    fn resolve_google_key_returns_bare_literal_untouched() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");

        // A bare literal key is passed through (matches the shared resolve_api_key rule).
        assert_eq!(resolve_google_key("literal-key"), "literal-key");
    }

    /// Gemini's `streamGenerateContent` API requires strictly alternating user/model roles in
    /// `contents`. An OpenAI history with one assistant `tool_calls` turn followed by N `tool`
    /// result messages would otherwise produce N consecutive "user" turns, which Gemini rejects.
    /// `convert_messages` must collapse them into a single user turn carrying N
    /// `functionResponse` parts — mirroring the consecutive-role merge the Anthropic adapter does.
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
        let (_, contents) = convert_messages(&messages);
        assert_eq!(
            contents.len(),
            2,
            "two tool results must collapse into one user turn"
        );
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(
            parts.len(),
            2,
            "the single user turn must carry both functionResponse parts"
        );
        assert_eq!(parts[0]["functionResponse"]["name"], "bash");
        assert_eq!(parts[0]["functionResponse"]["response"]["result"], "out1");
        assert_eq!(parts[1]["functionResponse"]["name"], "read");
        assert_eq!(parts[1]["functionResponse"]["response"]["result"], "out2");
    }

    /// Consecutive plain user messages must also merge into a single user turn so Gemini sees a
    /// legal alternating history (no two user turns in a row).
    #[test]
    fn convert_messages_merges_consecutive_user_turns() {
        let messages = vec![
            json!({ "role": "user", "content": "part one" }),
            json!({ "role": "user", "content": "part two" }),
        ];
        let (_, contents) = convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "part one");
        assert_eq!(parts[1]["text"], "part two");
    }

    /// Alternating user/assistant turns with no tool results must stay as separate turns — the
    /// merge must only fire on consecutive same-role turns.
    #[test]
    fn convert_messages_keeps_alternating_roles_when_no_tool_results() {
        let messages = vec![
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "assistant", "content": "hello" }),
            json!({ "role": "user", "content": "bye" }),
        ];
        let (_, contents) = convert_messages(&messages);
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[2]["role"], "user");
    }
}
