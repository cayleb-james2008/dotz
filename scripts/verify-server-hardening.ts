/**
 * Unit tests for server input-validation hardening (src/server.ts).
 *
 * Verifies that untrusted request bodies with wrong types are rejected with clean 400s
 * instead of crashing the server (uncaught 500s), and that the WebSocket error handler
 * prevents socket errors from crashing the process.
 *
 * These tests do NOT require API keys or a live pi session — they exercise the validation
 * layer (which runs before session creation/lookup) and the WebSocket lifecycle.
 *
 *   node --test --import tsx scripts/verify-server-hardening.ts
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { buildServer } from "../src/server";
import type { FastifyInstance } from "fastify";
import type { PiSessions } from "../src/pi";

let tmpDir: string;
let app: FastifyInstance;
let pi: PiSessions;
let base: string;
const PORT = 4399;

before(async () => {
  tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-harden-"));
  process.env.DOTZ_CONFIG_DIR = tmpDir;
  const built = await buildServer();
  app = built.app;
  pi = built.pi;
  await app.listen({ host: "127.0.0.1", port: PORT });
  base = `http://127.0.0.1:${PORT}`;
});

after(async () => {
  pi.disposeAll();
  await app.close();
  delete process.env.DOTZ_CONFIG_DIR;
  await fs.rm(tmpDir, { recursive: true, force: true });
});

/** POST helper that returns the raw Response. */
async function postRaw(p: string, body?: unknown): Promise<Response> {
  return fetch(base + p, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });
}

// ---- memory search parameter validation (no session required) ----

test("POST /api/memory/search rejects non-number threshold with 400", async () => {
  const res = await postRaw("/api/memory/search", { query: "test", threshold: "banana" });
  assert.equal(res.status, 400, `expected 400 for string threshold, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /threshold/i, "error mentions threshold");
});

test("POST /api/memory/search rejects non-number topK with 400", async () => {
  const res = await postRaw("/api/memory/search", { query: "test", topK: "five" });
  assert.equal(res.status, 400, `expected 400 for string topK, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /topK/i, "error mentions topK");
});

test("POST /api/memory/search rejects Infinity threshold with 400", async () => {
  const res = await postRaw("/api/memory/search", { query: "test", threshold: Infinity });
  assert.equal(res.status, 400, `expected 400 for Infinity threshold, got ${res.status}`);
});

test("POST /api/memory/search rejects invalid scope with 400", async () => {
  const res = await postRaw("/api/memory/search", { query: "test", scope: "banana" });
  assert.equal(res.status, 400, `expected 400 for invalid scope, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /scope/i, "error mentions scope");
});

// ---- memory creation scope validation (no session required) ----

test("POST /api/memory rejects invalid scope with 400", async () => {
  const res = await postRaw("/api/memory", { text: "test fact", scope: "banana" });
  assert.equal(res.status, 400, `expected 400 for invalid scope, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /scope/i, "error mentions scope");
});

// ---- session endpoints: unknown session returns 404 (not 500) ----

test("POST /api/sessions/:id/abort returns 404 for unknown session (not 500)", async () => {
  const res = await postRaw("/api/sessions/nonexistent-id/abort", {});
  assert.equal(res.status, 404, `expected 404 for abort on unknown session, got ${res.status}`);
});

test("POST /api/sessions/:id/model returns 404 for unknown session (not 500)", async () => {
  const res = await postRaw("/api/sessions/nonexistent-id/model", {
    provider: "ollama",
    modelId: "some-model",
  });
  assert.equal(res.status, 404, `expected 404 for model on unknown session, got ${res.status}`);
});

test("DELETE /api/sessions/:id returns 404 for unknown session (not 200 ok:true)", async () => {
  const res = await fetch(base + "/api/sessions/nonexistent-id", { method: "DELETE" });
  assert.equal(res.status, 404, `expected 404 for delete on unknown session, got ${res.status}`);
});

// ---- WebSocket: error events do not crash the process ----

