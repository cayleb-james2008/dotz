/** Verify the Ollama Cloud provider registers + the executive default binds, WITHOUT a live chat
 *  call (no Ollama Cloud usage consumed). The live turn is a separate step. */
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
// Isolate config so the ollama/glm-5.2 executive-default assertion is deterministic and never
// reads the operator's real ~/.dotz/config.json (config.ts resolves the dir lazily).
process.env.DOTZ_CONFIG_DIR = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-cfgtest-"));
const { buildServer } = await import("../src/server.ts");
const { resolveModel } = await import("../src/pi.ts");

const fails = [];
const ok = (c, m) => { console.log((c ? "  ✓ " : "  ✗ ") + m); if (!c) fails.push(m); };

const { app, pi } = await buildServer();
const e = await pi.create({ profileId: "solo" });
const reg = e.session.modelRegistry;
const provs = [...new Set(reg.getAll().map((m) => m.provider))];
ok(provs.includes("ollama"), `ollama provider registered`);

const glm = reg.find("ollama", "glm-5.2");
ok(!!glm, `ollama/glm-5.2 present in registry`);
console.log("    glm-5.2 model object:", JSON.stringify(glm));

const cur = e.session.model;
ok(!!cur && cur.provider === "ollama" && cur.id === "glm-5.2", `session executive default = ollama/glm-5.2 (got ${cur && cur.provider}/${cur && cur.id})`);

const kimi = resolveModel(e.session, { provider: "ollama", modelId: "kimi-k2.6" });
ok(!!kimi, `free-form ollama/kimi-k2.6 resolves (clone)`);
console.log("    kimi clone object:", JSON.stringify(kimi));

// Auth resolution regression guard (no live call): the provider's "$OLLAMA_API_KEY" reference must
// resolve to a REAL key value. A bare "OLLAMA_API_KEY" (or a stray "$") would reach the bearer
// header as a literal and Ollama returns 401 — which pi swallows into an empty reply. Key shape
// only is checked; the value is never logged. Skipped when the env key is absent.
if (process.env.OLLAMA_API_KEY) {
  const auth = await reg.getApiKeyAndHeaders(glm);
  ok(
    !!auth?.ok && !!auth.apiKey && auth.apiKey !== "$OLLAMA_API_KEY" && auth.apiKey !== "OLLAMA_API_KEY" && String(auth.apiKey).length > 16,
    `$OLLAMA_API_KEY resolves to a real key (not the literal reference → would 401)`
  );
} else {
  console.log("  ⚠ OLLAMA_API_KEY not set — skipping auth-resolution check");
}

pi.dispose(e.id);
console.log(fails.length ? `\n${fails.length} FAILED` : "\nOLLAMA PROVIDER REGISTRATION OK");
await app.close();
fs.rmSync(process.env.DOTZ_CONFIG_DIR, { recursive: true, force: true });
// process.exitCode (not process.exit) so the loop drains and we avoid the Windows libuv teardown race.
process.exitCode = fails.length ? 1 : 0;
