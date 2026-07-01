//! Keyboard command palette registry.
//!
//! Single source of truth for the action taxonomy surfaced in dotz's keyboard-driven command
//! palette (`Ctrl/Cmd+K` in the web UI). The vanilla `web/` UI fetches `GET /api/commands` once,
//! merges these action commands with its own panel-toggle commands, and renders a fuzzy,
//! arrow-navigable overlay so a power user can switch model/reasoning, navigate the workflow
//! graph, and re-run a step without reaching for the mouse.
//!
//! The catalog is built from `types::THINKING_LEVELS` and `types::provider_ids()` so the palette
//! can never drift from the backend's accepted reasoning levels / providers. Panel names are a
//! UI concern (the bento registry lives in `web/app.js`) and are NOT duplicated here.
//!
//! Design note: this module is deliberately dependency-free and pure — `catalog()` is a plain
//! function returning a `Vec<Command>`, which makes the palette's contract unit-testable without
//! spinning up axum. The handler below just wraps it in JSON.

use axum::{
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

/// A single palette entry. The UI dispatches on `action` (optionally with `arg`); `key` is an
/// accelerator hint rendered in the row (not enforced server-side — the UI owns keybindings).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub id: String,
    pub category: &'static str,
    pub label: String,
    pub description: String,
    pub key: Option<String>,
    pub action: String,
    pub arg: Option<String>,
    /// `true` if the action only makes sense with an active session (graph/step commands). The UI
    /// greys these out in the command center and refuses to fire them pre-session.
    pub requires_session: bool,
}

/// The closed set of categories the palette groups by. Adding a category here without a command
/// is a test failure (see `validate`), keeping the palette free of empty headings.
pub const CATEGORIES: [&str; 5] = ["model", "reasoning", "graph", "step", "view"];

pub fn is_known_category(c: &str) -> bool {
    CATEGORIES.contains(&c)
}

/// Title-case a slug for display ("ollama" → "Ollama", "xhigh" → "Xhigh"). Used so provider/level
/// ids render cleanly in palette labels without a per-entry label map to maintain.
fn title(slug: &str) -> String {
    let mut out = String::with_capacity(slug.len());
    let mut up = true;
    for ch in slug.chars() {
        if up && ch.is_ascii_alphabetic() {
            out.extend(ch.to_uppercase());
            up = false;
        } else {
            out.push(ch);
        }
        if ch.is_whitespace() || ch == '-' || ch == '_' {
            up = true;
        }
    }
    out
}

/// Build the full command catalog. Pure and side-effect-free so it can be asserted on in tests.
pub fn catalog() -> Vec<Command> {
    use crate::types;

    let mut out = Vec::new();

    // ---- model ----
    // One command per known provider, plus a free-form "set executive model id" prompt. Switching
    // provider is safe pre-session (it persists to config and the session inherits it on open),
    // so these are NOT requires_session.
    for pid in types::provider_ids() {
        out.push(Command {
            id: format!("model.provider.{pid}"),
            category: "model",
            label: format!("Switch provider → {}", title(pid)),
            description: format!("Set the active model provider to {pid} and apply its default models."),
            key: None,
            action: "model:provider".to_string(),
            arg: Some(pid.to_string()),
            requires_session: false,
        });
    }
    out.push(Command {
        id: "model.set-id".to_string(),
        category: "model",
        label: "Set executive model id…".to_string(),
        description: "Prompt for a model id and apply it as the executive (lead) model.".to_string(),
        key: None,
        action: "model:set-id".to_string(),
        arg: None,
        requires_session: false,
    });

    // ---- reasoning ----
    // One command per accepted thinking level. Built from `THINKING_LEVELS` so a level added there
    // automatically appears in the palette — no second place to edit.
    for &lvl in types::THINKING_LEVELS.iter() {
        out.push(Command {
            id: format!("reasoning.{lvl}"),
            category: "reasoning",
            label: format!("Reasoning → {}", title(lvl)),
            description: format!("Set the reasoning/thinking level to {lvl}."),
            key: None,
            action: "reasoning:set".to_string(),
            arg: Some(lvl.to_string()),
            requires_session: false,
        });
    }

    // ---- graph ----
    // Workflow-graph navigation. All require an active session with a workflow run.
    out.push(Command {
        id: "graph.fit".to_string(),
        category: "graph",
        label: "Graph: fit to view".to_string(),
        description: "Open the workflow graph panel and fit the DAG to the viewport.".to_string(),
        key: Some("G F".to_string()),
        action: "graph:fit".to_string(),
        arg: None,
        requires_session: true,
    });
    out.push(Command {
        id: "graph.reset".to_string(),
        category: "graph",
        label: "Graph: reset view".to_string(),
        description: "Reset the workflow graph pan/zoom to the default viewport.".to_string(),
        key: Some("G R".to_string()),
        action: "graph:reset".to_string(),
        arg: None,
        requires_session: true,
    });
    out.push(Command {
        id: "graph.focus-step".to_string(),
        category: "graph",
        label: "Graph: focus a step…".to_string(),
        description: "Open a step from the active workflow run and show its node detail.".to_string(),
        key: Some("G S".to_string()),
        action: "graph:focus-step".to_string(),
        arg: None,
        requires_session: true,
    });

    // ---- step ----
    // Re-run the step currently shown in the node-detail drawer. requires_session + an open node.
    out.push(Command {
        id: "step.rerun".to_string(),
        category: "step",
        label: "Re-run selected step".to_string(),
        description: "Re-run the step currently open in the node-detail drawer (no feedback).".to_string(),
        key: Some("R".to_string()),
        action: "step:rerun".to_string(),
        arg: None,
        requires_session: true,
    });
    out.push(Command {
        id: "step.rerun-feedback".to_string(),
        category: "step",
        label: "Re-run selected step with feedback…".to_string(),
        description: "Prompt for feedback and re-run the step currently open in the node-detail drawer.".to_string(),
        key: Some("Shift+R".to_string()),
        action: "step:rerun-feedback".to_string(),
        arg: None,
        requires_session: true,
    });

    // ---- view ----
    // Open/toggle the panels palette (the existing Ctrl+P grid) — surfaced here so a single
    // keyboard surface reaches every panel. Panel names stay UI-side; this just opens the grid.
    out.push(Command {
        id: "view.panels".to_string(),
        category: "view",
        label: "Open panels palette".to_string(),
        description: "Open the add/remove panel grid (the Ctrl+P panel palette).".to_string(),
        key: Some("Ctrl+P".to_string()),
        action: "view:panels".to_string(),
        arg: None,
        requires_session: false,
    });

    out
}

