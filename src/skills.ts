/**
 * dotz unified skills loader — discovers SKILL.md files across the four shared skill pools
 * (opencode, claude, codex/ecc, superpowers) plus any hermes skills and the bundled .pi/skills,
 * parses their frontmatter once, dedupes by name (project > user > plugin priority), and exposes
 * a name+description index for system-prompt injection plus a loadBody() helper for on-demand
 * full-body loading via the `skill` pi tool.
 *
 * This is the single skill-discovery path. It is deliberately separate from pi.ts and profiles.ts
 * to respect the no-circular-import convention (types.ts is the shared leaf).
 *
 * Frontmatter dialects handled:
 *   - Claude/OpenCode: `name`, `description`, optional `compatibility`, `tags`, `related_skills`
 *   - Hermes: nested `metadata.hermes.tags`, `platforms`, `related_skills`, `version`, `author`
 *   - ECC: `name`, `description`, `origin: ECC`
 *   - Superpowers: same as Claude
 *
 * Dedupe priority (highest wins): dotz (.pi) > opencode > claude > codex > ecc > superpowers > hermes.
 * Platform filter: skills declaring `platforms: [...]` are filtered to the current host (win32).
 * Umbrella routing: a skill whose body begins with "Class-level umbrella" or contains a leaf-table
 * is marked `isUmbrella`; loadBody() returns the umbrella body verbatim (the agent reads the table
 * and requests the leaf by name, which resolves through the same index).
 */
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { Skill } from "./types";

const DOTZ_PI = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", ".pi");
const dotzDataDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
const userSkillsDir = () => path.join(dotzDataDir(), "ai-agents", "skills");

export interface CreateSkillInput {
  name: string;
  description: string;
  body: string;
}

const RESOURCE_NAME = /^[a-z][a-z0-9-]{1,63}$/;

function requireResourceName(name: string): string {
  const value = name.trim();
  if (!RESOURCE_NAME.test(value)) {
    throw new Error("skill name must start with a lowercase letter and contain only lowercase letters, digits, and hyphens (2-64 chars)");
  }
  return value;
}

/** Persist a dotz-native user skill in the unified loader's highest-priority root. */
export async function createUserSkill(input: CreateSkillInput): Promise<Skill> {
  const name = requireResourceName(input.name);
  const description = input.description.replace(/\r?\n/g, " ").trim();
  const body = input.body.trim();
  if (!description) throw new Error("skill description is required");
  if (!body) throw new Error("skill body is required");
  const dir = path.join(userSkillsDir(), name);
  const file = path.join(dir, "SKILL.md");
  await fs.mkdir(dir, { recursive: true });
  const content = `---\nname: ${JSON.stringify(name)}\ndescription: ${JSON.stringify(description)}\n---\n\n${body}\n`;
  try {
    await fs.writeFile(file, content, { encoding: "utf-8", flag: "wx" });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") throw new Error(`skill \"${name}\" already exists`);
    throw error;
  }
  skillLoader.invalidate();
  return { name, description, path: file, source: "dotz" };
}

/** Max skills listed in the system-prompt index (overflow is summarized; full pool via /api/skills). */
const INDEX_CAP = 80;

/** Ordered scan roots (lowest priority first, so higher-priority pools overwrite in the map).
 *  Computed per load() so the DOTZ_SKILLS_PATHS override is read fresh from the environment. */
function scanRoots(): Array<{ dir: string; source: Skill["source"] }> {
  const roots: Array<{ dir: string; source: Skill["source"] }> = [
    { dir: path.join(os.homedir(), ".hermes", "skills"), source: "hermes" },
    {
      dir: path.join(os.homedir(), ".codex", "plugins", "cache", "openai-curated", "superpowers"),
      source: "superpowers",
    },
    { dir: path.join(os.homedir(), ".codex", "marketplaces", "ecc-local", "plugins", "ecc", "skills"), source: "ecc" },
    { dir: path.join(os.homedir(), ".codex", "skills"), source: "codex" },
    { dir: path.join(os.homedir(), ".claude", "skills"), source: "claude" },
    { dir: path.join(os.homedir(), ".config", "opencode", "skills"), source: "opencode" },
    { dir: path.join(DOTZ_PI, "skills"), source: "dotz" },
    { dir: userSkillsDir(), source: "dotz" },
  ];
  // Operator override: DOTZ_SKILLS_PATHS=dir1<sep>dir2 (path.delimiter). Each existing dir is
  // appended at the end (highest priority) so custom skill pools win over the built-in roots.
  const extra = (process.env.DOTZ_SKILLS_PATHS || "").split(path.delimiter).map((d) => d.trim()).filter(Boolean);
  for (const dir of extra) if (existsSync(dir)) roots.push({ dir, source: "dotz" });
  return roots;
}

