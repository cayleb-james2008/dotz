/**
 * dotz memory store — persistent per-project memory entries that get injected into the
 * agent's system prompt as durable context. Stored as JSON under ~/.dotz/memory.json.
 * Memory entries have a scope: "project" (only when that project is active) or "global"
 * (always injected regardless of active project).
 */
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import type { MemoryEntry } from "./types";

const DOTZ_DIR = path.join(os.homedir(), ".dotz");
const MEMORY_FILE = path.join(DOTZ_DIR, "memory.json");

async function ensureDir() {
  await fs.mkdir(DOTZ_DIR, { recursive: true });
}

async function readAll(): Promise<MemoryEntry[]> {
  try {
    const raw = await fs.readFile(MEMORY_FILE, "utf-8");
    return JSON.parse(raw) as MemoryEntry[];
  } catch {
    return [];
  }
}

async function writeAll(entries: MemoryEntry[]): Promise<void> {
  await ensureDir();
  await fs.writeFile(MEMORY_FILE, JSON.stringify(entries, null, 2), "utf-8");
}

export interface CreateMemoryInput {
  projectId: string;
  key: string;
  value: string;
  scope?: "project" | "global";
}

export class MemoryStore {
  async list(projectId?: string): Promise<MemoryEntry[]> {
    const all = await readAll();
    if (projectId) return all.filter((e) => e.projectId === projectId || e.scope === "global");
    return all;
  }

  async forProject(projectId: string | null): Promise<MemoryEntry[]> {
    const all = await readAll();
    if (!projectId) return all.filter((e) => e.scope === "global");
    return all.filter((e) => e.projectId === projectId || e.scope === "global");
  }

  async create(input: CreateMemoryInput): Promise<MemoryEntry> {
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
    const all = await readAll();
    all.push(entry);
    await writeAll(all);
    return entry;
  }

  async update(id: string, patch: Partial<Pick<MemoryEntry, "key" | "value" | "scope">>): Promise<MemoryEntry | undefined> {
    const all = await readAll();
    const idx = all.findIndex((e) => e.id === id);
    if (idx === -1) return undefined;
    all[idx] = { ...all[idx], ...patch, id: all[idx].id, updatedAt: Date.now() };
    await writeAll(all);
    return all[idx];
  }

  async remove(id: string): Promise<boolean> {
    const all = await readAll();
    const next = all.filter((e) => e.id !== id);
    if (next.length === all.length) return false;
    await writeAll(next);
    return true;
  }

  /** Render memory entries as a system-prompt block for injection into agent context. */
  renderForPrompt(entries: MemoryEntry[]): string {
    if (entries.length === 0) return "";
    const lines = entries.map((e) => `- [${e.scope}] ${e.key}: ${e.value}`);
    return `\n# dotz persistent memory\nThe following durable memory entries are user-curated context. Treat them as authoritative project knowledge:\n${lines.join("\n")}\n`;
  }
}

export const memoryStore = new MemoryStore();