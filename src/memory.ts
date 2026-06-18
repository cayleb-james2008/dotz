/**
 * dotz memory store — persistent per-project + global memory entries injected into the
 * agent's system prompt as durable context. Storage lives under the `.ai-agents` namespace:
 *   - global:  ~/.dotz/ai-agents/memory.json
 *   - project: <cwd>/.ai-agents/memory.json (when a project cwd is supplied)
 * Memory entries have a scope: "project" (only when that project is active) or "global"
 * (always injected regardless of active project).
 *
 * AGENTS.md files (project root + ~/.config/opencode/AGENTS.md global) are the *doctrine*
 * layer, read by buildResourceLoader in profiles.ts. This store is the *knowledge* layer
 * (structured, agent-curated). Don't blur them — AGENTS.md is prose, memory.json is data.
 */
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import type { MemoryEntry } from "./types";

const GLOBAL_MEMORY_DIR = path.join(os.homedir(), ".dotz", "ai-agents");
const GLOBAL_MEMORY_FILE = path.join(GLOBAL_MEMORY_DIR, "memory.json");

function projectMemoryFile(cwd: string): string {
  return path.join(cwd, ".ai-agents", "memory.json");
}

async function ensureDir(dir: string) {
  await fs.mkdir(dir, { recursive: true });
}

async function readJson(file: string): Promise<MemoryEntry[]> {
  try {
    const raw = await fs.readFile(file, "utf-8");
    return JSON.parse(raw) as MemoryEntry[];
  } catch {
    return [];
  }
}

async function writeJson(file: string, entries: MemoryEntry[]): Promise<void> {
  await ensureDir(path.dirname(file));
  await fs.writeFile(file, JSON.stringify(entries, null, 2), "utf-8");
}

/** Merge global + project-scoped memory files. Project entries override global on id collision. */
async function readAll(projectCwd?: string | null): Promise<MemoryEntry[]> {
  const global = await readJson(GLOBAL_MEMORY_FILE);
  if (!projectCwd) return global;
  const proj = await readJson(projectMemoryFile(projectCwd));
  const byId = new Map<string, MemoryEntry>();
  for (const e of global) byId.set(e.id, e);
  for (const e of proj) byId.set(e.id, e); // project wins
  return [...byId.values()];
}

/** Where to write an entry: global scope → global file; project scope → project file (or global if no cwd). */
function writeTarget(scope: "project" | "global", projectCwd?: string | null): string {
  if (scope === "global") return GLOBAL_MEMORY_FILE;
  if (projectCwd) return projectMemoryFile(projectCwd);
  return GLOBAL_MEMORY_FILE;
}

/** Read AGENTS.md from a project root (returns "" if absent). */
export async function readAgentsMd(cwd: string): Promise<string> {
  try {
    return await fs.readFile(path.join(cwd, "AGENTS.md"), "utf-8");
  } catch {
    return "";
  }
}

/** Write/overwrite AGENTS.md in a project root. */
export async function writeAgentsMd(cwd: string, content: string): Promise<void> {
  await fs.writeFile(path.join(cwd, "AGENTS.md"), content, "utf-8");
}

/** Append a section to AGENTS.md (creates the file if absent). */
export async function appendAgentsMdSection(cwd: string, section: string, body: string): Promise<void> {
  const existing = await readAgentsMd(cwd);
  const block = `\n## ${section}\n${body}\n`;
  if (existing.includes(`## ${section}`)) {
    // replace existing section
    const replaced = existing.replace(new RegExp(`\\n## ${section}[\\s\\S]*?(?=\\n## |$)`), block);
    await writeAgentsMd(cwd, replaced);
  } else {
    await writeAgentsMd(cwd, existing + block);
  }
}

export interface CreateMemoryInput {
  projectId: string;
  key: string;
  value: string;
  scope?: "project" | "global";
}

export class MemoryStore {
  async list(projectId?: string, projectCwd?: string | null): Promise<MemoryEntry[]> {
    const all = await readAll(projectCwd);
    if (projectId) return all.filter((e) => e.projectId === projectId || e.scope === "global");
    return all;
  }

