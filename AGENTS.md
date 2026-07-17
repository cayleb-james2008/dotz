# dotz — project agent guide

dotz is a **native-Rust, Claude-Code-style ultra-code coding-agent dashboard** — a bento-style
multi-agent coding interface with **on-the-fly workflow graphs, unified skills, on-device memory,
and recursive self-improvement wiring**, packaged as a signed Tauri NSIS installer for Windows.

## Canonical harness

Ultra Code is the canonical agent harness for dotz. Non-trivial sessions should load
`ultra-code` from `~/.config/opencode/skills/ultra-code` before planning, editing, subagent
dispersal, OpenSpec work, review, or verification. dotz OpenSpec, subagent, workflow graph,
sandbox, design, memory, and RSI behavior are host adapters under Ultra Code rather than a
competing harness.

## Distribution

**Operator preference: ship a signed Windows installer.** dotz packages via
`cargo tauri build` → `src-tauri/target/release/bundle/nsis/` (signed NSIS installer +
`latest.json`). The backend (`dotz-core`), the Tauri shell (`dotz-tauri`), the bundled
`.pi/` agent resources, and the vanilla-HTML UI are all packaged inside the single installer.
The operator runs the installer directly.

**Updates: cross-device auto-update via `tauri-plugin-updater`.** On launch the updater checks
the repo's latest release for a minisign-signed `latest.json`, verifies it against the bundled
pubkey, then installs + relaunches in place. No source-rebuild step, no detached helper script,
no separate updater executable. Signing key is configured via env (see `src-tauri/DEPLOY.md`).

## Architecture in one paragraph

dotz is a Rust workspace (edition 2021, v0.2.0) with two members: `dotz-core` (the pure library +
headless `serve`/`selfeval` bins) and `src-tauri` (the `dotz-tauri` desktop shell v0.2.8).
`dotz-core` owns the whole agent runtime — provider adapters, sessions, the workflow DAG,
sandbox, isolated browser, on-device memory, skills, projects, specs — and exposes it over a
lean axum + tower-http REST + WebSocket surface. The `serve` bin boots that surface headlessly on
`http://127.0.0.1:4317`. `dotz-tauri` is a thin Tauri 2 shell (`src/main.rs` + `build.rs`) that
loads the bundled vanilla-HTML UI (`web/`) into a WebView2 window and talks to the same backend.
The UI runs **identically in a browser** (against headless `serve`) and **inside the Tauri
WebView2 window** — no build step, no framework, no separate frontend bundle.

## Key files

### dotz-core

- `src/lib.rs` — crate root; re-exports the modules below.
- `src/agent/` — the agent runtime: `mod.rs` (session lifecycle), `provider.rs` + `provider_anthropic.rs`
  + `provider_google.rs` + `provider_health.rs` (multi-provider adapters), `session.rs` (turn loop),
  `subagent.rs` (runs each workflow step as an isolated LLM run, emits `step_tool` + `step_thinking`
  events onto the workflow channel), `tools.rs` (tool dispatch), `extra_tools.rs`, `event.rs`.
- `src/browser.rs` — isolated `agent-browser` controller (spawns the pinned external binary in a
  throwaway profile + origin allowlist; remote pages never touch the Tauri shell).
- `src/checkpoint.rs` — git diff/restore for reproducible run checkpoints.
- `src/commands.rs` — slash-command presets (`/ultra-code`, `/implement`, `/ultra-code-review`,
  `/self-improve`, etc.).
- `src/config.rs` — runtime config + env resolution (`DOTZ_PORT`, `DOTZ_SKILLS_PATHS`).
- `src/connections.rs` — local connections layer (GitHub/Vercel/Neon via provider CLIs; no OAuth
  app, no stored secrets; tokens never read or logged).
- `src/context_bus.rs` — shared context/event bus.
- `src/design.rs` — design-skill + design-system loader for the bundled `.pi/design-skills/` +
  `.pi/design-systems/` pools.
- `src/embed.rs` — on-device ONNX embeddings (all-MiniLM-L6-v2 via `ort`) injected in-process.
- `src/leaderboard.rs` — model/provider leaderboard surface.
- `src/living_docs.rs` — living-docs generation/maintenance.
- `src/memory.rs` — on-device memory store (`rusqlite` vector index + `MEMORY.md` source of truth).
- `src/profiles.rs` — profile loader (binds doctrine + skill index + project memory into the
  system prompt).
