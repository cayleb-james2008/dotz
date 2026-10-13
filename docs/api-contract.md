# dotz UI ↔ backend contract

The chat UI is a static SPA in `web/` served by the dotz server at `http://127.0.0.1:4317`.
It talks to the backend with **REST** for controls and a **WebSocket** for the live stream.
I keep the UI and backend event shapes here so changes can be checked on both sides.
This is a source contract, not a receipt that every route or native journey has been exercised.

## Lifecycle

1. `POST /api/sessions` → `{ sessionId, model, thinkingLevel, supportsThinking, availableThinkingLevels, tools }`
2. Open `ws://127.0.0.1:4317/ws?sessionId=<id>` → receive `{kind:"ready"}`, then a stream of `{kind:"event"}`.
3. Send prompts over the WS; adjust model/thinking/tools over REST.

The server rejects disallowed `Origin`/`Host` headers with `403`. When a session token is configured,
all `/api/*` routes except `/api/health` and the `/ws` upgrade require `x-dotz-token` or a `token`
query parameter; missing/wrong tokens return `401`. The Tauri shell injects its per-process token
out of band. See `dotz-core/src/server/guard.rs`; origin checks alone do not authenticate local
processes, and disabling authentication is not a fix for a failed diagnostic.

## REST

### Sessions + controls

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/health` | — | `{ ok, sessions }` |
| GET | `/api/profiles` | — | `{ profiles: Profile[], default: "workflow" }` |
| POST | `/api/sessions` | `{ cwd?, model?:{provider,modelId}, thinkingLevel?, tools?:string[], profileId?, projectId? }` | session summary |
| GET | `/api/sessions` | — | session summary[] |
| GET | `/api/sessions/:id` | — | summary + `{ stats }` |
| DELETE | `/api/sessions/:id` | — | `{ ok }` |
| GET | `/api/sessions/:id/models` | — | `{ current, default, providers:string[], providerMeta:ProviderMeta[], available:ModelInfo[] }` |
| POST | `/api/sessions/:id/model` | `{ provider, modelId }` | session summary |
| POST | `/api/sessions/:id/thinking` | `{ level }` | `{ thinkingLevel, supportsThinking, availableThinkingLevels }` |
| GET | `/api/sessions/:id/tools` | — | `{ active:string[], all:string[] }` |
| POST | `/api/sessions/:id/tools` | `{ tools:string[] }` | `{ active, all }` |
| GET | `/api/sessions/:id/commands` | — | `{ commands:{name,description,source}[] }` |
| POST | `/api/sessions/:id/abort` | — | `{ ok }` |
| GET | `/api/providers` | — | `{ providers: ProviderMeta[] }` |

`ModelInfo = { provider, modelId, name, reasoning, contextWindow? }`.
`ProviderMeta = { id, label, freeForm? }` — per-provider UI hints (`freeForm:true` means render as a
free-form model-id input rather than a fixed dropdown, e.g. OpenRouter).
`Profile = { id, name, tagline, workflow, thinkingLevel, model:ModelRef }`.
Session summary `= { sessionId, profileId:string|null, projectId:string|null, model:ModelInfo|null, thinkingLevel, supportsThinking, availableThinkingLevels:string[], tools:string[] }`.
`GET /api/sessions/:id/models` additionally returns `providerMeta:ProviderMeta[]` alongside
`providers`, `available`, `current`, and `default` so the model picker can render provider-aware UI
(multi-provider, not just OpenRouter).

### Projects

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/projects` | — | `{ projects: Project[] }` |
| POST | `/api/projects` | `{ name, cwd, profileId?, model?:ModelRef, thinkingLevel? }` | `Project` |
| GET | `/api/projects/:id` | — | `Project` |
| PATCH | `/api/projects/:id` | partial `Project` | `Project` |
| DELETE | `/api/projects/:id` | — | `{ ok, purged:{ memories, workflows, sessions } }` |
| GET | `/api/projects/:id/files` | — | `{ tree: FileTreeNode[] }` |

`Project = { id, name, cwd, profileId, model:ModelRef, thinkingLevel, createdAt, updatedAt }`.
`FileTreeNode = { path, type:"file"|"dir", children?:FileTreeNode[] }`. The tree is recursive to a
maximum depth of 3 and skips `node_modules` and `.git`. Paths are absolute on the server.
**DELETE cascades a purge of the project's dotz-side state** (all under `~/.dotz`): project-scoped
on-device memories, its workflow runs + run-records, and any live sessions. It **never touches the
project folder on disk** — `<cwd>/.ai-agents/MEMORY.md` and every file under `cwd` are left intact.

