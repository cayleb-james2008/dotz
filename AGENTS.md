# dotz — project agent guide

dotz is a **pi.dev-based, Claude-Code-style ultra-code coding-agent dashboard** — a bento-style
multi-agent coding interface with **on-the-fly workflow graphs, unified skills across the
opencode/claude/codex/ecc/superpowers pools, `.ai-agents` global memory, and recursive
self-improvement wiring**, packaged as a single Electron `.exe`.

## Distribution

**Operator preference: ship a single self-contained Windows exe.** dotz packages as one Electron executable (portable `release/dotz <version>.exe` + NSIS installer) — Node, the pi SDK, and the UI are all bundled. The operator runs the exe directly.

**Updates: SOURCE-REBUILD, not electron-updater.** A portable exe cannot self-replace while running, so dotz updates by `git pull --ff-only` + rebuild + relaunch (`src/updater.ts` + the pure core `src/updater-core.ts`). On launch it runs a background git check (fetch + `HEAD...@{u}` behind-count + dirty-tree detect) and surfaces "update available" to the renderer; the UPDATE & RESTART action spawns a **detached** `.bat` helper that waits for the app to exit, pulls, runs `npm run dist:portable`, and relaunches the rebuilt exe. **This assumes the dotz source repo + node/npm are present on the machine** (same assumption as the operator's other source-rebuild updaters). Don't reintroduce electron-updater for the portable target. Don't split into a second executable or add an external-runtime dependency without operator sign-off.

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
defaults. `src/memory.ts` is the **memory store**, now backed by **mem0** (self-hosted OSS, fully
on-device): one embedded mem0 `Memory` under `~/.dotz/ai-agents/mem0/` (sqlite vector + history via
`better-sqlite3`), with embeddings from a **bundled local transformers.js model** (`src/embedder.ts`,
all-MiniLM-L6-v2, 384-dim) injected in-process, and mem0's fact-extraction/consolidation LLM pointed
at the SAME Ollama Cloud chat dotz already uses (no new SaaS/key). Scope is partitioned by mem0
`userId` (`__global__` vs `proj:<cwd>`); typed-memory `category`, `folder`, and a timestamp live in
mem0 metadata. The git-committable **source of truth is `MEMORY.md`** (global `~/.dotz/ai-agents/`,
project `<cwd>/.ai-agents/`), regenerated on every write — the vector index is a derived cache.

**Memory is AUTONOMOUS** (the operator never manages it): the dotz-tools `before_agent_start` hook
does pre-task recall (semantic search of folder + global memory, relevance-thresholded, recency- and
graph-boosted) and injects it into the turn; the `agent_end` hook auto-captures durable facts from the
exchange and triggers threshold-based consolidation. `src/memory-graph.ts` is a lean on-device
entity/relationship layer (a second `better-sqlite3` table — no external graph DB) giving a 1-hop
recall boost + an observable graph. AGENTS.md files (project root + global
`~/.config/opencode/AGENTS.md`) remain the **doctrine** layer — read by `buildResourceLoader`, written
via the `agents_md` tool; mem0 memory is the **knowledge** layer. Don't blur them. Both persist across
restarts. Legacy `memory.json` files are imported once on first run and kept as a backup.

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

`web/` is a **vanilla JS bento dashboard** (no framework, no build step). Initial state: a minimal
**command center** launch surface — a large centered composer with project/profile/model badges and
quick chips, plus a compact top bar holding the brand, project selector, provider/model/reasoning
knobs, connection chip, and `+ PANELS` button. Opening a project hides the command center and reveals
the CSS-grid bento layout.

Progressive panel disclosure: only `chat` is visible initially; `+ PANELS` (or `Ctrl+P`) opens a
palette to add panels (`graph`, `brain`, `browser`, `memory`, `files`, `sandbox`, `skills`). Panels
auto-open on relevant events (workflow start → `graph`; sandbox start → `sandbox`). Drag-and-drop
repositioning via HTML5 DnD on panel headers swaps CSS grid spans **without re-cloning DOM**,
preserving panel state. Layout persists to `localStorage.dotz.layout.v1`.

The **composer** has an inline slash/skills/files palette: type `/` to filter workflow commands from
`GET /api/sessions/:id/commands`, `@` to filter skills, or `#` to filter project files from
`GET /api/projects/:id/files`. Arrow keys navigate, Enter inserts the selected item. Selected skills
are sent as `@{skill}` prompts; files as `#{path}` prompts.

The **workflow graph panel** auto-opens when a workflow starts. It renders live SVG DAGs with layered
topological layout, larger nodes, agent-icon initials, status rings (with animated pulses for
running steps), dashed animated edges for active paths, and click-to-inspect detail. A toolbar provides
fit/reset view controls. The side **node detail drawer** shows the step's task, status, output,
error, usage, thinking, sandbox run link (opens the sandbox panel), browser session link (opens the
browser panel and refreshes the screenshot), and tool-call IDs.

The **human-gate UI** renders a centered modal when a `{kind:"gate", gateId, plan}` WS event arrives,
showing the plan and optional feedback textarea; approval/rejection sends `gate.approve`/`gate.reject`
messages.

The **browser panel** provides real in-app browser controls: back/forward/reload, URL input + GO,
screenshot capture, and JS eval. Screenshots are displayed in the panel and auto-refreshed when opened
from a workflow step link. In browser-dev mode the backend stubs return a placeholder.

The **files panel** fetches and renders the recursive `GET /api/projects/:id/files` tree with
expandable directories.

The **brain float** is a compact top-right overlay showing live token/cost/ctx stats; toggled via the
brain icon or the brain panel. It updates from session stats and can be collapsed to a toggle button.

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

## Isolated browser

`src/browser.ts` is an **isolated `agent-browser` controller** — it spawns the pinned external
`agent-browser` binary in a disposable worker. Remote pages never run inside Dotz's Electron renderer
and never receive its preload; each session gets a throwaway profile (`mkdtemp` under the OS temp dir,
removed on stop) and an explicit origin allowlist enforced on every navigation. Frames are served as
JPEG bytes on `/api/browser/frame` (never embedded in the JSON event stream). REST endpoints:
`/api/browser/state|frame|start|act|stop`. Navigation, clicks, typing, and scrolling all go through
`act` as `BrowserActInput.action` values
(`navigate|observe|back|forward|reload|click|clickAt|type|key|select|scroll|wait`) — there is no
raw-JS `eval` (forbidden by `scripts/verify-browser-controller.mjs`). Full contract:
`docs/api-contract.md` → "Isolated interactive browser".

## Connections

`src/connections.ts` is the **local connections** layer: a simple in-browser login for GitHub, Vercel,
and Neon on this machine. It reads each provider's existing local auth and shells its own browser login —
there is **no OAuth app registration and no client secret**; the provider tooling owns the device/browser
flow and token storage. GitHub (`gh`) and Vercel (`vercel`) use their CLI for status/login/logout. Neon
logs in via `npx neonctl auth` (no global install) and keys status/logout off neonctl's `credentials.json`,
because neonctl has no logout command and no on-PATH `me` here. Login streams the CLI's device-code / URL
prompt so the user finishes in a normal browser tab; dotz never reads, stores, or logs the token. The
agent uses the same CLIs via `bash` once connected. REST endpoints: `GET /api/connections` (status of all
three), `POST|GET /api/connections/:provider/login` (start + stream output),
`POST /api/connections/:provider/logout`. Surfaced via the **CONNECTIONS** panel (`+ PANELS`).

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
- `src/browser.ts` — isolated `agent-browser` controller (spawns the external binary in a throwaway `mkdtemp` profile + origin allowlist; remote pages never touch the Electron renderer or its preload).
- `src/connections.ts` — local connections (GitHub/Vercel/Neon) via the provider CLIs; status + streamed browser login + logout. No OAuth app, no stored secrets; tokens never read or logged.
- `src/projects.ts` — persistent projects layer (name + cwd + profile/model/thinking defaults).
- `src/memory.ts` — mem0-backed memory store (`project` / `global` scope) under `~/.dotz/ai-agents/mem0/` + `MEMORY.md` mirrors + AGENTS.md read/write helpers + autonomy flag + recall event emitter. Build-time seed via `buildResourceLoader`; live per-turn recall + capture via the dotz-tools hooks.
- `src/embedder.ts` — bundled local transformers.js embedder (all-MiniLM-L6-v2, 384-dim), injected in-process into mem0 (no embeddings API/route).
- `src/memory-graph.ts` — lean on-device entity/relationship graph (`better-sqlite3`) for recall boost + observability; no external graph DB.
- `src/sandbox.ts` — sandbox runner (`terminal` + `web` modes, agent cursor, lifecycle events).
- `src/types.ts` — shared types (`ModelRef`, `ProviderMeta`, `Project`, `MemoryEntry`, `SandboxRun`,
  `Skill`, `WorkflowRun`, `WorkflowStep`, `ThinkingLevel`, `DEFAULT_MODEL`, `LOW_COST_MODELS`,
  `renderLowCostModels`). Kept separate from `pi.ts` to avoid a circular import — do not merge.
- `src/server.ts` — Fastify REST + WS + static. Routes: sessions, projects, memory, skills, workflows,
  sandbox, browser, human-gate.
- `src/updater.ts` / `src/updater-core.ts` — source-rebuild updater (git pull + `npm run dist:portable` + relaunch). Core is electron-free + unit-tested (`npm run test:updater`); the shell wires the IPC + detached helper.
- `src/main.ts` / `src/preload.ts` — Electron main + contextIsolation preload.
- `web/` — vanilla JS bento dashboard (project launcher, progressive panel disclosure, drag-and-drop,
  on-the-fly SVG workflow graph, skills/memory/sandbox/brain panels).
- `.pi/` — bundled agent resources (subagent extension, dotz-tools extension, agents, 6 workflow prompts).
  Shipped in the exe via `electron-builder.yml` `files:`.
- `.pi/extensions/dotz-tools/index.ts` — registers the dotz pi tools: dynamic resource tools
  `create_agent` / `list_agents` / `create_skill` / `list_skills`; `skill`; the mem0 memory tools
  `memory_list`, `memory_search`, `memory_add`, `memory_update`, `memory_delete`, `memory_consolidate`;
  `agents_md`; `rsi_baseline`, `rsi_compare`, `human_gate`; `browser_*`; `design_*` + the
  `resolveHumanGate`/`onGateRequest` server hooks. **Also registers the autonomous-memory lifecycle
  hooks** (`before_agent_start` → pre-task recall, `agent_end` → auto-capture + consolidation), gated
  by `isMemoryAutonomyEnabled()` so only the main server process runs them (never spawned subagents).
- `docs/api-contract.md` — the authoritative UI↔backend contract.

## Verification commands

```bash
npm run typecheck                      # tsc --noEmit — must be clean
npx tsx scripts/verify-profiles.mjs   # e2e: profiles + .pi bundle + subagent + dotz-tools surface
npx tsx scripts/verify-ui.mjs          # e2e: UI markup + profiles API
npx tsx scripts/verify-features.mjs    # e2e: projects + memory + sandbox + multi-provider (11 providers)
npx tsx scripts/verify-ultra.mjs       # e2e: 320 skills + workflows + .ai-agents memory + RSI/design/browser tools + 6 presets + Ollama
npx tsx scripts/verify-memory.ts       # e2e: mem0 memory OFFLINE (add/search/consolidate/graph/MEMORY.md mirror) — no network/keys
npx tsx scripts/verify-memory-live.ts  # e2e: live capture→recall against Ollama Cloud (needs $OLLAMA_API_KEY)
npm run dev:server                      # http://127.0.0.1:4317 (browser dev loop)
npm run build                           # esbuild → dist/main.js + dist/preload.cjs
npm run dist                            # → release/dotz <version>.exe (Windows portable)
npm run dist:publish                    # same as dist + publish the portable exe to the configured generic feed
```

## Source-rebuild updater

`src/updater.ts` (electron shell) + `src/updater-core.ts` (pure, unit-tested) update the portable
exe by **git pull + rebuild + relaunch** — electron-updater can't self-replace a running portable exe.

1. On launch, `checkForUpdatesOnLaunch` runs a background git check: `git fetch`, then
   `git rev-list --left-right --count HEAD...@{u}` for the behind-count, plus short shas and a
   `git status --porcelain` dirty-tree check. The tracking branch (`@{u}`) drives it; falls back to
   `origin/main`. The repo dir is resolved from `DOTZ_REPO_DIR` or by walking up from `process.execPath`
   to the nearest `.git`.
2. If behind > 0, the renderer shows the UPDATE AVAILABLE card (behind-count + `localSha → remoteSha`).
   A dirty tree is surfaced and blocks APPLY (an ff pull would fail).
3. CHECK FOR UPDATES (settings) re-runs the same git check on demand.
4. UPDATE & RESTART → `applyUpdate`: writes a detached `.bat` helper to the temp dir, spawns it
   detached (its own console for build output), then quits after 400ms. The helper waits for the app
   pid to exit, runs `git pull --ff-only`, `npm run dist:portable`, and relaunches the rebuilt exe
   (in dev: `npm run electron`).
5. The machine must have the dotz source repo + node/npm present (same as the operator's other
   source-rebuild updaters).

Test the core offline with `npm run test:updater` (fake git runner; no network, no electron-builder).

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
- `projects.ts` is a pure persistence layer. `memory.ts` wraps mem0; it has two intentional injection
  paths: a build-time **seed** via `buildResourceLoader` in `profiles.ts`, and live **per-turn recall**
  via the dotz-tools `before_agent_start` hook. Keep capture/recall gated by `isMemoryAutonomyEnabled()`
  (main process only). The `MEMORY.md` mirror is the git-committable source of truth — don't hand-edit
  the sqlite vector index; it's a derived cache.
- `skills.ts` is the single skill-discovery path. Don't add a second skill loader. The skill index
  is injected via `buildResourceLoader` (same as memory); the `skill(name)` tool loads bodies on demand.
- `workflows.ts` is the observability + control layer for multi-agent orchestration. It does NOT
  spawn agents — the `subagent` extension is still the execution backend. `WorkflowStore` only
  records and surfaces step DAGs. Keep it a pure persistence + event layer.
- `sandbox.ts` owns process lifecycle for both `terminal` and `web` runs; never spawn sandbox
  processes directly from `server.ts`. The agent cursor is a UI overlay driven by `sandbox_cursor`
  WS events — both sides (agent send + UI render) must consume the same event shape.
- `browser.ts` owns the isolated `agent-browser` controller. Remote pages never run inside Dotz's
  Electron renderer and never receive its preload — each session spawns the external `agent-browser`
  binary with a throwaway `mkdtemp` profile (never the user's personal Chrome profile) and an explicit
  origin allowlist. There is no raw-JS `eval` boundary (enforced by `scripts/verify-browser-controller.mjs`).
- `workflow-bridge.ts` is best-effort — it never blocks the agent loop. If it fails to synthesize a
  run, the subagent extension still works; the UI graph just doesn't populate for that call.
- `metrics.ts` is a pure measurement layer — no agent runtime. The RSI brain (the `/self-improve`
  preset + the `rsi_*`/`human_gate` tools) drives the loop; `metrics.ts` provides the evidence.
- `design.ts` is bundled offline design knowledge. If the `ui-ux-pro` MCP is available in the dev
  environment, the agent may also use it directly — but `design_*` tools must work standalone.
- esbuild leaves `node_modules` external so pi's jiti `.ts` extension loading works at runtime;
  `electron-builder.yml` sets `asar: false` for the same reason.
- **Frontend panels use class-scoped selectors or per-panel `querySelector` lookups.** Because panel
  templates are cloned, avoid global `getElementById` for panel internals that could appear twice.
  The default layout only contains one panel of each type, but the code should be robust to swapping
  without re-cloning DOM.
