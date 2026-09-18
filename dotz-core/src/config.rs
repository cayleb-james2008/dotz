//! dotz global config — port of src/config.ts. Persists provider/model/reasoning defaults to
//! ~/.dotz/config.json (DOTZ_CONFIG_DIR overrides), and exports DOTZ_SUBAGENT_MODEL for subagents.
use crate::types;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// C6: configuration for the `gateway` provider — a generic OpenAI-compatible passthrough behind
/// ONE endpoint + ONE key + a model allowlist (OmniRoute / OpenRouter-as-gateway / LiteLLM / any
/// OpenAI-compat gateway). Lives under `gateway` in `~/.dotz/config.json`. All fields are
/// optional so an empty/missing section degrades gracefully (the gateway endpoint resolves to
/// empty strings and `provider_endpoint` returns None → gateway is inert).
///
/// `api_key_ref` is an env-var NAME (e.g. `"OMNIROUTE_API_KEY"`), NOT a `$VAR` reference and NEVER
/// a raw key — `provider_endpoint` wraps it in `$` before handing it to `resolve_api_key`, so the
/// key is resolved via the same env → auth.json fallback as the other providers. The key value is
/// NEVER stored in config.json (only the var name).
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GatewayConfig {
    /// Base URL of the OpenAI-compatible gateway, e.g. `https://api.omniroute.ai/v1`. Validated
    /// by `provider::validate_gateway_base_url` before persist (https required, or http://localhost
    /// /127.0.0.1 to permit local LiteLLM proxies; other http:// is rejected as SSRF protection).
    #[serde(rename = "baseUrl", default)]
    pub base_url: String,
    /// Env-var NAME holding the gateway key (e.g. `OMNIROUTE_API_KEY`). Never the raw key.
    #[serde(rename = "apiKeyRef", default)]
    pub api_key_ref: String,
    /// Named presets the UI offers when the gateway provider is selected. The preset IDs are
    /// `omniroute`, `openrouter-gw`, `litellm`, `custom` (see `provider::gateway_presets`); the
    /// persisted list is the operator's selection so the UI can pre-select it on reload.
    #[serde(default)]
    pub presets: Vec<String>,
    /// Model ids the gateway routes to. Surfaced to the lead agent as `gateway/<model>` entries
    /// in the low-cost worker list so subagent tasks can be dispatched to gateway models.
    #[serde(rename = "modelAllowlist", default)]
    pub model_allowlist: Vec<String>,
}

