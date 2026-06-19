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
import { skillLoader } from "../../../src/skills";
import { memoryStore, readAgentsMd, writeAgentsMd, appendAgentsMdSection } from "../../../src/memory";
import { captureBaseline, compare, renderBaseline, type MetricsBaseline } from "../../../src/metrics";
import { getDesignSystem, getComponents, auditDesign, renderDesignSystem, renderComponents, renderAudit } from "../../../src/design";
import { browserController, type BrowserActInput, type BrowserStartInput } from "../../../src/browser";

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
      action: Type.Union(["navigate", "observe", "click", "type", "key", "select", "scroll", "wait"].map((value) => Type.Literal(value))),
      expectedSeq: Type.Optional(Type.Number()),
      url: Type.Optional(Type.String()),
      targetRef: Type.Optional(Type.String()),
      text: Type.Optional(Type.String()),
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

  // ---- memory_list tool ----
  pi.registerTool({
    name: "memory_list",
    label: "Memory List",
    description: "List durable memory entries from the .ai-agents namespace. Pass scope='global' for global entries or scope='project' (with projectId) for project entries.",
    parameters: Type.Object({
      scope: Type.Optional(Type.Union([Type.Literal("project"), Type.Literal("global")])),
      projectId: Type.Optional(Type.String()),
    }),
    async execute(_id, params) {
      const p = params as { scope?: "project" | "global"; projectId?: string };
      const entries = await memoryStore.list(p.projectId, p.scope === "project" ? process.cwd() : null);
      const text = entries.length === 0 ? "No memory entries found." : entries.map((e) => `[${e.scope}] ${e.key}: ${e.value}`).join("\n");
      return { content: [{ type: "text", text }], details: undefined };
    },
  });

  // ---- memory_add tool ----
  pi.registerTool({
    name: "memory_add",
    label: "Memory Add",
    description: "Add a durable memory entry to the .ai-agents namespace. Use scope='global' for cross-project knowledge or scope='project' for project-specific knowledge.",
    parameters: Type.Object({
      key: Type.String({ description: "Short key (e.g. 'convention:naming')" }),
      value: Type.String({ description: "The memory content" }),
      scope: Type.Optional(Type.Union([Type.Literal("project"), Type.Literal("global")])),
      projectId: Type.Optional(Type.String()),
    }),
    async execute(_id, params) {
      const p = params as { key: string; value: string; scope?: "project" | "global"; projectId?: string };
      const entry = await memoryStore.create({
        projectId: p.projectId || "",
        key: p.key,
        value: p.value,
        scope: p.scope || "project",
        projectCwd: p.scope === "project" ? process.cwd() : null,
      });
      return { content: [{ type: "text", text: `Memory saved: [${entry.scope}] ${entry.key}` }], details: undefined };
    },
  });

  // ---- memory_delete tool ----
  pi.registerTool({
    name: "memory_delete",
    label: "Memory Delete",
    description: "Delete a durable memory entry by id.",
    parameters: Type.Object({
      id: Type.String(),
    }),
    async execute(_id, params) {
      const ok = await memoryStore.remove((params as { id: string }).id, process.cwd());
      return { content: [{ type: "text", text: ok ? "Memory entry deleted." : "No such memory entry." }], details: undefined };
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
      const cwd = process.cwd();
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
      const cwd = process.cwd();
      try {
        const baseline = await captureBaseline(cwd);
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
        const result = await compare(baseline);
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
      const approved = await new Promise<boolean>((resolve) => {
        const timeout = setTimeout(() => { gateListeners.delete(gateId); resolve(false); }, 300000);
        registerGateListener(gateId, (ok) => { clearTimeout(timeout); resolve(ok); });
      });
      if (approved) return { content: [{ type: "text", text: "✓ Plan approved by user. Proceed with implementation." }], details: undefined };
      return { content: [{ type: "text", text: "✕ Plan not approved (or timed out after 5 min). Do NOT implement. Ask the user for guidance." }], details: undefined };
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
}