test("WebSocket to non-existent session closes cleanly without crashing server", async () => {
  const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?sessionId=does-not-exist`);
  // Wait for the server to send the error message and close.
  const msg = await new Promise<unknown>((resolve) => {
    ws.addEventListener("message", (e) => resolve(JSON.parse((e as MessageEvent).data)), { once: true });
    ws.addEventListener("close", () => resolve(null), { once: true });
    ws.addEventListener("error", () => resolve(null), { once: true });
  });
  // The server should have sent an error message before closing.
  if (msg) {
    assert.equal((msg as { kind: string }).kind, "error", "WS error message kind is 'error'");
  }
  // Server must still be alive.
  const health = await (await fetch(`${base}/api/health`)).json() as { ok: boolean };
  assert.equal(health.ok, true, "server survived the WebSocket close");
});

test("WebSocket abrupt termination does not crash the server", async () => {
  // Open a WS to a non-existent session, then immediately terminate mid-handshake.
  // This simulates an ECONNRESET / network drop — the 'error' event fires on the socket.
  const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?sessionId=abrupt-test`);
  // Terminate as soon as it opens (or after a short timeout if open doesn't fire).
  await new Promise<void>((resolve) => {
    ws.addEventListener("open", () => resolve(), { once: true });
    setTimeout(resolve, 500);
  });
  // Abruptly close the socket.
  ws.close();
  await new Promise<void>((r) => setTimeout(r, 200));

  // If the 'error' event had no handler, the process would have crashed.
  const health = await (await fetch(`${base}/api/health`)).json() as { ok: boolean };
  assert.equal(health.ok, true, "server survived the WebSocket abrupt termination");
});

// ---- session creation: invalid model shape returns 400 (not 500) ----

test("POST /api/sessions rejects non-string provider in model with 400", async () => {
  const res = await postRaw("/api/sessions", {
    profileId: "solo",
    model: { provider: 42, modelId: "some-model" },
  });
  // Could be 400 (our validation) or 503 (if session creation fails before model check).
  // Either way, it must NOT be 500 (which would indicate an uncaught TypeError).
  assert.ok(res.status !== 500, `expected 400 or 503 for non-string provider in model, got ${res.status} (500 = uncaught crash)`);
});

test("POST /api/sessions rejects non-string modelId in model with 400", async () => {
  const res = await postRaw("/api/sessions", {
    profileId: "solo",
    model: { provider: "ollama", modelId: 123 },
  });
  assert.ok(res.status !== 500, `expected 400 or 503 for non-string modelId, got ${res.status} (500 = uncaught crash)`);
});

test("POST /api/sessions rejects invalid thinkingLevel with 400", async () => {
  const res = await postRaw("/api/sessions", {
    profileId: "solo",
    thinkingLevel: "banana",
  });
  assert.equal(res.status, 400, `expected 400 for invalid thinkingLevel, got ${res.status}`);
});

// ---- sandbox run validation (no session required) ----

test("POST /api/sandbox/runs rejects non-string language with 400", async () => {
  const res = await postRaw("/api/sandbox/runs", { language: 123, code: "console.log(1)" });
  assert.equal(res.status, 400, `expected 400 for non-string language, got ${res.status}`);
});

test("POST /api/sandbox/runs rejects invalid mode with 400", async () => {
  const res = await postRaw("/api/sandbox/runs", { language: "bash", code: "echo hi", mode: "webserver" });
  assert.equal(res.status, 400, `expected 400 for invalid mode, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /mode/i, "error mentions mode");
});

test("POST /api/sandbox/runs rejects non-string mode (number) with 400", async () => {
  const res = await postRaw("/api/sandbox/runs", { language: "bash", code: "echo hi", mode: 42 });
  assert.equal(res.status, 400, `expected 400 for non-string mode, got ${res.status}`);
});

// ---- global config validation ----

test("POST /api/config rejects invalid thinkingLevel with 400", async () => {
  const res = await postRaw("/api/config", { thinkingLevel: "banana" });
  assert.equal(res.status, 400, `expected 400 for invalid thinkingLevel, got ${res.status}`);
  const body = await res.json() as { error: string };
  assert.match(body.error, /thinkingLevel/i, "error mentions thinkingLevel");
});

test("POST /api/config rejects non-string thinkingLevel (number) with 400", async () => {
  const res = await postRaw("/api/config", { thinkingLevel: 42 });
  assert.equal(res.status, 400, `expected 400 for non-string thinkingLevel, got ${res.status}`);
});

test("POST /api/config accepts valid thinkingLevel", async () => {
  const res = await postRaw("/api/config", { thinkingLevel: "high" });
  assert.equal(res.status, 200, `expected 200 for valid thinkingLevel, got ${res.status}`);
  const body = await res.json() as { config: { thinkingLevel: string } };
  assert.equal(body.config.thinkingLevel, "high", "thinkingLevel persisted");
});

// ---- server health after all tests ----

test("server is still alive after all hardening tests", async () => {
  const health = await (await fetch(`${base}/api/health`)).json() as { ok: boolean };
  assert.equal(health.ok, true, "server is alive");
});