# dotz

A **pi.dev-based, Claude-Desktop-style coding-agent dashboard** — a clean chat interface for
the [pi](https://pi.dev) coding agent, with live controls for **model, reasoning effort, tools,
skills, and subagent orchestration**, packaged as its own desktop app.

dotz embeds pi's SDK directly (`@earendil-works/pi-coding-agent`), so the dashboard server *is*
the agent — no subprocess, nothing to version separately. A lean Fastify backend exposes a
REST + WebSocket surface; a single self-contained cyberbrutalist UI binds to it; Electron wraps
it into a native window / portable `.exe`. dotz is **multi-provider** (not just OpenRouter),
keeps **persistent projects** + a **memory store** injected into the agent's system prompt, and
ships a **sandbox** with a live web preview the agent can drive an on-screen cursor over, plus a
native **Open Design** workspace (150+ design systems, live preview, HTML/PDF export).

## Architecture

```
dotz.exe  (Electron)
 ├─ main process (Node): boots the embedded pi SDK + Fastify on 127.0.0.1:4317
 │    src/pi.ts       — PiSessions: owns AgentSession lifecycle, fans events to WS subscribers
 │    src/profiles.ts — 6 operating profiles + bundled .pi loader (injects doctrine + project memory)
 │    src/projects.ts — persistent named workspaces (cwd + profile + model + thinking defaults)
 │    src/memory.ts   — mem0-backed autonomous memory (on-device; capture/recall/consolidate)
 │    src/embedder.ts — bundled local transformers.js embedder (all-MiniLM-L6-v2, 384-dim)
 │    src/sandbox.ts  — terminal + web sandbox runner with agent-cursor overlay
 │    src/types.ts    — shared types (ModelRef, ProviderMeta, Project, MemoryView, SandboxRun)
 │    src/server.ts   — Fastify: REST controls + WS event stream + static UI
 │    src/main.ts     — Electron: start server, open BrowserWindow → localhost
 │    .pi/            — bundled agent resources: extensions, agents, workflow prompts, + vendored
 │                    Open Design content (150+ design-systems + design-skills) for DESIGN mode
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
    - **DESIGN** — graphic/visual design backed by native Open Design (gallery, preview, export — see below).
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
- **Memory** — an **autonomous, on-device memory** backed by [mem0](https://github.com/mem0ai/mem0).
  Durable facts (`project` or `global` scope) are **captured automatically** from each task, **recalled
  automatically** before the next one (semantic search of the folder + global memory, injected into the
  turn), and **consolidated automatically** — you never manage it. Embeddings run from a bundled local
  model; the extraction LLM is the same Ollama Cloud chat dotz already uses, so no new key or cloud. A
  git-committable `MEMORY.md` mirror is the source of truth; the vector index is a derived cache.
  Memory persists across restarts alongside projects.
- **Sandbox** — run code in two modes. **`terminal`** mode streams stdout/stderr back into the
  chat. **`web`** mode starts a long-lived process bound to a local HTTP port and the UI renders an
  inline web preview iframe at that port; the agent drives an **agent cursor** over the live
  preview (`move` / `click` / `type` at `(x, y)`) and both the agent and the user see the same
  pointer state via `sandbox_cursor` events. Run lifecycle: `pending → running → done|error|killed`.

## Design mode (Open Design, native)

dotz ships a native port of [Open Design](https://github.com/nexu-io/open-design) — its content,
in dotz's own shell. Open the **DESIGN panel** from the `+ PANELS` palette: a design workspace with
a searchable gallery of **150+ bundled design systems** (Stripe, Linear, Apple, Notion, Vercel,
Figma, …), a live same-origin preview iframe, and one-click **HTML / PDF export** of the rendered
artifact.

- **Design systems** live at `.pi/design-systems/<slug>/` (each a `DESIGN.md` + `tokens.css` +
  `components.html`), served read-only via `GET /api/design/systems`. Apache-2.0 — see the bundled
  `LICENSE` + `NOTICE`.
- **Design skills** (150+) are vendored into the unified skill pool tagged `source: design` at
  *lowest* priority (they never shadow your own same-named skills) and kept out of the always-on
  prompt index — load any by name with the `skill` tool.
- **DESIGN profile** makes design the operating mode for a session; **`/design <brief>`** kicks off
  a design workflow; and an **auto-route** opens the panel + injects the Open Design doctrine
  whenever a request looks graphic/design-related, so design work lands in the native workspace even
  from another profile.

The preview iframe is sandboxed (`allow-same-origin allow-modals allow-popups`, **no** `allow-scripts`)
since the bundled systems are static HTML/CSS — a hardened default that still supports print-to-PDF.

## Setup

```bash
npm install
```

dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env vars). Set keys for the
providers you use — **Ollama Cloud** is the primary (executive `glm-5.2`, subagent `minimax-m3`)
and **OpenRouter** is the free fallback (`nex-agi/nex-n2-pro:free`):

```bash
OLLAMA_API_KEY=...        # primary — Ollama Cloud
OPENROUTER_API_KEY=...    # fallback — OpenRouter :free models
```

See [.env.example](.env.example). Never commit real keys.

## Run

```bash
# Browser (no Electron) — fastest dev loop; open http://127.0.0.1:4317
npm run dev:server

# Native desktop window (Electron)
npm run electron
```

## Build the single exe

```bash
npm run dist        # → release/dotz.exe  (the one Windows launcher)
```

`npm run build` bundles `src/` into `dist/` with esbuild (node_modules left external so pi's
runtime extension loading works); `electron-builder` then produces the portable executable.

## Source-rebuild updater

The portable `.exe` updates itself by **git pull + rebuild + relaunch** (electron-updater can't
self-replace a running portable exe). On launch it runs a background git check; if the local checkout
is behind its tracking branch, the UI shows an UPDATE AVAILABLE card with the behind-count and short
shas. **UPDATE & RESTART** spawns a detached helper that waits for dotz to exit, runs
`git pull --ff-only` + `npm run dist:portable`, and relaunches the freshly-built exe.

This assumes the dotz **source repo + node/npm are present** on the machine. The repo is resolved from
`DOTZ_REPO_DIR` or by walking up from the running exe to the nearest `.git`. A dirty working tree
blocks the update (an ff pull would fail) — commit or stash first.

Test the updater logic offline: `npm run test:updater`.

## Notes & caveats

- **Multi-provider auth**: dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env
  vars). Provide a working provider key for whichever provider you select — **Ollama Cloud**
  (`OLLAMA_API_KEY`, the primary: executive `glm-5.2`, subagent `minimax-m3`) or **OpenRouter**
  (`OPENROUTER_API_KEY`, the free fallback). **OpenRouter credits**: provider errors surface
  in-chat (e.g. `402 … can only afford N tokens`). Use `:free` models (the default
  `nex-agi/nex-n2-pro:free`) when the account balance is low.
- **Subagents** spawn separate `pi` processes (each an LLM run); the bundled agents default to the
  free model so `/implement` is runnable out of the box. Edit `.pi/agents/*.md` to change models.
- **Fonts** load from Google Fonts (online). Bundle locally for fully-offline use.
- The UI was specced for and can be refined in [claude.ai/design](https://claude.ai/design).
