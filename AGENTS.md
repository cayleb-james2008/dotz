# dotz — project agent guide

dotz is a **pi.dev-based, Claude-Code-style ultra-code coding-agent dashboard** — a bento-style
multi-agent coding interface with **on-the-fly workflow graphs, unified skills across the
opencode/claude/codex/ecc/superpowers pools, `.ai-agents` global memory, and recursive
self-improvement wiring**, packaged as a single Electron `.exe`.

## Architecture in one paragraph

dotz embeds pi's SDK **directly** (`@earendil-works/pi-coding-agent`), so the dashboard server *is*
the agent — no subprocess, nothing to version separately. `src/profiles.ts` loads the bundled
`.pi/` resources (subagent extension + dotz-tools extension + 4 agents + 3 workflow prompts) and
injects the active profile's doctrine + the unified skill index + project memory as
`appendSystemPrompt`. `src/skills.ts` discovers 300+ `SKILL.md` files across the opencode, claude,
codex/ecc, superpowers, hermes, and bundled `.pi/skills` pools, dedupes by name, and exposes a
`skill(name)` pi tool for on-demand full-body loading. `src/workflows.ts` is the first-class
`WorkflowRun` domain — DAGs of steps (agent + task + parents + status) that make the implicit
subagent orchestration observable by the UI's interactive SVG graph. `src/projects.ts` persists
named workspaces (cwd + profile + model + thinking defaults); `src/memory.ts` stores per-project
and global memory under the `.ai-agents` namespace (`~/.dotz/ai-agents/memory.json` global +
`<cwd>/.ai-agents/memory.json` project) and exposes `agents_md` read/write tools. `src/sandbox.ts`
runs code in `terminal` + `web` modes with an agent cursor overlay. `src/pi.ts` owns
`AgentSession` lifecycle and fans events to WebSocket subscribers. `src/server.ts` is a lean
Fastify surface (REST controls + WS event stream + static UI). `src/main.ts` boots the server in
the Electron main process. The UI in `web/` is a vanilla HTML/CSS/JS **bento dashboard** with a
project launcher, progressive panel disclosure, drag-and-drop repositioning, and an on-the-fly SVG
workflow graph — runs identically in a browser and inside the packaged app.

## Multi-provider models

The model surface is **multi-provider**, not just OpenRouter. `GET /api/providers` and the
`providerMeta` field on `GET /api/sessions/:id/models` describe each provider's UI mode
(`freeForm:true` → free-form model-id input, e.g. OpenRouter; otherwise a fixed list). `src/pi.ts`
resolves the `{provider, modelId}` pair per provider; `src/types.ts` carries the shared `ModelRef`.

## Projects + Memory

`src/projects.ts` is the **persistent projects layer**: each `Project` binds a name, a `cwd`, and
default `profileId` / `model` / `thinkingLevel`. Sessions created with a `projectId` inherit those
defaults. `src/memory.ts` is the **memory store**: `MemoryEntry`s are scoped `project` or `global`
and are injected into the agent's system prompt at session-build time via the project's
`buildResourceLoader`, so the agent sees durable notes without re-prompting. Storage lives under the
`.ai-agents` namespace: `~/.dotz/ai-agents/memory.json` (global) + `<cwd>/.ai-agents/memory.json`
(project). AGENTS.md files (project root + global `~/.config/opencode/AGENTS.md`) are the **doctrine**
layer — read by `buildResourceLoader`, written by the agent via the `agents_md` tool. Memory entries
are the **knowledge** layer — structured, agent-curated. Don't blur them. Both persist across
server restarts.

## Unified skills pool

`src/skills.ts` (`SkillLoader`) is the **single skill-discovery path**. It walks five scan roots
(`~/.config/opencode/skills`, `~/.claude/skills`, `~/.codex/skills`,
`~/.codex/marketplaces/ecc-local/plugins/ecc/skills`, `~/.codex/plugins/cache/openai-curated/superpowers`,
plus bundled `.pi/skills`), parses YAML frontmatter once (handling Claude/OpenCode/Hermes/ECC
dialects), dedupes by name with a priority order (dotz > opencode > claude > codex > ecc >
superpowers > hermes), filters by platform, and injects a compact name+description index into the
system prompt. The `skill(name)` pi tool (registered by the dotz-tools extension) loads the full
body on demand — same pattern as Claude Code's `Skill` tool and OpenCode's `skill` tool. This gives
the agent access to 300+ skills across all four ecosystems without bloating the system prompt.

## Workflow domain

