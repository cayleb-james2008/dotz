/**
 * dotz memory store — now backed by **mem0** (self-hosted OSS, fully on-device).
 *
 * Architecture (see AGENTS.md "Projects + Memory"):
 *   - ONE embedded mem0 `Memory` under `~/.dotz/ai-agents/mem0/` (sqlite vector + history).
 *     Vector store = mem0 `provider:"memory"` (better-sqlite3); embeddings = the bundled local
 *     transformers.js model (src/embedder.ts), injected in-process; the LLM that powers mem0's
 *     fact extraction / consolidation / update decisions is the SAME Ollama Cloud chat dotz
 *     already uses (openai-compatible provider → https://ollama.com/v1, $OLLAMA_API_KEY).
 *   - Scope is partitioned by mem0 `userId`: "__global__" for global, "proj:<cwd>" for project.
 *     Typed-memory category, folder, and a timestamp live in mem0 `metadata`.
 *   - The durable, git-committable, human-readable record is `MEMORY.md` (global under
 *     `~/.dotz/ai-agents/`, per-project under `<cwd>/.ai-agents/`). It is regenerated on every
 *     write and is the source of truth — the sqlite vector index is a derived cache.
 *
 * AUTONOMY: capture, update, consolidation, and recall all run automatically via the extension
 * hooks in `.pi/extensions/dotz-tools` (gated by `isMemoryAutonomyEnabled()`, which only the
 * main server process enables — never spawned subagents). The operator never has to manage memory.
 *
 * The `agents_md` helpers below are the *doctrine* layer (prose AGENTS.md) — unrelated to mem0
 * memory; kept here unchanged because dotz-tools + server import them from this module.
 */
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import os from "node:os";
import type { Memory as Mem0, MemoryItem } from "mem0ai/oss";
import type { MemoryView, MemoryScope } from "./types";
import { localEmbedder, EMBED_DIM } from "./embedder";
import { memoryGraph, type GraphView } from "./memory-graph";
import { getConfig } from "./config";

// ---- paths (respect DOTZ_CONFIG_DIR for relocation + test isolation, like config.ts) ----
const dotzDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
const aiAgentsDir = () => path.join(dotzDir(), "ai-agents");
const mem0Dir = () => path.join(aiAgentsDir(), "mem0");
const GLOBAL_USER = "__global__";

/** Coding-tuned fact extraction (mem0 `customInstructions`) — keep durable engineering facts,
 *  drop transient task chatter. This is what makes auto-capture useful instead of noisy. */
const CODING_INSTRUCTIONS = [
  "You are the durable memory of a coding agent working on software projects.",
  "Extract ONLY durable, reusable facts worth remembering across future sessions:",
  "- project conventions & code style, architecture/design decisions and their rationale,",
  "- build / test / lint / deploy commands, important file or module locations,",
  "- gotchas, workarounds, and non-obvious constraints, tooling/library choices,",
  "- explicit, stable USER preferences and standing instructions.",
  "IGNORE transient task state, one-off answers, ephemeral file contents, and pleasantries.",
  "Write each memory as a single concise, self-contained fact. If nothing is durable, extract nothing.",
].join(" ");

function scopeUser(scope: MemoryScope, projectCwd?: string | null): string {
  if (scope === "global" || !projectCwd) return GLOBAL_USER;
  // Case-fold only on Windows (case-insensitive FS). Lowercasing on Linux/macOS would merge two
  // genuinely distinct project dirs that differ only in case into one shared memory scope.
  const norm = path.resolve(projectCwd).replace(/\\/g, "/");
  return "proj:" + (process.platform === "win32" ? norm.toLowerCase() : norm);
}

function cosine(a: number[], b: number[]): number {
  let dot = 0, na = 0, nb = 0;
  for (let i = 0; i < a.length && i < b.length; i++) { dot += a[i] * b[i]; na += a[i] * a[i]; nb += b[i] * b[i]; }
  return dot / (Math.sqrt(na) * Math.sqrt(nb) || 1);
}

// ---- autonomy flag: ONLY the main server process enables it (subagents must not capture/recall) ----
let autonomyEnabled = false;
export function enableMemoryAutonomy(): void { autonomyEnabled = true; }
export function isMemoryAutonomyEnabled(): boolean { return autonomyEnabled; }

