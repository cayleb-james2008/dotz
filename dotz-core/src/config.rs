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
pub fn apply_env(c: &DotzConfig) {
    std::env::set_var(
        "DOTZ_SUBAGENT_MODEL",
        format!("{}/{}", c.provider, c.subagent_model),
    );
}

/// Load config.json, clamping any invalid present field to its default (mirror loadConfig).
pub fn load() -> DotzConfig {
    let mut cfg = DotzConfig::default();
    if let Ok(raw) = std::fs::read_to_string(config_file()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(p) = v.get("provider").and_then(|x| x.as_str()) {
                if types::is_known_provider(p) {
                    cfg.provider = p.to_string();
                }
            }
            if let Some(m) = v.get("executiveModel").and_then(|x| x.as_str()) {
                if !m.trim().is_empty() {
                    cfg.executive_model = m.to_string();
                }
            }
            if let Some(m) = v.get("subagentModel").and_then(|x| x.as_str()) {
                if !m.trim().is_empty() {
                    cfg.subagent_model = m.to_string();
                }
            }
            if let Some(t) = v.get("thinkingLevel").and_then(|x| x.as_str()) {
                if types::is_valid_thinking(t) {
                    cfg.thinking_level = t.to_string();
                }
            }
        }
    }
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
    if let Some(p) = &clean.provider {
        next.provider = p.clone();
    }
    if let Some(m) = &clean.executive_model {
        next.executive_model = m.clone();
    }
    if let Some(m) = &clean.subagent_model {
        next.subagent_model = m.clone();
    }
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
                "openrouter/openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
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
}

/// A validated config patch (built by the POST handler).
#[derive(Default)]
pub struct CleanPatch {
    pub provider: Option<String>,
    pub executive_model: Option<String>,
    pub subagent_model: Option<String>,
    pub thinking_level: Option<String>,
}
