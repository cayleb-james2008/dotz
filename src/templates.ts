/**
 * dotz workflow template library.
 *
 * Bundled workflow presets live in `.pi/prompts/` (read-only, shipped with the app). Operators can
 * create, edit, fork, and delete their own templates under `~/.dotz/ai-agents/templates/`. User
 * templates shadow bundled presets by id (filename) so operators can customize the built-in
 * workflows without touching the repo.
 *
 * The user template directory is also fed into `buildResourceLoader` as an additional prompt-template
 * path, so custom templates appear as slash commands in new AgentSessions.
 */
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";
import matter from "gray-matter";
import type { AgentSession } from "./pi";

const SELF = fileURLToPath(import.meta.url);
const BUNDLED_PROMPTS_DIR = path.resolve(path.dirname(SELF), "..", ".pi", "prompts");

// Respect DOTZ_CONFIG_DIR for operator relocation + test isolation (same as config.ts/memory.ts).
const dotzDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
export const userTemplatesDir = (): string => path.join(dotzDir(), "ai-agents", "templates");

export interface Template {
  id: string;
  name: string;
  description: string;
  body: string;
  source: "bundled" | "user";
  /** For user forks: the bundled preset id this template originated from. */
  origin?: string;
  tags?: string[];
  createdAt: number;
  updatedAt: number;
}

export interface TemplateInput {
  name: string;
  description?: string;
  body: string;
  tags?: string[];
}

export interface TemplateMeta {
  id: string;
  name: string;
  description: string;
  source: "bundled" | "user";
  origin?: string;
  tags?: string[];
  /** True if the template body contains the `$@` argument placeholder. */
  hasArgs: boolean;
  updatedAt: number;
}

function hasArgsPlaceholder(body: string): boolean {
  return body.includes("$@");
}

function sanitizeId(name: string): string {
  return name.toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-|-$/g, "").slice(0, 64) || "template";
}

async function ensureUserDir(): Promise<void> {
  await fs.mkdir(userTemplatesDir(), { recursive: true });
}

function parseTags(raw: unknown): string[] | undefined {
  if (!Array.isArray(raw)) return undefined;
  const tags = raw.filter((t): t is string => typeof t === "string");
  return tags.length ? tags : undefined;
}

function templateFromMatter(id: string, source: "bundled" | "user", body: string, fallbackName: string, origin?: string, createdAt?: number, updatedAt?: number): Template {
  const parsed = matter(body);
  const name = typeof parsed.data.name === "string" && parsed.data.name.trim() ? parsed.data.name.trim() : fallbackName;
  const description = typeof parsed.data.description === "string" ? parsed.data.description : "";
  const now = Date.now();
  return {
    id,
    name,
    description,
    body,
    source,
    origin: typeof parsed.data.origin === "string" ? parsed.data.origin : origin,
    tags: parseTags(parsed.data.tags),
    createdAt: typeof parsed.data.createdAt === "number" ? parsed.data.createdAt : (createdAt ?? now),
    updatedAt: typeof parsed.data.updatedAt === "number" ? parsed.data.updatedAt : (updatedAt ?? now),
  };
}

async function loadBundled(): Promise<Map<string, Template>> {
  const out = new Map<string, Template>();
  let files: import("node:fs").Dirent[] = [];
  try { files = await fs.readdir(BUNDLED_PROMPTS_DIR, { withFileTypes: true }); } catch { return out; }
  for (const f of files) {
    if (!f.isFile() || !f.name.endsWith(".md")) continue;
    const id = f.name.slice(0, -3);
    try {
      const body = await fs.readFile(path.join(BUNDLED_PROMPTS_DIR, f.name), "utf-8");
      out.set(id, templateFromMatter(id, "bundled", body, id));
    } catch { /* ignore unreadable bundled presets — the app still works without one */ }
  }
  return out;
}

async function loadUser(): Promise<Map<string, Template>> {
  const out = new Map<string, Template>();
  let files: import("node:fs").Dirent[] = [];
  try { files = await fs.readdir(userTemplatesDir(), { withFileTypes: true }); } catch { return out; }
  for (const f of files) {
    if (!f.isFile() || !f.name.endsWith(".md")) continue;
    const id = f.name.slice(0, -3);
    try {
      const body = await fs.readFile(path.join(userTemplatesDir(), f.name), "utf-8");
      out.set(id, templateFromMatter(id, "user", body, id));
    } catch { /* ignore unreadable user templates */ }
  }
  return out;
}

function merge(user: Map<string, Template>, bundled: Map<string, Template>): Map<string, Template> {
  const out = new Map(bundled);
  for (const [id, t] of user) out.set(id, t);
  return out;
}

function toMeta(t: Template): TemplateMeta {
  return {
    id: t.id,
    name: t.name,
    description: t.description,
    source: t.source,
    origin: t.origin,
    tags: t.tags,
    hasArgs: hasArgsPlaceholder(t.body),
    updatedAt: t.updatedAt,
  };
}

