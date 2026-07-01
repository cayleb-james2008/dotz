//! Shared value/type constants — port of src/types.ts. Serde field names match the JSON contract.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
        self.max_cost.map_or(false, |max| cost > max)
            || self.max_tokens.map_or(false, |max| total > max)
            || self
                .max_input_tokens
                .map_or(false, |max| input_tokens > max)
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
    let list = low_cost_models()
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

/// (executive, subagent) defaults for a provider, if known.
pub fn provider_default(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "ollama" => Some(("glm-5.2", "minimax-m3")),
        "openrouter" => Some(("nex-agi/nex-n2-pro:free", "nex-agi/nex-n2-pro:free")),
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
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
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
            &[
                "claude-3-5-sonnet-latest",
                "claude-3-opus-latest",
                "claude-3-5-haiku-latest",
            ],
        ),
        ("openai", &["gpt-4o", "gpt-4o-mini", "o3-mini"]),
        (
            "google",
            &["gemini-1.5-pro-latest", "gemini-1.5-flash-latest"],
        ),
        ("groq", &["llama-3.3-70b-versatile", "mixtral-8x7b-32768"]),
        ("mistral", &["mistral-large-latest", "mistral-small-latest"]),
        ("xai", &["grok-2-1212", "grok-2-vision-1212"]),
        ("deepseek", &["deepseek-chat", "deepseek-reasoner"]),
        ("cohere", &["command-r", "command-r-plus"]),
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
        assert_eq!(p.len(), 11);
        assert!(p.iter().any(|pm| pm.id == "ollama" && pm.free_form));
        assert!(p.iter().any(|pm| pm.id == "anthropic" && !pm.free_form));
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
        assert!(models
            .iter()
            .any(|m| m.provider == "ollama" && m.model_id == "minimax-m3"));
        assert!(models
            .iter()
            .any(|m| m.provider == "ollama" && m.model_id == "kimi-k2.7-code"));
        assert!(models.iter().any(|m| {
            m.provider == "openrouter" && m.model_id == "nvidia/nemotron-3-ultra-550b-a55b:free"
        }));
        assert!(models
            .iter()
            .any(|m| m.provider == "openrouter" && m.model_id == "nex-agi/nex-n2-pro:free"));
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
}
