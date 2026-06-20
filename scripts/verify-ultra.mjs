/** E2E: skills + workflows endpoints (ultra-code mode Phase 1).
 *  Boots the server in-process, exercises the unified skills pool and the workflow domain. */
import { buildServer } from "../src/server.ts";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// Isolate config + mem0 memory so this never mutates the operator's real ~/.dotz.
process.env.DOTZ_CONFIG_DIR = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-ultra-"));

const PORT = 4322;
const { app, pi } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;
const fails = [];
const ok = (c, m) => { console.log((c ? "  ✓ " : "  ✕ ") + m); if (!c) fails.push(m); };

// [1] skills pool — should discover 100+ skills across opencode/claude/codex/ecc/superpowers
const skillsRes = await (await fetch(base + "/api/skills")).json();
ok(Array.isArray(skillsRes.skills), `/api/skills returns array`);
ok(skillsRes.skills.length > 100, `skills pool large (got ${skillsRes.skills.length}, expect >100)`);
const sources = [...new Set(skillsRes.skills.map((s) => s.source))];
ok(sources.length >= 3, `multiple sources discovered: ${sources.join(", ")}`);
const hasSkillTool = skillsRes.skills.some((s) => s.name === "goal-driven-workflow" || s.name === "ultra-code-review" || s.name === "tdd-workflow");
ok(hasSkillTool, `known skill present in pool`);

// [2] skill detail — fetch a body
const sampleSkill = skillsRes.skills.find((s) => s.name === "goal-driven-workflow") || skillsRes.skills[0];
if (sampleSkill) {
  const detail = await (await fetch(base + "/api/skills/" + encodeURIComponent(sampleSkill.name))).json();
  ok(!!detail.body && detail.body.length > 50, `skill detail body loaded (${detail.body?.length} chars)`);
}

// [3] create a session (needed for workflow binding)
const sess = await (await fetch(base + "/api/sessions", {
  method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ profileId: "workflow" }),
})).json();
ok(!!sess.sessionId, `session created for workflows`);

// [4] tools — confirm skill + memory + agents_md tools are registered
const tools = await (await fetch(base + `/api/sessions/${sess.sessionId}/tools`)).json();
ok(tools.all.includes("skill"), `skill tool registered`);
ok(tools.all.includes("memory_list"), `memory_list tool registered`);
ok(tools.all.includes("memory_add"), `memory_add tool registered`);
ok(tools.all.includes("memory_search"), `memory_search tool registered`);
ok(tools.all.includes("memory_consolidate"), `memory_consolidate tool registered`);
ok(tools.all.includes("agents_md"), `agents_md tool registered`);
ok(tools.active.includes("skill"), `skill tool active in workflow profile`);
ok(tools.all.includes("create_agent"), `create_agent tool registered`);
ok(tools.all.includes("list_agents"), `list_agents tool registered`);
ok(tools.all.includes("create_skill"), `create_skill tool registered`);
ok(tools.all.includes("list_skills"), `list_skills tool registered`);

// [5] workflow create — a 3-step chain (scout → planner → worker)
const wfRun = await (await fetch(base + "/api/workflows", {
  method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({
    sessionId: sess.sessionId,
    label: "test: scout→planner→worker",
    origin: "verify-ultra",
    steps: [
      { agent: "scout", task: "map the codebase" },
      { agent: "planner", task: "produce a plan from {previous}", parents: [] }, // parents resolved below
      { agent: "worker", task: "implement the plan", parents: [] },
    ],
  }),
})).json();
ok(!!wfRun.id, `workflow run created`);
ok(wfRun.steps.length === 3, `workflow has 3 steps`);
ok(wfRun.status === "running", `workflow status = running (got ${wfRun.status})`);

// [6] workflow step state — mark step 0 done, verify step 1 becomes ready
const step0 = wfRun.steps[0];
const stepStateRes = await (await fetch(base + `/api/workflows/${wfRun.id}/step`, {
  method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ stepId: step0.id, status: "done", output: "codebase mapped" }),
})).json();
ok(stepStateRes.steps[0].status === "done", `step 0 marked done`);

// [7] workflow history
const hist = await (await fetch(base + "/api/workflows")).json();
ok(Array.isArray(hist.runs) && hist.runs.some((r) => r.id === wfRun.id), `workflow in history`);

// [8] workflow abort
const abortRes = await (await fetch(base + `/api/workflows/${wfRun.id}/abort`, {
  method: "POST",
})).json();
ok(abortRes.ok, `workflow abort returned ok`);

// [9] memory (mem0-backed) — create a global memory + recall it semantically
const memRes = await (await fetch(base + "/api/memory", {
  method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ text: "verify-ultra sentinel: the dashboard runs on port 4317", scope: "global" }),
})).json();
ok(!!memRes.id, `global memory created in mem0 store`);
const memList = await (await fetch(base + "/api/memory")).json();
ok(memList.entries.some((e) => /verify-ultra sentinel/.test(e.memory)), `global memory retrievable`);
const memSearch = await (await fetch(base + "/api/memory/search", {
  method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ query: "which port does the dashboard use", topK: 5, threshold: 0.1 }),
})).json();
ok(memSearch.results.some((r) => /4317/.test(r.memory)), `semantic recall surfaces the sentinel memory`);

// [10] Phase 2 tools — rsi_baseline, rsi_compare, human_gate, design_system, design_components, design_audit
ok(tools.all.includes("rsi_baseline"), `rsi_baseline tool registered`);
ok(tools.all.includes("rsi_compare"), `rsi_compare tool registered`);
ok(tools.all.includes("human_gate"), `human_gate tool registered`);
ok(tools.all.includes("design_system"), `design_system tool registered`);
ok(tools.all.includes("design_components"), `design_components tool registered`);
ok(tools.all.includes("design_audit"), `design_audit tool registered`);

// [11] commands — new workflow presets (ultra-code-review, e2e-test, self-improve)
const cmds = await (await fetch(base + `/api/sessions/${sess.sessionId}/commands`)).json();
const cmdNames = cmds.commands.map((c) => c.name);
ok(cmdNames.includes("ultra-code-review"), `/ultra-code-review preset registered`);
ok(cmdNames.includes("e2e-test"), `/e2e-test preset registered`);
ok(cmdNames.includes("self-improve"), `/self-improve preset registered`);

// [12] browser endpoints (stub in dev mode, but routes must exist)
const browserState = await (await fetch(base + "/api/browser/state")).json();
ok(typeof browserState.available === "boolean", `/api/browser/state responds (available=${browserState.available})`);

// [13] Ollama provider present + free-form
const provRes = await (await fetch(base + "/api/providers")).json();
const ollama = provRes.providers.find((p) => p.id === "ollama");
ok(!!ollama, `ollama provider registered`);
ok(!!ollama.freeForm, `ollama provider is free-form`);

// cleanup
if (memRes.id) await fetch(base + "/api/memory/" + memRes.id, { method: "DELETE" });
pi.disposeAll();
await app.close();
try { fs.rmSync(process.env.DOTZ_CONFIG_DIR, { recursive: true, force: true }); } catch {}
console.log("\n" + (fails.length ? `${fails.length} FAILURES:\n` + fails.map((f) => "  - " + f).join("\n") : "ALL ULTRA-CODE CHECKS PASSED"));
process.exit(fails.length ? 1 : 0);
