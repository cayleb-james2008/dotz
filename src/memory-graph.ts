/**
 * dotz memory graph — a LEAN, on-device entity/relationship layer for mem0 memories.
 *
 * mem0-ts has no graph store (Python-only) and its external graph backends (Neo4j/FalkorDB) would
 * break the single-exe constraint. mem0 *does* link entities→memories internally for recall; this
 * module adds the explicit entity↔entity *relationships* the operator asked for, the lean way:
 * a single extra table in the SAME embedded sqlite stack (better-sqlite3, no new dependency, no
 * LLM, no network). Entities are extracted heuristically (code identifiers, paths, CapWords); a
 * co-occurrence edge is recorded between every pair of entities that appear in the same memory.
 *
 * Used for (a) a small 1-hop recall boost in MemoryStore.search, and (b) an observable graph via
 * GET /api/memory/graph. Partitioned by `owner` (mem0 userId: "__global__" or "proj:<cwd>").
 */
import path from "node:path";
import os from "node:os";
import { mkdirSync } from "node:fs";
import Database from "better-sqlite3";

const dotzDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
const graphDbPath = () => path.join(dotzDir(), "ai-agents", "mem0", "graph.db");

const STOP = new Set(
  ("the a an and or but for to of in on at by with is are was were be been being this that these those it its as from into your you our we they them then than so do does did not no yes can will would should could may might must has have had use used using the then with this that have your into about will more most some such only also other into over under after before".split(
    /\s+/,
  )),
);

/** Heuristic entity extraction: backtick terms, dotted/dashed identifiers, paths, camelCase,
 *  snake_case, ALLCAPS, and Capitalized words. Cheap, deterministic, offline. */
export function extractEntities(text: string): string[] {
  const out = new Set<string>();
  const t = String(text || "");
  // backtick`code` | dotted/dashed/path.identifiers | snake_case | camelCase (lower or upper) | ALLCAPS | CapWord
  const re = /`([^`]+)`|([A-Za-z_$][\w$]*(?:[./\\-][A-Za-z_$][\w$]*)+)|([a-z]+_[a-z0-9_]+)|([a-zA-Z][a-z0-9]*(?:[A-Z][a-z0-9]+)+)|([A-Z]{2,})|([A-Z][a-z]{2,})/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(t))) {
    const tok = (m.slice(1).find((g) => g) || m[0] || "").trim();
    const key = tok.toLowerCase();
    if (tok.length >= 3 && tok.length <= 64 && !STOP.has(key)) out.add(tok);
    if (out.size >= 24) break;
  }
  return [...out];
}

export interface GraphView {
  nodes: Array<{ entity: string; count: number }>;
  edges: Array<{ src: string; dst: string; weight: number }>;
}

class MemoryGraph {
  private db: Database.Database | null = null;

  private database(): Database.Database {
    if (this.db) return this.db;
    mkdirSync(path.dirname(graphDbPath()), { recursive: true });
    const db = new Database(graphDbPath());
    db.pragma("journal_mode = WAL");
    db.exec(`
      CREATE TABLE IF NOT EXISTS nodes (owner TEXT, entity TEXT, count INTEGER DEFAULT 0, PRIMARY KEY (owner, entity));
      CREATE TABLE IF NOT EXISTS edges (owner TEXT, src TEXT, dst TEXT, weight INTEGER DEFAULT 0, PRIMARY KEY (owner, src, dst));
      CREATE TABLE IF NOT EXISTS mem_entities (owner TEXT, memory_id TEXT, entity TEXT, PRIMARY KEY (owner, memory_id, entity));
      CREATE INDEX IF NOT EXISTS idx_edges_src ON edges (owner, src);
      CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges (owner, dst);
      CREATE INDEX IF NOT EXISTS idx_mem ON mem_entities (memory_id);
    `);
    this.db = db;
    return db;
  }

  /** Index a memory: upsert its entities as nodes and pairwise co-occurrence edges. */
  indexMemory(owner: string, memoryId: string, text: string): void {
    try {
      const ents = extractEntities(text);
      if (ents.length === 0) return;
      const db = this.database();
      const upNode = db.prepare("INSERT INTO nodes (owner, entity, count) VALUES (?, ?, 1) ON CONFLICT(owner, entity) DO UPDATE SET count = count + 1");
      const upMem = db.prepare("INSERT OR IGNORE INTO mem_entities (owner, memory_id, entity) VALUES (?, ?, ?)");
      const upEdge = db.prepare("INSERT INTO edges (owner, src, dst, weight) VALUES (?, ?, ?, 1) ON CONFLICT(owner, src, dst) DO UPDATE SET weight = weight + 1");
      const tx = db.transaction(() => {
        for (const e of ents) { upNode.run(owner, e); upMem.run(owner, memoryId, e); }
        for (let i = 0; i < ents.length; i++) {
          for (let j = i + 1; j < ents.length; j++) {
            const [a, b] = ents[i].toLowerCase() < ents[j].toLowerCase() ? [ents[i], ents[j]] : [ents[j], ents[i]];
            upEdge.run(owner, a, b);
          }
        }
      });
      tx();
    } catch { /* graph is an enhancement; never block a memory write */ }
  }

  /** Drop a memory's entity links (counts left as-is — harmless aggregate, keeps this O(1)). */
  removeMemory(memoryId: string): void {
    try { this.database().prepare("DELETE FROM mem_entities WHERE memory_id = ?").run(memoryId); } catch { /* ignore */ }
  }

  /** Query entities + their 1-hop neighbors (lowercased) for a recall boost set. */
  expand(owner: string, text: string): Set<string> {
    const set = new Set<string>();
    try {
      const ents = extractEntities(text);
      if (ents.length === 0) return set;
      const db = this.database();
      const nbr = db.prepare("SELECT src, dst FROM edges WHERE owner = ? AND (src = ? OR dst = ?) ORDER BY weight DESC LIMIT 6");
      for (const e of ents) {
        set.add(e.toLowerCase());
        for (const row of nbr.all(owner, e, e) as Array<{ src: string; dst: string }>) {
          set.add(row.src.toLowerCase());
          set.add(row.dst.toLowerCase());
        }
      }
    } catch { /* ignore */ }
    return set;
  }

  /** Observable graph for an owner (for GET /api/memory/graph). */
  graph(owner: string, limit = 200): GraphView {
    try {
      const db = this.database();
      const nodes = db.prepare("SELECT entity, count FROM nodes WHERE owner = ? ORDER BY count DESC LIMIT ?").all(owner, limit) as Array<{ entity: string; count: number }>;
      const edges = db.prepare("SELECT src, dst, weight FROM edges WHERE owner = ? ORDER BY weight DESC LIMIT ?").all(owner, limit) as Array<{ src: string; dst: string; weight: number }>;
      return { nodes, edges };
    } catch { return { nodes: [], edges: [] }; }
  }
}

export const memoryGraph = new MemoryGraph();
