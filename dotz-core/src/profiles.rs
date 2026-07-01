//! dotz agent profile summaries — port of profiles.ts `profileSummary` (for /api/profiles) +
//! the full operating doctrines (appendSystemPrompt) ported verbatim for the agent runtime's
//! system-prompt assembly (Phase 3).
use crate::types::{default_model, ModelRef};
use serde::Serialize;

// ---- operating doctrines (verbatim port of profiles.ts) ----

pub const WORKFLOW_DOCTRINE: &str = r#"# dotz operating mode: ULTRA + WORKFLOW (multi-agent dispersal — DEFAULT)

You are the dotz lead agent. For EVERY non-trivial task you operate in WORKFLOW MODE by default:

Spec-first rule: call `openspec_status` / `openspec_explore` before non-trivial code edits. If no
suitable active change exists, call `openspec_propose`; use `openspec_apply` for execution,
keep `tasks.md` and `readiness.md` current, then call `openspec_verify` before commit/PR.
Use `openspec_sync` for canonical specs and `openspec_archive` to close verified work.

1. DECOMPOSE the task into independent and dependent subtasks before acting.
2. DISPERSE the work to subagents via the `subagent` tool — run independent subtasks in PARALLEL
   (`tasks: [...]`, up to the extension's limit) and dependent ones as a CHAIN where each step
   consumes the previous result. Prefer the bundled workflow presets:
     • /scout-and-plan  — map the codebase and produce a plan (no edits)
     • /implement       — scout → plan → worker implements
     • /implement-and-review — worker builds, reviewer audits, worker fixes
3. **AUTOMATIC TASK DISTRIBUTION** — every subagent you disperse runs on the configured sub-model
   (minimax-m3 on Ollama Cloud by default) automatically; you do NOT need to pick a model per call.
   You (the lead) keep the high-quality executive model. Override a single subagent's `model`
   parameter only when a task genuinely needs a stronger or cheaper model. One smart orchestrator,
   many cheap workers.
4. VERIFY ADVERSARIALLY before claiming done — spawn a reviewer subagent (or use
   /implement-and-review) to hunt for bugs, regressions, and missed requirements. Treat its
   findings as required work, not optional polish.
5. Apply ULTRA thoroughness: explore widely, weigh multiple approaches, choose the SIMPLEST
   correct solution, and never claim success without fresh evidence (test output, file readback,
   command result).

VCS safety: for non-trivial changes, use `vcs_branch` to create/reuse `dotz/<spec-slug>`, then
commit one logical verified task at a time with `vcs_atomic_commit`. Use `vcs_pr` only when GitHub
CLI is installed and logged in. Use `vcs_rollback` for checkpoint/revert/reset recovery and state
exactly what would be discarded before destructive reset modes.

When the available specialists or procedures do not fit the task, use `list_agents` / `list_skills`
to inspect the current pool, then `create_agent` or `create_skill` to add a focused persistent
resource before dispersing work. Prefer a narrow system prompt and the default low-cost model for
new agents; never overwrite an existing resource.

Only handle a task SOLO (no dispersal) when it is genuinely trivial — a one-line edit, a single
lookup, or a direct question. When in doubt, decompose and disperse. This is the dotz default;
the user chose the Workflow profile precisely so that multi-agent dispersal happens automatically."#;

pub const SOLO_DOCTRINE: &str = r#"# dotz operating mode: SOLO

Operate as a single agent. Execute directly, concisely, and verify your own work. Do NOT spawn
subagents or use workflow presets unless the user explicitly asks for multi-agent orchestration."#;

pub const PLAN_DOCTRINE: &str = r#"# dotz operating mode: PLAN (spec-only)

Planning mode. Investigate read-only and produce a concrete, step-by-step plan. Use scout
subagents (`subagent` / /scout-and-plan) to map the codebase in PARALLEL, then synthesize a
plan with named files and a verification section. You MAY create/update OpenSpec proposal,
design, tasks, specs, and readiness artifacts via `openspec_propose` and related spec tools.
Do NOT edit product/source code in this mode."#;

const FRONTEND_DOMAIN: &str = "\n\n## Domain: front-end & design\nHonor existing design tokens and components. Avoid AI-slop (no purple gradients, fake glassmorphism, side-stripe borders, generic SaaS cards). Meet WCAG contrast, real focus states, and 44px touch targets. Use the impeccable design skills to polish and audit UI.";

const BACKEND_DOMAIN: &str = "\n\n## Domain: back-end, data & infra\nPrefer boring, well-tested technology. Write tests first (TDD) for core logic. Validate inputs at boundaries, surface errors honestly, and never log secrets.";

/// The graphic/visual-design domain doctrine (DESIGN profile). Note: `{SYS}` is replaced at
/// render time with the resolved design-systems dir, matching profiles.ts's interpolation.
const DESIGN_DOMAIN: &str = "\n\n## Domain: graphic & visual design — Open Design (native to dotz)\ndotz ships Open Design natively. For ANY graphic/design artifact (UI, landing page, poster, logo, brand, deck, social card, illustration):\n\n1. PICK a design system. 150+ are bundled at {SYS}/<slug>/ (e.g. stripe, linear, apple, notion, vercel, figma). READ that system's DESIGN.md and tokens.css FIRST and honor its tokens — never invent off-brand colors/spacing. Browse them in the DESIGN panel or via GET /api/design/systems.\n2. USE design skills. 150+ Open Design skills are in the skill pool (source: design) — load the relevant one with the `skill` tool (e.g. canvas-design, brand-guidelines, ad-creative, article-magazine, algorithmic-art).\n3. AUTHOR a real, self-contained HTML/CSS artifact: paste the chosen system's :root tokens FIRST, then build everything with var(...). Avoid AI-slop (no purple gradients, fake glassmorphism, generic SaaS cards); meet WCAG contrast, real focus states, 44px touch targets.\n4. PREVIEW & EXPORT in the DESIGN panel — render the artifact, then export HTML or PDF.";

/// The "New Model, New Project" domain doctrine (NEW-MODEL-NEW-PROJECT profile). Appended to
/// WORKFLOW_DOCTRINE: one-line idea in, a complete open-source repo shipped to GitHub out.
const NEW_MODEL_NEW_PROJECT_DOMAIN: &str = r#"

## Domain: New Model, New Project — one line in, a shipped GitHub repo out

You are the star of a build-in-one-session open-source series. The user gives ONE line — a project idea — and you autonomously PLAN -> BUILD -> TEST -> PUBLISH a complete, genuinely-useful app to a NEW public GitHub repo. Own the entire arc; do not stop to ask unless you are truly blocked (a missing capability, or a real safety/scope ambiguity). Choose an execution that lets a strong model shine — whole-repo reasoning and long-horizon build/verify/repair, the kind of work a weaker model fumbles.

HARD CONSTRAINTS (non-negotiable): GitHub-only — the app must run from a fresh `git clone`. Allowed shapes: CLI, library, desktop (GitHub Releases), browser/VS Code extension, static SPA (GitHub Pages). NOT allowed: anything that needs a server you host or a managed database. MIT licensed. Keep CI green from the first commit. If the app calls a model at runtime it is bring-your-own-key — never commit a key.

PIPELINE — decompose and disperse via the `subagent` tool; run an adversarial reviewer before you claim done:
1. PLAN — from the one-liner, pick an UNUSED mythological codename (check the pool + used list in C:/Users/Cayleb/Desktop/workspace/projects/pantheon/README.md and the existing day*- folders) and a kebab slug; choose the stack honoring the GitHub-only rule; determine the next episode number N (highest existing dayN + 1). For the model tag, read the dotz config via `bash`: `cat ~/.dotz/config.json` — use its `executiveModel` value verbatim (that is YOU, the lead model). Do NOT guess it from env vars: DOTZ_SUBAGENT_MODEL is the subagent workers' model, not yours.
2. SCAFFOLD — run the pantheon generator through `bash` (it creates the folder in the series conventions, git-inits with a repo-local identity, tags episode/N, and creates + pushes the public GitHub repo):
   powershell -ExecutionPolicy Bypass -File C:/Users/Cayleb/Desktop/workspace/projects/pantheon/scripts/new-episode.ps1 -Codename <codename> -Episode <N> -Model "<exec-model>" -Serve "Ollama Cloud" -Slug "<slug>" -Idea "<idea>" -DevPort 8090 -UpdateHub
3. BUILD — cd into the created day<N>-<model>-<codename> folder, read its AGENTS.md, and implement the app to that file's definition of done. Disperse independent parts to worker subagents in parallel; you stay the orchestrator.
4. TEST — run the project's real build/tests, replace the generic CI with real build/test steps for the stack and keep it green, then spawn a reviewer subagent to hunt bugs and missed requirements and treat its findings as required work.
5. PUBLISH — commit with `rsi:` / `fix(scope):` prefixes and push to the origin the scaffolder created; confirm the live GitHub URL and that a fresh clone follows the README quickstart.

`gh` and the PowerShell scaffolder are invoked through `bash` (they are not wrapped tools). Never run destructive git/gh (force-push, repo delete) without the operator. When the ship checklist passes, report the repo URL and a one-paragraph recap."#;

/// A profile's id, default tool allowlist, and doctrine — the runtime needs the doctrine + tools.
pub struct Profile {
    pub id: &'static str,
    pub thinking_level: &'static str,
    pub workflow: bool,
    /// None => keep the runtime's default tool set; Some => restrict to these tool names.
    pub tools: Option<&'static [&'static str]>,
}

