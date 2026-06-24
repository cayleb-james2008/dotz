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
/// Validation (400s) happens in the POST handler before this is called.
pub fn update(current: &DotzConfig, clean: &CleanPatch) -> DotzConfig {
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
    apply_env(&next);
    let _ = save(&next);
    next
}

/// A validated config patch (built by the POST handler).
#[derive(Default)]
pub struct CleanPatch {
    pub provider: Option<String>,
    pub executive_model: Option<String>,
    pub subagent_model: Option<String>,
    pub thinking_level: Option<String>,
}
