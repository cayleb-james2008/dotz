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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
///
/// Q4: when a `$VAR` / `${VAR}` env var is unset or empty, falls back to the same key read from
/// `~/.pi/agent/auth.json` (the documented auth path — see `.env.example`). This makes the in-UI
/// key setter (`POST /api/provider/key`) actually work without a restart: the route writes the
/// key to auth.json under the env-var name, and this resolver consults auth.json when the env
/// var is missing. The auth.json read is cached in a `OnceLock` for the process life (see
/// `auth` module ponytail ceiling); a restart picks up POST-written keys.
pub fn resolve_api_key(reference: &str) -> String {
    let r = reference.trim();
    if let Some(var) = r.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
        // Env var missing/empty → fall back to auth.json (keyed by the same var name).
        if let Some(key) = crate::auth::lookup_key(var) {
            return key;
        }
        return String::new();
    }
    if let Some(var) = r.strip_prefix('$') {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
        if let Some(key) = crate::auth::lookup_key(var) {
            return key;
        }
        return String::new();
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

/// C6: the named gateway presets the UI offers when the gateway provider is selected. Each
/// preset pre-fills the base URL + key ref so the operator doesn't have to look them up. The
/// preset IDs are persisted in `GatewayConfig::presets` so the UI can pre-select the operator's
/// last choice on reload. `custom` is the empty preset (the operator fills in both fields).
pub const GATEWAY_PRESET_CUSTOM: &str = "custom";
pub const GATEWAY_PRESET_OMNIROUTE: &str = "omniroute";
pub const GATEWAY_PRESET_OPENROUTER_GW: &str = "openrouter-gw";
pub const GATEWAY_PRESET_LITELLM: &str = "litellm";

/// One named gateway preset. `key_ref` is an env-var NAME (e.g. `OMNIROUTE_API_KEY`), NOT a `$VAR`
/// reference — `provider_endpoint` wraps it in `$` so `resolve_api_key` resolves it via the same
/// env → auth.json fallback as the other providers. The key value is NEVER stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub key_ref: &'static str,
}

/// The named gateway presets, in UI order. `custom` is last so it's the "none of the above" option.
/// These are the three named presets the spec calls out (OmniRoute + OpenRouter-as-gateway +
/// LiteLLM); any other OpenAI-compatible gateway uses `custom`.
pub fn gateway_presets() -> Vec<GatewayPreset> {
    vec![
        GatewayPreset {
            id: GATEWAY_PRESET_OMNIROUTE,
            label: "OmniRoute",
            base_url: "https://api.omniroute.ai/v1",
            key_ref: "OMNIROUTE_API_KEY",
        },
        GatewayPreset {
            id: GATEWAY_PRESET_OPENROUTER_GW,
            label: "OpenRouter (gateway mode)",
            base_url: "https://openrouter.ai/api/v1",
            key_ref: "OPENROUTER_API_KEY",
        },
        GatewayPreset {
            id: GATEWAY_PRESET_LITELLM,
            label: "LiteLLM",
            base_url: "http://localhost:4000/v1",
            key_ref: "LITELLM_API_KEY",
        },
        GatewayPreset {
            id: GATEWAY_PRESET_CUSTOM,
            label: "Custom",
            base_url: "",
            key_ref: "",
        },
    ]
}

/// C6: validate a gateway base URL for SSRF safety. Allowed:
/// - `https://` anywhere (encrypted, trusted).
/// - `http://localhost` / `http://127.0.0.1` (loopback) — permits a local LiteLLM proxy.
///
/// Rejected: any other `http://` (would let a misconfigured gateway point dotz at an arbitrary
/// internal host). Empty/whitespace is rejected (the POST handler requires a non-empty URL).
/// Returns `Ok(())` on accept, `Err(message)` on reject.
pub fn validate_gateway_base_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if u.is_empty() {
        return Err("baseUrl is required".to_string());
    }
    if let Some(rest) = u.strip_prefix("https://") {
        if rest.is_empty() {
            return Err("baseUrl 'https://' has no host".to_string());
        }
        return Ok(());
    }
    if let Some(rest) = u.strip_prefix("http://") {
        // Permit only loopback hosts so a misconfigured gateway can't redirect dotz at an
        // arbitrary internal endpoint. `localhost` / `127.0.0.1` (optionally with a port) are
        // the documented local LiteLLM proxy hosts.
        let host = rest
            .split('/')
            .next()
            .unwrap_or(rest)
            .split(':')
            .next()
            .unwrap_or(rest);
        if host == "localhost" || host == "127.0.0.1" {
            return Ok(());
        }
        return Err(format!(
            "baseUrl must be https://, or http://localhost / http://127.0.0.1 \
             (a non-loopback http:// URL is SSRF-unsafe): {u}"
        ));
    }
    Err(format!(
        "baseUrl must start with https:// or http://localhost / http://127.0.0.1: {u}"
    ))
}

