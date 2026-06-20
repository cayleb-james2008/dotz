# dotz UI ↔ backend contract

The chat UI is a static SPA in `web/` served by the dotz server at `http://127.0.0.1:4317`.
It talks to the backend with **REST** for controls and a **WebSocket** for the live stream.
This is the contract the claude.ai/design artifact binds to.

## Lifecycle

1. `POST /api/sessions` → `{ sessionId, model, thinkingLevel, supportsThinking, availableThinkingLevels, tools }`
2. Open `ws://127.0.0.1:4317/ws?sessionId=<id>` → receive `{kind:"ready"}`, then a stream of `{kind:"event"}`.
3. Send prompts over the WS; adjust model/thinking/tools over REST.

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
| DELETE | `/api/projects/:id` | — | `{ ok }` |
| GET | `/api/projects/:id/files` | — | `{ tree: FileTreeNode[] }` |

`Project = { id, name, cwd, profileId, model:ModelRef, thinkingLevel, createdAt, updatedAt }`.
`FileTreeNode = { path, type:"file"|"dir", children?:FileTreeNode[] }`. The tree is recursive to a
maximum depth of 3 and skips `node_modules` and `.git`. Paths are absolute on the server.
A project is a **persistent named workspace** (cwd + profile + model + thinking defaults) that
survives server restarts. Sessions created with `projectId` inherit the project's `cwd`,
`profileId`, `model`, and `thinkingLevel` unless overridden on `POST /api/sessions`.

### Memory (mem0-backed, autonomous)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/memory?projectId=<id>` | — | `{ entries: MemoryView[] }` |
| POST | `/api/memory` | `{ projectId?, text, category?, folder?, scope? }` | `MemoryView` |
| PATCH | `/api/memory/:id` | `{ text, projectId? }` | `MemoryView` |
| DELETE | `/api/memory/:id?projectId=<id>` | — | `{ ok }` |
| POST | `/api/memory/search` | `{ query, projectId?, threshold?, topK?, folder?, scope?, category? }` | `{ results: MemoryView[] }` |
| POST | `/api/memory/consolidate` | `{ projectId? }` | `{ removed, kept }` |
| GET | `/api/memory/graph?projectId=<id>` | — | `{ global, project }` graph (`{nodes,edges}`) |

`MemoryView = { id, memory, scope:"project"|"global", category?, folder?, score?, createdAt?, updatedAt? }`
(`score` only on search/recall results). Memory is backed by **mem0** (self-hosted OSS, on-device):
the sqlite vector store + bundled local embedder live under `~/.dotz/ai-agents/mem0/`; the LLM that
powers extraction/consolidation is dotz's Ollama Cloud chat. Storage is partitioned by scope
(`"project"` by folder, `"global"` everywhere). A human-readable, git-committable `MEMORY.md` mirror
is written per scope (global `~/.dotz/ai-agents/MEMORY.md`, project `<cwd>/.ai-agents/MEMORY.md`) and
is the source of truth — the vector index is a derived cache.

Capture, update, consolidation, and recall are **automatic** (no operator action): the dotz-tools
`before_agent_start` hook injects the most relevant memories into each turn (broadcast to the UI as a
`{kind:"memory_recall", items}` WS event for observability), and the `agent_end` hook extracts
durable facts from the completed exchange. Legacy `~/.dotz/ai-agents/memory.json` + project
`memory.json` are imported once on first run (the JSON files are kept as a backup).

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
`{ kind:"event", sessionId, event }` where `event` is a pi AgentSession event |
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

`WorkflowStep = { id, agent, task, status:"pending"|"ready"|"running"|"done"|"error"|"skipped", parents, children, batch?, output?, error?, usage?, sandboxRunId?, browserSessionId?, toolCallIds?, thinking?, startedAt?, endedAt? }`.

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
