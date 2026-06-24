/**
 * dotz agent profiles. The DEFAULT profile ("workflow") puts the agent in Claude-style
 * "ultra + workflow" mode: every non-trivial task is decomposed and dispersed to subagents,
 * then adversarially verified. A profile is applied by injecting its doctrine as an
 * appendSystemPrompt via a DefaultResourceLoader (which still discovers bundled .pi skills,
 * subagent extension, and workflow presets).
 */
import path from "node:path";
import { fileURLToPath } from "node:url";
import { getAgentDir, SettingsManager, DefaultResourceLoader, type ResourceLoader } from "@earendil-works/pi-coding-agent";
import { DEFAULT_MODEL, renderLowCostModels, type ModelRef, type ThinkingLevel } from "./types";
import { memoryStore } from "./memory";
import { skillLoader } from "./skills";
import { userTemplatesDir } from "./templates";
import fs from "node:fs/promises";

/** Bundled .pi (skills / extensions / prompts) — resolved relative to this module so it works
 *  both in dev (src/) and in the packaged app (dist/, with .pi shipped alongside). */
const DOTZ_PI = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", ".pi");
/** Vendored Open Design systems (DESIGN.md + tokens.css per slug) — referenced by the DESIGN doctrine. */
const DESIGN_SYSTEMS_DIR = path.join(DOTZ_PI, "design-systems");

export interface Profile {
  id: string;
  name: string;
  tagline: string;
  model: ModelRef;
  thinkingLevel: ThinkingLevel;
  /** Tool allowlist; undefined keeps pi defaults (read,bash,edit,write) + extension tools (subagent). */
  tools?: string[];
  /** Whether this profile defaults to multi-agent workflow dispersal. */
  workflow: boolean;
  appendSystemPrompt: string;
}

const WORKFLOW_DOCTRINE = `# dotz operating mode: ULTRA + WORKFLOW (multi-agent dispersal — DEFAULT)

You are the dotz lead agent. For EVERY non-trivial task you operate in WORKFLOW MODE by default:

1. DECOMPOSE the task into independent and dependent subtasks before acting.
2. DISPERSE the work to subagents via the \`subagent\` tool — run independent subtasks in PARALLEL
   (\`tasks: [...]\`, up to the extension's limit) and dependent ones as a CHAIN where each step
   consumes the previous result. Prefer the bundled workflow presets:
     • /scout-and-plan  — map the codebase and produce a plan (no edits)
     • /implement       — scout → plan → worker implements
     • /implement-and-review — worker builds, reviewer audits, worker fixes
3. **AUTOMATIC TASK DISTRIBUTION** — every subagent you disperse runs on the configured sub-model
   (minimax-m3 on Ollama Cloud by default) automatically; you do NOT need to pick a model per call.
   You (the lead) keep the high-quality executive model. Override a single subagent's \`model\`
   parameter only when a task genuinely needs a stronger or cheaper model. One smart orchestrator,
   many cheap workers.
4. VERIFY ADVERSARIALLY before claiming done — spawn a reviewer subagent (or use
   /implement-and-review) to hunt for bugs, regressions, and missed requirements. Treat its
   findings as required work, not optional polish.
5. Apply ULTRA thoroughness: explore widely, weigh multiple approaches, choose the SIMPLEST
   correct solution, and never claim success without fresh evidence (test output, file readback,
   command result).

When the available specialists or procedures do not fit the task, use \`list_agents\` / \`list_skills\`
to inspect the current pool, then \`create_agent\` or \`create_skill\` to add a focused persistent
resource before dispersing work. Prefer a narrow system prompt and the default low-cost model for
new agents; never overwrite an existing resource.

Only handle a task SOLO (no dispersal) when it is genuinely trivial — a one-line edit, a single
lookup, or a direct question. When in doubt, decompose and disperse. This is the dotz default;
the user chose the Workflow profile precisely so that multi-agent dispersal happens automatically.`;

const SOLO_DOCTRINE = `# dotz operating mode: SOLO

Operate as a single agent. Execute directly, concisely, and verify your own work. Do NOT spawn
subagents or use workflow presets unless the user explicitly asks for multi-agent orchestration.`;

