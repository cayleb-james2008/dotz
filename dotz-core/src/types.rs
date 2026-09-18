//! Shared value/type constants — port of src/types.ts. Serde field names match the JSON contract.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_PROVIDER: &str = "ollama";

/// Hard cost/token budgets for a task or workflow. When a budget is set, the executor
/// aborts (or downgrades) a step before it can exceed the cap — so a fan-out can't
/// quietly burn the provider balance while unattended.
///
/// All fields are optional: an absent field means "no limit" for that dimension.
/// `max_cost` is in USD (the same unit `Usage.cost.total` reports).
/// `max_tokens` is the total token budget (input + output).
/// `max_input_tokens` is a separate cap on input tokens alone (prompt-bloat guard).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    /// Max cumulative cost (USD) before the step/run is aborted or downgraded.
    #[serde(rename = "maxCost", skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    /// Max cumulative total tokens (input + output) before the step/run is aborted.
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Max cumulative input tokens (prompt-bloat guard).
    #[serde(rename = "maxInputTokens", skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
}

impl Budget {
    /// True when no budget constraints are set (the common case — no enforcement needed).
    pub fn is_unbounded(&self) -> bool {
        self.max_cost.is_none() && self.max_tokens.is_none() && self.max_input_tokens.is_none()
    }

    /// True when the given cumulative usage exceeds ANY of the set budget limits.
    pub fn is_exceeded(&self, cost: f64, input_tokens: u64, output_tokens: u64) -> bool {
        let total = input_tokens.saturating_add(output_tokens);
        self.max_cost.is_some_and(|max| cost > max)
            || self.max_tokens.is_some_and(|max| total > max)
            || self.max_input_tokens.is_some_and(|max| input_tokens > max)
    }

    /// The highest consumption ratio (0.0..) across whichever of `max_cost`,
    /// `max_tokens`, and `max_input_tokens` are set. Returns `0.0` when the budget
    /// is unbounded (no limit on any dimension) — so callers can treat a `0.0`
    /// result as "no headroom signal to act on".
    ///
    /// Unlike `is_exceeded` (a hard boolean at 100%), this gives the runtime a
    /// *headroom* signal: e.g. `>= 0.8` means "near the cap, downgrade before the
    /// hard abort fires". Ratios can exceed `1.0` when a limit is already blown
    /// past (the hard-abort path owns that case); callers comparing against a
    /// threshold like `0.8` should use a plain `>=`.
    pub fn fraction_used(&self, cost: f64, input_tokens: u64, output_tokens: u64) -> f64 {
        let total = input_tokens.saturating_add(output_tokens);
        let mut max_ratio = 0.0_f64;
        if let Some(max_cost) = self.max_cost {
            if max_cost > 0.0 {
                max_ratio = max_ratio.max(cost / max_cost);
            }
        }
        if let Some(max_tokens) = self.max_tokens {
            // Guard against the degenerate max_tokens == 0 config.
            if max_tokens > 0 {
                max_ratio = max_ratio.max(total as f64 / max_tokens as f64);
            }
        }
        if let Some(max_input_tokens) = self.max_input_tokens {
            if max_input_tokens > 0 {
                max_ratio = max_ratio.max(input_tokens as f64 / max_input_tokens as f64);
            }
        }
        max_ratio
    }
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpecStatus {
    Draft,
    Ready,
    Applying,
    Verified,
    Blocked,
    Archived,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecArtifact {
    pub kind: String,
    pub path: String,
    pub exists: bool,
    #[serde(rename = "sizeBytes", skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessFinding {
    pub id: String,
    pub area: String,
    pub status: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecChange {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: SpecStatus,
    pub path: String,
    pub artifacts: Vec<SpecArtifact>,
    pub readiness: Vec<ReadinessFinding>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
    #[serde(rename = "archivedAt", skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LivingDocKind {
    AntiPatterns,
    NonInferables,
    ContextScope,
    LivingDocs,
}

impl LivingDocKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AntiPatterns => "anti_patterns",
            Self::NonInferables => "non_inferables",
            Self::ContextScope => "context_scope",
            Self::LivingDocs => "living_docs",
        }
    }

