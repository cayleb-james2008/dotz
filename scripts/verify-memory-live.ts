/**
 * LIVE verification of mem0 autonomous capture → recall against Ollama Cloud (infer:true uses the
 * real LLM for fact extraction). Gated on $OLLAMA_API_KEY — skips (exit 0) when absent. Uses the
 * bundled local embedder; isolated via DOTZ_CONFIG_DIR + a temp project dir.
 *
 *   npx tsx scripts/verify-memory-live.ts
 */
import os from "node:os";
import path from "node:path";
import fs from "node:fs";

if (!process.env.OLLAMA_API_KEY) {
  console.log("SKIP: $OLLAMA_API_KEY not set — live memory test skipped (offline checks cover the rest).");
  process.exit(0);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-mem-live-"));
process.env.DOTZ_CONFIG_DIR = path.join(tmp, "dotz");
const projectCwd = path.join(tmp, "project");
fs.mkdirSync(projectCwd, { recursive: true });

let failures = 0;
const ok = (c: boolean, m: string) => { console.log(`${c ? "PASS" : "FAIL"}  ${m}`); if (!c) failures++; };

const { memoryStore } = await import("../src/memory");

try {
  // Capture from a realistic exchange — mem0's LLM extracts the durable facts (infer:true).
  const views = await memoryStore.captureExchange(
    "Note for the future: this project deploys with `npm run dist`, and the dashboard listens on port 4317.",
    "Understood — I'll remember the deploy command and the dashboard port.",
    projectCwd,
  );
  ok(views.length > 0, `captureExchange extracted ${views.length} durable fact(s) via Ollama Cloud`);

  // Recall them on a semantically related query.
  const { items } = await memoryStore.recall("how do I deploy this app and which port is the dashboard on", projectCwd);
  ok(items.some((i) => /dist|4317|deploy|port/i.test(i.memory)), `recall surfaces a captured fact (${items.length} hits)`);

  console.log(failures === 0 ? "\nLIVE MEMORY CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
} catch (e) {
  console.error("LIVE_ERROR:", (e as Error).stack || (e as Error).message);
  failures++;
} finally {
  try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* ignore */ }
}
process.exit(failures === 0 ? 0 : 1);