const PLAN_DOCTRINE = `# dotz operating mode: PLAN (read-only)

Planning mode. Investigate read-only and produce a concrete, step-by-step plan. Use scout
subagents (\`subagent\` / /scout-and-plan) to map the codebase in PARALLEL, then synthesize a
plan with named files and a verification section. Do NOT edit files in this mode.`;

/** The graphic/visual-design domain doctrine — single source of truth, shared by the DESIGN profile
 *  (below) and the dotz-tools design auto-route, so the path / skill names / steps never drift.
 *  DESIGN_DOMAIN_MARKER is a stable substring the auto-route uses to detect when this doctrine is
 *  already present (e.g. under the DESIGN profile) and skip a duplicate injection. */
export const DESIGN_DOMAIN_MARKER = "## Domain: graphic & visual design";
export const DESIGN_DOMAIN_DOCTRINE =
  `\n\n${DESIGN_DOMAIN_MARKER} — Open Design (native to dotz)
dotz ships Open Design natively. For ANY graphic/design artifact (UI, landing page, poster, logo, brand, deck, social card, illustration):

1. PICK a design system. 150+ are bundled at ${DESIGN_SYSTEMS_DIR}/<slug>/ (e.g. stripe, linear, apple, notion, vercel, figma). READ that system's DESIGN.md and tokens.css FIRST and honor its tokens — never invent off-brand colors/spacing. Browse them in the DESIGN panel or via GET /api/design/systems.
2. USE design skills. 150+ Open Design skills are in the skill pool (source: design) — load the relevant one with the \`skill\` tool (e.g. canvas-design, brand-guidelines, ad-creative, article-magazine, algorithmic-art).
3. AUTHOR a real, self-contained HTML/CSS artifact: paste the chosen system's :root tokens FIRST, then build everything with var(...). Avoid AI-slop (no purple gradients, fake glassmorphism, generic SaaS cards); meet WCAG contrast, real focus states, 44px touch targets.
4. PREVIEW & EXPORT in the DESIGN panel — render the artifact, then export HTML or PDF.`;

const DESIGN_DOCTRINE = WORKFLOW_DOCTRINE + DESIGN_DOMAIN_DOCTRINE;

export const PROFILES: Profile[] = [
  {
    id: "workflow",
    name: "WORKFLOW",
    tagline: "Multi-agent dispersal by default · ultra",
    model: DEFAULT_MODEL,
    thinkingLevel: "high",
    tools: undefined,
    workflow: true,
    appendSystemPrompt: WORKFLOW_DOCTRINE,
  },
  {
    id: "solo",
    name: "SOLO",
    tagline: "Single agent · direct execution",
    model: DEFAULT_MODEL,
    thinkingLevel: "medium",
    tools: undefined,
    workflow: false,
    appendSystemPrompt: SOLO_DOCTRINE,
  },
  {
    id: "plan",
    name: "PLAN",
    tagline: "Read-only research & planning",
    model: DEFAULT_MODEL,
    thinkingLevel: "high",
    tools: ["read", "grep", "find", "ls", "subagent"],
    workflow: true,
    appendSystemPrompt: PLAN_DOCTRINE,
  },
  {
    id: "frontend",
    name: "FRONTEND",
    tagline: "UI / design workflow",
    model: DEFAULT_MODEL,
    thinkingLevel: "high",
    tools: undefined,
    workflow: true,
    appendSystemPrompt:
      WORKFLOW_DOCTRINE +
      `\n\n## Domain: front-end & design\nHonor existing design tokens and components. Avoid AI-slop (no purple gradients, fake glassmorphism, side-stripe borders, generic SaaS cards). Meet WCAG contrast, real focus states, and 44px touch targets. Use the impeccable design skills to polish and audit UI.`,
  },
  {
    id: "backend",
    name: "BACKEND",
    tagline: "APIs / data / infra workflow",
    model: DEFAULT_MODEL,
    thinkingLevel: "high",
    tools: undefined,
    workflow: true,
    appendSystemPrompt:
      WORKFLOW_DOCTRINE +
      `\n\n## Domain: back-end, data & infra\nPrefer boring, well-tested technology. Write tests first (TDD) for core logic. Validate inputs at boundaries, surface errors honestly, and never log secrets.`,
  },
  {
    id: "design",
    name: "DESIGN",
    tagline: "Graphic & visual design · Open Design (native)",
    model: DEFAULT_MODEL,
    thinkingLevel: "high",
    tools: undefined,
    workflow: true,
    appendSystemPrompt: DESIGN_DOCTRINE,
  },
];