// ---- recall observability emitter (mirrors the human-gate notify pattern in dotz-tools) ----
export interface RecallEvent { cwd: string | null; query: string; items: MemoryView[]; }
type RecallListener = (e: RecallEvent) => void;
const recallListeners = new Set<RecallListener>();
export function onMemoryRecall(fn: RecallListener): () => void { recallListeners.add(fn); return () => recallListeners.delete(fn); }
function emitRecall(e: RecallEvent): void { for (const fn of recallListeners) { try { fn(e); } catch { /* best-effort */ } } }

// ---- AGENTS.md helpers (doctrine layer — UNCHANGED; not part of mem0 memory) ----
export async function readAgentsMd(cwd: string): Promise<string> {
  try { return await fs.readFile(path.join(cwd, "AGENTS.md"), "utf-8"); } catch { return ""; }
}
export async function writeAgentsMd(cwd: string, content: string): Promise<void> {
  await fs.writeFile(path.join(cwd, "AGENTS.md"), content, "utf-8");
}
export async function appendAgentsMdSection(cwd: string, section: string, body: string): Promise<void> {
  const existing = await readAgentsMd(cwd);
  const block = `\n## ${section}\n${body}\n`;
  if (existing.includes(`## ${section}`)) {
    // Escape regex metachars in the heading (else `new RegExp` throws on e.g. "C++ notes") and use
    // a function replacer so `$&`/`$1` in the body aren't interpreted as replacement-string specials.
    const escSection = section.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const replaced = existing.replace(new RegExp(`\\n## ${escSection}[\\s\\S]*?(?=\\n## |$)`), () => block);
    await writeAgentsMd(cwd, replaced);
  } else {
    await writeAgentsMd(cwd, existing + block);
  }
}

export interface AddMemoryInput {
  text: string;
  scope?: MemoryScope;
  category?: string;
  folder?: string;
  projectCwd?: string | null;
  /** infer:true runs mem0's LLM extraction/dedup (autonomous capture); false stores verbatim. */
  infer?: boolean;
}

export interface SearchOpts {
  projectCwd?: string | null;
  scope?: MemoryScope;
  threshold?: number;
  topK?: number;
  folder?: string;
  category?: string;
}

const RECENCY_HALFLIFE_MS = 1000 * 60 * 60 * 24 * 30; // 30 days
const AUTO_CONSOLIDATE_EVERY = 25; // captures between automatic consolidation passes

export class MemoryStore {
  private mem: Mem0 | null = null;
  private memInit: Promise<Mem0> | null = null;
  private capturesSinceConsolidate = 0;
  /** Project cwds whose legacy memory.json import has been attempted (idempotent in-memory guard). */
  private legacyTried = new Set<string>();

  /** Lazily construct the mem0 engine (after the app is up so embeddings can run). Memoized via
   *  memInit so concurrent first callers share one init — otherwise two Memory instances would be
   *  built over the same sqlite files and importLegacy would run twice (double-importing legacy). */
  private engine(): Promise<Mem0> {
    if (this.mem) return Promise.resolve(this.mem);
    if (!this.memInit) {
      this.memInit = (async () => {
        const { Memory } = await import("mem0ai/oss");
        await fs.mkdir(mem0Dir(), { recursive: true });
        const cfg = getConfig();
        const baseURL = process.env.DOTZ_MEMORY_BASE_URL || "https://ollama.com/v1";
        const model = process.env.DOTZ_MEMORY_MODEL || cfg.executiveModel || "glm-5.2";
        const apiKey = process.env.DOTZ_MEMORY_API_KEY || process.env.OLLAMA_API_KEY || "dotz-no-key";
        const mem = new Memory({
          // Dummy openai embedder config (constructor needs a non-empty key) — replaced in-process below.
          embedder: { provider: "openai", config: { apiKey: "local", model: "local", embeddingDims: EMBED_DIM } },
          // mem0's extraction/consolidation LLM = the same Ollama Cloud chat dotz already uses.
          llm: { provider: "openai", config: { apiKey, baseURL, model, temperature: 0.1 } },
          vectorStore: { provider: "memory", config: { collectionName: "dotz_memories", dimension: EMBED_DIM, dbPath: path.join(mem0Dir(), "vectors.db") } },
          historyStore: { provider: "sqlite", config: { historyDbPath: path.join(mem0Dir(), "history.db") } },
          customInstructions: CODING_INSTRUCTIONS,
        });
        // mem0-ts has no custom-embedder provider — inject the bundled local embedder in-process.
        (mem as unknown as { embedder: typeof localEmbedder }).embedder = localEmbedder;
        this.mem = mem;
        await this.importLegacy(null).catch(() => { /* migration is best-effort */ });
        return mem;
      })().catch((e) => { this.memInit = null; throw e; }); // reset on failure so a later call retries
    }
    return this.memInit;
  }