A project is a **persistent named workspace** (cwd + profile + model + thinking defaults) that
survives server restarts. Sessions created with `projectId` inherit the project's `cwd`,
`profileId`, `model`, and `thinkingLevel` unless overridden on `POST /api/sessions`.

### Memory (local storage and embeddings, best-effort capture)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/memory?projectId=<id>` | — | `{ entries: MemoryView[] }` |
| POST | `/api/memory` | `{ projectId?, text, category?, folder?, scope? }` | `MemoryView` |
| PATCH | `/api/memory/:id` | `{ text, projectId? }` | `MemoryView` |
| DELETE | `/api/memory/:id?projectId=<id>` | — | `{ ok }` |
| POST | `/api/memory/search` | `{ query, projectId?, threshold?, topK?, folder?, scope?, category? }` | `{ results: MemoryView[] }` |
| POST | `/api/memory/consolidate` | `{ projectId? }` | `{ removed, kept }` |

`MemoryView = { id, memory, scope:"project"|"global", category?, folder?, score?, createdAt?, updatedAt? }`
(`score` only on search/recall results). **Storage and sentence embeddings are local:** bundled
SQLite (`rusqlite`) plus in-process `ort` ONNX all-MiniLM-L6-v2 embeddings (384 dimensions, no
embeddings API). The database is `<DOTZ_CONFIG_DIR>/ai-agents/memory.db`, defaulting to
`~/.dotz/ai-agents/memory.db`. Model files are separate under `assets/models/` (or the configured
asset/model location); this is not a mem0 directory or service.

Storage is partitioned by global scope (`__global__`) or normalized project cwd (`proj:<cwd>`),
with optional folder/category filters. Human-readable `MEMORY.md` mirrors live at
`<DOTZ_CONFIG_DIR>/ai-agents/MEMORY.md` and `<cwd>/.ai-agents/MEMORY.md`; the default global path is
`~/.dotz/ai-agents/MEMORY.md`. The mirror is the committable source of truth; the vector index is a
derived cache. The API above supports explicit inspection, edits, search, and consolidation.

The native Rust session loop (`dotz-core/src/agent/session.rs`) calls `memory::recall_async` before
a turn and schedules `memory::capture_exchange` after a completed exchange. These are best-effort
paths, not the old `before_agent_start`/`agent_end` extension hooks or a promise of useful capture.
Near-duplicate pruning/consolidation runs locally.

**Extraction can send conversation text off-device.** It uses an OpenAI-compatible chat endpoint:
`DOTZ_MEMORY_BASE_URL` (default `https://ollama.com/v1`), `DOTZ_MEMORY_MODEL` (configured executive
model, otherwise `glm-5.2`), and a non-empty `DOTZ_MEMORY_API_KEY`, otherwise the resolved
`OLLAMA_API_KEY`. Missing credentials, transport/parse errors, or failed embedding/add operations
can produce no captured facts. `DOTZ_MEMORY_TIMEOUT_MS` controls the extraction timeout (30 seconds
by default, clamped to 1 second–5 minutes).

