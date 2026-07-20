//! dotz agent profile summaries — port of profiles.ts `profileSummary` (for /api/profiles) +
//! the full operating doctrines (appendSystemPrompt) ported verbatim for the agent runtime's
//! system-prompt assembly (Phase 3).
use crate::types::{default_model, ModelRef};
use serde::Serialize;

// ---- operating doctrines (ported from profiles.ts with the local Ultra Code override) ----

pub const ULTRA_CODE_OVERRIDE_DOCTRINE: &str = r#"# dotz canonical harness: Ultra Code

For every non-trivial task, call the `skill` tool for `ultra-code` before planning, editing,
subagent dispersal, OpenSpec work, workflow execution, review, or verification. Treat Ultra Code as
the canonical harness and first instruction layer.

dotz-specific OpenSpec, subagent, workflow graph, sandbox, design, memory, and RSI behavior are host
adapters under Ultra Code. They extend the canonical harness; they do not compete with it.

Only skip loading `ultra-code` for genuinely trivial direct answers or one-line mechanical edits.
When in doubt, load Ultra Code first."#;

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

const DEBUG_DOMAIN: &str = "\n\n## Domain: systematic debugging — root-cause only\nFollow the 4-phase root-cause loop: UNDERSTAND (read the code, form a hypothesis) -> REPRODUCE (a failing test or exact repro steps) -> ISOLATE (bisect to the smallest input that triggers it) -> FIX (the actual cause, not the symptom). Never patch a symptom — a symptom patch is a second bug wearing a coat. Write the regression test FIRST, watch it fail, then make it pass with the fix. If you cannot write a failing test, you have not isolated the bug. Use `memory_search` for prior fixes to the same symbol; do not re-derive a solved problem.";

const REFACTOR_DOMAIN: &str = "\n\n## Domain: safe refactoring — behavior-preserving\nBehavior is frozen: the test suite MUST pass before and after every step. Smallest diff, one conceptual change per step, never mix a refactor with a feature or a fix. Commit between steps (`vcs_atomic_commit`) so a bad step is one revert away. Move in test-backed slices: if a slice has no test, write one before touching it. Use `read` + `grep` to confirm a symbol has no hidden callers before renaming. Never delete a test to make the build green — a deleted test is a regression you have already shipped.";

const DOCS_DOMAIN: &str = "\n\n## Domain: documentation — living and accurate\nMaintain living docs: use `living_docs_read` and `living_docs_suggest` to keep docs in sync with code, and `agents_md` to keep project `AGENTS.md` doctrine current. Every code snippet in a doc must compile or run — a snippet that does not is worse than no snippet (it teaches the wrong thing). No marketing fluff; audience-aware tone (operator vs contributor vs end user). Update the doc as part of the change that made it stale, not in a separate pass. Prefer deletion over rot: a deleted stale doc cannot mislead.";

/// The "New Model, New Project" domain doctrine (NEW-MODEL-NEW-PROJECT profile). Appended to
/// WORKFLOW_DOCTRINE: one-line idea in, a complete open-source repo shipped to GitHub out.
const NEW_MODEL_NEW_PROJECT_DOMAIN: &str = r#"

## Domain: New Model, New Project — one line in, a shipped GitHub repo out

You are the star of a build-in-one-session open-source series. ONE line in — a project idea — and you autonomously ship a genuinely-useful app to a NEW public GitHub repo. Own the full arc; ask only when truly blocked. Choose work a strong model shines at: whole-repo reasoning, long-horizon build/verify/repair.

HARD CONSTRAINTS: GitHub-only — runs from a fresh `git clone`. Allowed shapes: CLI, library, desktop (GitHub Releases), browser/VS Code extension, static SPA (GitHub Pages). NO server you host, NO managed DB. MIT licensed. Runtime model calls are bring-your-own-key — never commit a key.

REQUIRED CAPABILITY SPINE — you are the ORCHESTRATOR. Dispatch EACH phase below as its OWN named `subagent` so every capability is a distinct node in the live workflow graph the operator is watching; each subagent's tool calls stream onto its node as sub-nodes. Building a phase inline (no subagent) is a FAILED build even if the code works. Every phase is REQUIRED; a phase may be SKIPPED only when genuinely impossible, and only with an explicit logged line `skipped <phase>: <reason>` (e.g. browser-verify for a headless library). Give EVERY subagent for this episode `cwd: "<codename>"` (a relative cwd resolves against the session dir pantheon/projects, so this lands it inside pantheon/projects/<codename> — the EPISODE repo, not the hub).

