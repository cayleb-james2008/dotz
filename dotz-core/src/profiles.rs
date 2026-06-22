//! dotz agent profile summaries — port of profiles.ts `profileSummary` (for /api/profiles).
//! Full doctrines (appendSystemPrompt) land in Phase 3 (prompt assembly); not needed for this endpoint.
use crate::types::{default_model, ModelRef};
use serde::Serialize;

#[derive(Serialize)]
pub struct ProfileSummary {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub workflow: bool,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: &'static str,
    pub model: ModelRef,
}

/// The 6 profiles, in order, matching PROFILES in profiles.ts. (default = "workflow")
pub fn summaries() -> Vec<ProfileSummary> {
    let m = default_model;
    vec![
        ProfileSummary { id: "workflow", name: "WORKFLOW", tagline: "Multi-agent dispersal by default · ultra", workflow: true, thinking_level: "high", model: m() },
        ProfileSummary { id: "solo", name: "SOLO", tagline: "Single agent · direct execution", workflow: false, thinking_level: "medium", model: m() },
        ProfileSummary { id: "plan", name: "PLAN", tagline: "Read-only research & planning", workflow: true, thinking_level: "high", model: m() },
        ProfileSummary { id: "frontend", name: "FRONTEND", tagline: "UI / design workflow", workflow: true, thinking_level: "high", model: m() },
        ProfileSummary { id: "backend", name: "BACKEND", tagline: "APIs / data / infra workflow", workflow: true, thinking_level: "high", model: m() },
        ProfileSummary { id: "design", name: "DESIGN", tagline: "Graphic & visual design · Open Design (native)", workflow: true, thinking_level: "high", model: m() },
    ]
}