const HOST_PLATFORM = (() => {
  switch (process.platform) {
    case "win32":
      return "windows";
    case "darwin":
      return "macos";
    default:
      return "linux";
  }
})();

/** Minimal YAML frontmatter parser — handles flat keys, ARBITRARY-depth nesting (via an indent
 *  stack, so e.g. metadata.hermes.tags resolves), inline arrays, and block scalars (`|` literal /
 *  `>` folded). We avoid a full YAML dep because skill frontmatter is otherwise simple. */
function parseFrontmatter(raw: string): Record<string, unknown> {
  const m = raw.match(/^---\r?\n([\s\S]*?)\r?\n---/);
  if (!m) return {};
  const out: Record<string, unknown> = {};
  const lines = m[1].split(/\r?\n/);
  const indentOf = (s: string) => s.length - s.trimStart().length;
  // Each frame owns the object that more-indented keys attach to; the sentinel root holds top keys.
  const stack: Array<{ indent: number; obj: Record<string, unknown> }> = [{ indent: -1, obj: out }];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (!line.trim() || line.trimStart().startsWith("#")) continue;
    const km = line.match(/^\s*(\w[\w-]*):[ \t]*(.*)$/);
    if (!km) continue;
    const indent = indentOf(line);
    const key = km[1];
    const value = km[2];
    // Pop to the nearest strictly-shallower frame — that's this key's parent.
    while (stack.length > 1 && indent <= stack[stack.length - 1].indent) stack.pop();
    const parent = stack[stack.length - 1].obj;
    const block = value.trim().match(/^([|>])[+-]?\d*$/);
    if (block) {
      // Block scalar: consume the following lines indented deeper than this key.
      const fold = block[1] === ">";
      const collected: string[] = [];
      let blockIndent = -1;
      let j = i + 1;
      for (; j < lines.length; j++) {
        const bl = lines[j];
        if (!bl.trim()) { collected.push(""); continue; }
        const bi = indentOf(bl);
        if (bi <= indent) break;
        if (blockIndent < 0) blockIndent = bi;
        collected.push(bl.slice(Math.min(bi, blockIndent)));
      }
      while (collected.length && collected[collected.length - 1] === "") collected.pop();
      parent[key] = fold ? collected.join(" ").replace(/\s+/g, " ").trim() : collected.join("\n");
      i = j - 1;
    } else if (value.trim() === "") {
      // Empty value → a nested object to be filled by deeper lines (stays {} if none follow).
      const obj: Record<string, unknown> = {};
      parent[key] = obj;
      stack.push({ indent, obj });
    } else {
      parent[key] = parseScalar(value);
    }
  }
  return out;
}

