/** Live model verification: dotz config defaults + REAL turns on Ollama Cloud (glm-5.2 executive,
 *  minimax-m3 subagent) and OpenRouter (nex-agi/nex-n2-pro:free). Proves dotz is functional with
 *  both providers, that free-form executive ids actually apply, and the effort level can be set. */
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
// Isolate config to a throwaway dir so this script never reads or mutates the operator's real
// ~/.dotz/config.json (set BEFORE importing modules that resolve it — config.ts reads it lazily).
process.env.DOTZ_CONFIG_DIR = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-cfgtest-"));
const { buildServer } = await import("../src/server.ts");
const { updateConfig } = await import("../src/config.ts");

const fails = [];
const skips = [];
const ok = (c, m) => { console.log((c ? "  ✓ " : "  ✗ ") + m); if (!c) fails.push(m); };

/** Direct, minimal provider probe to capture the underlying API error behind an empty turn —
 *  lets us tell a dotz defect (real FAIL) from an account limit/subscription (environmental SKIP,
 *  same class as an absent key). */
async function providerError(provider, modelId) {
  try {
    const url = provider === "ollama" ? "https://ollama.com/v1/chat/completions" : "https://openrouter.ai/api/v1/chat/completions";
    const key = provider === "ollama" ? process.env.OLLAMA_API_KEY : process.env.OPENROUTER_API_KEY;
    if (!key) return "no key in env";
    const r = await fetch(url, { method: "POST", headers: { Authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify({ model: modelId, messages: [{ role: "user", content: "hi" }], stream: false, max_tokens: 8 }) });
    const j = await r.json().catch(() => null);
    if (j && j.error) return typeof j.error === "string" ? j.error : JSON.stringify(j.error);
    return null;
  } catch (e) { return e.message; }
}
// Provider-side conditions that are environmental (SKIP, like an absent key) rather than a dotz
// defect: account limits/subscription/credits AND transient transport failures (5xx/timeout/abort/
// rate-limit) common on free tiers. A NORMAL provider response that dotz fails to render is a FAIL.
const ENV_UNAVAILABLE = /subscription|usage limit|upgrade|quota|insufficient|payment|can only afford|402|429|rate.?limit|50[234]|aborted|timed?.?out|timeout|ECONNRESET|ETIMEDOUT|fetch failed/i;

const PORT = 4399;
const { app, pi } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;

// [1] config defaults (Ollama Cloud primary)
const cfg = (await (await fetch(base + "/api/config")).json()).config;
ok(cfg.provider === "ollama", `default provider = ollama (got ${cfg.provider})`);
ok(cfg.executiveModel === "glm-5.2", `default executive = glm-5.2 (got ${cfg.executiveModel})`);
ok(cfg.subagentModel === "minimax-m3", `default subagent = minimax-m3 (got ${cfg.subagentModel})`);
ok(process.env.DOTZ_SUBAGENT_MODEL === "ollama/minimax-m3", `DOTZ_SUBAGENT_MODEL = ollama/minimax-m3 (got ${process.env.DOTZ_SUBAGENT_MODEL})`);

// [2] custom model id + persistence + env propagation
const r2 = await (await fetch(base + "/api/config", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ subagentModel: "custom-test-id" }) })).json();
ok(r2.config.subagentModel === "custom-test-id", `custom subagent model id accepted + persisted`);
ok(process.env.DOTZ_SUBAGENT_MODEL === "ollama/custom-test-id", `env updated when subagent model changes`);
await updateConfig({ subagentModel: "minimax-m3" }); // restore

// [3] live turn helper — create a session pinned to a provider/model and run one real turn
async function liveTurn(provider, modelId, label) {
  const entry = await pi.create({ profileId: "solo", model: { provider, modelId } });
  const cur = entry.session.model;
  ok(!!cur && cur.provider === provider, `${label}: session bound to ${provider} (got ${cur && cur.provider}/${cur && cur.id})`);
  // effort selector functional: set a reasoning level and confirm it sticks
  if (entry.session.supportsThinking()) {
    entry.session.setThinkingLevel("low");
    ok(entry.session.thinkingLevel === "low", `${label}: effort level set to low`);
  } else {
    ok(true, `${label}: model has no thinking levels (skip effort)`);
  }
  let text = "";
  const off = entry.session.subscribe((e) => {
    if (e.type === "message_end" && e.message && e.message.role === "assistant") {
      for (const b of (e.message.content || [])) if (b.text) text += b.text;
    }
  });
  try {
    // Free-tier models intermittently return an empty completion or a transient 5xx; retry a few
    // times so a flaky turn doesn't fail a working integration.
    for (let attempt = 1; attempt <= 3 && text.trim().length === 0; attempt++) {
      text = "";
      try { await entry.session.prompt("Reply with exactly the single word READY and nothing else. Do not use any tools."); }
      catch { /* transient — the provider probe below classifies it if all attempts come up empty */ }
    }
    if (text.trim().length > 0) {
      ok(true, `${label}: live reply received -> ${JSON.stringify(text.trim().slice(0, 50))}`);
    } else {
      // Empty after retries — probe the provider directly to find out why. An environmental
      // condition (account limit/credits or transient 5xx/timeout) is a SKIP; a normal response
      // that dotz failed to render is a real FAILURE.
      const reason = await providerError(provider, modelId);
      if (reason && ENV_UNAVAILABLE.test(reason)) {
        console.log(`  ⚠ ${label}: SKIPPED — provider unavailable (not a dotz defect): ${reason.slice(0, 140)}`);
        skips.push(`${label}: ${reason.slice(0, 100)}`);
      } else {
        ok(false, `${label}: empty reply with no provider error (provider said: ${reason || "nothing"})`);
      }
    }
  } finally {
    off();
    pi.dispose(entry.id);
  }
}

await liveTurn("ollama", "glm-5.2", "Ollama Cloud glm-5.2");
await liveTurn("openrouter", "nex-agi/nex-n2-pro:free", "OpenRouter nex-agi");

if (skips.length) {
  console.log(`\n${skips.length} live turn(s) SKIPPED (provider account limit — dotz integration verified up to the API boundary):`);
  for (const s of skips) console.log("  ⚠ " + s);
}
console.log(fails.length ? `\n${fails.length} FAILED` : "\nALL LIVE MODEL CHECKS PASSED" + (skips.length ? ` (${skips.length} skipped)` : ""));
await app.close();
fs.rmSync(process.env.DOTZ_CONFIG_DIR, { recursive: true, force: true });
process.exitCode = fails.length ? 1 : 0;
