/** Phase 2 check: exercise the control REST endpoints against a running dotz server. */
export {};
const base = `http://127.0.0.1:${process.env.DOTZ_PORT || 4317}`;
const j = async (p: string, init?: RequestInit) => {
  const r = await fetch(base + p, init);
  return { status: r.status, body: (await r.json().catch(() => null)) as any };
};
const post = (p: string, b: unknown) =>
  j(p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(b) });

const sess = (await post("/api/sessions", {})).body;
const id = sess.sessionId as string;
console.log("created:", id, "| model:", JSON.stringify(sess.model));

const models = (await j(`/api/sessions/${id}/models`)).body;
console.log(`models: available=${models.available.length} providers=${models.providers.length} default=${JSON.stringify(models.default)}`);

console.log("thinking→off:", JSON.stringify((await post(`/api/sessions/${id}/thinking`, { level: "off" })).body));
console.log("tools GET:", JSON.stringify((await j(`/api/sessions/${id}/tools`)).body));
console.log("tools SET[read,bash]:", JSON.stringify((await post(`/api/sessions/${id}/tools`, { tools: ["read", "bash"] })).body));

const cmds = (await j(`/api/sessions/${id}/commands`)).body;
const bySource = cmds.commands.reduce((a: Record<string, number>, c: any) => ((a[c.source] = (a[c.source] || 0) + 1), a), {});
console.log(`commands: ${cmds.commands.length} bySource=${JSON.stringify(bySource)} sample=${JSON.stringify(cmds.commands.slice(0, 4).map((c: any) => c.name))}`);

const sw = (await post(`/api/sessions/${id}/model`, { provider: "openrouter", modelId: "openai/gpt-oss-20b:free" })).body;
console.log("switch to custom openrouter id →", JSON.stringify(sw?.model));

const state = (await j(`/api/sessions/${id}`)).body;
console.log("state:", JSON.stringify({ model: state.model, thinking: state.thinkingLevel, tools: state.tools, ctx: state.stats?.contextUsage }));

await j(`/api/sessions/${id}`, { method: "DELETE" });
console.log("disposed ok");
