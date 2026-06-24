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

/// Known per-model context windows for the seeded Ollama Cloud catalog (others fall back to 256k).
fn context_window_for(provider: &str, model_id: &str) -> u64 {
    match (provider, model_id) {
        ("ollama", "glm-5.2") => 1_000_000,
        ("ollama", "minimax-m3") => 524_288,
        ("ollama", "deepseek-v4-pro") => 524_288,
        ("ollama", "kimi-k2.7-code") => 262_144,
        ("local", _) => 32_768,
        _ => 256_000,
    }
}

/// Resolve a provider/model into endpoint + metadata. free-form providers (ollama/openrouter/local)
/// accept ANY model id; OpenAI-compatible catalog providers also accept any id (the upstream API
/// validates). Anthropic/Google return None here (their native adapters resolve endpoints themselves).
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
            if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let func = tc.get("function");
                    let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str());
                    if let Some(name) = name {
                        if !name.is_empty() {
                            let id = tc
                                .get("id")
                                .and_then(|i| i.as_str())
                                .map(|s| s.to_string())
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
        format!("{}…", &s[..n])
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
