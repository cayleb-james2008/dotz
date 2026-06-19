/** Regression coverage for the self-improvement fixes that the other verify-*.mjs don't exercise:
 *   - Fix 2  : DOTZ_SKILLS_PATHS override adds a custom skill pool at highest priority
 *   - Simplify 3: renderIndex() caps the system-prompt skill index (footer summarizes overflow)
 *   - Fix 3  : workflow-bridge matches results positionally when the task string drifts, and sweeps
 *              any non-terminal step to error on tool-call end (graph never hangs "running")
 *   - Fix 5  : POST /api/sessions/:id/reload-context rebuilds the session (fresh id, profile kept,
 *              old session disposed)
 * Boots nothing live (no LLM turns). Exits 0 on success. */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SkillLoader } from "../src/skills.ts";
import { workflowBridge } from "../src/workflow-bridge.ts";
import { workflowStore } from "../src/workflows.ts";
import { buildServer } from "../src/server.ts";

const fails = [];
const ok = (c, m) => { console.log((c ? "  ✓ " : "  ✗ ") + m); if (!c) fails.push(m); };
const tick = () => new Promise((r) => setTimeout(r, 60));

// ---- Fix 2: DOTZ_SKILLS_PATHS override ----
console.log("\n[1] DOTZ_SKILLS_PATHS custom skill pool");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-skilltest-"));
const skillDir = path.join(tmp, "my-custom-skill");
fs.mkdirSync(skillDir, { recursive: true });
fs.writeFileSync(path.join(skillDir, "SKILL.md"), `---\nname: dotz-custom-test-skill\ndescription: a test skill discovered via DOTZ_SKILLS_PATHS\n---\nbody`);
process.env.DOTZ_SKILLS_PATHS = tmp;
const loader = new SkillLoader();
await loader.load();
ok(loader.has("dotz-custom-test-skill"), "custom skill discovered from DOTZ_SKILLS_PATHS dir");
ok(loader.get("dotz-custom-test-skill")?.source === "dotz", "custom skill tagged source=dotz (highest priority)");
delete process.env.DOTZ_SKILLS_PATHS;
fs.rmSync(tmp, { recursive: true, force: true });

// ---- Simplify 3: renderIndex cap ----
console.log("\n[2] renderIndex() prompt cap");
const total = loader.list().length;
const idx = loader.renderIndex();
const listed = (idx.match(/^- /gm) || []).length;
if (total > 80) {
  ok(idx.includes("showing first 80"), `index header notes cap (pool has ${total} skills)`);
  ok(/and \d+ more/.test(idx), "index has a '+M more' overflow footer");
  ok(listed === 81, `index lists 80 skills + 1 footer line (got ${listed})`);
} else {
  ok(!idx.includes("showing first"), `index not capped — pool has only ${total} skills (≤80)`);
  ok(listed === total, `index lists all ${total} skills (got ${listed})`);
}

// ---- Fix 3: workflow-bridge positional fallback + sweep ----
console.log("\n[3] workflow-bridge positional fallback + non-terminal sweep");
const SID = "verify-fixes-session";
// (a) two parallel steps; end reports only one, with a DRIFTED task string for the reported one.
const tc1 = "verify-fixes-tc1";
workflowBridge.handleEvent(SID, null, { type: "tool_execution_start", toolName: "subagent", toolCallId: tc1,
  args: { tasks: [{ agent: "worker", task: "implement the login form with validation" }, { agent: "worker", task: "write the tests" }] } });
await tick();
const run1 = workflowStore.list().filter((r) => r.origin === "subagent-bridge").pop();
ok(!!run1 && run1.steps.length === 2, "bridge created a 2-step run from tool_execution_start");
ok(run1 && run1.steps.every((s) => s.status === "running"), "both steps marked running after start");
workflowBridge.handleEvent(SID, null, { type: "tool_execution_end", toolName: "subagent", toolCallId: tc1, isError: false,
  result: { details: { mode: "parallel", agentScope: "", projectAgentsDir: null,
    results: [{ agent: "worker", task: "implement the login form", exitCode: 0 }] } } }); // task TRUNCATED → positional match (idx 0)
await tick();
const after1 = workflowStore.get(run1.id);
ok(after1.steps[0].status === "done", "step 0 matched positionally despite truncated task → done");
ok(after1.steps[1].status === "error", "unreported step 1 swept to error (not left running)");
ok(after1.steps.every((s) => s.status === "done" || s.status === "error" || s.status === "skipped"), "no step left in a non-terminal state");

// (b) tool-call end with NO details + isError → all steps swept to error
const tc2 = "verify-fixes-tc2";
workflowBridge.handleEvent(SID, null, { type: "tool_execution_start", toolName: "subagent", toolCallId: tc2,
  args: { agent: "worker", task: "a single task that crashes" } });
await tick();
const run2 = workflowStore.list().filter((r) => r.origin === "subagent-bridge").pop();
workflowBridge.handleEvent(SID, null, { type: "tool_execution_end", toolName: "subagent", toolCallId: tc2, isError: true, result: undefined });
await tick();
const after2 = workflowStore.get(run2.id);
ok(after2.steps.every((s) => s.status === "error"), "errored tool-call with no details → all steps error (graph doesn't hang)");

// ---- Fix 5: reload-context route ----
console.log("\n[4] POST /api/sessions/:id/reload-context");
const { app, pi } = await buildServer();
await app.listen({ host: "127.0.0.1", port: 4322 });
const base = "http://127.0.0.1:4322";
const post = async (p, b) => (await fetch(base + p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(b || {}) }));
const sess = await (await post("/api/sessions", { profileId: "solo" })).json();
const oldId = sess.sessionId;
const reloaded = await (await post(`/api/sessions/${oldId}/reload-context`)).json();
ok(!!reloaded.sessionId && reloaded.sessionId !== oldId, `reload-context returns a fresh sessionId (old=${oldId?.slice(0, 8)} new=${reloaded.sessionId?.slice(0, 8)})`);
ok(reloaded.profileId === "solo", `reload-context preserves profile (got ${reloaded.profileId})`);
const oldStatus = (await fetch(base + `/api/sessions/${oldId}`)).status;
ok(oldStatus === 404, `old session disposed after reload (GET → ${oldStatus})`);
if (reloaded.sessionId) pi.dispose(reloaded.sessionId);

// project-bound path: reload-context must PRESERVE a mid-session model override (pi.create now lets
// an explicit opts.model win over the project's pinned model — the review's medium finding).
const proj = await (await post("/api/projects", { name: "verify-fixes-proj", cwd: process.cwd(), model: { provider: "openrouter", modelId: "nex-agi/nex-n2-pro:free" } })).json();
try {
  const psess = await (await post("/api/sessions", { projectId: proj.id })).json();
  ok(psess.model?.modelId === "nex-agi/nex-n2-pro:free", `project session starts on the project's pinned model (got ${psess.model?.modelId})`);
  await post(`/api/sessions/${psess.sessionId}/model`, { provider: "openrouter", modelId: "some/override-model:free" });
  const preloaded = await (await post(`/api/sessions/${psess.sessionId}/reload-context`)).json();
  ok(preloaded.model?.modelId === "some/override-model:free", `reload-context preserves the live model override for a project-bound session (got ${preloaded.model?.modelId})`);
  if (preloaded.sessionId) pi.dispose(preloaded.sessionId);
} finally {
  await fetch(base + `/api/projects/${proj.id}`, { method: "DELETE" });
}
await app.close();

console.log("\n" + (fails.length ? `${fails.length} FAILED` : "ALL FIX CHECKS PASSED"));
process.exitCode = fails.length ? 1 : 0;