If `DOTZ_COGNEE_URL` is configured, `memory.rs` can augment recall with Cognee results and forward
captured facts to it; queries and facts then cross that service boundary too. Cognee is optional
and has its own service/auth configuration. Local storage/embeddings therefore do not imply an
offline application or that exchanges never leave the device. See
[provider setup](provider-setup.md#memory-endpoints-and-privacy) before using sensitive text.

### AGENTS.md doctrine editor

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/agents_md?projectId=<id>` | — | `{ content, path }` |
| PATCH | `/api/agents_md?projectId=<id>` | `{ content: string }` | `{ ok, content, path }` |

Reads or overwrites the project's root `AGENTS.md` doctrine file. `content` is the full file
body (plain Markdown). The UI's DOCTRINE panel edits this directly; changes take effect on the
next session context reload because AGENTS.md is read at session-build time.

### Workflow templates (user-editable presets)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/templates` | — | `{ templates: TemplateMeta[] }` |
| GET | `/api/templates/:id` | — | `Template` |
| POST | `/api/templates` | `{ name, body, description?, tags? }` | `{ template: Template }` |
| PATCH | `/api/templates/:id` | partial `{ name?, body?, description?, tags? }` | `{ template: Template }` |
| DELETE | `/api/templates/:id` | — | `{ ok }` |
| POST | `/api/templates/:id/fork` | `{ name? }` | `{ template: Template }` |
| POST | `/api/templates/:id/run` | `{ sessionId, args? }` | `{ ok }` |

`Template = { id, name, description, body, source:"bundled"|"user", origin?, tags?, createdAt, updatedAt }`.
`TemplateMeta = { id, name, description, source, origin?, tags?, hasArgs, updatedAt }`.
Bundled presets ship in `.pi/prompts/` (the 6 workflow slash commands). User templates live under
`~/.dotz/ai-agents/templates/` and shadow bundled presets by id. Fork copies a bundled preset to
 the user store so it can be edited. `run` expands `$@` with `args` and sends the result as a
prompt to the live session — equivalent to typing the slash command in the composer.

### Spec-driven workflow (OpenSpec-compatible)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/specs/status?projectId=<id>` | - | `{ cwd, openspecDir, changes, active, blocked, native, cli }` |
| GET | `/api/specs/changes?projectId=<id>` | - | `{ changes: SpecChange[] }` |
| POST | `/api/specs/changes` | `{ projectId?, title, description?, slug? }` | `{ change: SpecChange }` |
| GET | `/api/specs/changes/:id?projectId=<id>` | - | `{ change: SpecChange }` |
| PATCH | `/api/specs/changes/:id` | `{ projectId?, title?, description?, proposal?, design?, tasks?, readiness?, spec? }` | `{ change }` |
| POST | `/api/specs/changes/:id/apply` | `{ projectId? }` | `{ change, tasks, message }` |
| POST | `/api/specs/changes/:id/verify` | `{ projectId? }` | `{ ok, change, missingArtifacts, pendingReadiness, incompleteTasks }` |
| POST | `/api/specs/changes/:id/sync` | `{ projectId? }` | `{ ok, copied, change }` |
| POST | `/api/specs/changes/:id/archive` | `{ projectId? }` | `{ ok, archivedPath, change }` |

`SpecChange = { id, title, description, status, path, artifacts:SpecArtifact[], readiness:ReadinessFinding[], createdAt, updatedAt, archivedAt? }`.
Changes are stored under `<cwd>/openspec/changes/<slug>/` with `proposal.md`, `design.md`,
`tasks.md`, `specs/`, and dotz's `readiness.md`. `sync` copies change-local specs into
`<cwd>/openspec/specs/`; `archive` moves the change under `openspec/changes/archive/`.

### Living docs

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/living-docs?projectId=<id>&scope=project|global` | - | `{ scope, docs, suggestions }` |
| PATCH | `/api/living-docs?projectId=<id>&scope=project|global` | `{ kind, content }` | `{ doc, docs }` |
| POST | `/api/living-docs/suggestions/:id/accept?projectId=<id>&scope=project|global` | - | `{ ok, suggestion, docs, suggestions }` |
| POST | `/api/living-docs/suggestions/:id/reject?projectId=<id>&scope=project|global` | - | `{ ok, suggestion, docs, suggestions }` |

`kind` is `anti_patterns`, `non_inferables`, `context_scope`, or `living_docs`. Project docs live in
`<cwd>/.ai-agents/`; global docs live in `~/.dotz/ai-agents/`. The agent prompt receives compact
summaries beside memory. Explicit high-confidence `agent_end` labels append automatically;
ambiguous labels become suggestions for the LIVING DOCS panel.

### VCS

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/vcs/status?projectId=<id>` | - | `{ status: VcsStatus }` |
| POST | `/api/vcs/branch` | `{ projectId?, slug?, name? }` | `{ ok, branch, reused, status }` |
| POST | `/api/vcs/commit` | `{ projectId?, message, body?, files? }` | `{ ok, commitId, status }` |
| POST | `/api/vcs/pr` | `{ projectId?, title?, body?, base?, draft? }` | `{ ok, url }` |
| POST | `/api/vcs/rollback` | `{ projectId?, target, mode:"checkpoint"|"revert"|"reset", confirm? }` | `{ ok, mode, target, status? }` |

`branch` creates/reuses `dotz/<slug>` unless `name` is already a full branch name. `commit` is an
atomic logical commit and stages `files` or all changes when `files` is omitted. `pr` uses `gh` only
when the GitHub CLI is installed and logged in. `reset` rollback requires `confirm:true`.

### Sandbox (visual web preview + agent cursor)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/sandbox/languages` | — | `{ languages: string[] }` |
| GET | `/api/sandbox/runs` | — | `{ runs: SandboxRun[] }` |
| GET | `/api/sandbox/runs/:id` | — | `SandboxRun` |
| POST | `/api/sandbox/runs` | `{ projectId?, language, code, timeoutMs?, mode? }` | `SandboxRun` |
| POST | `/api/sandbox/runs/:id/kill` | — | `{ ok }` |
| GET | `/api/sandbox/runs/:id/port` | — | `{ port }` (404 if no web port) |

### Isolated interactive browser

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/browser/state?sessionId?` | — | `{ available, observation, sessions }` |
| GET | `/api/browser/frame?sessionId&afterSeq?` | — | JPEG bytes (`204` when no newer frame) |
| POST | `/api/browser/start` | `{ projectId, url, allowedOrigins?, viewport?, workflowId?, stepId? }` | `BrowserObservation` |
| POST | `/api/browser/act` | `BrowserActInput` | next `BrowserObservation` (`409` for stale sequence-bound actions) |
| POST | `/api/browser/stop` | `{ sessionId }` | stopped `BrowserObservation` |

`BrowserActInput.action` is one of `navigate`, `observe`, `back`, `forward`, `reload`, `click`,
`clickAt`, `type`, `key`, `select`, `scroll`, or `wait`. Ref actions use `targetRef` + `expectedSeq`;
user frame clicks use viewport `x` / `y` + `expectedSeq`; focused typing uses `text` +
`expectedSeq`. Remote pages run in the pinned `agent-browser` worker with a disposable profile and
origin allowlist. Raw page evaluation, uploads, downloads, and clipboard access are not exposed.

`BrowserObservation` carries a monotonically increasing `seq`, the page URL/title/viewport,
interactive refs, current action/cursor, error counters, and frame metadata. The binary JPEG stays
on `/api/browser/frame`; it is never embedded in the JSON event stream.

**Candidate lifecycle boundary (2026-10-10):** a persistent browser daemon must outlive individual
commands; the sandbox's per-run Linux PID namespace does not by itself contain that daemon.
Persistent ownership/teardown repairs are under implementation and review. A stopped observation
is not an independently verified assertion that a reparented daemon, listener, and profile are
gone; require exact-source teardown receipts before treating that cleanup gap as resolved.

### Local connections (provider CLI browser login)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/connections` | — | `{ connections: ConnectionStatus[] }` |
| POST | `/api/connections/:provider/login` | — | `LoginState` (spawns the CLI browser login) |
| GET | `/api/connections/:provider/login` | — | `LoginState` (poll the streamed output) |
| POST | `/api/connections/:provider/logout` | — | `{ ok, output }` |

`:provider` is `github` \| `vercel` \| `neon`. `ConnectionStatus = { id, label, cli, installed, loggedIn, account?, hint? }`
is read from each provider's local auth (`gh` / `vercel` via their CLI; Neon via its neonctl `credentials.json`,
which `npx neonctl auth` writes — neonctl has no logout command, so Neon logout deletes that file).
`LoginState = { provider, running, exitCode, output }`
— `output` is the CLI's device-code / URL prompt streamed for the user to finish in a normal browser tab.
There is no OAuth app registration and no client secret; dotz never reads, stores, or logs the token.

