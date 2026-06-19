# dotz

A **pi.dev-based, Claude-Desktop-style coding-agent dashboard** — a clean chat interface for
the [pi](https://pi.dev) coding agent, with live controls for **model, reasoning effort, tools,
skills, and subagent orchestration**, packaged as its own desktop app.

dotz embeds pi's SDK directly (`@earendil-works/pi-coding-agent`), so the dashboard server *is*
the agent — no subprocess, nothing to version separately. A lean Fastify backend exposes a
REST + WebSocket surface; a single self-contained cyberbrutalist UI binds to it; Electron wraps
it into a native window / portable `.exe`. dotz is **multi-provider** (not just OpenRouter),
keeps **persistent projects** + a **memory store** injected into the agent's system prompt, and
ships a **sandbox** with a live web preview the agent can drive an on-screen cursor over.

## Architecture

```
dotz.exe  (Electron)
 ├─ main process (Node): boots the embedded pi SDK + Fastify on 127.0.0.1:4317
 │    src/pi.ts       — PiSessions: owns AgentSession lifecycle, fans events to WS subscribers
 │    src/profiles.ts — 5 operating profiles + bundled .pi loader (injects doctrine + project memory)
 │    src/projects.ts — persistent named workspaces (cwd + profile + model + thinking defaults)
 │    src/memory.ts   — per-project / global memory store, injected into the system prompt
 │    src/sandbox.ts  — terminal + web sandbox runner with agent-cursor overlay
 │    src/types.ts    — shared types (ModelRef, ProviderMeta, Project, MemoryEntry, SandboxRun)
 │    src/server.ts   — Fastify: REST controls + WS event stream + static UI
 │    src/main.ts     — Electron: start server, open BrowserWindow → localhost
 │    .pi/            — bundled agent resources: subagent extension, 4 agents, 3 workflow prompts
 └─ renderer: web/   — the cyberbrutalist chat UI (vanilla HTML/CSS/JS), same in browser + app
```

The UI is a plain web app (`fetch` + `WebSocket`), so it runs identically in a browser during
development and inside the Electron window. See [docs/api-contract.md](docs/api-contract.md) for
the full UI↔backend contract, and [docs/design-prompt.md](docs/design-prompt.md) for the UI spec.

## Controls (the five knobs)

- **Profile** — the top-bar segmented picker switches dotz's operating mode. Each profile injects
  a doctrine (`appendSystemPrompt`) and a default tool set; switching it starts a fresh session.
    - **WORKFLOW** *(default)* — multi-agent dispersal: every non-trivial task is decomposed and
      dispersed to `scout` / `planner` / `reviewer` / `worker` subagents, then adversarially
      verified.
    - **SOLO** — single agent, direct execution, no subagents unless asked.
    - **PLAN** — read-only research + planning (`read, grep, find, ls, subagent`; no edits).
    - **FRONTEND** — workflow mode tuned for UI/design work (WCAG, real focus states, no AI-slop).
    - **BACKEND** — workflow mode tuned for APIs/data/infra (TDD, boring tech, honest errors).
- **Model** — dotz is **multi-provider**, not just OpenRouter. Each provider declares its own UI
  mode via `ProviderMeta.freeForm`: OpenRouter is a **free-form model-id input** (not a giant
  dropdown), defaulting to `nex-agi/nex-n2-pro:free`; other providers may expose a fixed model list.
  The available-models list powers autocomplete suggestions.
- **Reasoning** — segmented slider `off → minimal → low → medium → high → xhigh`, constrained to
  what the active model supports.
- **Tools** — live toggle of pi's built-ins (`read, bash, edit, write, grep, find, ls`) plus the
  bundled `subagent` tool.
- **Skills / Subagents** — discovered via pi's command registry; the bundled `.pi/` extension
  provides the `subagent` tool and workflow presets `/implement`, `/scout-and-plan`,
  `/implement-and-review`, with agents `scout / planner / reviewer / worker`. Invoke a command by
  sending it in the composer (e.g. `/implement add a dark-mode toggle`).

## Projects, Memory, and Sandbox

- **Projects** — persistent named workspaces. Each `Project` binds a name, a `cwd`, and default
  `profile` / `model` / `thinking` settings; sessions created with a `projectId` inherit those
  defaults. Projects survive server restarts, so you can keep one config per codebase and jump back
  into it without re-configuring every session.
- **Memory** — a per-project (and global) memory store. `MemoryEntry`s are scoped `project` or
  `global` and are injected into the agent's system prompt at session-build time, so the agent has
  durable context ("this repo uses pnpm", "never touch `generated/`") without being told again on
  every turn. Memory persists across restarts alongside projects.
- **Sandbox** — run code in two modes. **`terminal`** mode streams stdout/stderr back into the
  chat. **`web`** mode starts a long-lived process bound to a local HTTP port and the UI renders an
  inline web preview iframe at that port; the agent drives an **agent cursor** over the live
  preview (`move` / `click` / `type` at `(x, y)`) and both the agent and the user see the same
  pointer state via `sandbox_cursor` events. Run lifecycle: `pending → running → done|error|killed`.

## Setup

```bash
npm install
```

dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env vars). Provide a working
provider key, e.g. `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY`, before running live turns.

## Run

```bash
# Browser (no Electron) — fastest dev loop; open http://127.0.0.1:4317
npm run dev:server

# Native desktop window (Electron)
npm run electron
```

## Build the single exe

```bash
npm run dist        # → release/dotz <version>.exe  (Windows portable)
```

`npm run build` bundles `src/` into `dist/` with esbuild (node_modules left external so pi's
runtime extension loading works); `electron-builder` then produces the portable executable.

## Auto-updater

The packaged `.exe` checks for updates on every launch via `electron-updater` and a generic HTTP(S)
release feed. If a newer version is available, it downloads silently and — for portable builds —
quits and installs so the next launch runs the new `.exe`.

To enable updates, host `latest.yml` plus `dotz <version>.exe` at a public URL, then point dotz at it:

- set the env var `DOTZ_UPDATE_URL=https://your-domain.com/releases`, or
- edit `build.publish.url` in `package.json` / `electron-builder.yml`.

Dev builds (`npm run dev:server`, `npm run electron`) skip the update check.

## Notes & caveats

- **Multi-provider auth**: dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env
  vars). Provide a working provider key for whichever provider you select (e.g.
  `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`). **OpenRouter credits**: provider errors surface
  in-chat (e.g. `402 … can only afford N tokens`). Use `:free` models (the default
  `nex-agi/nex-n2-pro:free`) when the account balance is low.
- **Subagents** spawn separate `pi` processes (each an LLM run); the bundled agents default to the
  free model so `/implement` is runnable out of the box. Edit `.pi/agents/*.md` to change models.
- **Fonts** load from Google Fonts (online). Bundle locally for fully-offline use.
- The UI was specced for and can be refined in [claude.ai/design](https://claude.ai/design).
