/**
 * End-to-end verification of the AGENTS.md doctrine editor panel.
 * Boots the dotz server in-process, exercises the new REST surface
 * (GET /api/agents_md + PATCH /api/agents_md), confirms disk persistence,
 * and smoke-tests the UI template + wiring. Exits 0 on success.
 */
import { buildServer } from "../src/server.ts";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";

// Isolate config so the test never mutates the operator's real ~/.dotz.
process.env.DOTZ_CONFIG_DIR = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-cfg-"));

const PORT = 4322;
const { app } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;

const failures = [];
const ok = (cond, msg) => { console.log((cond ? "  ✓ " : "  ✕ ") + msg); if (!cond) failures.push(msg); };
const apiReq = async (method, p, body) => {
  const opts = { method, headers: {} };
  if (body) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
  const r = await fetch(base + p, opts);
  const b = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(b.error || r.statusText);
  return b;
};
const get = (p) => apiReq("GET", p);
const patch = (p, b) => apiReq("PATCH", p, b);
const post = (p, b) => apiReq("POST", p, b);

try {
  // ---- 1. create a temp project ----
  console.log("\n[1] project setup");
  const tempCwd = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-doctrine-"));
  const proj = await post("/api/projects", { name: "doctrine-test", cwd: tempCwd, profileId: "workflow" });
  ok(proj.id && proj.cwd === tempCwd, `project created: ${proj.id}`);

  // ---- 2. GET returns empty content when no AGENTS.md exists ----
  console.log("\n[2] GET /api/agents_md");
  const read1 = await get(`/api/agents_md?projectId=${proj.id}`);
  ok(read1.content === "", "GET returns empty content when no AGENTS.md");
  ok(read1.path.endsWith("AGENTS.md"), "GET path points to AGENTS.md");

  // ---- 3. PATCH writes full doctrine back ----
  console.log("\n[3] PATCH /api/agents_md");
  const doctrine = "# Test Doctrine\n\n- Use tabs, not spaces.\n- Always run `npm run typecheck` before claiming done.\n";
  const write = await patch(`/api/agents_md?projectId=${proj.id}`, { content: doctrine });
  ok(write.ok === true, "PATCH returns ok");

  // ---- 4. GET reflects the write and disk matches ----
  console.log("\n[4] round-trip + disk persistence");
  const read2 = await get(`/api/agents_md?projectId=${proj.id}`);
  ok(read2.content === doctrine, "GET returns updated content");
  const disk = await fs.readFile(path.join(tempCwd, "AGENTS.md"), "utf-8");
  ok(disk === doctrine, "AGENTS.md persisted to disk");

  // ---- 5. validation guards ----
  console.log("\n[5] validation");
  const bad = await fetch(`${base}/api/agents_md?projectId=${proj.id}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ content: 123 }),
  });
  ok(!bad.ok, "PATCH rejects non-string content");
  const badBody = await bad.json().catch(() => ({}));
  ok(!!badBody.error, "error surfaced in response body");

  // ---- 6. UI smoke ----
  console.log("\n[6] UI wiring");
  const html = await (await fetch(base + "/")).text();
  ok(html.includes('id="tpl-doctrine"'), "index.html contains #tpl-doctrine");
  ok(html.includes('data-panel="doctrine"'), "template declares doctrine panel");
  const js = await (await fetch(base + "/app.js")).text();
  ok(js.includes("wireDoctrinePanel"), "app.js defines wireDoctrinePanel");
  ok(js.includes("/api/agents_md"), "app.js calls /api/agents_md");
  const css = await (await fetch(base + "/styles.css")).text();
  ok(css.includes(".doctrine-body"), "styles.css has doctrine panel styles");

  // ---- cleanup ----
  await fetch(`${base}/api/projects/${proj.id}`, { method: "DELETE" });
  try { await fs.rm(tempCwd, { recursive: true, force: true }); } catch {}
} catch (e) {
  console.error("\nunexpected failure:", e.message);
  failures.push(e.message);
}

await app.close();
try { await fs.rm(process.env.DOTZ_CONFIG_DIR, { recursive: true, force: true }); } catch {}

console.log("\n" + (failures.length === 0 ? "AGENTS.MD DOCTRINE CHECKS PASSED" : `${failures.length} FAILURES:`));
for (const f of failures) console.log("  - " + f);
process.exitCode = failures.length === 0 ? 0 : 1;
