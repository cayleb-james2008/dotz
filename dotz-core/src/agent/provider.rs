//! LLM provider layer. The primary (and only fully-wired) adapter is the OpenAI Chat-Completions
//! streaming client, which covers the 9 OpenAI-compatible providers dotz uses
//! (openrouter, ollama, openai, groq, mistral, xai, deepseek, cohere, local) — they differ only by
//! baseURL + auth. Anthropic (Messages API) and Google (Gemini API) have native adapters of their
//! own (see provider_anthropic.rs / provider_google.rs), selected by `adapter_for`.
//!
//! Auth mirrors the .pi extensions: an `apiKey` value of the form `$VAR` (or `${VAR}`) is resolved
//! from the environment at request time; a bare literal is sent verbatim. The Ollama Cloud key MUST
//! be referenced as `$OLLAMA_API_KEY` (see .pi/extensions/ollama-cloud/index.ts) — the `$` form is
//! what triggers env indirection.
//!
//! Reasoning trap (documented): DeepSeek / Ollama reasoning models stream their chain-of-thought in
//! `delta.reasoning_content`, separate from `delta.content`. We map reasoning_content → Thinking and
//! content → Text and NEVER fold one into the other — folding reasoning into text (or declaring the
//! model non-reasoning) yields an empty answer ("(no output)").
use super::event::Usage;
use crate::agent::event::Cost;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// One streamed increment from the provider, normalized across model dialects.
#[derive(Clone, Debug)]
pub enum StreamDelta {
    Text(String),
    Thinking(String),
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallArgs {
        index: usize,
        json: String,
    },
    Usage(Usage),
    Stop(String),
}

/// Provider-resolved catalog metadata for a model (baseURL + auth + context window).
#[derive(Clone, Debug)]
pub struct ResolvedModel {
    pub provider: String,
    pub model_id: String,
    pub base_url: String,
    pub api_key_ref: String,
    pub context_window: u64,
    pub reasoning: bool,
}

/// A request for a single streamed completion.
pub struct ChatRequest {
    pub model: ResolvedModel,
    /// OpenAI-shape messages: [{role, content}], already assembled by the session.
    pub messages: Vec<Value>,
    /// OpenAI-shape tool specs (function calling). Empty => omit `tools`.
    pub tools: Vec<Value>,
    /// reasoning_effort, mapped from the thinking level. None => omit.
    pub reasoning_effort: Option<String>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a completion, forwarding normalized deltas on `tx`. Returns Err on a transport/HTTP
    /// error (the session maps it to a stopReason:"error" message).
    async fn stream(&self, req: ChatRequest, tx: mpsc::Sender<StreamDelta>) -> Result<(), String>;
}

// ---- auth resolution (mirrors pi resolveConfigValue) ----

/// Resolve an apiKey reference. `$VAR` / `${VAR}` → env var value (empty if unset); a bare literal is
/// returned verbatim. This is the exact rule the .pi extensions rely on.
pub fn resolve_api_key(reference: &str) -> String {
    let r = reference.trim();
    if let Some(var) = r.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        return std::env::var(var).unwrap_or_default();
    }
    if let Some(var) = r.strip_prefix('$') {
        return std::env::var(var).unwrap_or_default();
    }
    r.to_string()
}

/// If `api_key_ref` names an env var (`$VAR` / `${VAR}`) but `resolved` came back empty, return a
/// clear, actionable error naming the variable. A bare-literal reference (including an empty one,
/// e.g. a local / no-auth provider) returns `None` so those setups keep working. Keeps
/// `resolve_api_key`'s `String` signature stable for its other callers.
fn missing_env_key_error(api_key_ref: &str, resolved: &str, provider: &str) -> Option<String> {
    if !resolved.trim().is_empty() {
        return None;
    }
    let r = api_key_ref.trim();
    let var = r
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .or_else(|| r.strip_prefix('$'))?;
    Some(format!(
        "missing api key for provider '{provider}': environment variable {var} is not set. \
         Set {var} and retry (an empty key 401s silently on Ollama Cloud)."
    ))
}