- `src/projects.rs` — persistent projects layer (name + cwd + profile/model defaults); owns the
  `agents_md` submodule for AGENTS.md read/write.
- `src/run_record.rs` — captures reproducible run records for the workflow graph.
- `src/sandbox.rs` — sandbox runner (`terminal` + `web` modes, agent cursor, lifecycle events).
- `src/self_eval.rs` — the eval harness backing the `selfeval` bin.
- `src/server/mod.rs` — lean axum + tower-http REST + WS surface (sessions, projects, memory,
  skills, workflows, sandbox, browser, human-gate).
- `src/skills.rs` — unified skills loader across the opencode/claude/codex/ecc/superpowers/hermes
  + bundled `.pi/` pools; dedupes by name, filters by platform.
- `src/specs.rs` — OpenSpec-style spec management.
- `src/templates.rs` — prompt + resource templates.
- `src/types.rs` — shared types (`ModelRef`, `ProviderMeta`, `Project`, `MemoryEntry`,
  `WorkflowRun`, `WorkflowStep`, `ToolCallRef`, `LOW_COST_MODELS`, `DEFAULT_MODEL`, etc.).
- `src/util.rs` — shared helpers.
- `src/vcs.rs` — git/vcs helpers.
- `src/verify.rs` — verification gate helpers (test/format/lint checks).
- `src/workflow_executor.rs` — drives workflow steps to completion.
- `src/workflows.rs` — first-class `WorkflowRun` DAG domain (`WorkflowStep`, `ToolCallRef` — now
  carries `args`+`result` for inspectable graph nodes) + status propagation + event emitter.

### Bins

- `dotz-core/src/bin/serve.rs` — headless backend on `http://127.0.0.1:4317`.
- `dotz-core/src/bin/selfeval.rs` — the self-eval harness.
- `dotz-core/src/bin/telemetry.rs` — standalone telemetry receiver (`receive`, shared-token gated
  for non-loopback binds) + weekly-active aggregator (`weekly`). See `docs/telemetry.md`.

### src-tauri

- `src-tauri/src/main.rs` — Tauri 2 main; boots the WebView2 shell on the bundled UI.
- `src-tauri/build.rs` — tauri-build 2 build script.
- `src-tauri/tauri.conf.json` — Tauri config (window, updater, single-instance, dialog).
- `src-tauri/capabilities/` + `src-tauri/permissions/` — Tauri capability/permission scoping.
- `src-tauri/icons/` — bundled icons.
- `src-tauri/DEPLOY.md` — signing key + release process.

### UI + bundled resources

- `web/` — vanilla HTML/CSS/JS bento dashboard (`app.js`, `index.html`, `styles.css`, `fonts/`,
  `fonts.css`). No build step, no framework.
- `.pi/` — bundled agent resources: `agents/`, `design-skills/`, `design-systems/`, `prompts/`.
- `docs/api-contract.md` — authoritative UI↔backend contract.
- `docs/design-prompt.md` — UI spec.

## Memory

`memory.rs` + `embed.rs` form the **on-device memory store**: `rusqlite` (bundled) for the vector
index + `ort` 2.0.0-rc.12 (download-binaries) running the all-MiniLM-L6-v2 ONNX model in-process
via `tokenizers` (onig). **NOT mem0, NOT transformers.js, NOT better-sqlite3.** Embeddings never
leave the machine; there is no embeddings API or remote route.

The git-committable **source of truth is `MEMORY.md`** (global `~/.dotz/ai-agents/` + project
`<cwd>/.ai-agents/`), regenerated on every write — the sqlite vector index is a **derived cache**;
don't hand-edit it. Scope is partitioned (`__global__` vs `proj:<cwd>`); typed-memory `category`,
`folder`, and a timestamp live in sqlite metadata.

**Memory is AUTONOMOUS** (the operator never manages it): pre-task recall (semantic search of
folder + global memory, relevance-thresholded, recency-boosted) is injected into the turn, and
durable facts are auto-captured from each exchange. AGENTS.md files (project root + global
`~/.config/opencode/AGENTS.md`) remain the **doctrine** layer — read via `projects::agents_md`,
written via the `agents_md` tool; `memory.rs` is the **knowledge** layer. Don't blur them. Both
persist across restarts.