function parseScalar(s: string): unknown {
  const t = s.trim();
  if (t === "") return "";
  // quoted string
  if ((t.startsWith('"') && t.endsWith('"')) || (t.startsWith("'") && t.endsWith("'"))) {
    return t.slice(1, -1);
  }
  // inline array
  if (t.startsWith("[") && t.endsWith("]")) {
    return t
      .slice(1, -1)
      .split(",")
      .map((x) => x.trim().replace(/^["']|["']$/g, ""))
      .filter(Boolean);
  }
  // number
  if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
  // boolean
  if (t === "true") return true;
  if (t === "false") return false;
  // multi-line description (folded) — return as-is, caller may strip
  return t;
}

function platformsOk(skill: Skill): boolean {
  if (!skill.platforms || skill.platforms.length === 0) return true;
  return skill.platforms.includes(HOST_PLATFORM);
}

/** Recursively find SKILL.md files under a root. */
async function findSkillFiles(root: string): Promise<string[]> {
  if (!existsSync(root)) return [];
  const out: string[] = [];
  async function walk(dir: string) {
    let entries: import("node:fs").Dirent[];
    try {
      entries = await fs.readdir(dir, { withFileTypes: true }) as unknown as import("node:fs").Dirent[];
    } catch {
      return;
    }
    for (const e of entries) {
      const full = path.join(dir, e.name as string);
      if (e.isDirectory()) await walk(full);
      else if (e.isFile() && (e.name as string).toLowerCase() === "skill.md") out.push(full);
    }
  }
  await walk(root);
  return out;
}

/** Parse one SKILL.md file into a Skill (without loading the full body). */
async function parseSkillFile(file: string, source: Skill["source"]): Promise<Skill | null> {
  try {
    const raw = await fs.readFile(file, "utf-8");
    const fm = parseFrontmatter(raw);
    // Accept only string/number scalars: numeric frontmatter (`name: 2048`) is coerced to a resolvable
    // string key, while an empty value the parser turned into {} must NOT become the literal
    // "[object Object]" — treat a non-scalar as absent so the dirname fallback fires.
    const name = (typeof fm.name === "string" || typeof fm.name === "number" ? String(fm.name) : "").trim()
      || path.basename(path.dirname(file));
    // Collapse a block-scalar / multi-line description to a single clean line for the compact index.
    const description = (typeof fm.description === "string" || typeof fm.description === "number" ? String(fm.description) : "").replace(/\s+/g, " ").trim();
    if (!name) return null;
    const platforms = (fm.platforms as string[] | undefined) ?? undefined;
    const skill: Skill = {
      name,
      description,
      path: file,
      source,
      tags: (fm.tags as string[] | undefined) ?? undefined,
      compatibility: (fm.compatibility as string | undefined) ?? undefined,
      platforms,
      relatedSkills: (fm.related_skills as string[] | undefined) ?? undefined,
    };
    // Hermes nests under metadata.hermes
    const hermesMeta = (fm.metadata as { hermes?: Record<string, unknown> } | undefined)?.hermes;
    if (hermesMeta) {
      if (hermesMeta.tags && Array.isArray(hermesMeta.tags)) skill.tags = (skill.tags ?? []).concat(hermesMeta.tags as string[]);
    }
    // detect umbrella (body header heuristics)
    const bodyStart = raw.replace(/^---[\s\S]*?---\r?\n/, "");
    skill.isUmbrella = /Class-level umbrella|umbrella skill/i.test(bodyStart.slice(0, 400));
    return skill;
  } catch {
    return null;
  }
}

export class SkillLoader {
  private index = new Map<string, Skill>();
  private loaded = false;
  private bodyCache = new Map<string, string>();

  /** Scan all roots and build the deduped index. Safe to call repeatedly (idempotent). */
  async load(): Promise<void> {
    if (this.loaded) return;
    const all: Skill[] = [];
    for (const root of scanRoots()) {
      const files = await findSkillFiles(root.dir);
      for (const f of files) {
        const s = await parseSkillFile(f, root.source);
        if (s && platformsOk(s)) all.push(s);
      }
    }
    // dedupe by name — higher priority overwrites (SCAN_ROOTS is ordered low→high)
    for (const s of all) this.index.set(s.name, s);
    this.loaded = true;
  }

  /** All discovered skills (deduped, platform-filtered). */
  list(): Skill[] {
    return [...this.index.values()].sort((a, b) => a.name.localeCompare(b.name));
  }

  get(name: string): Skill | undefined {
    return this.index.get(name);
  }

  has(name: string): boolean {
    return this.index.has(name);
  }

  /** Load the full body of a skill (cached). Strips frontmatter. */
  async loadBody(name: string): Promise<string | undefined> {
    const cached = this.bodyCache.get(name);
    if (cached !== undefined) return cached;
    const s = this.index.get(name);
    if (!s) return undefined;
    try {
      const raw = await fs.readFile(s.path, "utf-8");
      const body = raw.replace(/^---[\s\S]*?---\r?\n/, "");
      this.bodyCache.set(name, body);
      return body;
    } catch {
      return undefined;
    }
  }

  /** Render a compact name+description index for system-prompt injection. Capped at INDEX_CAP
   *  entries so a large skill pool doesn't bloat the system prompt; the overflow is summarized in
   *  a footer (the agent can still load any skill by name via the `skill` tool). */
  renderIndex(): string {
    const skills = this.list();
    if (skills.length === 0) return "";
    const shown = skills.slice(0, INDEX_CAP);
    const lines = shown.map((s) => `- ${s.name}: ${s.description.slice(0, 160)}`);
    if (skills.length > INDEX_CAP) {
      lines.push(`- …and ${skills.length - INDEX_CAP} more — call the \`skill\` tool by name, or GET /api/skills to browse/filter the full pool.`);
    }
    const heading = skills.length > INDEX_CAP ? `${skills.length} skills, showing first ${INDEX_CAP}` : `${skills.length} skills`;
    return `\n# dotz unified skill index (${heading})\nInvoke a skill's full instructions by calling the \`skill\` tool with its name. Skills are auto-discovered from opencode, claude, codex, ecc, superpowers, hermes, and bundled .pi pools.\n${lines.join("\n")}\n`;
  }

  invalidate(): void {
    this.index.clear();
    this.bodyCache.clear();
    this.loaded = false;
  }
}

export const skillLoader = new SkillLoader();