impl GatewayConfig {
    /// True when the section is effectively unset (no base URL and no key ref). Used by
    /// `provider_endpoint` to treat a missing/empty gateway as inert (returns None).
    pub fn is_empty(&self) -> bool {
        self.base_url.trim().is_empty() && self.api_key_ref.trim().is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DotzConfig {
    pub provider: String,
    #[serde(rename = "executiveModel")]
    pub executive_model: String,
    #[serde(rename = "subagentModel")]
    pub subagent_model: String,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    /// C6: optional gateway passthrough config. `None` when absent from config.json (a
    /// gateway-free install). Loaded leniently: a malformed `gateway` object is dropped to
    /// `None` rather than failing the whole config load (so a bad gateway block can't brick the
    /// rest of the config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayConfig>,
    /// B3: perf-dashboard recording opt-in. Default false (the privacy moat — perf data
    /// stays on the machine). Independent of the remote-telemetry opt-in (`telemetry.rs`)
    /// so an operator can have the local perf dashboard without enabling remote telemetry.
    /// Persisted under `perfRecording` so it survives a restart. `serde(default)` keeps
    /// pre-B3 config.json files loading unchanged.
    #[serde(rename = "perfRecording", default)]
    pub perf_recording: bool,
}

impl Default for DotzConfig {
    fn default() -> Self {
        let (exec, sub) = types::provider_default(types::DEFAULT_PROVIDER).unwrap();
        DotzConfig {
            provider: types::DEFAULT_PROVIDER.to_string(),
            executive_model: exec.to_string(),
            subagent_model: sub.to_string(),
            thinking_level: "high".to_string(),
            gateway: None,
            perf_recording: false,
        }
    }
}

pub fn dotz_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DOTZ_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dotz")
}

fn config_file() -> PathBuf {
    dotz_dir().join("config.json")
}

/// The bundled subagent extension reads DOTZ_SUBAGENT_MODEL to pin every dispersed subagent's model.
/// Normalizes the subagent model so a persisted value that already includes the provider prefix
/// (e.g. "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free") does not produce a doubled prefix
/// like "openrouter/openrouter/...". Also strips a *mismatched* provider prefix so switching
/// executive provider (e.g. openrouter -> ollama) does not leave a stale prefix in the env var.
/// If no subagent model is configured, derives the provider's default so the env var is always valid.
///
/// OpenRouter model ids are namespaced by upstream provider (e.g. "anthropic/claude-3.5-sonnet",
/// "openai/gpt-4o"). Stripping those namespaces would corrupt valid model ids — the "anthropic/"
/// prefix is a required OpenRouter namespace, not a stale provider prefix. So for OpenRouter we
/// strip only the *current* provider's prefix (via `strip_matching_provider_prefix`), never a
/// foreign one. For all other providers model ids are not namespaced, so stripping any known
/// provider prefix is safe.
pub fn apply_env(c: &DotzConfig) {
    // First normalize the current provider's own prefix (idempotent with load/update, but makes
    // apply_env self-contained so callers and tests don't have to pre-normalize).
    let model = types::strip_matching_provider_prefix(&c.provider, &c.subagent_model);
    // Then strip a stale prefix from a DIFFERENT provider — but NOT for OpenRouter, whose model
    // ids legitimately start with an upstream provider namespace.
    let model = strip_any_provider_prefix(&model, &c.provider);
    let model = if model.trim().is_empty() {
        default_subagent_model(&c.provider)
    } else {
        model
    };
    unsafe {
        std::env::set_var("DOTZ_SUBAGENT_MODEL", format!("{}/{}", c.provider, model));
    }
}

/// Strip any leading "<provider>/" prefix from a model string. This prevents stale prefixes from
/// surviving a provider change and then being re-prepended by apply_env. The match is
/// case-insensitive so a persisted value like "OpenRouter/..." is normalized the same way as
/// "openrouter/...".
///
/// `current_provider` gates the OpenRouter exception: OpenRouter model ids are legitimately
/// namespaced by upstream provider (e.g. "anthropic/claude-3.5-sonnet"), so stripping a foreign
/// provider prefix would corrupt a valid id. For every other provider model ids are bare, so
/// stripping any known-provider prefix is safe.
fn strip_any_provider_prefix(model: &str, current_provider: &str) -> String {
    let trimmed = model.trim();
    if current_provider.eq_ignore_ascii_case("openrouter") {
        return trimmed.to_string();
    }
    let lower = trimmed.to_lowercase();
    for pid in types::provider_ids() {
        let prefix = format!("{pid}/");
        if let Some(_rest) = lower.strip_prefix(&prefix) {
            // provider ids are ASCII, so the byte length of the original prefix equals the
            // lowercase prefix length.
            return trimmed[prefix.len()..].to_string();
        }
    }
    trimmed.to_string()
}

/// Default executive model id for a provider. Falls back to the global default provider's executive
/// when the provider has no registered default.
fn default_executive_model(provider: &str) -> String {
    types::provider_default(provider)
        .map(|(e, _)| e.to_string())
        .unwrap_or_else(|| {
            types::provider_default(types::DEFAULT_PROVIDER)
                .map(|(e, _)| e.to_string())
                .unwrap_or_default()
        })
}

/// Default subagent model id for a provider. Falls back to the global default provider's subagent
/// when the provider has no registered default.
fn default_subagent_model(provider: &str) -> String {
    types::provider_default(provider)
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| {
            types::provider_default(types::DEFAULT_PROVIDER)
                .map(|(_, s)| s.to_string())
                .unwrap_or_default()
        })
}