    pub fn file_name(&self) -> &'static str {
        match self {
            Self::AntiPatterns => "anti-patterns.md",
            Self::NonInferables => "non-inferables.md",
            Self::ContextScope => "context-scope.md",
            Self::LivingDocs => "living-docs.md",
        }
    }

    pub fn heading(&self) -> &'static str {
        match self {
            Self::AntiPatterns => "Anti-Patterns",
            Self::NonInferables => "Non-Inferables",
            Self::ContextScope => "Context Scope",
            Self::LivingDocs => "Living Docs",
        }
    }

    pub fn all() -> &'static [LivingDocKind] {
        &[
            LivingDocKind::AntiPatterns,
            LivingDocKind::NonInferables,
            LivingDocKind::ContextScope,
            LivingDocKind::LivingDocs,
        ]
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsStatus {
    pub cwd: String,
    #[serde(rename = "insideWorktree")]
    pub inside_worktree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(rename = "headSha", skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub dirty: bool,
    pub staged: bool,
    pub untracked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    #[serde(rename = "ghInstalled")]
    pub gh_installed: bool,
    #[serde(rename = "ghLoggedIn")]
    pub gh_logged_in: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AtomicCommitRequest {
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RollbackTarget {
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub target: String,
    #[serde(default = "default_rollback_mode")]
    pub mode: String,
    #[serde(default)]
    pub confirm: bool,
}

fn default_rollback_mode() -> String {
    "revert".to_string()
}

pub fn default_model() -> ModelRef {
    ModelRef {
        provider: "ollama".into(),
        model_id: "glm-5.2".into(),
    }
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
    ProviderMeta {
        id,
        label,
        free_form,
    }
}

/// The known providers, in UI order. `gateway` (C6) is a generic OpenAI-compatible passthrough
/// behind ONE endpoint + ONE key + a model allowlist (OmniRoute / OpenRouter-as-gateway / LiteLLM
/// / any OpenAI-compat gateway). It is free-form (the model id is whatever the gateway routes to)
/// and sits last so it has the lowest UI priority.
pub fn providers() -> Vec<ProviderMeta> {
    vec![
        pm("openrouter", "OpenRouter", true),
        pm("nvidia-nim", "NVIDIA NIM", true),
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
        pm("gateway", "Gateway", true),
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
        "nvidia-nim": { "executive": "z-ai/glm-5.2", "subagent": "z-ai/glm-5.2" },
        "local": { "executive": "qwen2.5-coder", "subagent": "qwen2.5-coder" },
    })
}

/// Suggested low-cost worker model ids — ONLY configured/available models (port of LOW_COST_MODELS).
pub fn low_cost_models() -> Vec<ModelRef> {
    vec![
        ModelRef {
            provider: "ollama".into(),
            model_id: "minimax-m3".into(),
        },
        ModelRef {
            provider: "ollama".into(),
            model_id: "kimi-k2.7-code".into(),
        },
        ModelRef {
            provider: "openrouter".into(),
            model_id: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
        },
        ModelRef {
            provider: "openrouter".into(),
            model_id: "nex-agi/nex-n2-pro:free".into(),
        },
    ]
}

/// C6: the low-cost worker list extended with the configured gateway model allowlist. Each
/// allowlist entry becomes a `gateway/<model>` ModelRef so the lead agent can disperse subagent
/// tasks to gateway-routed models (OmniRoute/OpenRouter-as-gateway/LiteLLM) the same way it
/// dispatches to the native providers. When no gateway is configured (or the allowlist is empty)
/// this is byte-identical to [`low_cost_models`], so a gateway-free install is unchanged.
pub fn low_cost_models_with_gateway() -> Vec<ModelRef> {
    let mut out = low_cost_models();
    if let Some(gw) = crate::config::load().gateway {
        for id in gw.model_allowlist.iter().filter(|s| !s.trim().is_empty()) {
            out.push(ModelRef {
                provider: "gateway".into(),
                model_id: id.trim().to_string(),
            });
        }
    }
    out
}

fn resolve_subagent_model(override_value: Option<&str>) -> String {
    override_value
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let (_, s) = provider_default(DEFAULT_PROVIDER).unwrap();
            format!("{DEFAULT_PROVIDER}/{s}")
        })
}

