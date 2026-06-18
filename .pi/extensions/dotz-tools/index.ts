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

export default function (pi: ExtensionAPI) {
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
        };
      }
      return { content: [{ type: "text", text: body }] };
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
      return { content: [{ type: "text", text }] };
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
      return { content: [{ type: "text", text: `Memory saved: [${entry.scope}] ${entry.key}` }] };
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
      return { content: [{ type: "text", text: ok ? "Memory entry deleted." : "No such memory entry." }] };
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
        return { content: [{ type: "text", text: content || "(no AGENTS.md in project root)" }] };
      }
      if (p.action === "append") {
        if (!p.section || !p.body) return { content: [{ type: "text", text: "append requires section and body" }], isError: true };
        await appendAgentsMdSection(cwd, p.section, p.body);
        return { content: [{ type: "text", text: `Appended section '${p.section}' to AGENTS.md` }] };
      }
      if (p.action === "write") {
        if (!p.body) return { content: [{ type: "text", text: "write requires body" }], isError: true };
        await writeAgentsMd(cwd, p.body);
        return { content: [{ type: "text", text: "AGENTS.md overwritten." }] };
      }
      return { content: [{ type: "text", text: "unknown action" }], isError: true };
    },
  });
}