`src/workflows.ts` (`WorkflowStore`) is the **observability + control layer** for multi-agent
orchestration. The execution backend stays the existing `subagent` extension (it spawns isolated
pi processes); this module records step DAGs (with parents/children, status propagation, readiness),
their statuses, outputs, and usage, and emits `workflow_*` events the UI renders as an interactive
SVG node/edge graph. Storage: JSON at `~/.dotz/ai-agents/workflows.json`. `WorkflowRun` = DAG of
`WorkflowStep`s; steps transition `pending → ready → running → done|error|skipped`; children auto-
promote to `ready` when all parents are `done`. REST: `/api/workflows` (CRUD + step + abort). WS:
`{kind:"workflow", runId, event}` events (`workflow_start`, `workflow_end`, `step_state`,
`step_added`).

## Bento UI

`web/` is a **vanilla JS bento dashboard** (no framework, no build step). Initial state: a project
launcher (pick or create a project → opens agent directly). Once a session is active, a CSS-grid
bento layout with progressive panel disclosure: only `chat` is visible initially; a `+ PANELS`
button (or `Ctrl+P`) opens a palette to add panels (`graph`, `brain`, `browser`, `memory`, `files`,
`sandbox`, `skills`). Panels auto-open on relevant events (workflow start → `graph`; sandbox start →
`sandbox`; memory tool call → `memory`). Drag-and-drop repositioning via HTML5 DnD on panel headers
(swaps grid spans). Layout persists to `localStorage.dotz.layout.v1`. The **workflow graph panel**
renders live SVG DAGs with layered topological layout, node state colors (pending/ready/running/
done/error/skipped), animated transitions, click-to-inspect step detail, and pan/zoom. The four
control knobs (model, provider, reasoning, workflows) live in the top bar.

## Sandbox

`src/sandbox.ts` is the **sandbox**: it runs code in `terminal` mode (stream stdout/stderr back) or
`web` mode (long-lived process bound to a local HTTP port). In `web` mode the UI renders an inline
preview iframe at the published port, and the agent drives an **agent cursor** over it via
`sandbox.cursor` WS messages (`move` / `click` / `type` at `(x, y)`). The UI renders the cursor as an
overlay; the server emits `sandbox_cursor` events back so both the agent and the user see the same
pointer state. Run lifecycle: `pending → running → done|error|killed`.

## Multi-provider + Ollama cloud + task distribution

The model surface is **multi-provider**: OpenRouter, **Ollama Cloud** (minimax-m3), Anthropic, OpenAI,
Google, Groq, Mistral, xAI, DeepSeek, Cohere, and Local. OpenRouter, Ollama, and Local are
**free-form model-id inputs** (not dropdowns). `resolveModel` clones any same-provider template for
unknown ids; Ollama falls back to cloning an OpenRouter template if no Ollama model is registered.

**Automatic task distribution**: `LOW_COST_MODELS` in `types.ts` lists the low-cost sub-models per
provider (Ollama: `minimax-m3`; OpenRouter: `nvidia/nemotron-3-ultra-550b-a55b:free`,
`nex-agi/nex-n2-pro:free`). This list is injected into the system prompt via `renderLowCostModels()`,
and the `subagent` tool accepts a `model` override parameter (format: `provider/model-id`). The main
agent (the high-quality orchestrator) selects a sub-model per task from this list, keeping cost down
while maximizing throughput. The WORKFLOW_DOCTRINE instructs the agent to match model capability to
task complexity.

## Subagent → workflow bridge

`src/workflow-bridge.ts` (`WorkflowBridge`) synthesizes `WorkflowRun` objects from the subagent
extension's `tool_execution_*` events so the UI graph auto-populates when the agent runs `/implement`,
`/implement-and-review`, or any subagent dispersal. It detects `toolName === "subagent"` in the event
stream, creates a run with one step per subagent result, and updates step states as the tool
progresses. This is the glue between the subagent extension (execution backend) and `WorkflowStore`
(observability layer). It is best-effort and never blocks the agent loop.

## RSI agent brain (recursive self-improvement)

`src/metrics.ts` is the **measurement layer** for the RSI loop: `captureBaseline(cwd)` runs typecheck
+ build + tests and stores the result; `compare(baseline)` re-measures and reports whether metrics
improved/regressed. Anti-gaming checks: tests must not be deleted, must still pass.

The RSI loop is driven by three pi tools registered by the dotz-tools extension:
- `rsi_baseline` — capture a verification baseline
- `rsi_compare` — re-measure and compare against a baseline
- `human_gate` — pause for user approval (RSI Phase 2 gate); the UI renders an approval card via a
  `{kind:"gate", gateId, plan}` WS event and replies with `{kind:"gate.approve"|"gate.reject"}`