/// Render the subagent-model directive for system-prompt injection (port of renderLowCostModels).
pub fn render_low_cost_models() -> String {
    let sub = resolve_subagent_model(std::env::var("DOTZ_SUBAGENT_MODEL").ok().as_deref());
    let list = low_cost_models_with_gateway()
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.model_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\n# dotz subagent model\nEvery subagent you disperse via the `subagent` tool runs on `{sub}` by default (the configured, known-working low-cost worker). You (the lead) keep the high-quality executive model.\n\nDo NOT pass a `model` override unless a task genuinely needs a different model — omitting `model` uses `{sub}`, which always works. If you must override, choose ONLY from this exact list of configured low-cost models: {list}. NEVER invent a model id (e.g. `openai/*`, `anthropic/*`, `google/*`, or any OpenRouter id) — unconfigured ids fail with auth/credit errors and silently break the fan-out.\n"
    )
}

/// Strip a leading "{provider}/" prefix from a model id when it matches the current provider.
/// This fixes the common UI mistake of pasting a full "provider/model-id" string into the model
/// field, which would otherwise be sent to the upstream API verbatim and fail. Prefixes that do
/// not match the current provider are preserved so cross-provider model namespaces (e.g. an
/// OpenRouter id that starts with "ollama/") are not corrupted. The provider segment is compared
/// case-insensitively so "Ollama/glm-5.2" under provider "ollama" is normalized too.
pub fn strip_matching_provider_prefix(provider: &str, model_id: &str) -> String {
    let trimmed = model_id.trim();
    if let Some(slash) = trimmed.find('/') {
        if trimmed[..slash].eq_ignore_ascii_case(provider) {
            return trimmed[slash + 1..].to_string();
        }
    }
    trimmed.to_string()
}

/// C6: parse a "provider/model-id" string into a `ModelRef`. The provider segment is lowercased
/// (so "Gateway/gpt-5.6" normalizes to "gateway"), and a redundant matching provider prefix is
/// stripped from the model id so the upstream API receives a bare id (so "gateway/gateway/x" →
/// ("gateway", "x"), not ("gateway", "gateway/x")). A bare id with no slash is treated as an
/// unknown provider with the whole string as the model id — callers that want the ollama fallback
/// (subagent dispatch) keep using their own fallback. This is the Rust-side `resolveModel` used by
/// the gateway route + the acceptance test; it does NOT validate the provider against the known
/// list (the caller validates separately where that matters).
pub fn resolve_model_ref(s: &str) -> ModelRef {
    let trimmed = s.trim();
    match trimmed.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => {
            let provider = p.to_ascii_lowercase();
            let model_id = strip_matching_provider_prefix(&provider, m);
            ModelRef { provider, model_id }
        }
        _ => ModelRef {
            provider: trimmed.to_ascii_lowercase(),
            model_id: trimmed.to_string(),
        },
    }
}

/// (executive, subagent) defaults for a provider, if known.
pub fn provider_default(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "ollama" => Some(("glm-5.2", "minimax-m3")),
        "openrouter" => Some(("nex-agi/nex-n2-pro:free", "nex-agi/nex-n2-pro:free")),
        "nvidia-nim" => Some(("z-ai/glm-5.2", "z-ai/glm-5.2")),
        "local" => Some(("qwen2.5-coder", "qwen2.5-coder")),
        _ => None,
    }
}