function buildBody(opts: {
  name: string;
  description?: string;
  body: string;
  origin?: string;
  tags?: string[];
  createdAt: number;
  updatedAt: number;
}): string {
  // Strip any existing frontmatter so we don't nest frontmatter blocks when forking or editing.
  const parsed = matter(opts.body);
  const content = parsed.content.trim();
  const lines = ["---"];
  lines.push(`name: ${opts.name}`);
  if (opts.description) lines.push(`description: ${opts.description}`);
  if (opts.origin) lines.push(`origin: ${opts.origin}`);
  if (opts.tags?.length) lines.push(`tags: [${opts.tags.map((t) => JSON.stringify(t)).join(", ")}]`);
  lines.push(`createdAt: ${opts.createdAt}`);
  lines.push(`updatedAt: ${opts.updatedAt}`);
  lines.push("---");
  if (content) {
    lines.push("");
    lines.push(content);
  }
  return lines.join("\n") + "\n";
}

export class TemplateStore {
  /** List all effective templates. User templates shadow bundled presets by id. */
  async list(): Promise<TemplateMeta[]> {
    const [bundled, user] = await Promise.all([loadBundled(), loadUser()]);
    const merged = merge(user, bundled);
    return [...merged.values()].map(toMeta).sort((a, b) => a.name.localeCompare(b.name));
  }

  /** Get the effective template for an id (user shadow wins). */
  async get(id: string): Promise<Template | null> {
    const [bundled, user] = await Promise.all([loadBundled(), loadUser()]);
    const merged = merge(user, bundled);
    return merged.get(id) ?? null;
  }

  /** Create a new user template. Rejects if a user template with the same id already exists. */
  async create(input: TemplateInput): Promise<Template> {
    if (!input.name || typeof input.name !== "string" || !input.name.trim()) throw new Error("name is required");
    if (typeof input.body !== "string") throw new Error("body is required");
    await ensureUserDir();
    const id = sanitizeId(input.name);
    const now = Date.now();
    const file = path.join(userTemplatesDir(), `${id}.md`);
    try {
      await fs.access(file);
      throw new Error(`template "${id}" already exists — use PATCH to edit`);
    } catch (err) {
      if ((err as Error).message.includes("already exists")) throw err;
      // ENOENT is expected and means we can write the new file.
    }
    const body = buildBody({ ...input, createdAt: now, updatedAt: now });
    await fs.writeFile(file, body, "utf-8");
    return templateFromMatter(id, "user", body, input.name, undefined, now, now);
  }

  /** Update a user template. Bundled presets are read-only; fork them first. */
  async update(id: string, input: Partial<TemplateInput>): Promise<Template | null> {
    const [bundled, user] = await Promise.all([loadBundled(), loadUser()]);
    const existing = user.get(id) ?? bundled.get(id);
    if (!existing) return null;
    if (existing.source === "bundled") throw new Error("bundled templates are read-only — fork first");
    const next: TemplateInput = {
      name: input.name ?? existing.name,
      description: input.description ?? existing.description,
      body: input.body ?? existing.body,
      tags: input.tags ?? existing.tags,
    };
    const now = Date.now();
    const body = buildBody({ ...next, origin: existing.origin, createdAt: existing.createdAt, updatedAt: now });
    await fs.writeFile(path.join(userTemplatesDir(), `${id}.md`), body, "utf-8");
    return templateFromMatter(id, "user", body, next.name, existing.origin, existing.createdAt, now);
  }

  /** Delete a user template. Bundled presets cannot be deleted. */
  async remove(id: string): Promise<boolean> {
    const file = path.join(userTemplatesDir(), `${id}.md`);
    try {
      await fs.rm(file);
      return true;
    } catch {
      return false;
    }
  }

  /** Copy a bundled preset into the user store so it can be edited. */
  async fork(bundledId: string, newName?: string): Promise<Template | null> {
    const bundled = await loadBundled();
    const t = bundled.get(bundledId);
    if (!t) return null;
    const baseName = newName?.trim() || `${t.name} (fork)`;
    const id = sanitizeId(baseName);
    await ensureUserDir();
    let finalId = id;
    const file = path.join(userTemplatesDir(), `${id}.md`);
    try {
      await fs.access(file);
      finalId = `${id}-${Date.now()}`;
    } catch { /* id is free */ }
    const now = Date.now();
    const body = buildBody({
      name: baseName,
      description: t.description,
      body: t.body,
      origin: bundledId,
      tags: t.tags,
      createdAt: now,
      updatedAt: now,
    });
    await fs.writeFile(path.join(userTemplatesDir(), `${finalId}.md`), body, "utf-8");
    return templateFromMatter(finalId, "user", body, baseName, bundledId, now, now);
  }

  /** Expand the template content (stripping frontmatter, then replacing `$@` with args) and send it as a prompt to the session. */
  async run(id: string, session: AgentSession, args?: string): Promise<boolean> {
    const t = await this.get(id);
    if (!t) return false;
    const content = matter(t.body).content.trim();
    const expanded = content.replace(/\$@/g, args ?? "");
    await session.prompt(expanded);
    return true;
  }
}

export const templateStore = new TemplateStore();