The `/self-improve` workflow preset (`/self-improve`) runs the full 7-phase loop:
measure → research → pick → plan (HUMAN GATE) → implement (TDD) → review (5-reviewer fan-out) →
simplify → verify. The brain panel's SELF-IMPROVE button sends this prompt.

## Open-Design (baked-in frontend tooling)

`src/design.ts` is the **baked-in design system**: bundled palettes (Catppuccin Mocha, fintech SaaS,
landing modern), typography pairings with Google Fonts imports, layout patterns, platform guidelines,
Lucide icon guidance, chart recommendations, and a UX/accessibility audit rubric (WCAG 2.2 AA, touch
targets, focus states, no AI-slop). Three pi tools expose it:
- `design_system(query)` — returns CSS tokens + palette + typography + layout
- `design_components(query)` — returns Lucide icons + chart types + framework rules
- `design_audit(target)` — returns UX/accessibility findings with specific fixes

The FRONTEND profile doctrine instructs the agent to use these tools for any frontend task.

## In-app browser

`src/browser.ts` embeds a Chromium instance inside the Electron window via `WebContentsView` (the
modern replacement for `BrowserView`). It uses a dedicated `--user-data-dir`
(`~/.dotz/ai-agents/browser-profile`) so it never conflicts with the user's personal Chrome profile.
In browser-dev mode (`npm run dev:server`, no Electron), it's a no-op stub — the UI shows the
placeholder. REST endpoints: `/api/browser/state|navigate|back|forward|reload|screenshot|eval`.

## Workflow presets

`.pi/prompts/` contains 6 slash-command presets:
- `/scout-and-plan` — scout → planner (no edits)
- `/implement` — scout → planner → worker
- `/implement-and-review` — worker → reviewer → worker (applies feedback)
- `/ultra-code-review` — 5-reviewer fan-out → isolated scorers → confidence ≥80 filter → report
- `/e2e-test` — discover surface → baseline → write e2e tests (parallel) → run → compare
- `/self-improve` — full RSI 7-phase loop with human gate

## Key files

- `src/pi.ts` — `PiSessions`: create/get/list/subscribe/dispose, model resolution (OpenRouter +
  Ollama + local free-form), command aggregation. **Import only from the top-level
  `@earendil-works/pi-coding-agent`**.
- `src/profiles.ts` — 5 profiles + `buildResourceLoader` (loads bundled `.pi` + injects doctrine +
  skill index + low-cost model list + project memory).
- `src/skills.ts` — unified skills loader (320 skills across opencode/claude/codex/ecc/superpowers/hermes/.pi pools).
- `src/workflows.ts` — workflow store (first-class `WorkflowRun` DAG, status propagation, event emitter, JSON-persisted).
- `src/workflow-bridge.ts` — synthesizes WorkflowRuns from subagent tool_execution events (auto-populates the graph).
- `src/metrics.ts` — RSI measurement layer (baseline + compare + anti-gaming).
- `src/design.ts` — Open-Design baked-in frontend tooling (palettes, typography, UX audit).
- `src/browser.ts` — in-app Electron browser (WebContentsView + dedicated profile).
- `src/projects.ts` — persistent projects layer (name + cwd + profile/model/thinking defaults).
- `src/memory.ts` — memory store (`project` / `global` scope) under `.ai-agents` namespace + AGENTS.md read/write helpers. Injected into the system prompt via `buildResourceLoader`.
- `src/sandbox.ts` — sandbox runner (`terminal` + `web` modes, agent cursor, lifecycle events).
- `src/types.ts` — shared types (`ModelRef`, `ProviderMeta`, `Project`, `MemoryEntry`, `SandboxRun`,
  `Skill`, `WorkflowRun`, `WorkflowStep`, `ThinkingLevel`, `DEFAULT_MODEL`, `LOW_COST_MODELS`,
  `renderLowCostModels`). Kept separate from `pi.ts` to avoid a circular import — do not merge.
- `src/server.ts` — Fastify REST + WS + static. Routes: sessions, projects, memory, skills, workflows,
  sandbox, browser, human-gate.
- `src/main.ts` / `src/preload.ts` — Electron main + contextIsolation preload.
- `web/` — vanilla JS bento dashboard (project launcher, progressive panel disclosure, drag-and-drop,
  on-the-fly SVG workflow graph, skills/memory/sandbox/brain panels).
- `.pi/` — bundled agent resources (subagent extension, dotz-tools extension, agents, 6 workflow prompts).
  Shipped in the exe via `electron-builder.yml` `files:`.