  private toView(it: MemoryItem, scopeFallback: MemoryScope): MemoryView {
    const md = (it.metadata || {}) as Record<string, unknown>;
    const ts = typeof md.ts === "number" ? md.ts : (it.createdAt ? Date.parse(it.createdAt) : undefined);
    return {
      id: it.id,
      memory: it.memory,
      scope: (md.scope as MemoryScope) || scopeFallback,
      category: typeof md.category === "string" ? md.category : undefined,
      folder: typeof md.folder === "string" ? md.folder : undefined,
      score: typeof it.score === "number" ? it.score : undefined,
      createdAt: ts,
      updatedAt: it.updatedAt ? Date.parse(it.updatedAt) : undefined,
    };
  }

  /** Import a project's legacy <cwd>/.ai-agents/memory.json into mem0 once, on first project use.
   *  engine() only imports the GLOBAL legacy store, so without this the per-project branch is dead.
   *  Idempotent: guarded by the in-memory legacyTried set + importLegacy's own `.mem0-migrated` marker. */
  private async ensureProjectLegacy(projectCwd?: string | null): Promise<void> {
    if (!projectCwd || this.legacyTried.has(projectCwd)) return;
    this.legacyTried.add(projectCwd);
    await this.importLegacy(projectCwd).catch(() => { /* migration is best-effort */ });
  }

  /** Add a memory. Manual adds use infer:false (verbatim); auto-capture uses infer:true. */
  async add(input: AddMemoryInput, opts: { skipMirror?: boolean } = {}): Promise<MemoryView[]> {
    const scope: MemoryScope = input.scope ?? (input.projectCwd ? "project" : "global");
    const userId = scopeUser(scope, input.projectCwd);
    const mem = await this.engine();
    await this.ensureProjectLegacy(input.projectCwd);
    const metadata: Record<string, unknown> = { scope, ts: Date.now() };
    if (input.category) metadata.category = input.category;
    if (input.folder) metadata.folder = input.folder;
    const res = await mem.add(input.text, { userId, metadata, infer: input.infer ?? false });
    // mem0's add() result doesn't echo custom metadata back, so overlay what we just wrote
    // (storage is correct — list/search read it back from the vector store).
    const views = (res.results || []).map((r) => {
      const v = this.toView(r, scope);
      v.scope = scope;
      if (input.category && !v.category) v.category = input.category;
      if (input.folder && !v.folder) v.folder = input.folder;
      return v;
    });
    for (const v of views) if (v.id) memoryGraph.indexMemory(userId, v.id, v.memory);
    if (!opts.skipMirror) await this.writeMirror(scope, input.projectCwd);
    return views;
  }

  /** Manual single add (REST/tool) — verbatim, returns the created view. */
  async create(input: { text: string; category?: string; scope?: MemoryScope; folder?: string; projectCwd?: string | null }): Promise<MemoryView> {
    const views = await this.add({ ...input, infer: false });
    return views[0] ?? { id: "", memory: input.text, scope: input.scope ?? (input.projectCwd ? "project" : "global") };
  }

  /** All memories for a scope: global, plus project (when a cwd is given). */
  async list(projectCwd?: string | null): Promise<MemoryView[]> {
    const mem = await this.engine();
    const out: MemoryView[] = [];
    const g = await mem.getAll({ filters: { user_id: GLOBAL_USER }, topK: 500 });
    for (const r of g.results) out.push(this.toView(r, "global"));
    if (projectCwd) {
      const p = await mem.getAll({ filters: { user_id: scopeUser("project", projectCwd) }, topK: 500 });
      for (const r of p.results) out.push(this.toView(r, "project"));
    }
    return out;
  }

