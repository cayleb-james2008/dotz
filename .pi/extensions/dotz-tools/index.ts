/**
 * dotz built-in tools extension — registers the `skill`, `memory_*`, and `agents_md` tools
 * that give the pi agent access to the unified skill pool, the .ai-agents memory namespace,
 * and project AGENTS.md doctrine files.
 *
 * This extension is loaded via additionalExtensionPaths in profiles.ts, alongside the subagent
 * extension. It uses the same ExtensionAPI.registerTool surface.
 */
import { type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { createUserSkill, skillLoader } from "../../../src/skills";
import { createUserAgent, discoverAgents } from "../subagent/agents";
import { memoryStore, readAgentsMd, writeAgentsMd, appendAgentsMdSection, isMemoryAutonomyEnabled } from "../../../src/memory";
import { projectStore } from "../../../src/projects";
import { captureBaseline, compare, renderBaseline, type MetricsBaseline } from "../../../src/metrics";
import { getDesignSystem, getComponents, auditDesign, renderDesignSystem, renderComponents, renderAudit } from "../../../src/design";
import { browserController, type BrowserActInput, type BrowserStartInput } from "../../../src/browser";

/** Flatten an AgentMessage's content (string or content-part array) to plain text. */
function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) return content.map((p) => (typeof p === "string" ? p : ((p as { text?: string })?.text ?? ""))).join(" ");
  return "";
}
/** Last message text for a given role in a message list (used by post-task auto-capture). */
function lastText(msgs: Array<{ role?: string; content?: unknown }>, role: string): string {
  for (let i = msgs.length - 1; i >= 0; i--) if (msgs[i].role === role) return textOf(msgs[i].content).trim();
  return "";
}

/** In-memory baseline registry (per session) — the RSI brain captures a baseline, then
 *  compares after the improvement. Keyed by a caller-provided label. */
const baselines = new Map<string, MetricsBaseline>();

/** Human-gate listeners — the UI registers a listener via WS; the tool awaits approval. */
type GateResolver = (approved: boolean, feedback?: string) => void;
const gateListeners = new Map<string, GateResolver>();

/** Gate-event listeners — the server registers one to forward gate requests to the WS UI. */
type GateNotify = (gateId: string, plan: string) => void;
let gateNotify: GateNotify | null = null;

/** Register a gate-event listener (called by the server to forward to WS). */
export function onGateRequest(fn: GateNotify): void { gateNotify = fn; }

/** Register a human-gate listener (called by the server when the UI approves/rejects). */
export function resolveHumanGate(gateId: string, approved: boolean, feedback?: string): void {
  const fn = gateListeners.get(gateId);
  if (fn) { fn(approved, feedback); gateListeners.delete(gateId); }
}

export function registerGateListener(gateId: string, fn: GateResolver): void {
  gateListeners.set(gateId, fn);
}