## Multi-provider models

The model surface is **multi-provider**, not just Ollama. Providers: **Ollama Cloud** (primary:
executive `glm-5.2`, subagent `minimax-m3`), OpenRouter (free fallback `nex-agi/nex-n2-pro:free`),
Anthropic, OpenAI, Google, Groq, Mistral, xAI, DeepSeek, Cohere, NVIDIA NIM, and Local. Ollama,
OpenRouter, and Local are **free-form model-id inputs** (not dropdowns); `resolveModel` clones any
same-provider template for unknown ids. Auth is resolved via `~/.pi/agent/auth.json` → env vars.

**Automatic task distribution**: `LOW_COST_MODELS` (in `types.rs`) lists the low-cost sub-models
per provider (Ollama `minimax-m3`; OpenRouter `nex-agi/nex-n2-pro:free`). This list is injected
into the system prompt, and the `subagent` tool accepts a `model` override (format
`provider/model-id`). The main agent (the high-quality orchestrator, `glm-5.2`) selects a sub-model
per task from this list, keeping cost down while maximizing throughput.

## Workflow domain

`workflows.rs` (`WorkflowRun` DAG, `WorkflowStep`, `ToolCallRef` — the latter now carries
`args`+`result` so graph nodes are fully inspectable) + `workflow_executor.rs` (drives steps) +
`agent/subagent.rs` (runs each subagent as an isolated LLM run and emits `step_tool` +
`step_thinking` events onto the workflow channel) make **the graph the single source of truth** —
execution and observability read the same nodes. `run_record.rs` captures reproducible records;
`checkpoint.rs` does git diff/restore. Steps transition
`pending → ready → running → done|error|skipped`; children auto-promote to `ready` when all
parents are `done`. The UI renders the live DAG as an interactive SVG node/edge graph.

## Sandbox + browser

`sandbox.rs` runs code in `terminal` mode (stream stdout/stderr back) or `web` mode (long-lived
process bound to a local HTTP port). In `web` mode the UI renders an inline preview iframe at the
published port, and the agent drives an **agent cursor** over it via `sandbox.cursor` WS messages
(`move`/`click`/`type` at `(x, y)`); the server emits `sandbox_cursor` events back so agent and
user see the same pointer state. Run lifecycle: `pending → running → done|error|killed`.

`browser.rs` is the **isolated `agent-browser` controller**: it spawns the pinned external binary
in a disposable worker with a throwaway `mkdtemp` profile (never the user's personal Chrome
profile) and an explicit origin allowlist enforced on every navigation. Remote pages never run
inside dotz's Tauri shell and never receive its preload. Frames are served as JPEG bytes on the
frame endpoint (never embedded in the JSON event stream). The `agent-browser` binary is shipped via
`npm install` + `npm run fetch-model` (same step that fetches the ONNX model).

## One-liners

- `projects.rs` — persistent named workspaces (cwd + profile + model defaults).
- `profiles.rs` — profile loader (doctrine + skill index + project memory injection).
- `skills.rs` — unified skills loader across opencode/claude/codex/ecc/superpowers/hermes + `.pi/` pools.
- `specs.rs` — OpenSpec-style spec management.
- `living_docs.rs` — living-docs generation/maintenance.
- `templates.rs` — prompt + resource templates.
- `connections.rs` — local GitHub/Vercel/Neon logins via provider CLIs (no OAuth app, no stored secrets).
- `leaderboard.rs` — model/provider leaderboard surface.
- `self_eval.rs` — eval harness (drives the `selfeval` bin).
- `verify.rs` — verification gate helpers.
- `vcs.rs` — git/vcs helpers.

## Verification commands

```bash
cargo test -p dotz-core                # the test gate — 526 tests (the Solomon RSI lane runs this)
cargo run -p dotz-core --bin serve      # headless backend on http://127.0.0.1:4317
cargo tauri dev                         # native desktop window (WebView2)
cargo tauri build                       # → src-tauri/target/release/bundle/nsis/ (signed installer + latest.json)
npm install && npm run fetch-model      # ship agent-browser binary + fetch all-MiniLM-L6-v2 ONNX into assets/models/
```