/// Configurable HTTP request timeout for every upstream LLM call. A hung provider connection
/// otherwise blocks the executive turn (or a subagent) indefinitely. Defaults to 5 minutes;
/// override with `DOTZ_PROVIDER_TIMEOUT_MS` (clamped to [1s, 1h]).
pub(crate) fn request_timeout() -> Duration {
    const DEFAULT_MS: u64 = 300_000; // 5 minutes
    const MIN_MS: u64 = 1_000; // 1 second
    const MAX_MS: u64 = 3_600_000; // 1 hour
    std::env::var("DOTZ_PROVIDER_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_MS))
}

// ---- catalog (port of the .pi provider registrations + types.ts PROVIDERS) ----

/// Base URL + apiKey-reference for each OpenAI-compatible provider. `local` honors
/// DOTZ_LOCAL_BASE_URL / DOTZ_LOCAL_API_KEY exactly like .pi/extensions/local.
fn provider_endpoint(provider: &str) -> Option<(String, String)> {
    let pair = |u: &str, k: &str| Some((u.to_string(), k.to_string()));
    match provider {
        "ollama" => pair("https://ollama.com/v1", "$OLLAMA_API_KEY"),
        "openrouter" => pair("https://openrouter.ai/api/v1", "$OPENROUTER_API_KEY"),
        "openai" => pair("https://api.openai.com/v1", "$OPENAI_API_KEY"),
        "groq" => pair("https://api.groq.com/openai/v1", "$GROQ_API_KEY"),
        "mistral" => pair("https://api.mistral.ai/v1", "$MISTRAL_API_KEY"),
        "xai" => pair("https://api.x.ai/v1", "$XAI_API_KEY"),
        "deepseek" => pair("https://api.deepseek.com/v1", "$DEEPSEEK_API_KEY"),
        "cohere" => pair("https://api.cohere.ai/compatibility/v1", "$COHERE_API_KEY"),
        // NVIDIA NIM (build.nvidia.com) — OpenAI-compatible; free preview models via an nvapi key.
        "nvidia-nim" => pair("https://integrate.api.nvidia.com/v1", "$NVIDIA_API_KEY"),
        "local" => {
            let base = std::env::var("DOTZ_LOCAL_BASE_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://localhost:11434/v1".into());
            let key = if std::env::var("DOTZ_LOCAL_API_KEY")
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            {
                "$DOTZ_LOCAL_API_KEY".to_string()
            } else {
                "local".to_string()
            };
            Some((base, key))
        }
        "anthropic" => pair("https://api.anthropic.com/v1", "$ANTHROPIC_API_KEY"),
        "google" => pair(
            "https://generativelanguage.googleapis.com/v1beta",
            "$GEMINI_API_KEY",
        ),
        _ => None,
    }
}

/// Known per-model context windows. Ollama Cloud models are listed explicitly; Anthropic and
/// Google use safe defaults (Claude 200k, Gemini 1M) since the native adapters accept any
/// upstream-valid model id. Everything else falls back to 256k.
fn context_window_for(provider: &str, model_id: &str) -> u64 {
    match (provider, model_id) {
        ("ollama", "glm-5.2") => 1_000_000,
        ("ollama", "minimax-m3") => 524_288,
        ("ollama", "deepseek-v4-pro") => 524_288,
        ("ollama", "kimi-k2.7-code") => 262_144,
        ("nvidia-nim", "z-ai/glm-5.2") => 1_000_000,
        ("local", _) => 32_768,
        ("anthropic", _) => 200_000,
        ("google", _) => 1_000_000,
        _ => 256_000,
    }
}

/// Resolve a provider/model into endpoint + metadata. All known providers resolve here: the
/// OpenAI-compatible providers use the shared chat-completions adapter, while anthropic/google use
/// their native adapters (selected by `adapter_for`). Model ids are passed through to the upstream
/// API; the upstream provider catalog is the source of truth for valid ids.
/// Default: ollama/glm-5.2.
pub fn resolve(provider: &str, model_id: &str) -> Option<ResolvedModel> {
    let (base_url, api_key_ref) = provider_endpoint(provider)?;
    Some(ResolvedModel {
        provider: provider.to_string(),
        model_id: model_id.to_string(),
        base_url,
        api_key_ref,
        context_window: context_window_for(provider, model_id),
        // All seeded Ollama/local models are reasoning models (see .pi extensions). For the other
        // OpenAI-compatible providers reasoning is gated by reasoning_effort being sent; true is safe.
        reasoning: true,
    })
}

/// Map a dotz thinking level → OpenAI `reasoning_effort`. "off" => None (omit the field).
pub fn reasoning_effort(thinking_level: &str) -> Option<String> {
    match thinking_level {
        "off" => None,
        // OpenAI's documented set is low|medium|high; minimal/xhigh are passed through for providers
        // (Ollama, xAI) that accept them — an unknown value is harmless (ignored upstream).
        other => Some(other.to_string()),
    }
}

// ---- OpenAI Chat-Completions streaming adapter ----

pub struct OpenAiChat {
    client: reqwest::Client,
}

impl OpenAiChat {
    pub fn new() -> Self {
        OpenAiChat {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiChat {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for OpenAiChat {
    async fn stream(&self, req: ChatRequest, tx: mpsc::Sender<StreamDelta>) -> Result<(), String> {
        let key = resolve_api_key(&req.model.api_key_ref);
        // Fail fast when a `$VAR`/`${VAR}` reference resolved to nothing: an unset key sends an
        // empty Bearer, and Ollama Cloud (and other hosted providers) answer 401 with an empty
        // reply that masquerades as a silent stop — or the turn just hangs to the timeout. A clear
        // error beats a 5-minute hang. Bare-literal keys (incl. empty, e.g. local/no-auth) pass
        // through untouched.
        if let Some(err) = missing_env_key_error(&req.model.api_key_ref, &key, &req.model.provider) {
            return Err(err);
        }
        let url = format!(
            "{}/chat/completions",
            req.model.base_url.trim_end_matches('/')
        );

        let mut body = json!({
            "model": req.model.model_id,
            "messages": req.messages,
            "stream": true,
            // Ask upstream for usage in the final SSE chunk (OpenAI + Ollama Cloud honor this).
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools);
        }
        if let Some(effort) = &req.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }

        let resp = self
            .client
            .post(&url)
            .timeout(request_timeout())
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!(
                "{} returned {status}: {}",
                req.model.provider,
                truncate(&detail, 400)
            ));
        }

        let mut stream = resp.bytes_stream().eventsource();
        let mut usage_seen: Option<Usage> = None;
        let mut stop: Option<String> = None;
        // Providers may stream a tool-call id and name across multiple chunks, sometimes sending
        // the id before the name. Track both the started indices and the id seen so far for each
        // index so we emit ToolCallStart exactly once, using the real id even when it arrived in
        // an earlier chunk.
        let mut tool_call_started: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut tool_call_ids: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();

        while let Some(ev) = stream.next().await {
            let ev = match ev {
                Ok(e) => e,
                Err(e) => return Err(format!("stream error: {e}")),
            };
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue, // skip a malformed/keepalive frame
            };

            // usage can arrive on a chunk with empty choices (stream_options.include_usage).
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                usage_seen = Some(parse_usage(u));
            }

            let choice = match v.get("choices").and_then(|c| c.get(0)) {
                Some(c) => c,
                None => continue,
            };
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                stop = Some(map_stop_reason(fr));
            }
            let delta = match choice.get("delta") {
                Some(d) => d,
                None => continue,
            };

            // Reasoning channel FIRST (separate block — never folded into text). DeepSeek/Ollama emit
            // `reasoning_content`; some OpenAI-compatible providers emit `reasoning`.
            if let Some(r) = delta.get("reasoning_content").and_then(|x| x.as_str()) {
                if !r.is_empty() {
                    let _ = tx.send(StreamDelta::Thinking(r.to_string())).await;
                }
            } else if let Some(r) = delta.get("reasoning").and_then(|x| x.as_str()) {
                if !r.is_empty() {
                    let _ = tx.send(StreamDelta::Thinking(r.to_string())).await;
                }
            }

            if let Some(c) = delta.get("content").and_then(|x| x.as_str()) {
                if !c.is_empty() {
                    let _ = tx.send(StreamDelta::Text(c.to_string())).await;
                }
            }

            // Tool calls (function calling). Each chunk carries a partial arg-fragment per index.
            // Some providers split the id and name across chunks; only emit ToolCallStart once per
            // index, but keep the real id even when it arrived before the name.
            if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    // Capture the id as soon as it appears, regardless of whether the name is here yet.
                    if let Some(id) = tc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        tool_call_ids.insert(index, id.to_string());
                    }
                    let func = tc.get("function");
                    let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str());
                    if let Some(name) = name {
                        if !name.is_empty() && tool_call_started.insert(index) {
                            let id = tool_call_ids
                                .get(&index)
                                .cloned()
                                .or_else(|| {
                                    tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                                })
                                .unwrap_or_else(|| format!("call_{index}"));
                            let _ = tx
                                .send(StreamDelta::ToolCallStart {
                                    index,
                                    id,
                                    name: name.to_string(),
                                })
                                .await;
                        }
                    }
                    if let Some(args) = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            let _ = tx
                                .send(StreamDelta::ToolCallArgs {
                                    index,
                                    json: args.to_string(),
                                })
                                .await;
                        }
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