`SandboxRun = { id, projectId, language, code, status:"pending"|"running"|"done"|"error"|"killed", output, exitCode, startedAt, endedAt }`.
`mode:"terminal"` runs the code as a plain process (stdout/stderr streamed back). `mode:"web"`
starts a long-lived process bound to a local port and returns that port via `.../port` and the
`sandbox_port` WS event, so the UI can render an inline web preview iframe while the agent drives a
cursor over it.

## WebSocket

**Client → server:** `{ kind:"prompt"|"steer"|"followUp"|"abort", text? }`
(during streaming, a `prompt` is auto-queued as a follow-up). Human-gate and sandbox control messages:

- `{ kind:"sandbox.start", language, code, mode:"terminal"|"web", projectId?, timeoutMs? }`
  — start a sandbox run (equivalent to `POST /api/sandbox/runs`).
- `{ kind:"sandbox.kill", runId }` — terminate a running sandbox.
- `{ kind:"sandbox.cursor", runId, x, y, action:"move"|"click"|"type", cursorText? }` — drive the
  agent cursor over a `mode:"web"` run's preview (move, click, or type at `(x, y)`).
- `{ kind:"gate.approve", gateId, feedback? }` / `{ kind:"gate.reject", gateId, feedback? }` —
  approve or reject a pending `human_gate` request.

**Server → client:** `{ kind:"ready", sessionId }` | `{ kind:"error", error }` |
`{ kind:"event", sessionId, event }` where `event` is a native Rust agent event |
`{ kind:"sandbox", sessionId, event }` where `event` is a sandbox event (below) |
`{ kind:"workflow", sessionId, runId, event }` where `event` is a workflow event |
`{ kind:"gate", gateId, plan }` where `plan` is the human-gate plan text.