export function getProfile(id?: string): Profile {
  return PROFILES.find((p) => p.id === id) || PROFILES[0];
}

/** Public profile summary for the UI. */
export function profileSummary(p: Profile) {
  return { id: p.id, name: p.name, tagline: p.tagline, workflow: p.workflow, thinkingLevel: p.thinkingLevel, model: p.model };
}

/** Build a resource loader that injects the profile's doctrine and loads the bundled .pi
 *  skills/extensions/prompts regardless of the session cwd (so it works in the packaged exe).
 *  If a projectId is supplied, the project's persistent memory entries are appended to the
 *  system prompt so the agent carries durable, user-curated context across sessions. */
export async function buildResourceLoader(
  cwd: string,
  profile: Profile,
  opts: { projectId?: string | null; appUrl?: string | null } = {}
): Promise<ResourceLoader> {
  const agentDir = getAgentDir();
  const settingsManager = SettingsManager.create(cwd, agentDir);
  const prompts = [profile.appendSystemPrompt];
  // Seed the system prompt with a recent slice of durable memory (global always; project when a
  // project is bound). Live, query-relevant recall happens per-turn via the dotz-tools
  // before_agent_start hook — this is just the always-on baseline.
  const seed = await memoryStore.forProject(opts.projectId ? cwd : null);
  const memBlock = memoryStore.renderForPrompt(seed);
  if (memBlock) prompts.push(memBlock);
  // Project context that dotz supplies AUTOMATICALLY so the agent never has to be hand-told it:
  //  - the working directory (the selected project root) — so the agent doesn't guess a path and
  //    `cd` somewhere nonexistent (e.g. /home/user/project); shell/git tools already run HERE.
  //  - the app URL (when configured) — so VISUAL / E2E / BUG-BOUNTY work drives THE RIGHT app with
  //    the in-app browser instead of grabbing whatever dev server is up (e.g. dotz's own UI).
  prompts.push(
    `# Project (dotz)\n` +
    `Your working directory (the selected project's root) is: ${cwd}\n` +
    `The shell, git, agents_md, and gate tools already operate HERE — do NOT \`cd\` to a guessed ` +
    `path; run commands relative to this root.` +
    (opts.appUrl
      ? `\nThis project's running app is served at: ${opts.appUrl} — for any VISUAL, E2E, or ` +
        `BUG-BOUNTY task, drive THAT url with the in-app browser (\`browser_start\` / \`browser_act\`); ` +
        `do NOT target any other dev server (not dotz's own UI, not an unrelated localhost port).`
      : ``)
  );
  // Inject the unified skill index (names + one-line descriptions) so the agent knows what
  // skills are available without loading every full body. The `skill` tool loads bodies on demand.
  await skillLoader.load();
  // Ensure the user template directory exists before handing it to the resource loader so the SDK
  // never trips over a missing path on first run.
  await fs.mkdir(userTemplatesDir(), { recursive: true });
  const skillIndex = skillLoader.renderIndex();
  if (skillIndex) prompts.push(skillIndex);
  // Inject the low-cost sub-model list so the main model can select sub-models for task distribution.
  prompts.push(renderLowCostModels());
  const resLoader = new DefaultResourceLoader({
    cwd,
    agentDir,
    settingsManager,
    appendSystemPrompt: prompts,
    additionalExtensionPaths: [
      path.join(DOTZ_PI, "extensions", "ollama-cloud"),
      path.join(DOTZ_PI, "extensions", "local"),
      path.join(DOTZ_PI, "extensions", "subagent"),
      path.join(DOTZ_PI, "extensions", "dotz-tools"),
    ],
    additionalSkillPaths: [path.join(DOTZ_PI, "skills")],
    additionalPromptTemplatePaths: [path.join(DOTZ_PI, "prompts"), userTemplatesDir()],
  });
  await resLoader.reload();
  return resLoader;
}
