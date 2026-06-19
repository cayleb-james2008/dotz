/**
 * End-to-end verification of the new dotz features: projects, memory, sandbox (terminal + web),
 * multi-provider model surface, and project-bound sessions. Boots the server in-process,
 * exercises every new REST endpoint + WS sandbox flow, then shuts down and exits 0 on success.
 *
 * NOTE: this script creates real files under a temp project cwd and runs real child processes
 * (node/bash). It cleans up the temp dir and kills all sandbox runs on exit.
 */
import { buildServer } from "../src/server.ts";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";

// Isolate config + mem0 memory in a temp dir so this never mutates the operator's real ~/.dotz.
process.env.DOTZ_CONFIG_DIR = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-cfg-"));

const PORT = 4321;
const { app, pi } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;

const failures = [];
const assert = (cond, msg) => { if (!cond) failures.push(msg); console.log((cond ? "  ✓ " : "  ✕ ") + msg); };
const get = async (p) => (await fetch(base + p)).json();
const post = async (p, body) => (await fetch(base + p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body || {}) })).json();
const patch = async (p, body) => (await fetch(base + p, { method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify(body || {}) })).json();
const del = async (p) => (await fetch(base + p, { method: "DELETE" })).json();

// ---- 1. providers ----
console.log("\n[1] GET /api/providers");
const { providers } = await get("/api/providers");
assert(providers.length >= 5, `providers count = ${providers.length} (>=5 expected)`);
assert(providers.some((p) => p.id === "openrouter" && p.freeForm), "openrouter provider present + freeForm");
assert(providers.some((p) => p.id === "anthropic"), "anthropic provider present");

// ---- 2. projects CRUD ----
console.log("\n[2] projects CRUD");
const tempCwd = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-test-"));
const proj = await post("/api/projects", { name: "test-project", cwd: tempCwd, profileId: "solo" });
assert(proj.id && proj.name === "test-project", `project created: ${proj.id}`);
assert(proj.cwd === tempCwd, `project cwd pinned: ${proj.cwd}`);
assert(proj.profileId === "solo", `project profile pinned: ${proj.profileId}`);
  const projList = await get("/api/projects");
  assert(projList.projects.length >= 1, `projects list = ${projList.projects.length} (expected >=1 due to persistent demo project)`);
const projGet = await get(`/api/projects/${proj.id}`);
assert(projGet.id === proj.id, "GET /api/projects/:id returns the project");
const patched = await patch(`/api/projects/${proj.id}`, { name: "test-project-renamed" });
assert(patched.name === "test-project-renamed", `project renamed: ${patched.name}`);
assert(patched.updatedAt >= proj.updatedAt, "project updatedAt bumped");

// ---- 3. memory CRUD (mem0-backed) ----
console.log("\n[3] memory CRUD");
const mem = await post("/api/memory", { projectId: proj.id, text: "use tabs not spaces", category: "convention", scope: "project" });
assert(mem.id && /tabs/.test(mem.memory), `memory created: ${mem.id}`);
const memList = await get(`/api/memory?projectId=${proj.id}`);
assert(memList.entries.length === 1, `memory list for project = ${memList.entries.length} (expected 1)`);
assert(/tabs/.test(memList.entries[0].memory), "memory text matches");
const globalMem = await post("/api/memory", { projectId: "", text: "always verify before claiming done", scope: "global" });
const memListGlobal = await get(`/api/memory?projectId=${proj.id}`);
assert(memListGlobal.entries.length === 2, `memory list includes global entries = ${memListGlobal.entries.length} (expected 2)`);
const searchRes = await post("/api/memory/search", { projectId: proj.id, query: "what indentation style", topK: 5, threshold: 0.1 });
assert(Array.isArray(searchRes.results) && searchRes.results.some((r) => /tabs/.test(r.memory)), `semantic search surfaces the convention (${searchRes.results.length} hits)`);
const patchedMem = await patch(`/api/memory/${mem.id}`, { text: "use 2-space indent", projectId: proj.id });
assert(/2-space/.test(patchedMem.memory), `memory updated: ${patchedMem.memory}`);
const delMem = await del(`/api/memory/${mem.id}?projectId=${proj.id}`);
assert(delMem.ok, "memory entry deleted");

// ---- 4. project-bound session ----
console.log("\n[4] project-bound session");
const boundSess = await post("/api/sessions", { projectId: proj.id });
assert(boundSess.projectId === proj.id, `session bound to project: ${boundSess.projectId}`);
assert(boundSess.profileId === "solo", `session inherited project profile (solo): ${boundSess.profileId}`);
const sessDetail = await get(`/api/sessions/${boundSess.sessionId}`);
assert(sessDetail.projectId === proj.id, "session detail has projectId");
pi.dispose(boundSess.sessionId);

