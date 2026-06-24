<div align="center">

<img src="docs/dotz-logo.png" alt="dotz" width="120" />

# dotz

**The ultra-code agent dashboard — one prompt becomes a team of AI coding agents.**

[![Download](https://img.shields.io/github/v/release/cayleb-james2008/dotz?label=download&color=b4befe)](https://github.com/cayleb-james2008/dotz/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/cayleb-james2008/dotz/total?color=a6e3a1)](https://github.com/cayleb-james2008/dotz/releases/latest)
[![Platform](https://img.shields.io/badge/platform-Windows-89b4fa)](https://github.com/cayleb-james2008/dotz/releases/latest)
[![Built with](https://img.shields.io/badge/built%20with-Rust%20%2B%20Tauri-cba6f7)](https://tauri.app)

</div>

dotz is a **native-Rust, Claude-Desktop-style coding-agent dashboard**. Give it one task and it
decomposes the work, disperses it to a team of subagents, runs them in parallel on a **live workflow
graph**, and adversarially verifies the result — with live controls for **model, reasoning effort,
tools, skills, and subagent orchestration**, all in a single self-updating desktop app.

Under the hood: `dotz-core` is an [axum](https://github.com/tokio-rs/axum) server with dotz's own
agent runtime (no third-party agent SDK, no subprocess), and a [Tauri](https://tauri.app) (WebView2)
shell wraps it into a signed, self-updating app. It is **multi-provider** (not just OpenRouter), keeps
**persistent projects** + an on-device **memory store** (`ort` ONNX embeddings) injected into the
agent's prompt, ships a **sandbox** with a live web preview the agent drives an on-screen cursor over,
and a native **Open Design** workspace (150+ design systems, live preview, HTML/PDF export).

<div align="center">
<img src="docs/screenshot.png" alt="dotz — the ultra-code agent dashboard" width="820" />
</div>

## How it works

```mermaid
flowchart LR
    U([Your prompt]) --> L[Lead agent]
    L -->|decompose + disperse| S[scout]
    L --> P[planner]
    L --> W[worker]
    L --> R[reviewer]
    S --> V{adversarial verify}
    P --> V
    W --> V
    R --> V
    V -->|pass| D([Verified result])
    V -->|gaps| L
```

Every non-trivial task fans out to `scout` / `planner` / `worker` / `reviewer` subagents that run in
parallel, then their output is adversarially verified before it lands. Pick a **profile** to change the
strategy (WORKFLOW · SOLO · PLAN · FRONTEND · BACKEND · DESIGN).

## Architecture

```mermaid
flowchart TD
    subgraph App["dotz desktop app · Tauri / WebView2"]
        UI["web/ UI<br/>vanilla HTML · CSS · JS"]
        Shell["src-tauri shell<br/>+ auto-updater"]
    end
    subgraph Core["dotz-core · Rust / axum @ 127.0.0.1:4317"]
        API["REST + WebSocket"]
        Agent["agent runtime<br/>chat · tools · subagents · providers"]
        Mem["memory<br/>ort ONNX embeddings"]
        Sand["sandbox + browser<br/>agent-cursor overlay"]
        Dsgn[".pi · Open Design<br/>150+ design systems"]
    end
    GH[(GitHub Releases)]
    UI <-->|fetch + WS| API
    Shell --> API
    API --> Agent
    Agent --> Mem
    Agent --> Sand
    Agent --> Dsgn
    Shell -.->|signed update| GH
```

- **`src-tauri/`** — thin Rust shell: starts dotz-core on `127.0.0.1:4317`, opens the WebView2 window, wires `tauri-plugin-updater` (signed cross-device updates).
- **`dotz-core/`** — axum server + dotz's own agent runtime: `agent/` (chat loop, tools, subagents, providers), `memory.rs` + `embed.rs` (on-device `ort` embeddings, all-MiniLM-L6-v2), `sandbox`/`browser` (agent-cursor overlay), `profiles.rs`/`projects.rs`, `server/` (REST + WS + the `serve` headless bin).
- **`web/`** — the cyberbrutalist chat UI (vanilla HTML/CSS/JS), identical in a browser and in the app.
- **`.pi/`** — bundled agent resources: agents, workflow prompts, and vendored Open Design content (150+ design systems + design skills).

The UI is a plain web app (`fetch` + `WebSocket`), so it runs identically in a browser pointed at the
headless `serve` binary and inside the Tauri window. See [docs/api-contract.md](docs/api-contract.md)
for the full UI↔backend contract, and [docs/design-prompt.md](docs/design-prompt.md) for the UI spec.

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
npm install        # ships the agent-browser binary + the @huggingface/transformers model fetcher
npm run fetch-model    # downloads the all-MiniLM-L6-v2 ONNX model into assets/models/ (bundled by Tauri)
```

dotz resolves provider auth from `~/.pi/agent/auth.json` → env vars. Set keys for the providers you
use — **Ollama Cloud** is the primary (executive `glm-5.2`, subagent `minimax-m3`) and **OpenRouter**
is the free fallback (`nex-agi/nex-n2-pro:free`):

```bash
OLLAMA_API_KEY=...        # primary — Ollama Cloud
OPENROUTER_API_KEY=...    # fallback — OpenRouter :free models
```

See [.env.example](.env.example). Never commit real keys.

## Run

```bash
# Headless backend (browser dev loop) — open http://127.0.0.1:4317
cargo run -p dotz-core --bin serve

# Native desktop window (Tauri / WebView2)
cargo tauri dev
```

## Build the installer

```bash
cargo tauri build   # → src-tauri/target/release/bundle/nsis/  (signed NSIS installer + latest.json)
```

The signed build needs the updater signing key in the environment — see
[src-tauri/DEPLOY.md](src-tauri/DEPLOY.md) for the full build + release flow.

## Cross-device auto-update

The installed app self-updates via `tauri-plugin-updater`: it checks this repo's
[latest release](https://github.com/cayleb-james2008/dotz/releases/latest) for a minisign-signed
`latest.json`, verifies it against the bundled pubkey, and installs + relaunches in place. Source and
signed releases live in this one public repo. Full topology and the release commands are in
[src-tauri/DEPLOY.md](src-tauri/DEPLOY.md).

## Notes & caveats

- **Multi-provider auth**: dotz uses pi's normal auth resolution (`~/.pi/agent/auth.json` → env
  vars). Provide a working provider key for whichever provider you select — **Ollama Cloud**
  (`OLLAMA_API_KEY`, the primary: executive `glm-5.2`, subagent `minimax-m3`) or **OpenRouter**
  (`OPENROUTER_API_KEY`, the free fallback). **OpenRouter credits**: provider errors surface
  in-chat (e.g. `402 … can only afford N tokens`). Use `:free` models (the default
  `nex-agi/nex-n2-pro:free`) when the account balance is low.
- **Subagents** run in dotz's native runtime (each a separate LLM run); the bundled agents default
  to the free model so `/implement` is runnable out of the box. Edit `.pi/agents/*.md` to change models.
- **Fonts** load from Google Fonts (online). Bundle locally for fully-offline use.
- The UI was specced for and can be refined in [claude.ai/design](https://claude.ai/design).