/// Validate a catalog batch: unique ids, known categories, non-empty label/description/action,
/// unique non-`None` keybindings (no two commands share the same accelerator), and that every
/// declared category has at least one command (no empty palette headings). Returns the first
/// problem found, or `Ok(())`.
pub fn validate(cmds: &[Command]) -> Result<(), String> {
    let mut seen_ids = std::collections::HashSet::new();
    // Track keybindings; only `Some(key)` entries collide — a `None` key means "no accelerator",
    // so any number of commands may legitimately have no keybinding.
    let mut seen_keys = std::collections::HashSet::new();
    for c in cmds {
        if c.id.is_empty() {
            return Err("command with empty id".to_string());
        }
        if !seen_ids.insert(&c.id) {
            return Err(format!("duplicate command id: {}", c.id));
        }
        if !is_known_category(c.category) {
            return Err(format!("unknown category '{}' on {}", c.category, c.id));
        }
        if c.label.is_empty() {
            return Err(format!("empty label on {}", c.id));
        }
        if c.description.is_empty() {
            return Err(format!("empty description on {}", c.id));
        }
        if c.action.is_empty() {
            return Err(format!("empty action on {}", c.id));
        }
        if let Some(k) = &c.key {
            if k.is_empty() {
                return Err(format!("empty keybinding on {}", c.id));
            }
            if !seen_keys.insert(k.clone()) {
                return Err(format!("duplicate keybinding '{}' on {}", k, c.id));
            }
        }
    }
    for cat in CATEGORIES {
        if !cmds.iter().any(|c| c.category == cat) {
            return Err(format!("category '{}' has no commands", cat));
        }
    }
    Ok(())
}

/// Serialize a command to the JSON shape the palette renders.
fn cmd_to_json(c: &Command) -> Value {
    json!({
        "id": c.id,
        "category": c.category,
        "label": c.label,
        "description": c.description,
        "key": c.key,
        "action": c.action,
        "arg": c.arg,
        "requiresSession": c.requires_session,
    })
}

async fn list_commands() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cmds = catalog();
    // The catalog is statically built from validated inputs, but a regression that produced an
    // invalid shape would silently break the palette — surface it as a 500 rather than ship a
    // half-rendered overlay. This also keeps the contract assertion live in non-test runs.
    if let Err(e) = validate(&cmds) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("invalid command catalog: {e}") })),
        ));
    }
    Ok(Json(json!({ "commands": cmds.iter().map(cmd_to_json).collect::<Vec<_>>() })))
}