CI (`.github/workflows/ci.yml`) runs on `windows-latest`: `cargo fmt --all -- --check`,
`cargo clippy -p dotz-core --all-targets -- -D warnings`, `cargo test -p dotz-core`.

**LLVM OOM is not a code failure.** On the 16 GB dev host with the live fleet resident, an
unbounded build/test run can die of memory pressure — LLVM OOM / "paging file too small" /
exit 1455 / `STATUS_STACK_BUFFER_OVERRUN` — and the wreckage masquerades as compile errors
inside crates.io deps (E0460/E0786/E0463 cascades). `.cargo/config.toml` pins `build.jobs = 2`
to prevent it; also bound test parallelism (`cargo test -- --test-threads=2`). If one of those
signatures still appears, re-run the same command once bounded before treating the gate as red —
only a reproducible second failure is code-red.

## Provider config

dotz uses normal auth resolution (`~/.pi/agent/auth.json` → env vars). Provide a working provider
key, e.g. `OLLAMA_API_KEY` (primary) or `OPENROUTER_API_KEY` (free fallback). See `.env.example`.
OpenRouter + Ollama + Local are **free-form model-id inputs** (not dropdowns). Port via
`DOTZ_PORT` (default 4317); extra skill pools via `DOTZ_SKILLS_PATHS`. Use `:free` models when the
balance is low (provider errors surface in-chat). The low-cost sub-model list (Ollama `minimax-m3`,
OpenRouter `nex-agi/nex-n2-pro:free`) is injected into the system prompt for automatic task
distribution.

## Known caveats

- Fonts load from Google Fonts (online). Bundle locally for fully-offline use.
- Subagents run in dotz's native runtime as **separate LLM runs** (not separate processes, not the
  pi SDK). The bundled `.pi/agents/*.md` default to the free model so `/implement` is runnable out
  of the box.

## Conventions

- The four control knobs are the product surface; don't add new ones without intent.
- Keep the backend lean (**axum + tokio only**). The UI is vanilla JS — no build step, no framework.
- `workflows.rs` + `workflow_executor.rs` + `agent/subagent.rs` together own the workflow graph.
  The graph is the single source of truth — execution and observability read the same `WorkflowStep`
  nodes via `step_tool` + `step_thinking` events. Keep `subagent.rs` the sole emitter of those
  events; don't add a second bridge or synthesizer.
- `run_record.rs` + `checkpoint.rs` capture reproducible records and git diff/restore. Don't bypass
  them for workflow replay.
- `sandbox.rs` owns process lifecycle for both `terminal` and `web` runs; never spawn sandbox
  processes directly from `server`. The agent cursor is a UI overlay driven by `sandbox_cursor`
  WS events — both sides (agent send + UI render) must consume the same event shape.
- `browser.rs` owns the isolated `agent-browser` controller. Remote pages never run inside dotz's
  Tauri shell and never receive its preload — each session spawns the external `agent-browser`
  binary with a throwaway `mkdtemp` profile and an explicit origin allowlist.
- `memory.rs` is the on-device memory store (rusqlite + ort ONNX). It has two intentional injection
  paths: a build-time **seed** via the profile loader, and live **per-turn recall**. The `MEMORY.md`
  mirror is the git-committable source of truth — don't hand-edit the sqlite vector index; it's a
  derived cache.
- `skills.rs` is the single skill-discovery path. Don't add a second skill loader.
- **Don't reintroduce the Electron/TypeScript/pi-SDK/mem0 stack — it was removed 2026-06-24.** No
  Fastify, no `src/pi.ts`, no `src/*.ts`, no electron-builder, no transformers.js, no better-sqlite3.
  The native-Rust workspace is the product.
- **Frontend panels use class-scoped selectors or per-panel `querySelector` lookups.** Because
  panel templates are cloned, avoid global `getElementById` for panel internals that could appear
  twice.

## Code style — ponytail

Follow `.claude/skills/ponytail` (vendored MIT skill): YAGNI, stdlib first, native platform
features before dependencies, one line over fifty, shortest working diff, deletion over addition.
Never simplify away input validation at trust boundaries, error/data-loss handling, security,
accessibility, or tests. Mark a deliberate shortcut with a `# ponytail:` comment naming its
ceiling and the upgrade path. Commands: `/ponytail-review` (flag over-engineering in a diff),
`/ponytail-audit` (scan the repo).