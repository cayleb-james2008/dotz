/**
 * Offline, deterministic verification of the mem0-backed memory store (src/memory.ts).
 * Uses the BUNDLED local embedder (transformers.js) and infer:false (no LLM) — so it runs with
 * ZERO network and ZERO API keys. Isolated via DOTZ_CONFIG_DIR + a temp project dir.
 *
 *   npx tsx scripts/verify-memory.ts
 */
import os from "node:os";
import path from "node:path";
import fs from "node:fs";

const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-mem-verify-"));
process.env.DOTZ_CONFIG_DIR = path.join(tmpRoot, "dotz");
const projectCwd = path.join(tmpRoot, "project");
fs.mkdirSync(projectCwd, { recursive: true });

let failures = 0;
function ok(cond: boolean, msg: string) {
  console.log(`${cond ? "PASS" : "FAIL"}  ${msg}`);
  if (!cond) failures++;
}

const { memoryStore } = await import("../src/memory");

try {
  // 1. create global + project memories (verbatim, infer:false)
  const g = await memoryStore.create({ text: "Always run npm run typecheck before claiming done.", scope: "global", category: "feedback" });
  ok(!!g.id && g.scope === "global", `global memory created: ${g.id.slice(0, 8)}`);
  const p = await memoryStore.create({ text: "The portable exe is built with npm run dist.", scope: "project", category: "command", folder: "scripts", projectCwd });
  ok(!!p.id && p.scope === "project" && p.category === "command" && p.folder === "scripts", `project memory created with category+folder`);

  // 2. list: global-only vs project+global
  const globalOnly = await memoryStore.list(null);
  ok(globalOnly.length === 1 && globalOnly[0].scope === "global", `list(null) returns global only (${globalOnly.length})`);
  const both = await memoryStore.list(projectCwd);
  ok(both.length === 2, `list(projectCwd) returns project + global (${both.length})`);

  // 3. semantic search finds the relevant memory + carries a score
  const hits = await memoryStore.search("how do I build the windows executable", { projectCwd, topK: 5, threshold: 0.1 });
  ok(hits.length > 0 && /npm run dist/.test(hits[0].memory), `search surfaces the build command (top: "${hits[0]?.memory ?? "—"}")`);
  ok(typeof hits[0]?.score === "number", `search results carry a relevance score (${hits[0]?.score?.toFixed(3)})`);

  // 4. MEMORY.md mirror written (git-committable source of truth)
  const projMirror = path.join(projectCwd, ".ai-agents", "MEMORY.md");
  const globalMirror = path.join(process.env.DOTZ_CONFIG_DIR!, "ai-agents", "MEMORY.md");
  ok(fs.existsSync(projMirror) && /npm run dist/.test(fs.readFileSync(projMirror, "utf-8")), `project MEMORY.md mirror written`);
  ok(fs.existsSync(globalMirror) && /typecheck/.test(fs.readFileSync(globalMirror, "utf-8")), `global MEMORY.md mirror written`);

  // 5. update + delete
  const upd = await memoryStore.update(p.id, "The portable exe is built with `npm run dist` (Windows).", projectCwd);
  ok(!!upd && /Windows/.test(upd.memory), `update rewrites memory text`);
  const removed = await memoryStore.remove(g.id, projectCwd);
  ok(removed === true, `remove deletes a memory`);
  const afterDel = await memoryStore.list(projectCwd);
  ok(afterDel.length === 1, `list after delete = 1 (${afterDel.length})`);

  // 6. consolidation drops a near-duplicate
  await memoryStore.create({ text: "The portable exe is built with `npm run dist` on Windows.", scope: "project", category: "command", projectCwd });
  const before = (await memoryStore.list(projectCwd)).filter((m) => m.scope === "project").length;
  const con = await memoryStore.consolidate(projectCwd);
  const after = (await memoryStore.list(projectCwd)).filter((m) => m.scope === "project").length;
  ok(con.removed >= 1 && after < before, `consolidate removed ${con.removed} near-duplicate(s) (${before} → ${after})`);

  // 7. entity/relationship graph (offline, heuristic extraction + co-occurrence edges)
  await memoryStore.create({ text: "buildResourceLoader injects memory via appendSystemPrompt.", scope: "project", category: "architecture", projectCwd });
  await memoryStore.create({ text: "buildResourceLoader also loads the skillLoader index.", scope: "project", category: "architecture", projectCwd });
  const graph = memoryStore.graphFor(projectCwd);
  const hasNode = graph.project?.nodes.some((n) => /buildResourceLoader/i.test(n.entity)) ?? false;
  const hasEdge = (graph.project?.edges.length ?? 0) > 0;
  ok(hasNode, `graph extracted entity node (buildResourceLoader)`);
  ok(hasEdge, `graph recorded co-occurrence edge(s) (${graph.project?.edges.length ?? 0})`);

  console.log(failures === 0 ? "\nALL MEMORY CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
} catch (e) {
  console.error("VERIFY_MEMORY_ERROR:", (e as Error).stack || (e as Error).message);
  failures++;
} finally {
  try { fs.rmSync(tmpRoot, { recursive: true, force: true }); } catch { /* ignore */ }
}
process.exit(failures === 0 ? 0 : 1);