### Sandbox event types (server → client, inside `{ kind:"sandbox", sessionId, event }`)

- `sandbox_start` — `{ type, runId, run:SandboxRun }` (run created/started).
- `sandbox_output` — `{ type, runId, stream:"stdout"|"stderr", line }` (streamed process output).
- `sandbox_port` — `{ type, runId, port }` (a `mode:"web"` run published its HTTP port → render the
  preview iframe at `http://127.0.0.1:<port>`).
- `sandbox_cursor` — `{ type, runId, x, y, action, text? }` (the agent moved/clicked/typed; render
  the cursor overlay at `(x, y)`).
- `sandbox_end` — `{ type, runId, run:SandboxRun }` (run finished: `status` is `done`/`error`/`killed`).

### Event types to render

- `agent_start` / `agent_end` — turn boundaries (show/clear the streaming spinner).
- `message_start` / `message_end` — `{ message }`. The assistant `message_end` carries the
  final message; **on error**, `message.stopReason === "error"` and `message.errorMessage` is set.
- `message_update` — `{ assistantMessageEvent }`. The streaming spine. Sub-types:
  `text_start|delta|end`, `thinking_start|delta|end`, `toolcall_start|delta|end`, `done`, `error`.
  **Each carries `partial` = the full current assistant message** with
  `content: Block[]`, `usage`, `model`. **Render strategy: on every `message_update`, replace the
  active assistant bubble's content from `assistantMessageEvent.partial.content`** — no manual
  delta accumulation needed.
- `tool_execution_start` — `{ toolCallId, toolName, args }` (open a tool card).
- `tool_execution_update` — `{ toolCallId, partialResult }` (cumulative — replace the card body).
- `tool_execution_end` — `{ toolCallId, toolName, result:{content:[{type:"text",text}]}, isError }`.
- `queue_update` — `{ steering:string[], followUp:string[] }` (pending-message badges).
- `thinking_level_changed` / `session_info_changed` — reflect control state.

### Workflow events (server → client, inside `{ kind:"workflow", sessionId, runId, event }`)

- `workflow_start` — `{ type, run:WorkflowRun }`.
- `workflow_end` — `{ type, run:WorkflowRun }`.
- `step_added` — `{ type, step:WorkflowStep }`.
- `step_state` — `{ type, stepId, status, output?, error?, usage?, sandboxRunId?, browserSessionId?, toolCallIds?, thinking? }`.

`WorkflowStep = { id, agent, task, status:"pending"|"ready"|"running"|"done"|"error"|"skipped", parents, children, output?, error?, usage?, sandboxRunId?, browserSessionId?, toolCallIds?, thinking?, specChangeId?, specTaskId?, commitId?, readinessStatus?, rollbackTarget?, startedAt?, endedAt? }`.

### Gate events (server ↔ client, human approval)

- Server → client: `{ kind:"gate", gateId, plan }` — emitted when the agent calls the `human_gate`
  tool. The UI should render an approval card showing `plan` and buttons to approve/reject.
- Client → server: `{ kind:"gate.approve", gateId, feedback? }` or
  `{ kind:"gate.reject", gateId, feedback? }` — resolves the awaiting `human_gate` call. Optional
  `feedback` is passed back to the agent as the user's response.

### Content block shapes (in `message.content` / `partial.content`)

- `{ type:"text", text }`
- `{ type:"thinking", thinking, thinkingSignature }`
- `{ type:"toolCall", id, name, arguments }`

## Control knobs (what the UI exposes)

1. **Model picker.** From `GET .../models` (now also returns `providerMeta:ProviderMeta[]`) and
   `GET /api/providers`. The surface is **multi-provider** — each provider has its own input mode:
   OpenRouter is a **free-form model-id input** (text field, default
   `nex-agi/nex-n2-pro:free`) while other providers may expose a fixed model list; `ProviderMeta.freeForm`
   tells the UI which to render. The `available` list powers an optional `<datalist>` of suggestions.
   Selecting → `POST .../model`.
2. **Reasoning slider.** `off → minimal → low → medium → high → xhigh`, constrained to
   `availableThinkingLevels`; disabled when `supportsThinking` is false. → `POST .../thinking`.
3. **Skills / tools panel.** Tools from `GET .../tools` (checkboxes → `POST .../tools`).
   Skills/commands from `GET .../commands` (`source:"skill"`), invoked by sending a
   `{kind:"prompt", text:"/skill:name args"}` over the WS.
4. **Subagent panel.** Commands with `source:"prompt"` (workflow templates like `/implement`)
   and `source:"extension"`; invoked the same way (prompt with the command string).