- `.pi/extensions/dotz-tools/index.ts` — registers 13 pi tools: `skill`, `memory_list`, `memory_add`,
  `memory_delete`, `agents_md`, `rsi_baseline`, `rsi_compare`, `human_gate`, `design_system`,
  `design_components`, `design_audit` + the `resolveHumanGate`/`onGateRequest` server hooks.
- `docs/api-contract.md` — the authoritative UI↔backend contract.

## Verification commands

```bash
npm run typecheck                      # tsc --noEmit — must be clean
npx tsx scripts/verify-profiles.mjs   # e2e: profiles + .pi bundle + subagent + 13 dotz-tools tools
npx tsx scripts/verify-ui.mjs          # e2e: UI markup + profiles API
npx tsx scripts/verify-features.mjs    # e2e: projects + memory + sandbox + multi-provider (11 providers)
npx tsx scripts/verify-ultra.mjs       # e2e: 320 skills + workflows + .ai-agents memory + RSI/design/browser tools + 6 presets + Ollama
npm run dev:server                      # http://127.0.0.1:4317 (browser dev loop)
npm run build                           # esbuild → dist/main.js + dist/preload.cjs
npm run dist                            # → release/dotz <version>.exe (Windows portable)
```

## Provider config

dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env vars). Provide a working
provider key, e.g. `OPENROUTER_API_KEY` or `OLLAMA_API_KEY`. OpenRouter + Ollama + Local are
**free-form model-id inputs** (not dropdowns), defaulting to `nex-agi/nex-n2-pro:free`. Use `:free`
models when the balance is low (provider errors surface in-chat as `stopReason:"error"` +
`errorMessage`). The low-cost sub-model list (Ollama `minimax-m3`, OpenRouter
`nvidia/nemotron-3-ultra-550b-a55b:free`, `nex-agi/nex-n2-pro:free`) is injected into the system
prompt for automatic task distribution.

## Known caveats

- The libuv `Assertion failed ... async.c:76` on process exit is a known clean-shutdown quirk of
  the pi SDK under Node on Windows; it is cosmetic and does not affect results. (The dev server
  uses `dispose → SIGINT/SIGTERM → exit 0`; in-process test scripts may print it.)
- Subagents spawn separate `pi` processes; the bundled agents default to the free model so
  `/implement` is runnable out of the box.
- Fonts load from Google Fonts (online). Bundle locally for fully-offline use.

## Conventions

- The four control knobs are the product surface; don't add new ones without intent.
- Keep the backend lean (Fastify + pi SDK only). The UI is vanilla JS — no build step, no framework.
- `profiles.ts` and `types.ts` keep `DEFAULT_MODEL`/`ModelRef`/`ThinkingLevel` separate from `pi.ts`
  to avoid a circular import. Don't merge them.
- `projects.ts` and `memory.ts` are pure persistence layers (no agent runtime). Memory is injected
  into the system prompt via `buildResourceLoader` in `profiles.ts` — keep that the single injection
  point; don't add a second path.
- `skills.ts` is the single skill-discovery path. Don't add a second skill loader. The skill index
  is injected via `buildResourceLoader` (same as memory); the `skill(name)` tool loads bodies on demand.
- `workflows.ts` is the observability + control layer for multi-agent orchestration. It does NOT
  spawn agents — the `subagent` extension is still the execution backend. `WorkflowStore` only
  records and surfaces step DAGs. Keep it a pure persistence + event layer.
- `sandbox.ts` owns process lifecycle for both `terminal` and `web` runs; never spawn sandbox
  processes directly from `server.ts`. The agent cursor is a UI overlay driven by `sandbox_cursor`
  WS events — both sides (agent send + UI render) must consume the same event shape.
- `browser.ts` owns the in-app Electron browser (`WebContentsView`). It must never reuse the user's
  personal Chrome profile — always use the dedicated `~/.dotz/ai-agents/browser-profile`. In
  browser-dev mode it's a no-op stub.
- `workflow-bridge.ts` is best-effort — it never blocks the agent loop. If it fails to synthesize a
  run, the subagent extension still works; the UI graph just doesn't populate for that call.
- `metrics.ts` is a pure measurement layer — no agent runtime. The RSI brain (the `/self-improve`
  preset + the `rsi_*`/`human_gate` tools) drives the loop; `metrics.ts` provides the evidence.
- `design.ts` is bundled offline design knowledge. If the `ui-ux-pro` MCP is available in the dev
  environment, the agent may also use it directly — but `design_*` tools must work standalone.
- esbuild leaves `node_modules` external so pi's jiti `.ts` extension loading works at runtime;
  `electron-builder.yml` sets `asar: false` for the same reason.