export default function (pi: ExtensionAPI) {
  // The SELECTED PROJECT's working dir. dotz's project tools (agents_md, rsi_baseline, memory_*,
  // list_agents) must operate on the project — NOT on dotz's own `process.cwd()` (where electron
  // launched, e.g. the dotz repo), which made `agents_md` return dotz's own doctrine and ran the gate
  // in the wrong place. The framework's authoritative session cwd arrives on the agent-start hook;
  // capture it and resolve every project tool against it (process.cwd() only as a pre-first-turn
  // fallback). dotz runs one in-process agent at a time, so this module-scoped capture is race-free.
  let sessionCwd: string | null = null;
  const projectCwd = (): string => sessionCwd || process.cwd();
  pi.on("before_agent_start", async (_event, ctx) => { if (ctx?.cwd) sessionCwd = ctx.cwd; });

  // The bound project's configured gate/test command, looked up by cwd — so rsi_baseline/compare can
  // run a NON-Node suite (e.g. a python venv's pytest) instead of the Node defaults. undefined when no
  // project matches or none is configured (captureBaseline then falls back to its Node chain).
  const gateCommandFor = async (cwd: string): Promise<string | undefined> => {
    try {
      const norm = (s: string) => s.replace(/[\\/]+/g, "/").replace(/\/$/, "").toLowerCase();
      const proj = (await projectStore.list()).find((p) => norm(p.cwd) === norm(cwd));
      return proj?.gateCommand?.trim() || undefined;
    } catch { return undefined; }
  };

  // ---- dynamic resources: create specialists and reusable procedures during a workflow ----
  pi.registerTool({
    name: "create_agent",
    label: "Create Agent",
    description: "Create a persistent Pi user agent that is immediately discoverable by the subagent tool. Names are lowercase kebab-case and existing agents are never overwritten.",
    parameters: Type.Object({
      name: Type.String(),
      description: Type.String(),
      systemPrompt: Type.String(),
      tools: Type.Optional(Type.Array(Type.String())),
      model: Type.Optional(Type.String({ description: "Provider/model-id; defaults to ollama/minimax-m3" })),
    }),
    async execute(_id, params) {
      try {
        const agent = createUserAgent(params as Parameters<typeof createUserAgent>[0]);
        return { content: [{ type: "text", text: JSON.stringify(agent) }], details: agent };
      } catch (error) {
        return { content: [{ type: "text", text: (error as Error).message }], isError: true, details: undefined };
      }
    },
  });

  pi.registerTool({
    name: "list_agents",
    label: "List Agents",
    description: "List bundled, user, and project agents available to the subagent tool in the current working directory.",
    parameters: Type.Object({}),
    async execute() {
      const agents = discoverAgents(projectCwd(), "both").agents.map(({ name, description, source, model }) => ({ name, description, source, model }));
      return { content: [{ type: "text", text: JSON.stringify(agents) }], details: agents };
    },
  });

  pi.registerTool({
    name: "create_skill",
    label: "Create Skill",
    description: "Create a persistent dotz skill in the unified skill pool. Names are lowercase kebab-case and existing skills are never overwritten.",
    parameters: Type.Object({ name: Type.String(), description: Type.String(), body: Type.String() }),
    async execute(_id, params) {
      try {
        const skill = await createUserSkill(params as Parameters<typeof createUserSkill>[0]);
        return { content: [{ type: "text", text: JSON.stringify(skill) }], details: skill };
      } catch (error) {
        return { content: [{ type: "text", text: (error as Error).message }], isError: true, details: undefined };
      }
    },
  });

  pi.registerTool({
    name: "list_skills",
    label: "List Skills",
    description: "List every skill in dotz's unified skill pool with its source and one-line description.",
    parameters: Type.Object({}),
    async execute() {
      await skillLoader.load();
      const skills = skillLoader.list().map(({ name, description, source }) => ({ name, description, source }));
      return { content: [{ type: "text", text: JSON.stringify(skills) }], details: skills };
    },
  });

  // ---- monitored browser tools: the complete Pi-facing browser surface ----
  pi.registerTool({
    name: "browser_start",
    label: "Browser Start",
    description: "Start an isolated monitored browser session owned by a Dotz project/workflow. Only http(s) origins on the explicit allowlist are permitted.",
    parameters: Type.Object({
      projectId: Type.String(),
      workflowId: Type.Optional(Type.String()),
      stepId: Type.Optional(Type.String()),
      url: Type.String(),
      allowedOrigins: Type.Optional(Type.Array(Type.String())),
      viewport: Type.Optional(Type.Object({ width: Type.Number(), height: Type.Number() })),
    }),
    async execute(_id, params) {
      try {
        const observation = await browserController.start(params as BrowserStartInput);
        return { content: [{ type: "text", text: JSON.stringify(observation) }], details: observation };
      } catch (error) {
        return { content: [{ type: "text", text: (error as Error).message }], isError: true, details: undefined };
      }
    },
  });

  pi.registerTool({
    name: "browser_act",
    label: "Browser Act",
    description: "Perform one typed browser action and return the next versioned observation. Element refs require their observation sequence; raw evaluation, uploads, downloads, and clipboard access are unavailable.",
    parameters: Type.Object({
      sessionId: Type.String(),
      action: Type.Union([
        Type.Literal("navigate"), Type.Literal("observe"), Type.Literal("back"), Type.Literal("forward"),
        Type.Literal("reload"), Type.Literal("click"), Type.Literal("clickAt"), Type.Literal("type"),
        Type.Literal("key"), Type.Literal("select"), Type.Literal("scroll"), Type.Literal("wait"),
      ]),
      expectedSeq: Type.Optional(Type.Number()),
      url: Type.Optional(Type.String()),
      targetRef: Type.Optional(Type.String()),
      text: Type.Optional(Type.String()),
      x: Type.Optional(Type.Number()),
      y: Type.Optional(Type.Number()),
      key: Type.Optional(Type.String()),
      values: Type.Optional(Type.Array(Type.String())),
      direction: Type.Optional(Type.Union([Type.Literal("up"), Type.Literal("down"), Type.Literal("left"), Type.Literal("right")])),
      pixels: Type.Optional(Type.Number()),
      milliseconds: Type.Optional(Type.Number()),
    }),
    async execute(_id, params) {
      try {
        const observation = await browserController.act(params as BrowserActInput);
        return { content: [{ type: "text", text: JSON.stringify(observation) }], details: observation };
      } catch (error) {
        return { content: [{ type: "text", text: (error as Error).message }], isError: true, details: undefined };
      }
    },
  });

  pi.registerTool({
    name: "browser_stop",
    label: "Browser Stop",
    description: "Stop an isolated browser session and remove its disposable profile and event stream.",
    parameters: Type.Object({ sessionId: Type.String() }),
    async execute(_id, params) {
      try {
        const observation = await browserController.stop((params as { sessionId: string }).sessionId);
        return { content: [{ type: "text", text: JSON.stringify(observation) }], details: observation };
      } catch (error) {
        return { content: [{ type: "text", text: (error as Error).message }], isError: true, details: undefined };
      }
    },
  });

  // ---- skill tool: load a skill's full body by name ----
  pi.registerTool({
    name: "skill",
    label: "Skill",
    description: [
      "Load the full instructions of a named skill from the dotz unified skill pool.",
      "Skills are auto-discovered from opencode, claude, codex, ecc, superpowers, hermes, and bundled .pi pools.",
      "Use this when a task matches a skill in the index (shown in the system prompt).",
      "Returns the skill's markdown body — read it and apply its workflow.",
    ].join(" "),
    parameters: Type.Object({
      name: Type.String({ description: "Exact skill name from the skill index" }),
    }),
    async execute(_id, params) {
      const name = (params as { name: string }).name;
      await skillLoader.load();
      const body = await skillLoader.loadBody(name);
      if (!body) {
        const available = skillLoader.list().slice(0, 20).map((s) => s.name).join(", ");
        return {
          content: [{ type: "text", text: `Skill "${name}" not found. Available (first 20): ${available}` }],
          isError: true,
          details: undefined,
        };
      }
      return { content: [{ type: "text", text: body }], details: undefined };
    },
  });

  // ---- memory tools (mem0-backed). Capture + recall + consolidation are AUTOMATIC via the
  //      before_agent_start / agent_end hooks at the bottom of this file; these tools let the
  //      agent inspect or self-curate memory on demand. ----
  pi.registerTool({
    name: "memory_list",
    label: "Memory List",
    description: "List durable memories. scope='global' for cross-project knowledge, scope='project' for this project/folder. Ids are shown for memory_update/memory_delete.",
    parameters: Type.Object({
      scope: Type.Optional(Type.Union([Type.Literal("project"), Type.Literal("global")])),
    }),
    async execute(_id, params) {
      const p = params as { scope?: "project" | "global" };
      const all = await memoryStore.list(p.scope === "global" ? null : projectCwd());
      const items = p.scope ? all.filter((e) => e.scope === p.scope) : all;
      const text = items.length === 0 ? "No memories found." : items.map((e) => `[${e.scope}${e.category ? "/" + e.category : ""}] (${e.id.slice(0, 8)}) ${e.memory}`).join("\n");
      return { content: [{ type: "text", text }], details: undefined };
    },
  });

  pi.registerTool({
    name: "memory_search",
    label: "Memory Search",
    description: "Semantically recall durable memories relevant to a query (searches this project's folder + global). Use to pull context before acting.",
    parameters: Type.Object({
      query: Type.String({ description: "What to recall" }),
      scope: Type.Optional(Type.Union([Type.Literal("project"), Type.Literal("global")])),
      topK: Type.Optional(Type.Number()),
    }),
    async execute(_id, params) {
      const p = params as { query: string; scope?: "project" | "global"; topK?: number };
      const items = await memoryStore.search(p.query, { projectCwd: projectCwd(), scope: p.scope, topK: p.topK ?? 8 });
      const text = items.length === 0 ? "No relevant memories." : items.map((e) => `[${e.scope}${e.category ? "/" + e.category : ""}] (${(e.score ?? 0).toFixed(2)}) ${e.memory}`).join("\n");
      return { content: [{ type: "text", text }], details: undefined };
    },
  });

  pi.registerTool({
    name: "memory_add",
    label: "Memory Add",
    description: "Save a durable fact now (memory is also captured automatically). scope='global' for cross-project, 'project' for this project. category e.g. convention|architecture|command|gotcha|user|feedback|reference.",
    parameters: Type.Object({
      text: Type.String({ description: "The fact to remember (one concise sentence)" }),
      scope: Type.Optional(Type.Union([Type.Literal("project"), Type.Literal("global")])),
      category: Type.Optional(Type.String({ description: "Memory category (convention, architecture, command, gotcha, user, feedback, reference, ...)" })),
      folder: Type.Optional(Type.String({ description: "Folder this fact is about, relative to the project root" })),
    }),
    async execute(_id, params) {
      const p = params as { text: string; scope?: "project" | "global"; category?: string; folder?: string };
      const scope = p.scope ?? "project";
      const v = await memoryStore.create({ text: p.text, category: p.category, folder: p.folder, scope, projectCwd: scope === "project" ? projectCwd() : null });
      return { content: [{ type: "text", text: `Memory saved: [${v.scope}${v.category ? "/" + v.category : ""}] ${v.memory}` }], details: undefined };
    },
  });

  pi.registerTool({
    name: "memory_update",
    label: "Memory Update",
    description: "Edit an existing memory's text by id (ids shown by memory_list / memory_search). The agent's self-curation of its own memory.",
    parameters: Type.Object({ id: Type.String(), text: Type.String({ description: "Replacement fact" }) }),
    async execute(_id, params) {
      const p = params as { id: string; text: string };
      const v = await memoryStore.update(p.id, p.text, projectCwd());
      return { content: [{ type: "text", text: v ? `Memory updated: ${v.memory}` : "No such memory." }], isError: !v, details: undefined };
    },
  });

  pi.registerTool({
    name: "memory_delete",
    label: "Memory Delete",
    description: "Delete a durable memory by id.",
    parameters: Type.Object({ id: Type.String() }),
    async execute(_id, params) {
      const ok = await memoryStore.remove((params as { id: string }).id, projectCwd());
      return { content: [{ type: "text", text: ok ? "Memory deleted." : "No such memory." }], details: undefined };
    },
  });

  pi.registerTool({
    name: "memory_consolidate",
    label: "Memory Consolidate",
    description: "Merge near-duplicate memories and prune them now (this also runs automatically on a threshold).",
    parameters: Type.Object({}),
    async execute() {
      const r = await memoryStore.consolidate(projectCwd());
      return { content: [{ type: "text", text: `Consolidated: removed ${r.removed} duplicate(s), ${r.kept} kept.` }], details: undefined };
    },
  });

  // ---- agents_md tool: read/update the project AGENTS.md doctrine file ----
  pi.registerTool({
    name: "agents_md",
    label: "AGENTS.md",
    description: [
      "Read or update the project's AGENTS.md doctrine file.",
      "Actions: 'read' (return current content), 'append' (add a ## section), 'write' (replace full content).",
      "AGENTS.md is prose doctrine loaded into the system prompt — use it for durable project rules, conventions, and architecture notes.",
    ].join(" "),
    parameters: Type.Object({
      action: Type.Union([Type.Literal("read"), Type.Literal("append"), Type.Literal("write")]),
      section: Type.Optional(Type.String({ description: "Section heading (for action='append')" })),
      body: Type.Optional(Type.String({ description: "Section body (for append) or full content (for write)" })),
    }),
    async execute(_id, params) {
      const p = params as { action: "read" | "append" | "write"; section?: string; body?: string };
      const cwd = projectCwd();
      if (p.action === "read") {
        const content = await readAgentsMd(cwd);
        return { content: [{ type: "text", text: content || "(no AGENTS.md in project root)" }], details: undefined };
      }
      if (p.action === "append") {
        if (!p.section || !p.body) return { content: [{ type: "text", text: "append requires section and body" }], isError: true, details: undefined };
        await appendAgentsMdSection(cwd, p.section, p.body);
        return { content: [{ type: "text", text: `Appended section '${p.section}' to AGENTS.md` }], details: undefined };
      }
      if (p.action === "write") {
        if (!p.body) return { content: [{ type: "text", text: "write requires body" }], isError: true, details: undefined };
        await writeAgentsMd(cwd, p.body);
        return { content: [{ type: "text", text: "AGENTS.md overwritten." }], details: undefined };
      }
      return { content: [{ type: "text", text: "unknown action" }], isError: true, details: undefined };
    },
  });

  // ---- rsi_baseline tool: capture a verification baseline for the RSI loop ----
  pi.registerTool({
    name: "rsi_baseline",
    label: "RSI Baseline",
    description: [
      "Capture a verification baseline for the recursive self-improvement loop.",
      "Runs typecheck + build + tests in the project cwd and stores the result.",
      "Returns the baseline summary. Use `rsi_compare` after an improvement to prove the needle moved.",
    ].join(" "),
    parameters: Type.Object({
      label: Type.Optional(Type.String({ description: "Label for this baseline (default: 'current')" })),
    }),
    async execute(_id, params) {
      const p = (params as { label?: string }) || {};
      const label = p.label || "current";
      const cwd = projectCwd();
      try {
        const baseline = await captureBaseline(cwd, await gateCommandFor(cwd));
        baselines.set(label, baseline);
        return { content: [{ type: "text", text: renderBaseline(baseline) }], details: undefined };
      } catch (e) {
        return { content: [{ type: "text", text: `baseline failed: ${(e as Error).message}` }], isError: true, details: undefined };
      }
    },
  });

  // ---- rsi_compare tool: re-measure and compare against a baseline ----
  pi.registerTool({
    name: "rsi_compare",
    label: "RSI Compare",
    description: [
      "Re-measure the project and compare against a captured baseline.",
      "Returns a before/after summary showing whether metrics improved, regressed, or stayed flat.",
      "Anti-gaming checks: tests must not be deleted, must still pass.",
    ].join(" "),
    parameters: Type.Object({
      label: Type.Optional(Type.String({ description: "Baseline label to compare against (default: 'current')" })),
    }),
    async execute(_id, params) {
      const p = (params as { label?: string }) || {};
      const label = p.label || "current";
      const baseline = baselines.get(label);
      if (!baseline) return { content: [{ type: "text", text: `no baseline found for label '${label}'. Capture one with rsi_baseline first.` }], isError: true, details: undefined };
      try {
        const result = await compare(baseline, projectCwd(), await gateCommandFor(projectCwd()));
        baselines.delete(label);
        return { content: [{ type: "text", text: `RSI compare result:\n${result.summary}\nFiles: ${result.baseline.fileCount} → ${result.after.fileCount} (${result.fileCountDelta >= 0 ? "+" : ""}${result.fileCountDelta})` }], details: undefined };
      } catch (e) {
        return { content: [{ type: "text", text: `compare failed: ${(e as Error).message}` }], isError: true, details: undefined };
      }
    },
  });

  // ---- human_gate tool: pause for user approval (the RSI Phase 2 gate) ----
  pi.registerTool({
    name: "human_gate",
    label: "Human Gate",
    description: [
      "Pause execution and ask the user for approval before proceeding.",
      "Use this after planning an improvement (RSI Phase 2) and before implementing.",
      "Returns {approved: true} or {approved: false, feedback: '...'}. Do NOT proceed if not approved.",
    ].join(" "),
    parameters: Type.Object({
      plan: Type.String({ description: "The plan to approve (what will be implemented)" }),
      gateId: Type.Optional(Type.String({ description: "Unique gate id (auto-generated if omitted)" })),
    }),
    async execute(_id, params) {
      const p = (params as { plan: string; gateId?: string }) || {};
      const gateId = p.gateId || `gate-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      // Notify the UI (via the server's WS) that a gate is pending.
      try { if (gateNotify) gateNotify(gateId, p.plan); } catch { /* best-effort */ }
      // The UI will call resolveHumanGate(gateId, approved, feedback) via a WS message.
      // We await until that happens. Timeout after 5 minutes.
      const result = await new Promise<{ approved: boolean; feedback?: string }>((resolve) => {
        const timeout = setTimeout(() => { gateListeners.delete(gateId); resolve({ approved: false }); }, 300000);
        registerGateListener(gateId, (ok, fb) => { clearTimeout(timeout); resolve({ approved: ok, feedback: fb }); });
      });
      if (result.approved) return { content: [{ type: "text", text: "✓ Plan approved by user. Proceed with implementation." }], details: undefined };
      // Surface the user's rejection feedback (the documented {approved:false, feedback} contract) so
      // the agent can act on WHY it was rejected; an empty reason also covers the timeout case.
      const reason = result.feedback && result.feedback.trim()
        ? `User feedback: ${result.feedback.trim()}`
        : "No reason given (or timed out after 5 min).";
      return { content: [{ type: "text", text: `✕ Plan not approved. Do NOT implement. ${reason} Incorporate this guidance and revise.` }], details: undefined };
    },
  });

  // ---- design_system tool: get a design system (tokens, palette, typography, layout) ----
  pi.registerTool({
    name: "design_system",
    label: "Design System",
    description: [
      "Get a complete design system (CSS tokens, color palette, typography pairing, layout pattern, platform guidelines) for a frontend task.",
      "Pass a query describing the product type + intent (e.g. 'fintech dashboard dark', 'landing page hero', 'catppuccin mocha').",
      "Returns ready-to-paste CSS custom properties + font imports + layout guidance.",
    ].join(" "),
    parameters: Type.Object({
      query: Type.String({ description: "Product type + intent + style (e.g. 'saas analytics dashboard dark')" }),
    }),
    async execute(_id, params) {
      const q = (params as { query: string }).query || "dashboard";
      const ds = getDesignSystem(q);
      return { content: [{ type: "text", text: renderDesignSystem(ds) }], details: undefined };
    },
  });

  // ---- design_components tool: get icon + chart + framework guidance ----
  pi.registerTool({
    name: "design_components",
    label: "Design Components",
    description: [
      "Get component guidance (Lucide icons, chart-type recommendations, framework rules) for a frontend task.",
      "Pass a query describing what you're building (e.g. 'user profile settings', 'analytics chart').",
    ].join(" "),
    parameters: Type.Object({
      query: Type.String({ description: "What you're building (e.g. 'notification dropdown', 'data table')" }),
    }),
    async execute(_id, params) {
      const q = (params as { query: string }).query || "general";
      const c = getComponents(q);
      return { content: [{ type: "text", text: renderComponents(c) }], details: undefined };
    },
  });

  // ---- design_audit tool: run a UX/accessibility audit against a target ----
  pi.registerTool({
    name: "design_audit",
    label: "Design Audit",
    description: [
      "Run a UX + accessibility audit against a target description.",
      "Returns findings (critical/warning/suggestion) with specific fixes + passed checks.",
      "Use this as a verification gate before claiming a frontend task is complete.",
    ].join(" "),
    parameters: Type.Object({
      target: Type.String({ description: "What to audit (e.g. 'dark theme login form', 'mobile dashboard layout')" }),
    }),
    async execute(_id, params) {
      const t = (params as { target: string }).target || "general UI";
      const a = auditDesign(t);
      return { content: [{ type: "text", text: renderAudit(a) }], details: undefined };
    },
  });

  // ---- AUTONOMOUS memory lifecycle (gated to the main server process via isMemoryAutonomyEnabled,
  //      so spawned subagents never capture/recall and never churn the shared store) ----

  // Pre-task recall: before each user task, search folder + global memory and inject the most
  // relevant memories into THIS turn's system prompt (local search — no LLM, sub-second).
  pi.on("before_agent_start", async (event, ctx) => {
    if (!isMemoryAutonomyEnabled()) return;
    try {
      const { block } = await memoryStore.recall(event.prompt || "", ctx.cwd);
      if (block) return { systemPrompt: (event.systemPrompt || "") + block };
    } catch { /* recall is best-effort, never blocks the turn */ }
  });

  // Post-task capture: after each task completes, extract durable facts from the exchange (mem0's
  // LLM consolidation). Fire-and-forget so the agent loop is never blocked; then maybe consolidate.
  pi.on("agent_end", async (event, ctx) => {
    if (!isMemoryAutonomyEnabled()) return;
    try {
      const msgs = (event.messages || []) as Array<{ role?: string; content?: unknown }>;
      const userText = lastText(msgs, "user");
      const assistantText = lastText(msgs, "assistant");
      if (!userText && !assistantText) return;
      const cwd = ctx.cwd;
      void memoryStore
        .captureExchange(userText, assistantText, cwd)
        .then(() => memoryStore.maybeAutoConsolidate(cwd))
        .catch(() => { /* best-effort */ });
    } catch { /* best-effort */ }
  });
}