/// Curated model catalog surfaced to the UI for fixed-provider dropdowns and free-form
/// provider datalist suggestions. Free-form providers (ollama/openrouter/local) get their
/// known defaults + low-cost suggestions; fixed providers get a small set of common current
/// model ids so the model selector is usable instead of empty.
///
/// Every provider that has a registered default (`provider_default`) contributes BOTH its
/// default executive and subagent model ids, so the picker's datalist always contains the model
/// dotz boots with (e.g. `ollama/glm-5.2`). Without this, the ollama catalog listed only the
/// low-cost workers (`minimax-m3`, `kimi-k2.7-code`) and the operator's current executive model
/// was missing from the suggestions — clearing the input or switching providers and back lost
/// the default executive as a selectable option. The result is deduplicated by
/// `(provider, model_id)` so a default that already overlaps a low-cost suggestion (e.g. the
/// openrouter default `nex-agi/nex-n2-pro:free`) is not listed twice.
pub fn available_models() -> Vec<ModelRef> {
    let mut out: Vec<ModelRef> = Vec::new();
    // Deduplicate by (provider, model_id) while preserving first-seen order.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut push = |out: &mut Vec<ModelRef>, provider: &str, model_id: &str| {
        let key = (provider.to_string(), model_id.to_string());
        if seen.insert(key) {
            out.push(ModelRef {
                provider: provider.into(),
                model_id: model_id.into(),
            });
        }
    };

    // Free-form provider suggestions (datalist). These model ids are validated by the upstream
    // provider's catalog; the suggestions are the same ones dotz already recommends elsewhere.
    for m in low_cost_models() {
        push(&mut out, &m.provider, &m.model_id);
    }

    // Each provider with a registered default contributes its default executive AND subagent model
    // ids, so the picker always offers the model dotz boots with (not just the low-cost workers).
    for pid in provider_ids() {
        if let Some((exec, sub)) = provider_default(pid) {
            push(&mut out, pid, exec);
            push(&mut out, pid, sub);
        }
    }

    // Fixed-provider dropdown entries. The UI hides the free-form input for these providers, so
    // without a catalog the operator could not pick a model at all. These ids are the current
    // flagship models for each provider; unknown ids are rejected by the upstream API, not by dotz.
    let fixed: &[(&str, &[&str])] = &[
        (
            "anthropic",
            &["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5"],
        ),
        ("openai", &["gpt-5.5", "gpt-5.4-mini", "gpt-5.1"]),
        ("google", &["gemini-3.1-pro", "gemini-3.5-flash"]),
        ("groq", &["openai/gpt-oss-120b", "openai/gpt-oss-20b"]),
        ("mistral", &["mistral-large-latest", "mistral-small-latest"]),
        ("xai", &["grok-4.3", "grok-build-0.1"]),
        ("deepseek", &["deepseek-chat", "deepseek-reasoner"]),
        ("cohere", &["command-a-plus-05-2026", "command-a-03-2025"]),
    ];
    for (provider, ids) in fixed {
        for id in *ids {
            push(&mut out, provider, id);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_levels_and_validation() {
        for level in THINKING_LEVELS {
            assert!(is_valid_thinking(level));
        }
        assert!(!is_valid_thinking(""));
        assert!(!is_valid_thinking("max"));
    }

    #[test]
    fn default_model_points_to_ollama() {
        let m = default_model();
        assert_eq!(m.provider, "ollama");
        assert_eq!(m.model_id, "glm-5.2");
    }

    #[test]
    fn providers_list_is_stable() {
        let p = providers();
        assert_eq!(p.len(), 13);
        assert!(p.iter().any(|pm| pm.id == "ollama" && pm.free_form));
        assert!(p.iter().any(|pm| pm.id == "nvidia-nim" && pm.free_form));
        assert!(p.iter().any(|pm| pm.id == "anthropic" && !pm.free_form));
        // C6: gateway is a free-form provider (model id is whatever the gateway routes to).
        assert!(p.iter().any(|pm| pm.id == "gateway" && pm.free_form));
    }

    #[test]
    fn known_provider_detection() {
        assert!(is_known_provider("openrouter"));
        assert!(is_known_provider("ollama"));
        assert!(!is_known_provider("fake-provider"));
    }

    #[test]
    fn provider_ids_match_providers() {
        let ids = provider_ids();
        assert_eq!(ids.len(), providers().len());
        assert!(ids.contains(&"ollama"));
    }

    #[test]
    fn provider_default_returns_expected_pairs() {
        assert_eq!(provider_default("ollama"), Some(("glm-5.2", "minimax-m3")));
        assert_eq!(
            provider_default("openrouter"),
            Some(("nex-agi/nex-n2-pro:free", "nex-agi/nex-n2-pro:free"))
        );
        assert_eq!(
            provider_default("local"),
            Some(("qwen2.5-coder", "qwen2.5-coder"))
        );
        assert_eq!(provider_default("unknown"), None);
    }

    #[test]
    fn strip_matching_provider_prefix_removes_current_provider() {
        assert_eq!(
            strip_matching_provider_prefix("ollama", "ollama/glm-5.2"),
            "glm-5.2"
        );
    }

    #[test]
    fn strip_matching_provider_prefix_preserves_mismatched_provider() {
        assert_eq!(
            strip_matching_provider_prefix("openrouter", "ollama/glm-5.2"),
            "ollama/glm-5.2"
        );
    }

    #[test]
    fn strip_matching_provider_prefix_preserves_bare_model_id() {
        assert_eq!(
            strip_matching_provider_prefix("ollama", "glm-5.2"),
            "glm-5.2"
        );
    }

    #[test]
    fn strip_matching_provider_prefix_trims_whitespace() {
        assert_eq!(
            strip_matching_provider_prefix("ollama", "  ollama/glm-5.2  "),
            "glm-5.2"
        );
    }

    /// A pasted model id may have a provider prefix in a different case (e.g. "Ollama/glm-5.2").
    /// The prefix check is case-insensitive for the provider segment only, so the bare model id
    /// still reaches the upstream API.
    #[test]
    fn strip_matching_provider_prefix_is_case_insensitive_for_provider_segment() {
        assert_eq!(
            strip_matching_provider_prefix("ollama", "Ollama/glm-5.2"),
            "glm-5.2"
        );
        assert_eq!(
            strip_matching_provider_prefix("openrouter", "OPENROUTER/nex-agi/nex-n2-pro:free"),
            "nex-agi/nex-n2-pro:free"
        );
    }

    #[test]
    fn provider_defaults_json_matches_low_cost_openrouter_models() {
        let defs = provider_defaults_json();
        let or = defs.get("openrouter").expect("openrouter defaults present");
        assert_eq!(
            or.get("executive").and_then(|v| v.as_str()),
            Some("nex-agi/nex-n2-pro:free")
        );
        assert_eq!(
            or.get("subagent").and_then(|v| v.as_str()),
            Some("nex-agi/nex-n2-pro:free")
        );
        let ollama = defs.get("ollama").expect("ollama defaults present");
        assert_eq!(
            ollama.get("executive").and_then(|v| v.as_str()),
            Some("glm-5.2")
        );
        assert_eq!(
            ollama.get("subagent").and_then(|v| v.as_str()),
            Some("minimax-m3")
        );
    }

    #[test]
    fn low_cost_models_includes_ollama_and_openrouter_workers() {
        let models = low_cost_models();
        assert!(
            models
                .iter()
                .any(|m| m.provider == "ollama" && m.model_id == "minimax-m3")
        );
        assert!(
            models
                .iter()
                .any(|m| m.provider == "ollama" && m.model_id == "kimi-k2.7-code")
        );
        assert!(models.iter().any(|m| {
            m.provider == "openrouter" && m.model_id == "nvidia/nemotron-3-ultra-550b-a55b:free"
        }));
        assert!(
            models
                .iter()
                .any(|m| m.provider == "openrouter" && m.model_id == "nex-agi/nex-n2-pro:free")
        );
    }

    #[test]
    fn resolve_subagent_model_uses_override_when_present() {
        assert_eq!(
            resolve_subagent_model(Some("openrouter/nvidia/nemotron-3-ultra-550b-a55b:free")),
            "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
        );
    }

    #[test]
    fn resolve_subagent_model_falls_back_to_default() {
        assert_eq!(resolve_subagent_model(None), "ollama/minimax-m3");
        assert_eq!(resolve_subagent_model(Some("")), "ollama/minimax-m3");
    }

    #[test]
    fn resolve_subagent_model_ignores_whitespace_only_override() {
        assert_eq!(resolve_subagent_model(Some("   ")), "ollama/minimax-m3");
        assert_eq!(resolve_subagent_model(Some("\t\n")), "ollama/minimax-m3");
    }

    #[test]
    fn render_low_cost_models_contains_default_and_low_cost_list() {
        let rendered = render_low_cost_models();
        assert!(rendered.contains("ollama/minimax-m3"));
        assert!(rendered.contains("nvidia/nemotron-3-ultra-550b-a55b:free"));
        assert!(rendered.contains("nex-agi/nex-n2-pro:free"));
        assert!(rendered.contains("Do NOT pass a `model` override"));
        assert!(rendered.contains("minimax-m3"));
    }

    /// The model catalog must expose entries for fixed providers so the UI can render a dropdown.
    /// Without this, non-free-form providers (anthropic, openai, ...) show an empty selector.
    #[test]
    fn available_models_includes_fixed_provider_entries() {
        let models = available_models();
        assert!(
            models.iter().any(|m| m.provider == "anthropic"),
            "catalog must include anthropic models"
        );
        assert!(
            models.iter().any(|m| m.provider == "openai"),
            "catalog must include openai models"
        );
        assert!(
            models.iter().any(|m| m.provider == "google"),
            "catalog must include google models"
        );
        // Every entry must be a known provider.
        for m in &models {
            assert!(
                is_known_provider(&m.provider),
                "catalog entry {} is not a known provider",
                m.provider
            );
        }
    }

    /// Free-form providers should still contribute suggestions to the datalist (their default
    /// worker models), while fixed providers contribute dropdown entries. The UI filters by provider.
    #[test]
    fn available_models_includes_free_form_suggestions() {
        let models = available_models();
        assert!(
            models.iter().any(|m| m.provider == "ollama"),
            "catalog must include ollama suggestions"
        );
        assert!(
            models.iter().any(|m| m.provider == "openrouter"),
            "catalog must include openrouter suggestions"
        );
        assert!(
            models.iter().any(|m| m.provider == "local"),
            "catalog must include local suggestions"
        );
    }

    /// The catalog must include each provider's default executive model so the UI's model picker
    /// always offers the model dotz boots with as a selectable suggestion. Before this fix the
    /// ollama catalog listed only the low-cost workers (`minimax-m3`, `kimi-k2.7-code`) and the
    /// default executive `glm-5.2` was missing — clearing the input or switching providers and
    /// back lost the boot default as a pickable option.
    #[test]
    fn available_models_includes_default_executive_for_each_provider() {
        let models = available_models();
        for pid in provider_ids() {
            if let Some((exec, _sub)) = provider_default(pid) {
                assert!(
                    models
                        .iter()
                        .any(|m| m.provider == pid && m.model_id == exec),
                    "catalog must include the default executive for {pid}: {exec}"
                );
            }
        }
        // Specifically assert the previously-missing ollama boot default.
        assert!(
            models
                .iter()
                .any(|m| m.provider == "ollama" && m.model_id == "glm-5.2"),
            "catalog must include ollama/glm-5.2 (the boot default executive)"
        );
    }

    /// The catalog must not contain duplicate `(provider, model_id)` pairs. A default that
    /// overlaps a low-cost suggestion (e.g. openrouter's `nex-agi/nex-n2-pro:free`, or local's
    /// executive == subagent `qwen2.5-coder`) must be listed once, not twice.
    #[test]
    fn available_models_has_no_duplicates() {
        let models = available_models();
        let mut seen = std::collections::HashSet::new();
        for m in &models {
            let key = (m.provider.clone(), m.model_id.clone());
            assert!(
                seen.insert(key),
                "duplicate catalog entry for {}/{}",
                m.provider,
                m.model_id
            );
        }
    }

    #[test]
    fn budget_unbounded_when_all_fields_none() {
        let b = Budget::default();
        assert!(b.is_unbounded());
        assert!(!b.is_exceeded(100.0, 1_000_000, 500_000));
    }

    #[test]
    fn budget_cost_limit_triggers_when_exceeded() {
        let b = Budget {
            max_cost: Some(1.0),
            ..Default::default()
        };
        assert!(!b.is_exceeded(0.5, 100, 100));
        assert!(b.is_exceeded(1.01, 100, 100));
        // Boundary: exactly at limit is NOT exceeded.
        assert!(!b.is_exceeded(1.0, 100, 100));
    }

    #[test]
    fn budget_tokens_limit_triggers_when_exceeded() {
        let b = Budget {
            max_tokens: Some(1_000),
            ..Default::default()
        };
        assert!(!b.is_exceeded(0.0, 500, 499)); // 999 total
        assert!(b.is_exceeded(0.0, 500, 501)); // 1001 total
        assert!(b.is_exceeded(0.0, 1_001, 0)); // all input
    }

    #[test]
    fn budget_input_tokens_limit_triggers_independently() {
        let b = Budget {
            max_input_tokens: Some(500),
            ..Default::default()
        };
        // Under input cap but huge output.
        assert!(!b.is_exceeded(0.0, 400, 100_000));
        // Over input cap even with zero output.
        assert!(b.is_exceeded(0.0, 501, 0));
    }

    #[test]
    fn budget_any_limit_triggers() {
        // If ANY of the three limits is exceeded, the whole budget is exceeded.
        let b = Budget {
            max_cost: Some(10.0),
            max_tokens: Some(1_000_000),
            max_input_tokens: Some(500),
        };
        // Cost OK, tokens OK, input over.
        assert!(b.is_exceeded(5.0, 600, 10));
        // Cost over, tokens OK, input OK.
        assert!(b.is_exceeded(20.0, 100, 100));
    }

    #[test]
    fn budget_fraction_used_unbounded_is_zero() {
        // No limits set → there is no ratio to compute; 0.0 means "nothing to act on".
        let b = Budget::default();
        assert_eq!(b.fraction_used(1_000_000.0, 9_999_999, 9_999_999), 0.0);
    }

    #[test]
    fn budget_fraction_used_cost_ratio() {
        let b = Budget {
            max_cost: Some(1.0),
            ..Default::default()
        };
        assert_eq!(b.fraction_used(0.8, 0, 0), 0.8);
        assert_eq!(b.fraction_used(0.5, 0, 0), 0.5);
        // Exactly at the limit → 1.0 (still not "exceeded" per is_exceeded, but the
        // headroom signal is maxed).
        assert_eq!(b.fraction_used(1.0, 0, 0), 1.0);
        // Already blown past → ratio > 1.0 (the hard-abort path owns this case).
        assert_eq!(b.fraction_used(1.5, 0, 0), 1.5);
    }

    #[test]
    fn budget_fraction_used_tokens_ratio() {
        let b = Budget {
            max_tokens: Some(1_000),
            ..Default::default()
        };
        // 400 input + 400 output = 800 total → 0.8.
        assert_eq!(b.fraction_used(0.0, 400, 400), 0.8);
        // Input-only counts toward the total-token cap too.
        assert_eq!(b.fraction_used(0.0, 1_000, 0), 1.0);
    }

    #[test]
    fn budget_fraction_used_input_tokens_ratio() {
        let b = Budget {
            max_input_tokens: Some(500),
            ..Default::default()
        };
        // 400 of 500 input → 0.8, regardless of output.
        assert_eq!(b.fraction_used(0.0, 400, 100_000), 0.8);
    }

    #[test]
    fn budget_fraction_used_takes_max_across_dims() {
        // Two limits set; the input-token dim is closer to its cap → that wins.
        let b = Budget {
            max_cost: Some(10.0),
            max_input_tokens: Some(500),
            ..Default::default()
        };
        // cost 5.0/10.0 = 0.5; input 400/500 = 0.8 → max is 0.8.
        assert_eq!(b.fraction_used(5.0, 400, 0), 0.8);
        // Flip it: cost dim now closer.
        assert_eq!(b.fraction_used(9.0, 100, 0), 0.9);
    }

    #[test]
    fn budget_fraction_used_zero_max_cost_is_no_signal() {
        // A degenerate max_cost:Some(0.0) config would otherwise divide by zero;
        // treat it as "no signal on that dim" rather than +inf.
        let b = Budget {
            max_cost: Some(0.0),
            max_tokens: Some(1_000),
            ..Default::default()
        };
        // 500/1000 total → 0.5; the 0.0 cost dim contributes nothing.
        assert_eq!(b.fraction_used(100.0, 250, 250), 0.5);
    }

    #[test]
    fn budget_serialize_skip_none_fields() {
        let b = Budget {
            max_cost: Some(5.0),
            ..Default::default()
        };
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["maxCost"], serde_json::json!(5.0));
        assert!(json.get("maxTokens").is_none());
        assert!(json.get("maxInputTokens").is_none());
    }

    // ---- C6: gateway provider (OmniRoute / OpenRouter-as-gateway / LiteLLM passthrough) ----

    /// C6: the `gateway` ProviderMeta must be free-form (the model id is whatever the gateway
    /// routes to, not a fixed dropdown).
    #[test]
    fn gateway_provider_meta_is_free_form() {
        let all = providers();
        let gw = all
            .iter()
            .find(|p| p.id == "gateway")
            .expect("gateway must be a known provider");
        assert!(gw.free_form, "gateway must be free_form");
        assert_eq!(gw.label, "Gateway");
        // Gateway is the lowest-priority entry (last in the list).
        let ids: Vec<&str> = all.iter().map(|p| p.id).collect();
        assert_eq!(ids.last().copied(), Some("gateway"));
    }

    /// C6: `is_known_provider` must recognize "gateway" so config validation + the model picker
    /// accept it the same way as the other 12 providers.
    #[test]
    fn gateway_is_a_known_provider() {
        assert!(is_known_provider("gateway"));
        assert!(provider_ids().contains(&"gateway"));
    }

    /// C6: `resolve_model_ref("gateway/gpt-5.6")` → `(provider: "gateway", model_id: "gpt-5.6")`.
    /// A bare model id (no slash) is treated as an unknown provider with the whole string as the
    /// model id — callers that want an ollama fallback keep using their own fallback.
    #[test]
    fn resolve_model_for_gateway_prefix() {
        let m = resolve_model_ref("gateway/gpt-5.6");
        assert_eq!(m.provider, "gateway");
        assert_eq!(m.model_id, "gpt-5.6");
    }

    /// C6: the provider segment is lowercased so a mixed-case "Gateway/..." still resolves.
    #[test]
    fn resolve_model_ref_lowercases_provider_segment() {
        let m = resolve_model_ref("Gateway/claude-sonnet-5");
        assert_eq!(m.provider, "gateway");
        assert_eq!(m.model_id, "claude-sonnet-5");
    }

    /// C6: a redundant matching provider prefix is stripped so the upstream gateway receives a
    /// bare model id (so "gateway/gateway/x" → ("gateway", "x"), matching the executive/subagent
    /// normalization behavior for the other free-form providers).
    #[test]
    fn resolve_model_ref_strips_redundant_gateway_prefix() {
        let m = resolve_model_ref("gateway/gateway/gpt-5.6");
        assert_eq!(m.provider, "gateway");
        assert_eq!(m.model_id, "gpt-5.6");
    }

    /// C6: a model id with a foreign provider prefix is preserved (not corrupted) so
    /// cross-provider namespaces survive — e.g. an OpenRouter-style "anthropic/claude-..." id
    /// routed through a gateway keeps its upstream namespace.
    #[test]
    fn resolve_model_ref_preserves_foreign_prefix() {
        let m = resolve_model_ref("gateway/anthropic/claude-sonnet-5");
        assert_eq!(m.provider, "gateway");
        assert_eq!(m.model_id, "anthropic/claude-sonnet-5");
    }

    /// C6: a bare id with no slash is treated as an unknown provider + the whole string as the
    /// model id. Callers wanting the ollama fallback (subagent dispatch) keep their own fallback.
    #[test]
    fn resolve_model_ref_bare_id_has_no_provider_fallback() {
        let m = resolve_model_ref("gpt-5.6");
        assert_eq!(m.provider, "gpt-5.6");
        assert_eq!(m.model_id, "gpt-5.6");
    }

    /// C6: when a gateway config has a model allowlist, those models appear in the
    /// runtime-extended low-cost list (as `gateway/<model>` entries) so the lead agent can dispatch
    /// subagent tasks to gateway-routed models. Uses a temp DOTZ_CONFIG_DIR so it never touches
    /// the operator's real config; serialized on the shared config-dir test lock.
    #[test]
    fn gateway_model_allowlist_in_low_cost_models() {
        let guard = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-types-gateway-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &dir) };

        // Write a config with a gateway allowlist. The base config fields are required by
        // DotzConfig::load's normalization; gateway is the C6 addition.
        std::fs::write(
            dir.join("config.json"),
            serde_json::json!({
                "provider": "ollama",
                "executiveModel": "glm-5.2",
                "subagentModel": "minimax-m3",
                "thinkingLevel": "high",
                "gateway": {
                    "baseUrl": "https://api.omniroute.ai/v1",
                    "apiKeyRef": "OMNIROUTE_API_KEY",
                    "presets": ["omniroute"],
                    "modelAllowlist": ["gpt-5.6", "claude-sonnet-5", "glm-5.2"],
                }
            })
            .to_string(),
        )
        .unwrap();

        let models = low_cost_models_with_gateway();
        // The base low-cost workers are still present (gateway-free install is unchanged).
        assert!(
            models
                .iter()
                .any(|m| m.provider == "ollama" && m.model_id == "minimax-m3")
        );
        // Each allowlist entry appears as a gateway/<model> entry.
        for id in ["gpt-5.6", "claude-sonnet-5", "glm-5.2"] {
            assert!(
                models
                    .iter()
                    .any(|m| m.provider == "gateway" && m.model_id == id),
                "low_cost_models_with_gateway must include gateway/{id}"
            );
        }

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
    }

    /// C6: with NO gateway configured, `low_cost_models_with_gateway` is byte-identical to
    /// `low_cost_models` — a gateway-free install is unchanged.
    #[test]
    fn low_cost_models_with_gateway_unchanged_when_no_config() {
        let guard = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-types-gateway-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &dir) };

        // No config.json → load() returns defaults with gateway: None.
        let extended = low_cost_models_with_gateway();
        let base = low_cost_models();
        assert_eq!(
            extended.len(),
            base.len(),
            "no gateway config → extended list must equal the base list"
        );
        assert!(
            !extended.iter().any(|m| m.provider == "gateway"),
            "no gateway config → no gateway entries"
        );

        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
    }
}
