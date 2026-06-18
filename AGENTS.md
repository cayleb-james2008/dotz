# dotz — project agent guide

dotz is a **pi.dev-based, Claude-Desktop-style coding-agent dashboard** — a clean chat interface
for the pi coding agent, with live controls for **profile, model, reasoning effort, tools, skills,
and subagent orchestration**, packaged as a single Electron `.exe`.

## Architecture in one paragraph

dotz embeds pi's SDK **directly** (`@earendil-works/pi-coding-agent`), so the dashboard server *is*
the agent — no subprocess, nothing to version separately. `src/profiles.ts` loads the bundled
`.pi/` resources (subagent extension + 4 agents + 3 workflow prompts) and injects the active
profile's doctrine as an `appendSystemPrompt`. `src/projects.ts` persists named workspaces (cwd +
profile + model + thinking defaults) that sessions bind to; `src/memory.ts` stores per-project (and
global) memory entries that `buildResourceLoader` injects into the system prompt so the agent has
durable context across turns. `src/sandbox.ts` runs code in a sandbox with a `terminal` mode and a
`web` mode (long-lived process on a local port) plus an agent cursor the UI overlays on the live web
preview. `src/pi.ts` owns `AgentSession` lifecycle and fans events to WebSocket subscribers. `src/server.ts`
is a lean Fastify surface (REST controls + WS event stream + static UI). `src/main.ts` boots the
server in the Electron main process and opens a native window. The UI in `web/` is plain
HTML/CSS/JS talking over fetch + WebSocket, so it runs identically in a browser (`npm run dev:server`)
and inside the packaged app.

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
`buildResourceLoader`, so the agent sees durable notes without re-prompting. Both persist across
server restarts.

## Sandbox

`src/sandbox.ts` is the **sandbox**: it runs code in `terminal` mode (stream stdout/stderr back) or
`web` mode (long-lived process bound to a local HTTP port). In `web` mode the UI renders an inline
preview iframe at the published port, and the agent drives an **agent cursor** over it via
`sandbox.cursor` WS messages (`move` / `click` / `type` at `(x, y)`). The UI renders the cursor as an
overlay; the server emits `sandbox_cursor` events back so both the agent and the user see the same
pointer state. Run lifecycle: `pending → running → done|error|killed`.

## Key files

- `src/pi.ts` — `PiSessions`: create/get/list/subscribe/dispose, model resolution (OpenRouter
  custom-id support), command aggregation. **Import only from the top-level
  `@earendil-works/pi-coding-agent`** — `pi-ai`/`pi-agent-core` are nested and not resolvable from
  the project root.
- `src/profiles.ts` — 5 profiles (WORKFLOW/SOLO/PLAN/FRONTEND/BACKEND) + `buildResourceLoader`
  (loads bundled `.pi` + injects doctrine + injects project memory).
- `src/projects.ts` — persistent projects layer (name + cwd + profile/model/thinking defaults).
- `src/memory.ts` — memory store (`project` / `global` scope), injected into the system prompt via
  `buildResourceLoader`.
- `src/sandbox.ts` — sandbox runner (`terminal` + `web` modes, agent cursor, lifecycle events).
- `src/types.ts` — shared types (`ModelRef`, `ProviderMeta`, `Project`, `MemoryEntry`,
  `SandboxRun`, `ThinkingLevel`, `DEFAULT_MODEL`). Kept separate from `pi.ts` to avoid a circular
  import — do not merge.
- `src/server.ts` — Fastify REST + WS + static. The `need()` helper resolves `:id` → session entry.
- `src/main.ts` / `src/preload.ts` — Electron main + contextIsolation preload.
- `web/` — cyberbrutalist/Catppuccin-Mocha chat UI (vanilla, no framework).
- `.pi/` — bundled agent resources (subagent extension, agents, workflow prompts). Shipped in the
  exe via `electron-builder.yml` `files:`.
- `docs/api-contract.md` — the authoritative UI↔backend contract.

## Verification commands

```bash
npm run typecheck                      # tsc --noEmit — must be clean
npx tsx scripts/verify-profiles.mjs   # e2e: profiles + .pi bundle + subagent tool
npx tsx scripts/verify-ui.mjs          # e2e: UI markup + profiles API
npx tsx scripts/verify-features.mjs    # e2e: projects + memory + sandbox + multi-provider
npm run dev:server                      # http://127.0.0.1:4317 (browser dev loop)
npm run build                           # esbuild → dist/main.js + dist/preload.cjs
npm run dist                            # → release/dotz <version>.exe (Windows portable)
```

## Provider config

dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env vars). Provide a working
provider key, e.g. `OPENROUTER_API_KEY`. OpenRouter is a **free-form model-id input** (not a
dropdown), defaulting to `nex-agi/nex-n2-pro:free`; use `:free` models when the balance is low
(provider errors surface in-chat as `stopReason:"error"` + `errorMessage`).

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
- `sandbox.ts` owns process lifecycle for both `terminal` and `web` runs; never spawn sandbox
  processes directly from `server.ts`. The agent cursor is a UI overlay driven by `sandbox_cursor`
  WS events — both sides (agent send + UI render) must consume the same event shape.
- esbuild leaves `node_modules` external so pi's jiti `.ts` extension loading works at runtime;
  `electron-builder.yml` sets `asar: false` for the same reason.