  async forProject(projectId: string | null, projectCwd?: string | null): Promise<MemoryEntry[]> {
    const all = await readAll(projectCwd);
    if (!projectId) return all.filter((e) => e.scope === "global");
    return all.filter((e) => e.projectId === projectId || e.scope === "global");
  }

  async create(input: CreateMemoryInput & { projectCwd?: string | null }): Promise<MemoryEntry> {
    const now = Date.now();
    const entry: MemoryEntry = {
      id: randomUUID(),
      projectId: input.projectId,
      key: input.key,
      value: input.value,
      scope: input.scope || "project",
      createdAt: now,
      updatedAt: now,
    };
    const scope = entry.scope;
    const target = writeTarget(scope, input.projectCwd);
    const entries = await readJson(target);
    entries.push(entry);
    await writeJson(target, entries);
    return entry;
  }

  async update(id: string, patch: Partial<Pick<MemoryEntry, "key" | "value" | "scope">>, projectCwd?: string | null): Promise<MemoryEntry | undefined> {
    // search both stores for the entry
    const globalEntries = await readJson(GLOBAL_MEMORY_FILE);
    const projEntries = projectCwd ? await readJson(projectMemoryFile(projectCwd)) : [];
    let entry: MemoryEntry | undefined;
    let inGlobal = false;
    let idx = globalEntries.findIndex((e) => e.id === id);
    if (idx >= 0) {
      entry = globalEntries[idx];
      inGlobal = true;
    } else {
      idx = projEntries.findIndex((e) => e.id === id);
      if (idx >= 0) entry = projEntries[idx];
    }
    if (!entry) return undefined;
    const updated: MemoryEntry = { ...entry, ...patch, id: entry.id, updatedAt: Date.now() };
    // if scope changed, move between files
    if (patch.scope && patch.scope !== entry.scope) {
      const newTarget = writeTarget(patch.scope, projectCwd);
      if (inGlobal) globalEntries.splice(idx, 1);
      else projEntries.splice(idx, 1);
      const newEntries = await readJson(newTarget);
      newEntries.push(updated);
      await writeJson(GLOBAL_MEMORY_FILE, globalEntries);
      if (projectCwd) await writeJson(projectMemoryFile(projectCwd), projEntries);
      await writeJson(newTarget, newEntries);
    } else {
      if (inGlobal) {
        globalEntries[idx] = updated;
        await writeJson(GLOBAL_MEMORY_FILE, globalEntries);
      } else {
        projEntries[idx] = updated;
        if (projectCwd) await writeJson(projectMemoryFile(projectCwd), projEntries);
      }
    }
    return updated;
  }

  async remove(id: string, projectCwd?: string | null): Promise<boolean> {
    const globalEntries = await readJson(GLOBAL_MEMORY_FILE);
    const projEntries = projectCwd ? await readJson(projectMemoryFile(projectCwd)) : [];
    let removed = false;
    const gIdx = globalEntries.findIndex((e) => e.id === id);
    if (gIdx >= 0) {
      globalEntries.splice(gIdx, 1);
      await writeJson(GLOBAL_MEMORY_FILE, globalEntries);
      removed = true;
    }
    const pIdx = projEntries.findIndex((e) => e.id === id);
    if (pIdx >= 0) {
      projEntries.splice(pIdx, 1);
      if (projectCwd) await writeJson(projectMemoryFile(projectCwd), projEntries);
      removed = true;
    }
    return removed;
  }

  /** Render memory entries as a system-prompt block for injection into agent context. */
  renderForPrompt(entries: MemoryEntry[]): string {
    if (entries.length === 0) return "";
    const lines = entries.map((e) => `- [${e.scope}] ${e.key}: ${e.value}`);
    return `\n# dotz persistent memory (.ai-agents)\nThe following durable memory entries are user-curated context. Treat them as authoritative project knowledge:\n${lines.join("\n")}\n`;
  }
}

export const memoryStore = new MemoryStore();