// ---- 5. sandbox terminal run ----
console.log("\n[5] sandbox terminal run");
const langs = await get("/api/sandbox/languages");
// Pick a shell language that exists on this OS. bash on POSIX, powershell on Windows.
const shellLang = process.platform === "win32" ? "powershell" : "bash";
assert(langs.languages.includes(shellLang), `${shellLang} language available: ${langs.languages.join(",")}`);
const echoCmd = shellLang === "powershell" ? 'Write-Output "hello-from-sandbox"' : 'echo hello-from-sandbox';
const run = await post("/api/sandbox/runs", { language: shellLang, code: echoCmd, mode: "terminal" });
assert(run.id && run.status === "running", `sandbox run started: ${run.id} status=${run.status}`);
// Wait for the run to complete (poll up to 8s — shell startup on Windows can be slow).
let finished = null;
for (let i = 0; i < 80; i++) {
  await new Promise((r) => setTimeout(r, 100));
  const r = await get(`/api/sandbox/runs/${run.id}`);
  if (r.status === "done" || r.status === "error") { finished = r; break; }
}
assert(finished, "sandbox run completed within 8s");
assert(finished && finished.status === "done", `sandbox run status = ${finished && finished.status} (expected done)`);
assert(finished && finished.output.includes("hello-from-sandbox"), `sandbox output includes echo: ${(finished && finished.output || "").slice(0, 100)}`);
assert(finished && finished.exitCode === 0, `sandbox exitCode = ${finished && finished.exitCode} (expected 0)`);

// ---- 6. sandbox kill ----
console.log("\n[6] sandbox kill (long-running)");
const longCode = shellLang === "powershell" ? "Start-Sleep -Seconds 30; Write-Output done" : "sleep 30; echo done";
const longRun = await post("/api/sandbox/runs", { language: shellLang, code: longCode, mode: "terminal", timeoutMs: 60000 });
const killed = await post(`/api/sandbox/runs/${longRun.id}/kill`, {});
assert(killed.ok, "sandbox kill returned ok");
// Wait briefly for the kill to take effect.
await new Promise((r) => setTimeout(r, 400));
const longRunAfter = await get(`/api/sandbox/runs/${longRun.id}`);
assert(longRunAfter.status === "killed" || longRunAfter.status === "error", `long run status after kill = ${longRunAfter.status}`);

// ---- 7. multi-provider model surface ----
console.log("\n[7] multi-provider model surface");
const sess = await post("/api/sessions", { profileId: "workflow" });
const models = await get(`/api/sessions/${sess.sessionId}/models`);
assert(models.providerMeta && models.providerMeta.length >= 5, `models response includes providerMeta (${models.providerMeta && models.providerMeta.length} providers)`);
assert(models.providers && models.providers.length >= 1, `models response includes providers list`);
assert(models.available && models.available.length > 0, `available models non-empty (${models.available && models.available.length})`);
// Resolve an openrouter custom model id (free-form).
const orModel = await post(`/api/sessions/${sess.sessionId}/model`, { provider: "openrouter", modelId: "some/custom-model:free" });
assert(orModel.model && orModel.model.modelId === "some/custom-model:free", `free-form openrouter model resolved: ${orModel.model && orModel.model.modelId}`);
pi.dispose(sess.sessionId);

// ---- 8. web-mode sandbox port detection ----
console.log("\n[8] sandbox web mode (node http server + port detection)");
// Start a tiny node http server that listens on an ephemeral port and prints it.
const webCode = `import http from 'node:http';\nconst s = http.createServer((req,res)=>res.end('dotz-sandbox-web'));\ns.listen(0,()=>console.log('listening on port ' + s.address().port));\nsetTimeout(()=>s.close(), 25000);\n`;
const webRun = await post("/api/sandbox/runs", { language: "javascript", code: webCode, mode: "web", timeoutMs: 30000 });
assert(webRun.id, `web sandbox run started: ${webRun.id}`);
// Poll for port detection (up to 8s — node startup + port probe).
let detectedPort = null;
for (let i = 0; i < 80; i++) {
  await new Promise((r) => setTimeout(r, 100));
  try {
    const r = await get(`/api/sandbox/runs/${webRun.id}/port`);
    if (r.port) { detectedPort = r.port; break; }
  } catch { /* 404 until port detected */ }
}
assert(detectedPort && detectedPort > 1024, `web sandbox port detected = ${detectedPort}`);
// Fetch the sandboxed server to confirm it actually serves.
if (detectedPort) {
  const resp = await fetch(`http://127.0.0.1:${detectedPort}`);
  const body = await resp.text();
  assert(resp.status === 200 && body === "dotz-sandbox-web", `sandboxed http server responds: status=${resp.status} body=${body.slice(0, 40)}`);
}
await post(`/api/sandbox/runs/${webRun.id}/kill`, {});

// ---- cleanup ----
console.log("\n[cleanup]");
await del(`/api/projects/${proj.id}`);
await del(`/api/memory/${globalMem.id}`);
pi.disposeAll();
await app.close();
try { await fs.rm(tempCwd, { recursive: true, force: true }); } catch {}
try { await fs.rm(process.env.DOTZ_CONFIG_DIR, { recursive: true, force: true }); } catch {}

console.log("\n" + (failures.length === 0 ? "ALL NEW-FEATURE CHECKS PASSED" : `${failures.length} FAILURES:`));
for (const f of failures) console.log("  - " + f);
process.exit(failures.length === 0 ? 0 : 1);