/// Stateless `Router<()>` merged into `server::app()`.
pub fn router() -> Router<()> {
    Router::new().route("/api/commands", get(list_commands))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types;

    #[test]
    fn catalog_is_valid() {
        let cmds = catalog();
        // validate() is the contract; failing it here would also fail the live endpoint (500),
        // so this is the canonical guard.
        validate(&cmds).expect("command catalog must be valid");
    }

    #[test]
    fn catalog_ids_are_unique() {
        let cmds = catalog();
        let ids: Vec<&str> = cmds.iter().map(|c| c.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate command ids: {ids:?}");
    }

    #[test]
    fn every_reasoning_level_has_a_command() {
        let cmds = catalog();
        for &lvl in types::THINKING_LEVELS.iter() {
            let id = format!("reasoning.{lvl}");
            assert!(
                cmds.iter().any(|c| c.id == id && c.category == "reasoning" && c.arg.as_deref() == Some(lvl)),
                "missing reasoning command for level {lvl}"
            );
        }
    }

    #[test]
    fn every_provider_has_a_switch_command() {
        let cmds = catalog();
        for pid in types::provider_ids() {
            let id = format!("model.provider.{pid}");
            assert!(
                cmds.iter().any(|c| c.id == id && c.category == "model" && c.arg.as_deref() == Some(pid)),
                "missing model switch command for provider {pid}"
            );
        }
    }

    #[test]
    fn graph_and_step_commands_require_session() {
        let cmds = catalog();
        for c in cmds.iter().filter(|c| c.category == "graph" || c.category == "step") {
            assert!(c.requires_session, "{} should require a session", c.id);
        }
    }

    #[test]
    fn model_and_reasoning_commands_do_not_require_session() {
        // Provider/reasoning switches persist to config and apply on session open, so the palette
        // must let a power user set them from the command center before any session exists.
        let cmds = catalog();
        for c in cmds.iter().filter(|c| c.category == "model" || c.category == "reasoning") {
            assert!(!c.requires_session, "{} should not require a session", c.id);
        }
    }

    #[test]
    fn no_empty_category_headings() {
        // A category declared in CATEGORIES but with zero commands would render an empty palette
        // heading — validate() rejects this, so assert directly.
        let cmds = catalog();
        for cat in CATEGORIES {
            assert!(cmds.iter().any(|c| c.category == cat), "category {cat} has no commands");
        }
    }

    #[test]
    fn validate_catches_duplicate_id() {
        let mut cmds = catalog();
        cmds.push(cmds[0].clone());
        assert!(validate(&cmds).is_err(), "duplicate id should be rejected");
    }

    #[test]
    fn validate_catches_unknown_category() {
        let mut cmds = catalog();
        cmds[0].category = "nope";
        assert!(validate(&cmds).is_err(), "unknown category should be rejected");
    }

    #[test]
    fn title_capitalizes_slug() {
        assert_eq!(title("ollama"), "Ollama");
        assert_eq!(title("xhigh"), "Xhigh");
        assert_eq!(title("open-router"), "Open-Router");
    }

    #[test]
    fn catalog_keybindings_are_unique() {
        // Regression: no two registered commands may share the same accelerator. A collision
        // would silently shadow one of them in the palette, so this must fail loudly. `None` keys
        // are intentionally allowed to repeat — "no keybinding" is not a binding.
        let cmds = catalog();
        // validate() now enforces this, so the canonical contract catches it first.
        validate(&cmds).expect("catalog must be valid (incl. unique keybindings)");
        // Belt-and-suspenders: scan explicitly so a future refactor of validate() can't quietly
        // drop the assertion without this test going red.
        let keys: Vec<&str> = cmds.iter().filter_map(|c| c.key.as_deref()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate keybindings: {keys:?}");
    }

    #[test]
    fn validate_catches_duplicate_keybinding() {
        let mut cmds = catalog();
        // Pin two commands to the same non-`None` accelerator.
        let dup = "Ctrl+X".to_string();
        cmds[0].key = Some(dup.clone());
        cmds[1].key = Some(dup.clone());
        let err = validate(&cmds).expect_err("duplicate keybinding should be rejected");
        assert!(err.contains("duplicate keybinding"), "unexpected error: {err}");
    }

    #[test]
    fn validate_catches_empty_keybinding() {
        let mut cmds = catalog();
        cmds[0].key = Some(String::new());
        let err = validate(&cmds).expect_err("empty keybinding string should be rejected");
        assert!(err.contains("empty keybinding"), "unexpected error: {err}");
    }

    #[test]
    fn router_is_wired() {
        // The endpoint is stateless (Router<()>), so it composes without app state — assert the
        // builder returns a non-panicking router. This catches a route-registration regression.
        let _ = router();
    }
}