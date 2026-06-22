/**
 * dotz projects store — a persistent registry of project workspaces.
 * Each project pins a cwd, profile, model, and thinking level so the user can switch
 * between codebases without re-configuring the agent each time. Stored as a single
 * JSON file under the dotz data dir (~/.dotz/projects.json) so it survives restarts
 * and is human-readable/editable.
 */
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import { DEFAULT_MODEL, type Project, type ModelRef, type ThinkingLevel } from "./types";

// Respect DOTZ_CONFIG_DIR for operator relocation + test isolation (same as config.ts/memory.ts).
const dotzDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
const projectsFile = () => path.join(dotzDir(), "projects.json");

async function ensureDir() {
  await fs.mkdir(dotzDir(), { recursive: true });
}

async function readAll(): Promise<Project[]> {
  try {
    const raw = await fs.readFile(projectsFile(), "utf-8");
    const parsed = JSON.parse(raw);
    // Guard a hand-edited non-array store ({}, null, 42, …) — without this the array consumers
    // (push/findIndex/filter/find) throw TypeError and every project op 500s until the file is fixed.
    return Array.isArray(parsed) ? (parsed as Project[]) : [];
  } catch {
    return [];
  }
}

async function writeAll(projects: Project[]): Promise<void> {
  await ensureDir();
  await fs.writeFile(projectsFile(), JSON.stringify(projects, null, 2), "utf-8");
}

export interface CreateProjectInput {
  name: string;
  cwd: string;
  profileId?: string;
  model?: ModelRef;
  thinkingLevel?: ThinkingLevel;
  appUrl?: string;
  gateCommand?: string;
}

export class ProjectStore {
  // Serialize read-modify-write ops: the store is ONE JSON file, so two concurrent create/update/
  // remove requests can each readAll the same state and writeAll over each other, losing one update.
  // ponytail: a single in-process chain (this store is per-process); a cross-process lock if dotz
  // ever runs multiple server processes against the same file.
  private writeChain: Promise<unknown> = Promise.resolve();
  private serialize<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.writeChain.then(fn, fn);
    this.writeChain = run.then(() => {}, () => {});
    return run;
  }

  async list(): Promise<Project[]> {
    return readAll();
  }

  async get(id: string): Promise<Project | undefined> {
    return (await readAll()).find((p) => p.id === id);
  }

  async create(input: CreateProjectInput): Promise<Project> {
    return this.serialize(async () => {
    const now = Date.now();
    const project: Project = {
      id: randomUUID(),
      name: input.name,
      cwd: input.cwd,
      profileId: input.profileId || "workflow",
      model: input.model || DEFAULT_MODEL,
      thinkingLevel: input.thinkingLevel || "high",
      ...(input.appUrl?.trim() ? { appUrl: input.appUrl.trim() } : {}),
      ...(input.gateCommand?.trim() ? { gateCommand: input.gateCommand.trim() } : {}),
      createdAt: now,
      updatedAt: now,
    };
    const all = await readAll();
    all.push(project);
    await writeAll(all);
    return project;
    });
  }

  async update(id: string, patch: Partial<Omit<Project, "id" | "createdAt">>): Promise<Project | undefined> {
    return this.serialize(async () => {
    const all = await readAll();
    const idx = all.findIndex((p) => p.id === id);
    if (idx === -1) return undefined;
    all[idx] = { ...all[idx], ...patch, id: all[idx].id, createdAt: all[idx].createdAt, updatedAt: Date.now() };
    await writeAll(all);
    return all[idx];
    });
  }

  async remove(id: string): Promise<boolean> {
    return this.serialize(async () => {
    const all = await readAll();
    const next = all.filter((p) => p.id !== id);
    if (next.length === all.length) return false;
    await writeAll(next);
    return true;
    });
  }
}

export const projectStore = new ProjectStore();