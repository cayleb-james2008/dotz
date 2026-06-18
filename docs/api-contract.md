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

`Project = { id, name, cwd, profileId, model:ModelRef, thinkingLevel, createdAt, updatedAt }`.
A project is a **persistent named workspace** (cwd + profile + model + thinking defaults) that
survives server restarts. Sessions created with `projectId` inherit the project's `cwd`,
`profileId`, `model`, and `thinkingLevel` unless overridden on `POST /api/sessions`.

### Memory

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/memory?projectId=<id>` | — | `{ entries: MemoryEntry[] }` |
| POST | `/api/memory` | `{ projectId, key, value, scope? }` | `MemoryEntry` |
| PATCH | `/api/memory/:id` | `{ key?, value?, scope? }` | `MemoryEntry` |
| DELETE | `/api/memory/:id` | — | `{ ok }` |

`MemoryEntry = { id, projectId, key, value, scope:"project"|"global", createdAt, updatedAt }`.
Memory entries (`scope:"project"`) are scoped to a `projectId`; `scope:"global"` entries apply
across all sessions. Entries are injected into the agent's system prompt at session build time via
the project's `buildResourceLoader`, so the agent sees them as durable context without being told
again.

### Sandbox (visual web preview + agent cursor)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/sandbox/languages` | — | `{ languages: string[] }` |
| GET | `/api/sandbox/runs` | — | `{ runs: SandboxRun[] }` |
| GET | `/api/sandbox/runs/:id` | — | `SandboxRun` |
| POST | `/api/sandbox/runs` | `{ projectId?, language, code, timeoutMs?, mode? }` | `SandboxRun` |
| POST | `/api/sandbox/runs/:id/kill` | — | `{ ok }` |
| GET | `/api/sandbox/runs/:id/port` | — | `{ port }` (404 if no web port) |

`SandboxRun = { id, projectId, language, code, status:"pending"|"running"|"done"|"error"|"killed", output, exitCode, startedAt, endedAt }`.
`mode:"terminal"` runs the code as a plain process (stdout/stderr streamed back). `mode:"web"`
starts a long-lived process bound to a local port and returns that port via `.../port` and the
`sandbox_port` WS event, so the UI can render an inline web preview iframe while the agent drives a
cursor over it.

## WebSocket

**Client → server:** `{ kind:"prompt"|"steer"|"followUp"|"abort", text? }`
(during streaming, a `prompt` is auto-queued as a follow-up). Sandbox control messages:

- `{ kind:"sandbox.start", language, code, mode:"terminal"|"web", projectId?, timeoutMs? }`
  — start a sandbox run (equivalent to `POST /api/sandbox/runs`).
- `{ kind:"sandbox.kill", runId }` — terminate a running sandbox.
- `{ kind:"sandbox.cursor", runId, x, y, action:"move"|"click"|"type", cursorText? }` — drive the
  agent cursor over a `mode:"web"` run's preview (move, click, or type at `(x, y)`).

**Server → client:** `{ kind:"ready", sessionId }` | `{ kind:"error", error }` |
`{ kind:"event", sessionId, event }` where `event` is a pi AgentSession event |
`{ kind:"sandbox", sessionId, event }` where `event` is a sandbox event (below).

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