/// Load config.json, clamping any invalid present field to its default (mirror loadConfig).
pub fn load() -> DotzConfig {
    let mut cfg = DotzConfig::default();
    let mut explicit_executive = false;
    let mut explicit_subagent = false;
    let mut provider_invalid = false;
    if let Ok(raw) = std::fs::read_to_string(config_file()) {
        // A corrupt config.json silently fell back to *all* defaults — an operator whose file got
        // truncated or hand-edited into invalid JSON would see provider/model/thinking reset with
        // no explanation. Surface the parse failure; the default-fallback behavior is unchanged.
        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!(
                    "config: {} is not valid JSON ({e}); using default configuration. \
                     Fix or remove the file to restore your settings.",
                    config_file().display()
                );
                None
            }
        };
        if let Some(v) = parsed {
            if let Some(p) = v.get("provider").and_then(|x| x.as_str()) {
                if types::is_known_provider(p) {
                    cfg.provider = p.to_string();
                } else {
                    provider_invalid = true;
                }
            }
            if let Some(m) = v.get("executiveModel").and_then(|x| x.as_str()) {
                if !m.trim().is_empty() {
                    cfg.executive_model = m.to_string();
                    explicit_executive = true;
                }
            }
            if let Some(m) = v.get("subagentModel").and_then(|x| x.as_str()) {
                if !m.trim().is_empty() {
                    cfg.subagent_model = m.to_string();
                    explicit_subagent = true;
                }
            }
            if let Some(t) = v.get("thinkingLevel").and_then(|x| x.as_str()) {
                if types::is_valid_thinking(t) {
                    cfg.thinking_level = t.to_string();
                }
            }
            // C6: parse the optional gateway section leniently. A malformed `gateway` object is
            // dropped to None (with a stderr warning) rather than failing the whole config load —
            // a bad gateway block must not brick provider/model/thinking. `gateway: null` and a
            // missing field both leave `cfg.gateway = None`.
            if let Some(gw_v) = v.get("gateway") {
                if !gw_v.is_null() {
                    match serde_json::from_value::<GatewayConfig>(gw_v.clone()) {
                        Ok(gw) => cfg.gateway = Some(gw),
                        Err(e) => eprintln!(
                            "config: {} has a malformed 'gateway' section ({e}); \
                             ignoring it. Fix or remove the gateway block to restore it.",
                            config_file().display()
                        ),
                    }
                }
            }
            // B3: parse the optional perfRecording flag (default false). Any non-bool value is
            // ignored so a corrupt field never bricks the config — matches the lenient gateway
            // parse contract.
            if let Some(b) = v.get("perfRecording").and_then(|x| x.as_bool()) {
                cfg.perf_recording = b;
            }
        }
    }
    // If the user changed provider but never set an executive/subagent model, derive both from the
    // new provider instead of leaving stale defaults in place (which would pin the lead/sub agents
    // to models from the previous provider's ecosystem, or break outright when the persisted
    // provider string was invalid and clamped to the default).
    if provider_invalid || !explicit_executive {
        cfg.executive_model = default_executive_model(&cfg.provider);
    }
    if !explicit_subagent {
        cfg.subagent_model = default_subagent_model(&cfg.provider);
    }
    // Normalize explicit model ids so a "provider/model-id" value pasted by the operator does not
    // get sent to the upstream API with a doubled provider prefix.
    cfg.executive_model =
        types::strip_matching_provider_prefix(&cfg.provider, &cfg.executive_model);
    cfg.subagent_model = types::strip_matching_provider_prefix(&cfg.provider, &cfg.subagent_model);
    apply_env(&cfg);
    cfg
}

pub fn save(c: &DotzConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(dotz_dir())?;
    std::fs::write(config_file(), serde_json::to_string_pretty(c)?)?;
    Ok(())
}

/// Apply an already-validated patch, persist, set env, return the new config (mirror updateConfig).
/// Validation (400s) happens in the POST handler before this is called. Persist failures are
/// propagated so the REST handler can surface a 500 instead of silently accepting a config that
/// will be lost on restart.
pub fn update(current: &DotzConfig, clean: &CleanPatch) -> std::io::Result<DotzConfig> {
    let mut next = current.clone();
    let mut provider_changed = false;
    if let Some(p) = &clean.provider {
        provider_changed = next.provider != *p;
        next.provider = p.clone();
    }
    if let Some(m) = &clean.executive_model {
        next.executive_model = m.clone();
    } else if provider_changed {
        // Provider changed without an explicit executive model: re-derive so the lead agent doesn't
        // get pinned to a model from the previous provider's ecosystem.
        next.executive_model = default_executive_model(&next.provider);
    }
    if let Some(m) = &clean.subagent_model {
        next.subagent_model = m.clone();
    } else if provider_changed {
        // Provider changed without an explicit subagent model: re-derive so subagents don't get
        // pinned to a model from the previous provider's ecosystem.
        next.subagent_model = default_subagent_model(&next.provider);
    }
    // Strip a matching provider prefix from explicitly-set model ids so they reach the API as bare
    // model ids (e.g. "ollama/glm-5.2" under provider "ollama" becomes "glm-5.2").
    next.executive_model =
        types::strip_matching_provider_prefix(&next.provider, &next.executive_model);
    next.subagent_model =
        types::strip_matching_provider_prefix(&next.provider, &next.subagent_model);
    if let Some(t) = &clean.thinking_level {
        next.thinking_level = t.clone();
    }
    save(&next)?;
    apply_env(&next);
    Ok(next)
}

