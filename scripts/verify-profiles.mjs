/**
 * End-to-end verification that profiles + the .pi bundle are wired in.
 * Boots the dotz server in-process, creates a workflow session, and asserts:
 *   - /api/profiles returns the 5 profiles
 *   - session summary carries profileId:"workflow"
 *   - /tools includes the `subagent` tool (from the bundled extension)
 *   - /commands surfaces the workflow prompts (/implement, /scout-and-plan, /implement-and-review)
 * Then shuts down and exits 0 on success, non-zero on failure.
 */
import { buildServer } from "../src/server.ts";

const PORT = 4319;

const { app, pi } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;

const failures = [];
const assert = (cond, msg) => { if (!cond) failures.push(msg); console.log((cond ? "  ✓ " : "  ✕ ") + msg); };

const get = async (p) => (await fetch(base + p)).json();
const post = async (p, body) => (await fetch(base + p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body || {}) })).json();

console.log("\n[1] /api/profiles");
const { profiles, default: defProf } = await get("/api/profiles");
assert(profiles.length === 5, `profiles count = ${profiles.length} (expected 5)`);
assert(defProf === "workflow", `default profile = ${defProf} (expected workflow)`);
assert(profiles.some((p) => p.id === "workflow" && p.workflow), "workflow profile present + workflow:true");

console.log("\n[2] POST /api/sessions {profileId:workflow}");
const sess = await post("/api/sessions", { profileId: "workflow" });
assert(sess.profileId === "workflow", `session.profileId = ${sess.profileId} (expected workflow)`);
assert(sess.model && sess.model.modelId === "nex-agi/nex-n2-pro:free", `default model = ${sess.model && sess.model.modelId}`);
const sid = sess.sessionId;

console.log("\n[3] GET /api/sessions/:id/tools — expect subagent tool active");
const tools = await get(`/api/sessions/${sid}/tools`);
console.log("    active:", JSON.stringify(tools.active));
console.log("    all:   ", JSON.stringify(tools.all));
assert(tools.all.includes("subagent"), "subagent tool registered by the extension");
assert(tools.active.includes("subagent"), "subagent tool is ACTIVE (profile allows all tools)");

console.log("\n[4] GET /api/sessions/:id/commands — expect workflow prompts + extension cmd");
const { commands } = await get(`/api/sessions/${sid}/commands`);
const names = commands.map((c) => c.name);
console.log("    commands:", JSON.stringify(names));
assert(commands.length > 0, `commands non-empty (got ${commands.length})`);
assert(names.includes("implement"), "/implement prompt template present");
assert(names.includes("scout-and-plan"), "/scout-and-plan prompt template present");
assert(names.includes("implement-and-review"), "/implement-and-review prompt template present");

console.log("\n[5] POST /api/sessions {profileId:plan} — expect read-only tools, no edit/write");
const planSess = await post("/api/sessions", { profileId: "plan" });
assert(planSess.profileId === "plan", `plan session.profileId = ${planSess.profileId}`);
assert(planSess.tools.includes("subagent") && !planSess.tools.includes("edit"), `plan tools = ${JSON.stringify(planSess.tools)} (read-only + subagent)`);
pi.dispose(planSess.sessionId);

console.log("\n[6] GET /api/sessions — list reflects active session");
const list = await get("/api/sessions");
assert(list.length === 1 && list[0].profileId === "workflow", `sessions list = ${list.length}, first.profileId = ${list[0] && list[0].profileId}`);

// cleanup
pi.dispose(sid);
await app.close();

console.log("\n" + (failures.length === 0 ? "ALL CHECKS PASSED" : `${failures.length} FAILURES:`));
for (const f of failures) console.log("  - " + f);
process.exit(failures.length === 0 ? 0 : 1);