0. RECALL & SCAFFOLD — `scout`: `memory_search` prior episodes/conventions. Pick an UNUSED mythological codename + kebab slug and the next episode N (highest Ep in C:/Users/Cayleb/Desktop/workspace/projects/pantheon/README.md + 1, or 1 if none). Read your executive model from `bash`: `cat ~/.dotz/config.json` -> use its `executiveModel` verbatim (that is YOU; DOTZ_SUBAGENT_MODEL is the workers'). Then SCAFFOLD via `bash` (creates + pushes the public repo, tags episode/N, updates the hub):
   powershell -ExecutionPolicy Bypass -File C:/Users/Cayleb/Desktop/workspace/projects/pantheon/scripts/new-episode.ps1 -Codename <codename> -Episode <N> -Model "<exec-model>" -Serve "Ollama Cloud" -Slug "<slug>" -Idea "<idea>" -DevPort 8090 -UpdateHub
1. DESIGN — `ui-ux-pro`: `design_use` to pick + load an Open Design system and honor its tokens. MUST produce an APP ICON (favicon.svg/.ico for web/SPA, or an app/desktop icon) committed to the repo, wired into the app, and embedded in the README — a first-class deliverable, not an afterthought.
2. SPEC — `spec-owner`: `openspec_propose` the change (proposal/design/tasks/specs/readiness) mapped to the build units, then `openspec_verify` before build.
3. SKILLS — `skill-agent-builder`: `create_skill`/`create_agent` ONLY if a real recurring capability gap exists; else log `skipped skills: no capability gap`.
4. BUILD — fan out one `worker` per independent unit (each crate/module, the frontend, the CI, the tests) in PARALLEL, each with `cwd: "<codename>"` and the file paths it owns. You conduct; you do NOT write large source files inline. Replace the generic CI with real build/test for the stack. Integrate and gate their results.
5. SANDBOX-VERIFY — `sandbox-runner`: `sandbox_run` (terminal) the project's REAL build/test command in the episode cwd. Treat failures as required work — hand back to a worker, re-run until green. This local green is the AUTHORITATIVE ship gate.
6. E2E & BUG-BOUNTY — `browser-operator`: `sandbox_run` (mode "web") to launch the built app in its own sandbox, capture its local url/port, then `browser_start` + `browser_act` to drive the REAL FRONTEND (navigate → click → scroll → type → screenshot) as a user — NOT the backend. Load `@e2e-test` and `@bug-bounty` via the `skill` tool, exercise real user flows, and hunt bugs with screenshot evidence. Bugs are REQUIRED work: hand each to a worker/build-fixer and re-verify before this phase is done. (Skip only for a headless CLI/library, logged.)
7. DOCS & BEAUTIFY — `docs-maintainer`: `living_docs_update` + `agents_md`, and BEAUTIFY the episode repo: README with a title, one-line description, CI + license badges, a screenshot, clone-and-run instructions, and the app icon embedded.
8. SHIP — `platform-operator`: `vcs_atomic_commit` (`rsi:` / `fix(scope):` prefixes), push to origin, then via `bash` set the GitHub metadata: `gh repo edit <owner>/<codename> --description "<one-liner>" --add-topic <slug> --add-topic <lang>`. VERIFY CI HONESTLY: after push, check `gh run list -R <owner>/<codename> --limit 1` up to 3 times (~10s apart). If a run appears, `gh run watch -R <owner>/<codename> --exit-status` and keep it green. If none appears, check `gh api repos/<owner>/<codename>/actions/permissions --jq .enabled`; if Actions is disabled, log EXACTLY `skipped CI: Actions disabled at account level` and continue on the authoritative local + E2E gate. NEVER invent "propagation delay", NEVER push empty commits to retrigger, NEVER claim CI is green when it is not. Report CI status honestly.
9. SELF-IMPROVE & SCORE — `self-improvement-reviewer`: `rsi_baseline`/`rsi_compare` the gate, then spawn a scoring `subagent` (cwd `<codename>`) with a FIXED judge model (NOT your own; the same judge every episode) to rate `difficulty` (1-5) and `quality` (0-100: correctness, completeness, cleanliness, docs) with a one-line note, then via `bash`:
   powershell -ExecutionPolicy Bypass -File C:/Users/Cayleb/Desktop/workspace/projects/pantheon/scripts/score-episode.ps1 -Codename <codename> -Difficulty <d> -Quality <q> -JudgeNote "<note>"

AUTH & TIMEOUTS: every subagent runs on the Ollama Cloud sub-model — `OLLAMA_API_KEY` must be set (an empty key 401s; the provider now fails fast with a missing-key error). A long build phase can exceed the 5-min subagent timeout — set `DOTZ_SUBAGENT_TIMEOUT_MS` higher (e.g. 900000) via `bash` before dispatching build workers.

`gh` and the PowerShell scaffolder run through `bash` (not wrapped tools). Never run destructive git/gh (force-push, repo delete) without the operator. When the ship checklist passes, report the repo URL and a one-paragraph recap."#;

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

/// True for one of the ten known profile ids.
pub fn is_valid(id: &str) -> bool {
    matches!(
        id,
        "workflow"
            | "solo"
            | "plan"
            | "frontend"
            | "backend"
            | "design"
            | "new-model-new-project"
            | "debug"
            | "refactor"
            | "docs"
    )
}

/// Resolve a profile by id (default = "workflow"), returning its runtime config.
///
/// # ponytail: a custom-profile loader that reads `~/.dotz/presets/<name>/manifest.json` and
/// surfaces installed profile presets here is the C8 follow-up. For now the profile list is
/// the hardcoded ten below; marketplace presets can only be prompts/agents/skills/design (which
/// already scan dirs) — a profile preset installs to disk but doesn't appear in this list until
/// the custom loader lands.
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
        "debug" => Profile {
            id: "debug",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        "refactor" => Profile {
            id: "refactor",
            thinking_level: "high",
            workflow: true,
            tools: None,
        },
        "docs" => Profile {
            id: "docs",
            thinking_level: "medium",
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
    let profile_doctrine = match id {
        "solo" => SOLO_DOCTRINE.to_string(),
        "plan" => PLAN_DOCTRINE.to_string(),
        "frontend" => format!("{WORKFLOW_DOCTRINE}{FRONTEND_DOMAIN}"),
        "backend" => format!("{WORKFLOW_DOCTRINE}{BACKEND_DOMAIN}"),
        "design" => format!(
            "{WORKFLOW_DOCTRINE}{}",
            DESIGN_DOMAIN.replace("{SYS}", design_systems_dir)
        ),
        "new-model-new-project" => format!("{WORKFLOW_DOCTRINE}{NEW_MODEL_NEW_PROJECT_DOMAIN}"),
        "debug" => format!("{WORKFLOW_DOCTRINE}{DEBUG_DOMAIN}"),
        "refactor" => format!("{WORKFLOW_DOCTRINE}{REFACTOR_DOMAIN}"),
        "docs" => format!("{WORKFLOW_DOCTRINE}{DOCS_DOMAIN}"),
        _ => WORKFLOW_DOCTRINE.to_string(),
    };
    format!("{ULTRA_CODE_OVERRIDE_DOCTRINE}\n\n{profile_doctrine}")
}

/// Max tool-rounds per turn before the loop pauses (the user resumes with a "continue"). The
/// autonomous "own the full arc" profiles must run start-to-finish without pausing, so they get a
/// high ceiling (still bounded, so a runaway can't loop forever); interactive profiles keep a low
/// cap for frequent check-ins.
pub fn max_rounds(id: &str) -> usize {
    match id {
        "new-model-new-project" => 400,
        "refactor" => 40,
        "debug" => 30,
        "workflow" => 60,
        "docs" => 20,
        _ => 12,
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

/// The 10 profiles, in order, matching PROFILES in profiles.ts. (default = "workflow")
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
        ProfileSummary {
            id: "debug",
            name: "DEBUG",
            tagline: "Systematic root-cause debugging · 4-phase",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "refactor",
            name: "REFACTOR",
            tagline: "Safe behavior-preserving refactoring",
            workflow: true,
            thinking_level: "high",
            model: m(),
        },
        ProfileSummary {
            id: "docs",
            name: "DOCS",
            tagline: "Living documentation authoring & maintenance",
            workflow: true,
            thinking_level: "medium",
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
            "debug",
            "refactor",
            "docs",
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

    #[test]
    fn every_profile_loads_ultra_code_as_the_canonical_harness() {
        for id in [
            "workflow",
            "solo",
            "plan",
            "frontend",
            "backend",
            "design",
            "new-model-new-project",
            "debug",
            "refactor",
            "docs",
        ] {
            let d = doctrine(id, "C:/design-systems");
            assert!(
                d.starts_with(ULTRA_CODE_OVERRIDE_DOCTRINE),
                "{id} should start with the Ultra Code override"
            );
            for needle in [
                "ultra-code",
                "canonical harness",
                "skill` tool",
                "OpenSpec",
                "subagent",
                "workflow",
            ] {
                assert!(
                    d.contains(needle),
                    "{id} doctrine must mention Ultra Code adapter rule: {needle}"
                );
            }
        }
    }

    /// The pantheon doctrine must build on WORKFLOW_DOCTRINE and mandate the required capability
    /// spine: the new design/sandbox tools, the app-icon + E2E requirements, and the honest CI gate.
    #[test]
    fn pantheon_doctrine_mandates_the_capability_spine() {
        let d = doctrine("new-model-new-project", "");
        assert!(
            d.starts_with(ULTRA_CODE_OVERRIDE_DOCTRINE),
            "spine starts with the canonical Ultra Code override"
        );
        assert!(
            d.contains(WORKFLOW_DOCTRINE),
            "spine still builds on workflow doctrine"
        );
        for needle in [
            "REQUIRED CAPABILITY SPINE",
            "design_use",
            "sandbox_run",
            "APP ICON",
            "E2E & BUG-BOUNTY",
            "gh repo edit",
            "skipped CI: Actions disabled at account level",
            "OLLAMA_API_KEY",
        ] {
            assert!(
                d.contains(needle),
                "pantheon doctrine must mention: {needle}"
            );
        }
    }

    /// The three Phase-1 quick-win profiles (debug / refactor / docs) must each resolve to a
    /// `Profile` via `get()`, appear in `summaries()`, and carry a doctrine that builds on
    /// WORKFLOW_DOCTRINE with their own domain suffix. Guards against a future edit that adds the
    /// id to `is_valid` but forgets the `get()` arm or the doctrine branch.
    #[test]
    fn all_new_profiles_resolve() {
        for id in ["debug", "refactor", "docs"] {
            let p = get(Some(id));
            assert_eq!(p.id, id, "get({id:?}) must return the {id} profile");
            assert!(is_valid(id), "{id} must be in the is_valid allow-list");
            assert!(
                p.workflow,
                "{id} is a workflow-domain profile (workflow=true)"
            );
            // Each new profile's doctrine starts with the Ultra Code override and contains both
            // the shared WORKFLOW_DOCTRINE and its own domain heading.
            let d = doctrine(id, "C:/design-systems");
            assert!(
                d.starts_with(ULTRA_CODE_OVERRIDE_DOCTRINE),
                "{id} doctrine must start with the Ultra Code override"
            );
            assert!(
                d.contains(WORKFLOW_DOCTRINE),
                "{id} doctrine must build on WORKFLOW_DOCTRINE"
            );
        }
        // Domain-specific needles: each doctrine must mention its own domain heading.
        assert!(
            doctrine("debug", "").contains("systematic debugging"),
            "debug doctrine must mention systematic debugging"
        );
        assert!(
            doctrine("refactor", "").contains("safe refactoring"),
            "refactor doctrine must mention safe refactoring"
        );
        assert!(
            doctrine("docs", "").contains("living and accurate"),
            "docs doctrine must mention living and accurate"
        );
        // The three new ids appear in summaries() (the /api/profiles surface).
        let ids: Vec<&str> = summaries().iter().map(|s| s.id).collect();
        for id in ["debug", "refactor", "docs"] {
            assert!(
                ids.contains(&id),
                "{id} must appear in summaries() (/api/profiles)"
            );
        }
        // max_rounds returns the spec'd ceilings (debug 30, refactor 40, docs 20).
        assert_eq!(max_rounds("debug"), 30);
        assert_eq!(max_rounds("refactor"), 40);
        assert_eq!(max_rounds("docs"), 20);
    }
}
