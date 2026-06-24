//! dotz global config — port of src/config.ts. Persists provider/model/reasoning defaults to
//! ~/.dotz/config.json (DOTZ_CONFIG_DIR overrides), and exports DOTZ_SUBAGENT_MODEL for subagents.
use crate::types;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DotzConfig {
    pub provider: String,
    #[serde(rename = "executiveModel")]
    pub executive_model: String,
    #[serde(rename = "subagentModel")]
    pub subagent_model: String,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
}

impl Default for DotzConfig {
    fn default() -> Self {
        let (exec, sub) = types::provider_default(types::DEFAULT_PROVIDER).unwrap();
        DotzConfig {
            provider: types::DEFAULT_PROVIDER.to_string(),
            executive_model: exec.to_string(),
            subagent_model: sub.to_string(),
            thinking_level: "high".to_string(),
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
pub fn apply_env(c: &DotzConfig) {
    let model = strip_any_provider_prefix(&c.subagent_model);
    let model = if model.trim().is_empty() {
        default_subagent_model(&c.provider)
    } else {
        model
    };
    std::env::set_var("DOTZ_SUBAGENT_MODEL", format!("{}/{}", c.provider, model));
}

/// Strip any leading "<provider>/" prefix from a model string. This prevents stale prefixes from
/// surviving a provider change and then being re-prepended by apply_env.
fn strip_any_provider_prefix(model: &str) -> String {
    let trimmed = model.trim();
    for pid in types::provider_ids() {
        if let Some(rest) = trimmed.strip_prefix(&format!("{pid}/")) {
            return rest.to_string();
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
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        let result = f(&dir);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
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
            std::env::set_var("DOTZ_CONFIG_DIR", &fake_dir);
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
                "update must fail when config dir is a file, got: {:?}",
                result
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
            };
            apply_env(&cfg);
            assert_eq!(
                std::env::var("DOTZ_SUBAGENT_MODEL").unwrap(),
                "ollama/nvidia/nemotron-3-ultra-550b-a55b:free"
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
}

/// A validated config patch (built by the POST handler).
#[derive(Default)]
pub struct CleanPatch {
    pub provider: Option<String>,
    pub executive_model: Option<String>,
    pub subagent_model: Option<String>,
    pub thinking_level: Option<String>,
}