/// Base URL + apiKey-reference for each OpenAI-compatible provider. `local` honors
/// DOTZ_LOCAL_BASE_URL / DOTZ_LOCAL_API_KEY exactly like .pi/extensions/local.
/// C6: `gateway` reads its base URL + key ref from the persisted `GatewayConfig` (not a hardcoded
/// map) so the operator points it at OmniRoute / OpenRouter-as-gateway / LiteLLM / any
/// OpenAI-compat gateway. The key ref is stored as a bare env-var NAME in config; we wrap it in
/// `$` here so `resolve_api_key` resolves it via the same env → auth.json fallback as the others.
/// Returns None when the gateway section is missing/empty (so the gateway is inert until
/// configured — `resolve` returns None and the session/subagent code surfaces a clear error).
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
        // C6: gateway reads its endpoint from the persisted config. The key ref is a bare env-var
        // name in config (e.g. "OMNIROUTE_API_KEY"); we wrap it in `$` so resolve_api_key applies
        // the env → auth.json fallback. An empty/missing section returns None (gateway is inert).
        "gateway" => {
            let gw = crate::config::load().gateway?;
            if gw.is_empty() {
                return None;
            }
            let base = gw.base_url.trim().to_string();
            if base.is_empty() {
                return None;
            }
            let key_ref = if gw.api_key_ref.trim().is_empty() {
                // No key configured — empty ref → resolve_api_key returns empty (no-auth gateway).
                String::new()
            } else {
                format!("${}", gw.api_key_ref.trim())
            };
            Some((base, key_ref))
        }
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
        if let Some(err) = missing_env_key_error(&req.model.api_key_ref, &key, &req.model.provider)
        {
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
        assert!(result.is_ok(), "stream ended with error: {result:?}");

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
        assert!(result.is_ok(), "stream ended with error: {result:?}");

        match start {
            Some(StreamDelta::ToolCallStart { id, name, .. }) => {
                assert_eq!(
                    id, "call_real",
                    "ToolCallStart must use the id from the earlier chunk, not a fallback"
                );
                assert_eq!(name, "bash");
            }
            other => {
                panic!("expected exactly one ToolCallStart with id 'call_real', got {other:?}")
            }
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
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_PROVIDER_TIMEOUT_MS", "750") };

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
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_PROVIDER_TIMEOUT_MS", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_PROVIDER_TIMEOUT_MS") },
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

    /// Q4 acceptance: when a `$VAR` env var is unset, `resolve_api_key` must fall back to the
    /// same key read from `~/.pi/agent/auth.json`. This is what makes the in-UI key setter
    /// (POST /api/provider/key) actually work without a restart: the route writes the key to
    /// auth.json under the env-var name, and this resolver consults auth.json when the env var
    /// is missing. Uses a unique test var + temp auth dir so it never touches the operator's
    /// real keys or env. Serialized so the env var + cache state don't race with other tests.
    #[test]
    fn resolve_api_key_falls_back_to_auth_json() {
        // Process-wide env lock (not a private static): DOTZ_PI_AGENT_DIR is also flipped by
        // auth.rs tests and the server provider-key tests, and two tests mutating the same var
        // under different locks still race. A per-test private lock only excludes itself.
        let _guard = crate::util::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Unique var name + key value so this test is isolated from real env + other tests.
        let var = format!("DOTZ_TEST_FALLBACK_KEY_{}", uuid::Uuid::new_v4());
        let key_val = format!("sk-fallback-from-auth-json-{}", uuid::Uuid::new_v4());

        // Ensure the env var is unset (save + restore prior value if any).
        let prev = std::env::var(&var).ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var(&var) };

        // Point auth.json at a temp dir + write the key under our test var.
        let dir =
            std::env::temp_dir().join(format!("dotz-resolve-fallback-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev_auth_dir = std::env::var("DOTZ_PI_AGENT_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_PI_AGENT_DIR", dir.to_string_lossy().to_string()) };
        let auth = serde_json::json!({ var.clone(): key_val });
        std::fs::write(dir.join("auth.json"), auth.to_string()).unwrap();
        crate::auth::refresh_cache();

        // resolve_api_key("$VAR") with the env var UNSET must return the auth.json value.
        let resolved = resolve_api_key(&format!("${var}"));
        assert_eq!(
            resolved, key_val,
            "resolve_api_key must fall back to auth.json when the env var is unset"
        );

        // The ${VAR} form must also fall back.
        let resolved_brace = resolve_api_key(&format!("${{{var}}}"));
        assert_eq!(
            resolved_brace, key_val,
            "resolve_api_key must fall back to auth.json for the ${{VAR}} form too"
        );

        // Cleanup: restore env + remove temp dir.
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var(&var, p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var(&var) },
        }
        match prev_auth_dir {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_PI_AGENT_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_PI_AGENT_DIR") },
        }
        crate::auth::refresh_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Q4: when the env var IS set, it must take precedence over auth.json (env wins). This
    /// preserves the existing behavior so an operator who exports a key in their shell is not
    /// silently overridden by a stale auth.json entry. Same isolation as the fallback test.
    #[test]
    fn resolve_api_key_env_takes_precedence_over_auth_json() {
        // Same process-wide env lock as the fallback test above (see comment there).
        let _guard = crate::util::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let var = format!("DOTZ_TEST_PRECEDENCE_KEY_{}", uuid::Uuid::new_v4());
        let env_val = format!("sk-from-env-{}", uuid::Uuid::new_v4());
        let auth_val = format!("sk-from-auth-{}", uuid::Uuid::new_v4());

        let prev = std::env::var(&var).ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var(&var, &env_val) };

        let dir =
            std::env::temp_dir().join(format!("dotz-resolve-precedence-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let prev_auth_dir = std::env::var("DOTZ_PI_AGENT_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_PI_AGENT_DIR", dir.to_string_lossy().to_string()) };
        std::fs::write(
            dir.join("auth.json"),
            serde_json::json!({ var.clone(): auth_val }).to_string(),
        )
        .unwrap();
        crate::auth::refresh_cache();

        // Env var wins.
        let resolved = resolve_api_key(&format!("${var}"));
        assert_eq!(
            resolved, env_val,
            "env var must take precedence over auth.json"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var(&var, p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var(&var) },
        }
        match prev_auth_dir {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_PI_AGENT_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_PI_AGENT_DIR") },
        }
        crate::auth::refresh_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- C6: gateway provider (OmniRoute / OpenRouter-as-gateway / LiteLLM passthrough) ----
    //
    // The gateway endpoint reads from the persisted config (DOTZ_CONFIG_DIR → ~/.dotz/config.json),
    // so these tests point DOTZ_CONFIG_DIR at a fresh temp dir + serialize on the shared config-dir
    // lock so they never race with config::tests or touch the operator's real config.

    /// RAII guard: point `DOTZ_CONFIG_DIR` at a fresh temp dir for the lifetime of the guard, and
    /// on drop restore the prior env value + remove the temp dir. Using a guard (instead of a
    /// closure) keeps the test body simple when we want to assert between setup and teardown.
    struct ConfigDirGuard {
        dir: std::path::PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn setup_config_dir() -> ConfigDirGuard {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "dotz-provider-gateway-test-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::create_dir_all(&dir);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", dir.to_string_lossy().to_string()) };
        ConfigDirGuard { dir, _guard: g }
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            // The setup set it; restore is handled by the test below. Defensive: clear it.
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") };
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// C6: when a gateway config is set, `provider_endpoint("gateway")` returns the configured
    /// base URL + the `$KEY_REF` form (so resolve_api_key applies the env → auth.json fallback).
    #[test]
    fn gateway_endpoint_reads_from_config() {
        let guard = setup_config_dir();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // Write a config with the gateway section.
        std::fs::write(
            guard.dir.join("config.json"),
            serde_json::json!({
                "provider": "ollama",
                "executiveModel": "glm-5.2",
                "subagentModel": "minimax-m3",
                "thinkingLevel": "high",
                "gateway": {
                    "baseUrl": "https://api.omniroute.ai/v1",
                    "apiKeyRef": "OMNIROUTE_API_KEY",
                    "presets": ["omniroute"],
                    "modelAllowlist": ["gpt-5.6"],
                }
            })
            .to_string(),
        )
        .unwrap();

        let (base, key) = provider_endpoint("gateway").expect("configured gateway must resolve");
        assert_eq!(base, "https://api.omniroute.ai/v1");
        assert_eq!(
            key, "$OMNIROUTE_API_KEY",
            "key ref must be wrapped in $ so resolve_api_key resolves it"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        drop(guard);
    }

    /// C6: when no gateway config is present, `provider_endpoint("gateway")` returns None so the
    /// gateway is inert (resolve() returns None, the session surfaces a clear error). This is the
    /// graceful-degradation contract: a gateway-free install is unchanged.
    #[test]
    fn gateway_endpoint_defaults_when_no_config() {
        let guard = setup_config_dir();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // No config.json at all.
        assert!(provider_endpoint("gateway").is_none(), "no config → None");

        // A config WITHOUT a gateway section also → None.
        std::fs::write(
            guard.dir.join("config.json"),
            serde_json::json!({
                "provider": "ollama",
                "executiveModel": "glm-5.2",
                "subagentModel": "minimax-m3",
                "thinkingLevel": "high"
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            provider_endpoint("gateway").is_none(),
            "config without gateway section → None"
        );

        // An empty gateway section → None (is_empty() true).
        std::fs::write(
            guard.dir.join("config.json"),
            serde_json::json!({
                "provider": "ollama",
                "executiveModel": "glm-5.2",
                "subagentModel": "minimax-m3",
                "thinkingLevel": "high",
                "gateway": { "baseUrl": "", "apiKeyRef": "", "presets": [], "modelAllowlist": [] }
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            provider_endpoint("gateway").is_none(),
            "empty gateway section → None (inert)"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        drop(guard);
    }

    /// C6: `resolve("gateway", ...)` returns a ResolvedModel wired to the configured endpoint, and
    /// the gateway routes to the OpenAI-compat adapter (adapter_for("gateway") → OpenAiChat).
    #[test]
    fn resolve_returns_metadata_for_configured_gateway() {
        let guard = setup_config_dir();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::fs::write(
            guard.dir.join("config.json"),
            serde_json::json!({
                "provider": "ollama",
                "executiveModel": "glm-5.2",
                "subagentModel": "minimax-m3",
                "thinkingLevel": "high",
                "gateway": {
                    "baseUrl": "http://localhost:4000/v1",
                    "apiKeyRef": "LITELLM_API_KEY",
                    "presets": ["litellm"],
                    "modelAllowlist": ["gpt-5.6"],
                }
            })
            .to_string(),
        )
        .unwrap();

        let m = resolve("gateway", "gpt-5.6").expect("configured gateway must resolve");
        assert_eq!(m.provider, "gateway");
        assert_eq!(m.model_id, "gpt-5.6");
        assert_eq!(m.base_url, "http://localhost:4000/v1");
        assert_eq!(m.api_key_ref, "$LITELLM_API_KEY");
        // Gateway is OpenAI-compatible → adapter_for routes it to OpenAiChat (not a native one).
        let _adapter = adapter_for("gateway"); // must not panic; type is opaque Box<dyn Provider>.

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        drop(guard);
    }

    /// C6: `resolve("gateway", ...)` returns None when no gateway is configured (so the session
    /// surfaces a clear "not resolvable" error instead of sending an empty-key request).
    #[test]
    fn resolve_returns_none_for_unconfigured_gateway() {
        let guard = setup_config_dir();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        assert!(resolve("gateway", "gpt-5.6").is_none());
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        drop(guard);
    }

    /// C6: the named presets include OmniRoute + OpenRouter (gateway mode) + LiteLLM + Custom, in
    /// that order. These pre-fill the base URL + key ref so the operator doesn't have to look them up.
    #[test]
    fn gateway_presets_include_omniroute_openrouter_litellm() {
        let presets = gateway_presets();
        let ids: Vec<&str> = presets.iter().map(|p| p.id).collect();
        assert!(ids.contains(&"omniroute"), "presets must include omniroute");
        assert!(
            ids.contains(&"openrouter-gw"),
            "presets must include openrouter-gw"
        );
        assert!(ids.contains(&"litellm"), "presets must include litellm");
        assert!(ids.contains(&"custom"), "presets must include custom");
        // OmniRoute preset pre-fills the documented endpoint + key var.
        let omniroute = presets
            .iter()
            .find(|p| p.id == "omniroute")
            .expect("omniroute preset");
        assert_eq!(omniroute.base_url, "https://api.omniroute.ai/v1");
        assert_eq!(omniroute.key_ref, "OMNIROUTE_API_KEY");
        // OpenRouter-as-gateway preset.
        let or = presets
            .iter()
            .find(|p| p.id == "openrouter-gw")
            .expect("openrouter-gw preset");
        assert_eq!(or.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(or.key_ref, "OPENROUTER_API_KEY");
        // LiteLLM preset (local proxy).
        let litellm = presets
            .iter()
            .find(|p| p.id == "litellm")
            .expect("litellm preset");
        assert_eq!(litellm.base_url, "http://localhost:4000/v1");
        assert_eq!(litellm.key_ref, "LITELLM_API_KEY");
    }

    /// C6: SSRF validation — https:// anywhere is accepted, http://localhost / 127.0.0.1 is
    /// accepted (local LiteLLM proxy), any other http:// is REJECTED so a misconfigured gateway
    /// can't redirect dotz at an arbitrary internal endpoint.
    #[test]
    fn validate_gateway_base_url_accepts_https_and_loopback_http() {
        assert!(validate_gateway_base_url("https://api.omniroute.ai/v1").is_ok());
        assert!(validate_gateway_base_url("https://openrouter.ai/api/v1").is_ok());
        assert!(validate_gateway_base_url("http://localhost:4000/v1").is_ok());
        assert!(validate_gateway_base_url("http://127.0.0.1:4000/v1").is_ok());
        assert!(validate_gateway_base_url("http://localhost").is_ok());
    }

    /// C6: SSRF validation — non-loopback http:// is REJECTED (the SSRF guard).
    #[test]
    fn validate_gateway_base_url_rejects_non_loopback_http() {
        assert!(validate_gateway_base_url("http://api.omniroute.ai/v1").is_err());
        assert!(validate_gateway_base_url("http://192.168.1.5/v1").is_err());
        assert!(validate_gateway_base_url("http://10.0.0.1/v1").is_err());
        assert!(validate_gateway_base_url("http://169.254.169.254/latest/meta-data/").is_err());
        // The error message names the SSRF concern so it's actionable.
        let err = validate_gateway_base_url("http://api.example.com/v1").unwrap_err();
        assert!(
            err.contains("SSRF"),
            "error must name the SSRF concern: {err}"
        );
    }

    /// C6: SSRF validation — empty / non-http(s) / malformed schemes are rejected.
    #[test]
    fn validate_gateway_base_url_rejects_empty_and_non_http_schemes() {
        assert!(validate_gateway_base_url("").is_err());
        assert!(validate_gateway_base_url("   ").is_err());
        assert!(validate_gateway_base_url("ftp://example.com/v1").is_err());
        assert!(
            validate_gateway_base_url("api.omniroute.ai/v1").is_err(),
            "no scheme"
        );
        assert!(
            validate_gateway_base_url("https://").is_err(),
            "https:// with no host"
        );
    }
}
