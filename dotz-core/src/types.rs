//! Shared value/type constants — port of src/types.ts. Serde field names match the JSON contract.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_PROVIDER: &str = "ollama";

pub const THINKING_LEVELS: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];
pub fn is_valid_thinking(s: &str) -> bool {
    THINKING_LEVELS.contains(&s)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

pub fn default_model() -> ModelRef {
    ModelRef { provider: "ollama".into(), model_id: "glm-5.2".into() }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderMeta {
    pub id: &'static str,
    pub label: &'static str,
    // freeForm is present (true) only for free-form providers; absent otherwise (matches types.ts).
    #[serde(rename = "freeForm", skip_serializing_if = "is_false")]
    pub free_form: bool,
}
fn is_false(b: &bool) -> bool {
    !*b
}

const fn pm(id: &'static str, label: &'static str, free_form: bool) -> ProviderMeta {
    ProviderMeta { id, label, free_form }
}

/// The 11 known providers, in UI order.
pub fn providers() -> Vec<ProviderMeta> {
    vec![
        pm("openrouter", "OpenRouter", true),
        pm("ollama", "Ollama Cloud", true),
        pm("anthropic", "Anthropic", false),
        pm("openai", "OpenAI", false),
        pm("google", "Google", false),
        pm("groq", "Groq", false),
        pm("mistral", "Mistral", false),
        pm("xai", "xAI", false),
        pm("deepseek", "DeepSeek", false),
        pm("cohere", "Cohere", false),
        pm("local", "Local (Ollama/LM Studio)", true),
    ]
}

pub fn is_known_provider(id: &str) -> bool {
    providers().iter().any(|p| p.id == id)
}

pub fn provider_ids() -> Vec<&'static str> {
    providers().iter().map(|p| p.id).collect()
}

/// Provider-aware default executive/subagent model ids (only ollama/openrouter/local have defaults).
pub fn provider_defaults_json() -> Value {
    json!({
        "ollama": { "executive": "glm-5.2", "subagent": "minimax-m3" },
        "openrouter": { "executive": "nex-agi/nex-n2-pro:free", "subagent": "nex-agi/nex-n2-pro:free" },
        "local": { "executive": "qwen2.5-coder", "subagent": "qwen2.5-coder" },
    })
}

/// Suggested low-cost worker model ids — ONLY configured/available models (port of LOW_COST_MODELS).
pub fn low_cost_models() -> Vec<ModelRef> {
    vec![
        ModelRef { provider: "ollama".into(), model_id: "minimax-m3".into() },
        ModelRef { provider: "ollama".into(), model_id: "kimi-k2.7-code".into() },
    ]
}

/// Render the subagent-model directive for system-prompt injection (port of renderLowCostModels).
pub fn render_low_cost_models() -> String {
    let sub = std::env::var("DOTZ_SUBAGENT_MODEL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let (_, s) = provider_default(DEFAULT_PROVIDER).unwrap();
        format!("{DEFAULT_PROVIDER}/{s}")
    });
    let list = low_cost_models()
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.model_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\n# dotz subagent model\nEvery subagent you disperse via the `subagent` tool runs on `{sub}` by default (the configured, known-working low-cost worker). You (the lead) keep the high-quality executive model.\n\nDo NOT pass a `model` override unless a task genuinely needs a different model — omitting `model` uses `{sub}`, which always works. If you must override, choose ONLY from this exact list of configured low-cost models: {list}. NEVER invent a model id (e.g. `openai/*`, `anthropic/*`, `google/*`, or any OpenRouter id) — unconfigured ids fail with auth/credit errors and silently break the fan-out.\n"
    )
}

/// (executive, subagent) defaults for a provider, if known.
pub fn provider_default(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "ollama" => Some(("glm-5.2", "minimax-m3")),
        "openrouter" => Some(("nex-agi/nex-n2-pro:free", "nex-agi/nex-n2-pro:free")),
        "local" => Some(("qwen2.5-coder", "qwen2.5-coder")),
        _ => None,
    }
}