/// C6: set (or clear) the gateway section of the config, persist, and return the new config.
/// `gateway = None` clears the section (it is omitted from the persisted JSON via
/// `skip_serializing_if`). Validation (SSRF base-URL check, key-ref shape) happens in the POST
/// handler before this is called. Persist failures are propagated so the REST handler can
/// surface a 500. The gateway config is independent of provider/model/thinking, so this does NOT
/// touch `apply_env` (DOTZ_SUBAGENT_MODEL is unaffected by the gateway).
pub fn set_gateway(
    current: &DotzConfig,
    gateway: Option<GatewayConfig>,
) -> std::io::Result<DotzConfig> {
    let mut next = current.clone();
    next.gateway = gateway;
    save(&next)?;
    Ok(next)
}

/// B3: set the `perfRecording` flag in the config, persist, and return the new config. The flag
/// is independent of the remote-telemetry opt-in (`telemetry.rs`) — it gates the LOCAL perf
/// dashboard only. Persist failures are propagated so the perf toggle route can surface a 500.
/// The perf flag is independent of provider/model/thinking, so this does NOT touch `apply_env`.
pub fn set_perf_recording(current: &DotzConfig, on: bool) -> std::io::Result<DotzConfig> {
    let mut next = current.clone();
    next.perf_recording = on;
    save(&next)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &dir) };
        let result = f(&dir);
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
        result
    }

    #[test]
    fn save_and_load_round_trip() {
        with_tmp_dir(|dir| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            save(&cfg).unwrap();
            assert!(dir.join("config.json").exists());

            let loaded = load();
            assert_eq!(loaded.provider, "openrouter");
            assert_eq!(loaded.executive_model, "nex-agi/nex-n2-pro");
            assert_eq!(loaded.subagent_model, "minimax-m3");
            assert_eq!(loaded.thinking_level, "medium");
        });
    }

    #[test]
    fn update_persists_to_disk_and_sets_env() {
        with_tmp_dir(|dir| {
            let base = load();
            let patch = CleanPatch {
                provider: Some("openrouter".into()),
                executive_model: Some("anthropic/claude-sonnet-4".into()),
                subagent_model: Some("openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into()),
                thinking_level: Some("xhigh".into()),
            };
            let next = update(&base, &patch).unwrap();
            assert_eq!(next.provider, "openrouter");
            assert_eq!(next.thinking_level, "xhigh");

            let raw = std::fs::read_to_string(dir.join("config.json")).unwrap();
            assert!(raw.contains("openrouter"));
            assert!(raw.contains("xhigh"));

            let reloaded = load();
            assert_eq!(reloaded.provider, "openrouter");
            assert_eq!(reloaded.thinking_level, "xhigh");

            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
            );
        });
    }

    #[test]
    fn update_propagates_save_failure() {
        with_tmp_dir(|dir| {
            // Make the "config dir" path exist as a file so create_dir_all fails.
            let fake_dir = dir.join("fake-config-dir");
            std::fs::write(&fake_dir, "not a directory").unwrap();
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &fake_dir) };
            assert!(fake_dir.is_file(), "fake_dir must be a file for this test");

            let base = DotzConfig::default();
            let patch = CleanPatch {
                provider: Some("openrouter".into()),
                ..Default::default()
            };
            let env_before = std::env::var("DOTZ_SUBAGENT_MODEL").unwrap_or_default();
            let result = update(&base, &patch);
            assert!(
                result.is_err(),
                "update must fail when config dir is a file, got: {result:?}"
            );

            // DOTZ_SUBAGENT_MODEL must NOT change when persistence failed.
            let env_after = std::env::var("DOTZ_SUBAGENT_MODEL").unwrap_or_default();
            assert_eq!(
                env_after, env_before,
                "env must not update when config save fails: before={env_before}, after={env_after}"
            );
        });
    }

    #[test]
    fn apply_env_strips_matching_provider_prefix() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro".into(),
                subagent_model: "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
            );

            // A model id without the provider prefix is left unchanged.
            let cfg2 = DotzConfig {
                provider: "ollama".into(),
                subagent_model: "minimax-m3".into(),
                ..cfg
            };
            apply_env(&cfg2);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/minimax-m3"
            );
        });
    }

    /// Switching executive provider must not leave a stale provider prefix in DOTZ_SUBAGENT_MODEL.
    /// Without this normalization, a subagent_model of "openrouter/..." under provider "ollama"
    /// would become the invalid "ollama/openrouter/...".
    #[test]
    fn apply_env_strips_mismatched_provider_prefix() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/nvidia/nemotron-3-ultra-550b-a55b:free"
            );
        });
    }

    /// Provider prefixes in the persisted subagent model must be stripped case-insensitively.
    /// Without this, "OpenRouter/model" under provider "openrouter" becomes the doubled
    /// invalid prefix "openrouter/OpenRouter/model".
    #[test]
    fn apply_env_strips_case_insensitive_provider_prefix() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro:free".into(),
                subagent_model: "OpenRouter/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
            );

            // Also covers a mismatched provider prefix with different casing.
            let cfg2 = DotzConfig {
                provider: "ollama".into(),
                subagent_model: "OPENROUTER/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                ..cfg
            };
            apply_env(&cfg2);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/nvidia/nemotron-3-ultra-550b-a55b:free"
            );
        });
    }

    /// OpenRouter model ids are namespaced by upstream provider (e.g. "anthropic/claude-3.5-sonnet",
    /// "openai/gpt-4o"). `strip_any_provider_prefix` used to strip those namespaces, corrupting
    /// valid model ids: under provider "openrouter", "anthropic/claude-3.5-sonnet" became
    /// "openrouter/claude-3.5-sonnet" (an invalid OpenRouter id that 404s). The fix skips
    /// foreign-prefix stripping for OpenRouter so the required namespace survives into
    /// DOTZ_SUBAGENT_MODEL.
    #[test]
    fn apply_env_preserves_openrouter_namespaced_model_ids() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "anthropic/claude-3.5-sonnet-latest".into(),
                subagent_model: "anthropic/claude-3.5-sonnet-latest".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/anthropic/claude-3.5-sonnet-latest",
                "OpenRouter namespaced model id must keep its upstream-provider namespace"
            );

            // A different upstream namespace (openai/gpt-4o) must also be preserved.
            let cfg2 = DotzConfig {
                subagent_model: "openai/gpt-4o".into(),
                ..cfg
            };
            apply_env(&cfg2);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/openai/gpt-4o",
                "OpenRouter namespaced model id must keep its upstream-provider namespace"
            );
        });
    }

    /// The OpenRouter exception must NOT silently keep a stale *current-provider* prefix. A
    /// subagent_model of "openrouter/nvidia/..." under provider "openrouter" must still shed the
    /// redundant "openrouter/" so the env var is "openrouter/nvidia/...", not the doubled
    /// "openrouter/openrouter/nvidia/...".
    #[test]
    fn apply_env_strips_current_provider_prefix_even_for_openrouter() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro:free".into(),
                subagent_model: "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free",
                "the current provider's own prefix must still be stripped for OpenRouter"
            );
        });
    }

    #[test]
    fn load_derives_subagent_model_when_missing() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            save(&cfg).unwrap();

            // Write a config that changes provider but omits subagentModel.
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "openrouter",
  "executiveModel": "nex-agi/nex-n2-pro",
  "thinkingLevel": "medium"
}"#,
            )
            .unwrap();

            let loaded = load();
            assert_eq!(loaded.provider, "openrouter");
            assert_eq!(
                loaded.subagent_model, "nex-agi/nex-n2-pro:free",
                "subagent_model must be derived from the configured provider when omitted"
            );
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nex-agi/nex-n2-pro:free"
            );
        });
    }

    /// A config that switches provider without specifying an executive model must not keep the old
    /// provider's executive model (which would produce an invalid provider/model pair at runtime).
    #[test]
    fn load_derives_executive_model_when_missing() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            save(&cfg).unwrap();

            // Provider changes to openrouter, but executiveModel is omitted.
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "openrouter",
  "subagentModel": "nex-agi/nex-n2-pro:free",
  "thinkingLevel": "medium"
}"#,
            )
            .unwrap();

            let loaded = load();
            assert_eq!(loaded.provider, "openrouter");
            assert_eq!(
                loaded.executive_model, "nex-agi/nex-n2-pro:free",
                "executive_model must be derived from the configured provider when omitted"
            );
        });
    }

    /// An invalid persisted provider is clamped to the default provider, and the executive model
    /// must be re-derived so it matches the clamped provider instead of keeping a stale value
    /// from the invalid one.
    #[test]
    fn load_derives_executive_model_when_provider_invalid() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "not-a-real-provider",
  "executiveModel": "some-foreign-model",
  "thinkingLevel": "medium"
}"#,
            )
            .unwrap();

            let loaded = load();
            assert_eq!(loaded.provider, types::DEFAULT_PROVIDER);
            assert_eq!(
                loaded.executive_model, "glm-5.2",
                "executive_model must be derived after an invalid provider is clamped to default"
            );
        });
    }

    #[test]
    fn update_re_derives_subagent_model_when_provider_changes() {
        with_tmp_dir(|_| {
            let base = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: None,
                perf_recording: false,
            };
            let patch = CleanPatch {
                provider: Some("openrouter".into()),
                ..Default::default()
            };
            let next = update(&base, &patch).unwrap();
            assert_eq!(next.provider, "openrouter");
            assert_eq!(
                next.subagent_model, "nex-agi/nex-n2-pro:free",
                "provider change without explicit subagent_model must re-derive"
            );
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nex-agi/nex-n2-pro:free"
            );
        });
    }

    /// Changing provider via update() without an explicit executive model must re-derive it, just
    /// like subagent_model, so the lead agent doesn't end up with the previous provider's model.
    #[test]
    fn update_re_derives_executive_model_when_provider_changes() {
        with_tmp_dir(|_| {
            let base = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: None,
                perf_recording: false,
            };
            let patch = CleanPatch {
                provider: Some("openrouter".into()),
                ..Default::default()
            };
            let next = update(&base, &patch).unwrap();
            assert_eq!(next.provider, "openrouter");
            assert_eq!(
                next.executive_model, "nex-agi/nex-n2-pro:free",
                "provider change without explicit executive_model must re-derive"
            );
        });
    }

    #[test]
    fn update_preserves_explicit_subagent_model_for_unrelated_changes() {
        with_tmp_dir(|_| {
            let base = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: None,
                perf_recording: false,
            };
            let patch = CleanPatch {
                thinking_level: Some("xhigh".into()),
                ..Default::default()
            };
            let next = update(&base, &patch).unwrap();
            assert_eq!(next.thinking_level, "xhigh");
            assert_eq!(
                next.subagent_model, "minimax-m3",
                "unrelated patch must not re-derive subagent_model"
            );
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/minimax-m3"
            );
        });
    }

    #[test]
    fn apply_env_derives_when_subagent_model_empty() {
        with_tmp_dir(|_| {
            let cfg = DotzConfig {
                provider: "openrouter".into(),
                executive_model: "nex-agi/nex-n2-pro".into(),
                subagent_model: "".into(),
                thinking_level: "medium".into(),
                gateway: None,
                perf_recording: false,
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "openrouter/nex-agi/nex-n2-pro:free"
            );
        });
    }

    /// A persisted executiveModel that redundantly includes the current provider prefix (a common
    /// copy/paste mistake) must be normalized to a bare model id so the upstream API receives the
    /// correct value. Without this, the provider call would use "ollama/glm-5.2" as the model id
    /// and fail.
    #[test]
    fn load_strips_matching_provider_prefix_from_model_ids() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "ollama",
  "executiveModel": "ollama/glm-5.2",
  "subagentModel": "ollama/minimax-m3",
  "thinkingLevel": "medium"
}"#,
            )
            .unwrap();

            let loaded = load();
            assert_eq!(loaded.provider, "ollama");
            assert_eq!(
                loaded.executive_model, "glm-5.2",
                "matching provider prefix must be stripped from executive_model"
            );
            assert_eq!(
                loaded.subagent_model, "minimax-m3",
                "matching provider prefix must be stripped from subagent_model"
            );
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/minimax-m3"
            );
        });
    }

    /// update() must normalize explicit model ids that include the current provider prefix.
    /// The returned config and the persisted file should both store bare model ids.
    #[test]
    fn update_strips_matching_provider_prefix_from_model_ids() {
        with_tmp_dir(|_| {
            let base = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: None,
                perf_recording: false,
            };
            let patch = CleanPatch {
                provider: Some("openrouter".into()),
                executive_model: Some("openrouter/nex-agi/nex-n2-pro:free".into()),
                subagent_model: Some("openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into()),
                thinking_level: None,
            };
            let next = update(&base, &patch).unwrap();
            assert_eq!(next.provider, "openrouter");
            assert_eq!(
                next.executive_model, "nex-agi/nex-n2-pro:free",
                "update must strip matching provider prefix from executive_model"
            );
            assert_eq!(
                next.subagent_model, "nvidia/nemotron-3-ultra-550b-a55b:free",
                "update must strip matching provider prefix from subagent_model"
            );

            let raw = std::fs::read_to_string(config_file()).unwrap();
            assert!(
                !raw.contains("openrouter/nex-agi"),
                "persisted executive_model must not contain the provider prefix"
            );
            assert!(
                !raw.contains("openrouter/nvidia"),
                "persisted subagent_model must not contain the provider prefix"
            );
        });
    }

    /// Prefixes that do NOT match the current provider must be preserved so cross-provider model
    /// namespaces (e.g. an OpenRouter model id that starts with "ollama/") are not corrupted.
    #[test]
    fn load_preserves_mismatched_provider_prefix() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "openrouter",
  "executiveModel": "ollama/glm-5.2",
  "subagentModel": "ollama/minimax-m3",
  "thinkingLevel": "medium"
}"#,
            )
            .unwrap();

            let loaded = load();
            assert_eq!(loaded.provider, "openrouter");
            assert_eq!(
                loaded.executive_model, "ollama/glm-5.2",
                "a mismatched provider prefix must not be stripped"
            );
        });
    }

    /// A config.json that is not valid JSON at all (truncated, hand-mangled) must degrade to the
    /// default configuration rather than panicking. This pins the graceful-degradation contract for
    /// the top-level parse: load() never crashes the process on a corrupt config file. The parse
    /// failure is surfaced to stderr (see load()), but the returned config is still the default.
    #[test]
    fn load_degrades_to_defaults_on_invalid_json() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(&file, b"{ not valid json at all ]").unwrap();

            let loaded = load();
            let default = DotzConfig::default();
            assert_eq!(
                loaded.provider, default.provider,
                "invalid JSON config must fall back to the default provider"
            );
            assert_eq!(
                loaded.executive_model, default.executive_model,
                "invalid JSON config must fall back to the default executive model"
            );
            assert_eq!(
                loaded.subagent_model, default.subagent_model,
                "invalid JSON config must fall back to the default subagent model"
            );
            assert_eq!(
                loaded.thinking_level, default.thinking_level,
                "invalid JSON config must fall back to the default thinking level"
            );
        });
    }

    // ---- C6: gateway config section ----

    /// C6: a persisted gateway section round-trips through save/load.
    #[test]
    fn save_and_load_round_trips_gateway_section() {
        with_tmp_dir(|dir| {
            let cfg = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: Some(GatewayConfig {
                    base_url: "https://api.omniroute.ai/v1".into(),
                    api_key_ref: "OMNIROUTE_API_KEY".into(),
                    presets: vec!["omniroute".into()],
                    model_allowlist: vec!["gpt-5.6".into(), "claude-sonnet-5".into()],
                }),
                perf_recording: false,
            };
            save(&cfg).unwrap();
            assert!(dir.join("config.json").exists());

            let loaded = load();
            let gw = loaded
                .gateway
                .as_ref()
                .expect("gateway section must round-trip");
            assert_eq!(gw.base_url, "https://api.omniroute.ai/v1");
            assert_eq!(gw.api_key_ref, "OMNIROUTE_API_KEY");
            assert_eq!(gw.presets, vec!["omniroute".to_string()]);
            assert_eq!(
                gw.model_allowlist,
                vec!["gpt-5.6".to_string(), "claude-sonnet-5".to_string()]
            );
        });
    }

    /// C6: a missing gateway section loads as None (gateway-free install is unchanged).
    #[test]
    fn load_returns_none_gateway_when_section_absent() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "ollama",
  "executiveModel": "glm-5.2",
  "subagentModel": "minimax-m3",
  "thinkingLevel": "high"
}"#,
            )
            .unwrap();
            let loaded = load();
            assert!(loaded.gateway.is_none(), "missing gateway section → None");
        });
    }

    /// C6: `gateway: null` loads as None (an explicit null clears the section).
    #[test]
    fn load_returns_none_gateway_when_section_null() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "ollama",
  "executiveModel": "glm-5.2",
  "subagentModel": "minimax-m3",
  "thinkingLevel": "high",
  "gateway": null
}"#,
            )
            .unwrap();
            let loaded = load();
            assert!(loaded.gateway.is_none(), "explicit null gateway → None");
        });
    }

    /// C6: a malformed gateway section is dropped to None WITHOUT bricking the rest of the config
    /// (provider/model/thinking still load). The parse failure is surfaced to stderr.
    #[test]
    fn load_drops_malformed_gateway_without_bricking_config() {
        with_tmp_dir(|_| {
            let file = config_file();
            std::fs::write(
                &file,
                r#"{
  "provider": "ollama",
  "executiveModel": "glm-5.2",
  "subagentModel": "minimax-m3",
  "thinkingLevel": "high",
  "gateway": { "baseUrl": 12345 }
}"#,
            )
            .unwrap();
            let loaded = load();
            // The non-gateway fields still load.
            assert_eq!(loaded.provider, "ollama");
            assert_eq!(loaded.executive_model, "glm-5.2");
            // The malformed gateway is dropped to None (not panicked on).
            assert!(
                loaded.gateway.is_none(),
                "malformed gateway → None, not a panic"
            );
        });
    }

    /// C6: `set_gateway` persists the gateway section and returns the new config; the persisted
    /// file includes it; a subsequent `load()` sees it.
    #[test]
    fn set_gateway_persists_and_returns_new_config() {
        with_tmp_dir(|dir| {
            let base = load();
            assert!(base.gateway.is_none());

            let gw = GatewayConfig {
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key_ref: "OPENROUTER_API_KEY".into(),
                presets: vec!["openrouter-gw".into()],
                model_allowlist: vec!["anthropic/claude-sonnet-5".into()],
            };
            let next = set_gateway(&base, Some(gw.clone())).unwrap();
            assert_eq!(next.gateway.as_ref().unwrap().base_url, gw.base_url);

            // Persisted file contains the gateway block.
            let raw = std::fs::read_to_string(dir.join("config.json")).unwrap();
            assert!(raw.contains("openrouter.ai/api/v1"));
            assert!(raw.contains("OPENROUTER_API_KEY"));

            // A fresh load sees it.
            let reloaded = load();
            assert_eq!(reloaded.gateway.as_ref().unwrap().base_url, gw.base_url);
            assert_eq!(
                reloaded.gateway.as_ref().unwrap().model_allowlist,
                vec!["anthropic/claude-sonnet-5".to_string()]
            );
        });
    }

    /// C6: `set_gateway(None)` clears an existing gateway section (omitted from persisted JSON).
    #[test]
    fn set_gateway_none_clears_existing_section() {
        with_tmp_dir(|dir| {
            let cfg = DotzConfig {
                provider: "ollama".into(),
                executive_model: "glm-5.2".into(),
                subagent_model: "minimax-m3".into(),
                thinking_level: "high".into(),
                gateway: Some(GatewayConfig {
                    base_url: "https://api.omniroute.ai/v1".into(),
                    api_key_ref: "OMNIROUTE_API_KEY".into(),
                    presets: vec![],
                    model_allowlist: vec![],
                }),
                perf_recording: false,
            };
            save(&cfg).unwrap();
            assert!(dir.join("config.json").exists());

            let next = set_gateway(&cfg, None).unwrap();
            assert!(next.gateway.is_none());

            let raw = std::fs::read_to_string(dir.join("config.json")).unwrap();
            assert!(
                !raw.contains("omniroute"),
                "clearing the gateway must omit it from the persisted JSON, got: {raw}"
            );
        });
    }

    /// C6: `GatewayConfig::is_empty` is true when both base URL + key ref are empty (the gateway
    /// is inert), false otherwise.
    #[test]
    fn gateway_config_is_empty_when_both_fields_blank() {
        assert!(GatewayConfig::default().is_empty());
        assert!(
            GatewayConfig {
                base_url: "   ".into(),
                api_key_ref: "".into(),
                presets: vec![],
                model_allowlist: vec![],
            }
            .is_empty()
        );
        assert!(
            !GatewayConfig {
                base_url: "https://api.omniroute.ai/v1".into(),
                api_key_ref: "".into(),
                presets: vec![],
                model_allowlist: vec![],
            }
            .is_empty()
        );
        assert!(
            !GatewayConfig {
                base_url: "".into(),
                api_key_ref: "OMNIROUTE_API_KEY".into(),
                presets: vec![],
                model_allowlist: vec![],
            }
            .is_empty()
        );
    }

    /// C6: a config with a gateway section must still serialize the other fields unchanged, and
    /// the gateway section uses the camelCase JSON keys the UI contract expects.
    #[test]
    fn gateway_section_uses_camel_case_json_keys() {
        let cfg = DotzConfig {
            provider: "ollama".into(),
            executive_model: "glm-5.2".into(),
            subagent_model: "minimax-m3".into(),
            thinking_level: "high".into(),
            gateway: Some(GatewayConfig {
                base_url: "http://localhost:4000/v1".into(),
                api_key_ref: "LITELLM_API_KEY".into(),
                presets: vec!["litellm".into()],
                model_allowlist: vec!["gpt-5.6".into()],
            }),
            perf_recording: false,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        let gw = &json["gateway"];
        assert_eq!(gw["baseUrl"], "http://localhost:4000/v1");
        assert_eq!(gw["apiKeyRef"], "LITELLM_API_KEY");
        assert_eq!(gw["presets"], serde_json::json!(["litellm"]));
        assert_eq!(gw["modelAllowlist"], serde_json::json!(["gpt-5.6"]));
    }
}

/// A validated config patch (built by the POST handler).
#[derive(Default)]
pub struct CleanPatch {
    pub provider: Option<String>,
    pub executive_model: Option<String>,
    pub subagent_model: Option<String>,
    pub thinking_level: Option<String>,
}