/// OpenAI finish_reason → dotz stopReason. "stop"|"length"(→max_tokens)|"tool_calls"(→tool_use).
fn map_stop_reason(fr: &str) -> String {
    match fr {
        "length" => "max_tokens".into(),
        "tool_calls" => "tool_use".into(),
        other => other.to_string(), // "stop" stays "stop"
    }
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let input = g("prompt_tokens");
    let output = g("completion_tokens");
    // cached tokens, when the provider reports them (prompt_tokens_details.cached_tokens).
    let cache_read = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let total = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(input + output);
    Usage {
        input,
        output,
        cache_read,
        cache_write: 0,
        total_tokens: total,
        // Cost stays zero — the Ollama Cloud catalog has cost {0,0,0,0} (see .pi extension). A real
        // priced provider would multiply token counts by per-token cost here.
        cost: Cost::default(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Back up to the nearest UTF-8 char boundary at or before byte `n` so the slice
        // doesn't panic when a multi-byte character straddles the cut point — the common
        // case for real provider error bodies (JSON with Unicode, non-English messages).
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Pick the adapter for a provider id. anthropic → native Messages API, google → native Gemini API,
/// everything else (ollama/openrouter/openai/groq/mistral/xai/deepseek/cohere/local) → OpenAI-compatible.
pub fn adapter_for(provider: &str) -> Box<dyn Provider> {
    match provider {
        "anthropic" => Box::new(super::provider_anthropic::AnthropicMessages::new()),
        "google" => Box::new(super::provider_google::GoogleGemini::new()),
        _ => Box::new(OpenAiChat::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn sse(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    /// Some OpenAI-compatible providers stream a tool-call name across multiple chunks, or repeat
    /// the name in later fragments. Without the `tool_call_started` guard the adapter emitted a
    /// new `ToolCallStart` (with a freshly-generated id) for every chunk containing a name,
    /// producing duplicate tool-call blocks in the assistant message and breaking tool_call_id
    /// matching.
    #[tokio::test]
    async fn streaming_tool_call_start_emitted_once_per_index() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
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
            let _ = headers_tx.send(());

            let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
            let _ = stream.write_all(response).await;
            // First chunk establishes id + name.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"bash","arguments":""}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            // Second chunk repeats the name for the same index; must NOT start a second call.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"bash","arguments":"{"}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            // Third chunk completes the arguments.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"command\":\"echo hi\"}"}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            // Final chunk ends the turn.
            let _ = stream
                .write_all(sse(r#"{"choices":[{"finish_reason":"tool_calls"}]}"#).as_bytes())
                .await;
            // Close the connection so the client stream ends cleanly.
        });

        let client = OpenAiChat::new();
        let model = ResolvedModel {
            provider: "test".into(),
            model_id: "test".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            api_key_ref: "test-key".into(),
            context_window: 1000,
            reasoning: false,
        };
        let req = ChatRequest {
            model,
            messages: vec![],
            tools: vec![],
            reasoning_effort: None,
        };
        let (tx, mut rx) = mpsc::channel::<StreamDelta>(16);

        let stream_task = tokio::spawn(async move { client.stream(req, tx).await });
        // Wait until the fake server has accepted the request so we know streaming is in progress.
        let _ = headers_rx.await;

        let mut starts = 0;
        let mut arg_chunks = 0;
        let mut final_stop = String::new();
        while let Some(d) = rx.recv().await {
            match d {
                StreamDelta::ToolCallStart {
                    ref id, ref name, ..
                } => {
                    starts += 1;
                    assert_eq!(id, "call_abc");
                    assert_eq!(name, "bash");
                }
                StreamDelta::ToolCallArgs { .. } => arg_chunks += 1,
                StreamDelta::Stop(s) => final_stop = s,
                _ => {}
            }
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream_task)
            .await
            .expect("stream task timed out")
            .unwrap();
        assert!(result.is_ok(), "stream ended with error: {:?}", result);

        assert_eq!(
            starts, 1,
            "only one ToolCallStart should be emitted per index even when name repeats"
        );
        assert_eq!(
            arg_chunks, 2,
            "both argument fragments should still be delivered"
        );
        assert_eq!(final_stop, "tool_use");
    }

    /// Some OpenAI-compatible providers stream the tool-call id in one chunk and the name in a
    /// later chunk. The adapter must preserve that id so the emitted `ToolCallStart` carries the
    /// provider's real tool_call_id; otherwise the downstream tool result would use a generated
    /// fallback id and the API would reject the mismatched tool_call_id.
    #[tokio::test]
    async fn streaming_tool_call_id_preserved_across_chunks() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
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
            let _ = headers_tx.send(());

            let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
            let _ = stream.write_all(response).await;
            // First chunk carries only the id — the function name is empty/absent.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_real","function":{"name":"","arguments":""}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            // Second chunk finally carries the name for the same index.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"bash"}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            // Third chunk completes the arguments.
            let _ = stream
                .write_all(
                    sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"echo hi\"}"}}]}}]}"#)
                        .as_bytes(),
                )
                .await;
            let _ = stream
                .write_all(sse(r#"{"choices":[{"finish_reason":"tool_calls"}]}"#).as_bytes())
                .await;
        });

        let client = OpenAiChat::new();
        let model = ResolvedModel {
            provider: "test".into(),
            model_id: "test".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            api_key_ref: "test-key".into(),
            context_window: 1000,
            reasoning: false,
        };
        let req = ChatRequest {
            model,
            messages: vec![],
            tools: vec![],
            reasoning_effort: None,
        };
        let (tx, mut rx) = mpsc::channel::<StreamDelta>(16);

        let stream_task = tokio::spawn(async move { client.stream(req, tx).await });
        let _ = headers_rx.await;

        let mut start: Option<StreamDelta> = None;
        while let Some(d) = rx.recv().await {
            if matches!(d, StreamDelta::ToolCallStart { .. }) {
                start = Some(d);
            }
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stream_task)
            .await
            .expect("stream task timed out")
            .unwrap();
        assert!(result.is_ok(), "stream ended with error: {:?}", result);

        match start {
            Some(StreamDelta::ToolCallStart { id, name, .. }) => {
                assert_eq!(
                    id, "call_real",
                    "ToolCallStart must use the id from the earlier chunk, not a fallback"
                );
                assert_eq!(name, "bash");
            }
            other => panic!(
                "expected exactly one ToolCallStart with id 'call_real', got {:?}",
                other
            ),
        }
    }

    /// A hung provider that accepts the HTTP connection but never sends a response must not block
    /// the adapter forever. The configurable `DOTZ_PROVIDER_TIMEOUT_MS` bounds the wait and the
    /// adapter returns an error promptly.
    #[tokio::test]
    async fn provider_request_times_out_on_hung_endpoint() {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = LOCK.lock().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        // Server: accept the connection, drain request headers, then park forever.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::with_capacity(8192);
            let mut tmp = [0u8; 256];
            loop {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            std::future::pending::<()>().await;
        });

        let prev = std::env::var("DOTZ_PROVIDER_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_PROVIDER_TIMEOUT_MS", "750");

        let client = OpenAiChat::new();
        let model = ResolvedModel {
            provider: "test".into(),
            model_id: "test".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            api_key_ref: "test-key".into(),
            context_window: 1000,
            reasoning: false,
        };
        let req = ChatRequest {
            model,
            messages: vec![],
            tools: vec![],
            reasoning_effort: None,
        };
        let (tx, _rx) = mpsc::channel::<StreamDelta>(4);

        let start = tokio::time::Instant::now();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.stream(req, tx))
                .await
                .expect("test wrapper timed out waiting for provider timeout");
        let elapsed = start.elapsed();

        match prev {
            Some(p) => std::env::set_var("DOTZ_PROVIDER_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_PROVIDER_TIMEOUT_MS"),
        }
        server.abort();

        assert!(
            result.is_err(),
            "hung provider should return an error, got: {result:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "adapter should wait for the configured timeout before returning, elapsed: {elapsed:?}"
        );
    }

    /// Anthropic and Google have native adapters in this crate, but they still need a
    /// `ResolvedModel` (base URL + auth ref + context window) so `session::run_turn` and the
    /// REST `post_model` handler can route to them. Before the fix `provider::resolve` returned
    /// None for these providers, making them unusable end-to-end even though the adapters exist.
    #[test]
    fn resolve_returns_metadata_for_native_adapter_providers() {
        let anthropic = resolve("anthropic", "claude-sonnet-4").expect("anthropic should resolve");
        assert_eq!(anthropic.provider, "anthropic");
        assert_eq!(anthropic.base_url, "https://api.anthropic.com/v1");
        assert_eq!(anthropic.api_key_ref, "$ANTHROPIC_API_KEY");
        assert_eq!(anthropic.context_window, 200_000);
        assert!(anthropic.reasoning);

        let google = resolve("google", "gemini-2.5-pro").expect("google should resolve");
        assert_eq!(google.provider, "google");
        assert_eq!(
            google.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(google.api_key_ref, "$GEMINI_API_KEY");
        assert_eq!(google.context_window, 1_000_000);
        assert!(google.reasoning);
    }

    /// NVIDIA NIM (build.nvidia.com) is a free-form OpenAI-compatible provider: resolve must map it
    /// to the integrate.api.nvidia.com base + the nvapi key env ref, and pass any model id through.
    #[test]
    fn resolve_returns_nvidia_nim_openai_compatible_endpoint() {
        let m = resolve("nvidia-nim", "z-ai/glm-5.2").expect("nvidia-nim should resolve");
        assert_eq!(m.provider, "nvidia-nim");
        assert_eq!(m.base_url, "https://integrate.api.nvidia.com/v1");
        assert_eq!(m.api_key_ref, "$NVIDIA_API_KEY");
        assert_eq!(m.model_id, "z-ai/glm-5.2");
        assert_eq!(m.context_window, 1_000_000);
    }

    /// The native adapter providers accept arbitrary model ids (the upstream API validates them),
    /// just like the free-form OpenAI-compatible providers.
    #[test]
    fn resolve_accepts_any_model_id_for_native_adapter_providers() {
        assert!(resolve("anthropic", "any-model-id").is_some());
        assert!(resolve("google", "any-model-id").is_some());
    }

    /// A `$VAR` key that resolved empty must fail fast with an actionable, variable-named error —
    /// an unset `$OLLAMA_API_KEY` otherwise sends an empty Bearer and 401s silently. A bare literal
    /// (incl. an empty one, e.g. local/no-auth) must NOT error, so those setups keep working.
    #[test]
    fn missing_env_key_error_only_fires_for_unset_env_refs() {
        let err = missing_env_key_error("$OLLAMA_API_KEY", "", "ollama")
            .expect("unset $VAR must produce an error");
        assert!(err.contains("OLLAMA_API_KEY"), "names the var: {err}");
        assert!(err.contains("ollama"), "names the provider: {err}");
        // ${VAR} form too.
        assert!(missing_env_key_error("${GROQ_KEY}", "", "groq").is_some());
        // A resolved key is fine.
        assert!(missing_env_key_error("$OLLAMA_API_KEY", "sk-real", "ollama").is_none());
        // Bare literals (incl. empty) never error — local / no-auth providers.
        assert!(missing_env_key_error("", "", "local").is_none());
        assert!(missing_env_key_error("literal-key", "literal-key", "local").is_none());
    }

    /// `truncate` is called on upstream provider error bodies in every adapter's stream error
    /// path. The old `&s[..n]` panics when byte `n` falls inside a multi-byte UTF-8 character —
    /// the common case for real provider error responses (JSON with Unicode, non-English
    /// messages). This test plants a string whose `n`-th byte is the second byte of a 2-byte
    /// character and confirms `truncate` no longer panics and still emits the ellipsis marker.
    #[test]
    fn truncate_handles_multibyte_char_boundary_without_panicking() {
        // 399 ASCII chars + "é" (U+00E9, 2 UTF-8 bytes 0xC3 0xA9). Byte 400 is 0xA9 — the
        // second byte of "é" — which is NOT a char boundary, so `&s[..400]` panics.
        let input = format!("{}é", "a".repeat(399));
        let truncated = truncate(&input, 400);
        assert!(
            truncated.ends_with('…'),
            "truncated string should end with the ellipsis marker"
        );
        // The 399 ASCII chars must survive; only the split multi-byte char is dropped.
        assert!(
            truncated.starts_with(&"a".repeat(399)),
            "ASCII prefix before the cut should be preserved"
        );
        // Must not contain the raw bytes of the split character.
        assert!(
            !truncated.contains('é'),
            "the split multi-byte character must not appear in the truncated output"
        );
    }

    /// `truncate` must return the input unchanged when it fits within the cap.
    #[test]
    fn truncate_returns_input_unchanged_when_within_cap() {
        assert_eq!(truncate("hello", 400), "hello");
        assert_eq!(truncate("", 400), "");
    }
}
