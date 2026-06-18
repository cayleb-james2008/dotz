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
import type { Project, ModelRef, ThinkingLevel } from "./types";

const DOTZ_DIR = path.join(os.homedir(), ".dotz");
const PROJECTS_FILE = path.join(DOTZ_DIR, "projects.json");

async function ensureDir() {
  await fs.mkdir(DOTZ_DIR, { recursive: true });
}

async function readAll(): Promise<Project[]> {
  try {
    const raw = await fs.readFile(PROJECTS_FILE, "utf-8");
    return JSON.parse(raw) as Project[];
  } catch {
    return [];
  }
}

async function writeAll(projects: Project[]): Promise<void> {
  await ensureDir();
  await fs.writeFile(PROJECTS_FILE, JSON.stringify(projects, null, 2), "utf-8");
}

export interface CreateProjectInput {
  name: string;
  cwd: string;
  profileId?: string;
  model?: ModelRef;
  thinkingLevel?: ThinkingLevel;
}

export class ProjectStore {
  async list(): Promise<Project[]> {
    return readAll();
  }

  async get(id: string): Promise<Project | undefined> {
    return (await readAll()).find((p) => p.id === id);
  }

  async create(input: CreateProjectInput): Promise<Project> {
    const now = Date.now();
    const project: Project = {
      id: randomUUID(),
      name: input.name,
      cwd: input.cwd,
      profileId: input.profileId || "workflow",
      model: input.model || { provider: "openrouter", modelId: "nex-agi/nex-n2-pro:free" },
      thinkingLevel: input.thinkingLevel || "high",
      createdAt: now,
      updatedAt: now,
    };
    const all = await readAll();
    all.push(project);
    await writeAll(all);
    return project;
  }

  async update(id: string, patch: Partial<Omit<Project, "id" | "createdAt">>): Promise<Project | undefined> {
    const all = await readAll();
    const idx = all.findIndex((p) => p.id === id);
    if (idx === -1) return undefined;
    all[idx] = { ...all[idx], ...patch, id: all[idx].id, createdAt: all[idx].createdAt, updatedAt: Date.now() };
    await writeAll(all);
    return all[idx];
  }

  async remove(id: string): Promise<boolean> {
    const all = await readAll();
    const next = all.filter((p) => p.id !== id);
    if (next.length === all.length) return false;
    await writeAll(next);
    return true;
  }
}

export const projectStore = new ProjectStore();