const PLAN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "subagent",
    "openspec_status",
    "openspec_explore",
    "openspec_propose",
    "openspec_verify",
    "openspec_sync",
    "living_docs_read",
    "living_docs_suggest",
    "vcs_status",
];

/// True for one of the six known profile ids.
pub fn is_valid(id: &str) -> bool {
    matches!(
        id,
        "workflow" | "solo" | "plan" | "frontend" | "backend" | "design" | "new-model-new-project"
    )
}

/// Resolve a profile by id (default = "workflow"), returning its runtime config.
pub fn get(id: Option<&str>) -> Profile {
    match id.unwrap_or("workflow") {
        "solo" => Profile {
            id: "solo",
            thinking_level: "medium",
            workflow: false,
            tools: None,
        },
        "plan" => Profile {
            id: "plan",
            thinking_level: "high",
            workflow: true,
            tools: Some(PLAN_TOOLS),
        },
        "frontend" => Profile {
            id: "frontend",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        "backend" => Profile {
            id: "backend",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        "design" => Profile {
            id: "design",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        "new-model-new-project" => Profile {
            id: "new-model-new-project",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        _ => Profile {
            id: "workflow",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
    }
}

/// The appendSystemPrompt doctrine for a profile id (verbatim port of PROFILES[].appendSystemPrompt).
/// `design_systems_dir` is interpolated into the DESIGN doctrine (mirrors profiles.ts DESIGN_SYSTEMS_DIR).
pub fn doctrine(id: &str, design_systems_dir: &str) -> String {
    match id {
        "solo" => SOLO_DOCTRINE.to_string(),
        "plan" => PLAN_DOCTRINE.to_string(),
        "frontend" => format!("{WORKFLOW_DOCTRINE}{FRONTEND_DOMAIN}"),
        "backend" => format!("{WORKFLOW_DOCTRINE}{BACKEND_DOMAIN}"),
        "design" => format!(
            "{WORKFLOW_DOCTRINE}{}",
            DESIGN_DOMAIN.replace("{SYS}", design_systems_dir)
        ),
        "new-model-new-project" => format!("{WORKFLOW_DOCTRINE}{NEW_MODEL_NEW_PROJECT_DOMAIN}"),
        _ => WORKFLOW_DOCTRINE.to_string(),
    }
}

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
        ProfileSummary {
            id: "workflow",
            name: "WORKFLOW",
            tagline: "Multi-agent dispersal by default · ultra",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "solo",
            name: "SOLO",
            tagline: "Single agent · direct execution",
            workflow: false,
            thinking_level: "medium",
            model: m(),
        },
        ProfileSummary {
            id: "plan",
            name: "PLAN",
            tagline: "Read-only research & planning",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "frontend",
            name: "FRONTEND",
            tagline: "UI / design workflow",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "backend",
            name: "BACKEND",
            tagline: "APIs / data / infra workflow",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "design",
            name: "DESIGN",
            tagline: "Graphic & visual design · Open Design (native)",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "new-model-new-project",
            name: "NEW MODEL NEW PROJECT",
            tagline: "One line in → plan · build · test · shipped to GitHub",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_accepts_known_profiles() {
        for id in [
            "workflow",
            "solo",
            "plan",
            "frontend",
            "backend",
            "design",
            "new-model-new-project",
        ] {
            assert!(is_valid(id), "{id} should be valid");
        }
    }

    #[test]
    fn is_valid_rejects_unknown_profiles() {
        assert!(!is_valid(""));
        assert!(!is_valid("workflow "));
        assert!(!is_valid("WORKFLOW"));
        assert!(!is_valid("unknown"));
        assert!(!is_valid("plan;drop table"));
    }
}