  /** Build-time seed for the system prompt (a small, recent slice — live recall is per-turn). */
  async forProject(projectCwd?: string | null): Promise<MemoryView[]> {
    const all = await this.list(projectCwd);
    return all.sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0)).slice(0, 40);
  }

  /** Semantic search with relevance threshold, folder/recency re-rank. The heart of recall. */
  async search(query: string, opts: SearchOpts = {}): Promise<MemoryView[]> {
    if (!query.trim()) return [];
    const mem = await this.engine();
    await this.ensureProjectLegacy(opts.projectCwd);
    const threshold = opts.threshold ?? 0.3;
    const topK = opts.topK ?? 8;
    const scopes: MemoryScope[] = opts.scope ? [opts.scope] : (opts.projectCwd ? ["project", "global"] : ["global"]);
    const collected: MemoryView[] = [];
    for (const sc of scopes) {
      const filters: Record<string, unknown> = { user_id: scopeUser(sc, opts.projectCwd) };
      if (opts.category) filters.category = opts.category;
      try {
        const r = await mem.search(query, { filters, topK: topK * 2, threshold });
        for (const it of r.results) collected.push(this.toView(it, sc));
      } catch { /* a scope with no rows can throw — ignore */ }
    }
    // 1-hop graph expansion: entities in the query + their neighbors give a small recall boost.
    const boost = new Set<string>();
    for (const sc of scopes) for (const e of memoryGraph.expand(scopeUser(sc, opts.projectCwd), query)) boost.add(e);
    return this.rerank(collected, opts.folder, boost).slice(0, topK);
  }

  /** Combine semantic score with recency decay, folder-match, and a graph-relationship boost. */
  private rerank(items: MemoryView[], folder?: string, boost?: Set<string>): MemoryView[] {
    const now = Date.now();
    const norm = (p: string) => p.replace(/\\/g, "/").replace(/^\.?\//, "").toLowerCase();
    const f = folder ? norm(folder) : null;
    const boostList = boost && boost.size ? [...boost] : null;
    return items
      .map((it) => {
        const base = it.score ?? 0;
        const age = now - (it.createdAt ?? now);
        const recency = Math.exp(-Math.max(0, age) / RECENCY_HALFLIFE_MS); // 1 → 0
        const folderBoost = f && it.folder && (norm(it.folder) === f || f.startsWith(norm(it.folder)) || norm(it.folder).startsWith(f)) ? 0.1 : 0;
        const text = (it.memory || "").toLowerCase();
        const graphBoost = boostList && boostList.some((e) => text.includes(e)) ? 0.05 : 0;
        return { it, rank: base * 0.8 + recency * 0.2 + folderBoost + graphBoost };
      })
      .sort((a, b) => b.rank - a.rank)
      .map((x) => x.it);
  }

  /** Observable entity/relationship graph for the UI/REST (global + the current project). */
  graphFor(projectCwd?: string | null): { global: GraphView; project: GraphView | null } {
    return {
      global: memoryGraph.graph(GLOBAL_USER),
      project: projectCwd ? memoryGraph.graph(scopeUser("project", projectCwd)) : null,
    };
  }

  /** Pre-task recall: search + emit observability event + render an injectable prompt block. */
  async recall(query: string, projectCwd?: string | null, folder?: string): Promise<{ items: MemoryView[]; block: string }> {
    let items: MemoryView[] = [];
    try { items = await this.search(query, { projectCwd, folder, topK: 8, threshold: 0.3 }); } catch { items = []; }
    const block = this.renderForPrompt(items, { recalled: true });
    emitRecall({ cwd: projectCwd ?? null, query, items });
    return { items, block };
  }

  /** Auto-capture a completed user↔assistant exchange (mem0 extracts durable facts via the LLM). */
  async captureExchange(userText: string, assistantText: string, projectCwd?: string | null): Promise<MemoryView[]> {
    const u = (userText || "").trim();
    const a = (assistantText || "").trim();
    if (u.length < 8 && a.length < 40) return []; // skip trivial exchanges
    const scope: MemoryScope = projectCwd ? "project" : "global";
    const userId = scopeUser(scope, projectCwd);
    const mem = await this.engine();
    await this.ensureProjectLegacy(projectCwd);
    const res = await mem.add(
      [{ role: "user", content: u }, { role: "assistant", content: a }],
      { userId, metadata: { scope, ts: Date.now() }, infer: true },
    );
    const views = (res.results || []).map((r) => this.toView(r, scope));
    for (const v of views) if (v.id) memoryGraph.indexMemory(userId, v.id, v.memory);
    if (views.length) {
      await this.writeMirror(scope, projectCwd);
      // Count actual CAPTURES (facts stored), not exchanges — mem0 extracts nothing from trivial turns,
      // and consolidation should fire per AUTO_CONSOLIDATE_EVERY captures, not once every 25 turns.
      this.capturesSinceConsolidate += views.length;
    }
    return views;
  }

  /** Run automatic consolidation if enough new captures have accumulated (non-blocking caller). */
  async maybeAutoConsolidate(projectCwd?: string | null): Promise<void> {
    if (this.capturesSinceConsolidate < AUTO_CONSOLIDATE_EVERY) return;
    this.capturesSinceConsolidate = 0;
    try { await this.consolidate(projectCwd); } catch { /* best-effort */ }
  }

  /** Merge/prune near-duplicate memories per scope (keeps the newest of each near-dup cluster). */
  async consolidate(projectCwd?: string | null): Promise<{ removed: number; kept: number }> {
    const mem = await this.engine();
    const scopes: Array<[MemoryScope, string]> = projectCwd
      ? [["project", scopeUser("project", projectCwd)], ["global", GLOBAL_USER]]
      : [["global", GLOBAL_USER]];
    let removed = 0, kept = 0;
    for (const [sc, userId] of scopes) {
      const all = (await mem.getAll({ filters: { user_id: userId }, topK: 1000 })).results;
      if (all.length > 1) {
        const embs = await localEmbedder.embedBatch(all.map((r) => r.memory));
        const tsOf = (r: MemoryItem) => (typeof r.metadata?.ts === "number" ? (r.metadata!.ts as number) : (r.createdAt ? Date.parse(r.createdAt) : 0));
        const dropped = new Set<string>();
        for (let i = 0; i < all.length; i++) {
          if (dropped.has(all[i].id)) continue;
          for (let j = i + 1; j < all.length; j++) {
            if (dropped.has(all[j].id)) continue;
            if (cosine(embs[i], embs[j]) > 0.97) {
              const dropI = tsOf(all[j]) >= tsOf(all[i]);
              dropped.add(dropI ? all[i].id : all[j].id);
              // If all[i] itself was just dropped, stop using it as the comparison anchor — else a
              // dropped item keeps matching later items and transitively prunes non-duplicates.
              if (dropI) break;
            }
          }
        }
        for (const id of dropped) { try { await mem.delete(id); memoryGraph.removeMemory(id); removed++; } catch { /* ignore */ } }
        kept += all.length - dropped.size;
      } else {
        kept += all.length;
      }
      await this.writeMirror(sc, projectCwd);
    }
    return { removed, kept };
  }

  async update(id: string, text: string, projectCwd?: string | null): Promise<MemoryView | null> {
    const mem = await this.engine();
    try { await mem.update(id, text); } catch { return null; }
    const it = await mem.get(id).catch(() => null);
    // Re-sync the entity graph with the edited text (add()/remove() maintain it — update() must too,
    // or the graph keeps the OLD text's entities and never gains the new ones).
    memoryGraph.removeMemory(id);
    if (it) {
      const scope = (it.metadata?.scope as MemoryScope) || "global";
      memoryGraph.indexMemory(scopeUser(scope, projectCwd), id, text);
    }
    await this.writeMirrorAll(projectCwd);
    return it ? this.toView(it, (it.metadata?.scope as MemoryScope) || "global") : null;
  }

  async remove(id: string, projectCwd?: string | null): Promise<boolean> {
    const mem = await this.engine();
    try { await mem.delete(id); } catch { return false; }
    memoryGraph.removeMemory(id);
    await this.writeMirrorAll(projectCwd);
    return true;
  }

  /** Render memories as an injectable system-prompt block (build-time seed or live recall). */
  renderForPrompt(items: MemoryView[], opts: { recalled?: boolean } = {}): string {
    if (!items.length) return "";
    const lines = items.map((e) => `- [${e.scope}${e.category ? "/" + e.category : ""}] ${e.memory}`);
    const title = opts.recalled ? "dotz recalled memory (relevant to this task)" : "dotz persistent memory (.ai-agents)";
    return `\n# ${title}\nDurable, agent-curated context — treat as authoritative project knowledge:\n${lines.join("\n")}\n`;
  }

  // ---- MEMORY.md mirror (git-committable source of truth; vector index is derived) ----
  private async writeMirror(scope: MemoryScope, projectCwd?: string | null): Promise<void> {
    try {
      const mem = await this.engine();
      if (scope === "global") {
        const items = (await mem.getAll({ filters: { user_id: GLOBAL_USER }, topK: 1000 })).results.map((r) => this.toView(r, "global"));
        await this.renderMirrorFile(path.join(aiAgentsDir(), "MEMORY.md"), "dotz global memory", items);
      } else if (projectCwd) {
        const items = (await mem.getAll({ filters: { user_id: scopeUser("project", projectCwd) }, topK: 1000 })).results.map((r) => this.toView(r, "project"));
        await this.renderMirrorFile(path.join(projectCwd, ".ai-agents", "MEMORY.md"), "dotz project memory", items);
      }
    } catch { /* mirror is best-effort, never blocks a write */ }
  }
  private async writeMirrorAll(projectCwd?: string | null): Promise<void> {
    await this.writeMirror("global");
    if (projectCwd) await this.writeMirror("project", projectCwd);
  }
  private async renderMirrorFile(file: string, title: string, items: MemoryView[]): Promise<void> {
    await fs.mkdir(path.dirname(file), { recursive: true });
    let out = `# ${title}\n\n> Generated by dotz from mem0 — human-readable, git-committable mirror. The vector index\n> under ~/.dotz/ai-agents/mem0 is a derived cache, rebuildable from this file.\n`;
    if (!items.length) { out += "\n_(no memories yet)_\n"; await fs.writeFile(file, out, "utf-8"); return; }
    const byCat = new Map<string, MemoryView[]>();
    for (const it of items) { const c = it.category || "general"; let arr = byCat.get(c); if (!arr) { arr = []; byCat.set(c, arr); } arr.push(it); }
    for (const [cat, list] of [...byCat.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
      out += `\n## ${cat}\n`;
      for (const it of list) out += `- ${it.memory}${it.folder ? `  _(folder: ${it.folder})_` : ""}\n`;
    }
    await fs.writeFile(file, out, "utf-8");
  }

  // ---- one-time migration from the legacy JSON memory store (kept as a backup) ----
  async importLegacy(projectCwd?: string | null): Promise<number> {
    let n = 0;
    const readOld = async (file: string): Promise<Array<{ key?: string; value?: string; scope?: string }>> => {
      try { return JSON.parse(await fs.readFile(file, "utf-8")); } catch { return []; }
    };
    // global
    const gMarker = path.join(mem0Dir(), ".migrated-global");
    if (!existsSync(gMarker)) {
      const old = await readOld(path.join(aiAgentsDir(), "memory.json"));
      for (const e of old) if (e.value) { await this.add({ text: e.key ? `${e.key}: ${e.value}` : e.value, scope: "global", category: e.key, infer: false }, { skipMirror: true }); n++; }
      if (n > 0) await this.writeMirror("global"); // one mirror write, not one full rewrite per entry
      await fs.mkdir(mem0Dir(), { recursive: true });
      await fs.writeFile(gMarker, new Date().toISOString(), "utf-8");
    }
    // project
    if (projectCwd) {
      const pMarker = path.join(projectCwd, ".ai-agents", ".mem0-migrated");
      if (!existsSync(pMarker)) {
        const old = await readOld(path.join(projectCwd, ".ai-agents", "memory.json"));
        let pn = 0;
        for (const e of old) if (e.value) { await this.add({ text: e.key ? `${e.key}: ${e.value}` : e.value, scope: "project", category: e.key, projectCwd, infer: false }, { skipMirror: true }); n++; pn++; }
        if (pn > 0) await this.writeMirror("project", projectCwd); // one mirror write, not one per entry
        await fs.mkdir(path.join(projectCwd, ".ai-agents"), { recursive: true });
        await fs.writeFile(pMarker, new Date().toISOString(), "utf-8");
      }
    }
    return n;
  }
}

export const memoryStore = new MemoryStore();
