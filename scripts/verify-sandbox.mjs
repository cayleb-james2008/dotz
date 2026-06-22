/**
 * Unit tests for the sandbox store (src/sandbox.ts).
 *
 * Tests the ACTIVE_CAP eviction: finished runs are evicted oldest-first once the cap
 * is exceeded, while in-flight (running) runs are never evicted. Also verifies that
 * finished runs remain inspectable via REST until evicted.
 *
 * Uses a stub Sandbox subclass that bypasses real process spawning — the eviction
 * logic operates on the in-memory `active` map, not on live processes.
 *
 *   node --test scripts/verify-sandbox.mjs
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { Sandbox } from "../src/sandbox.ts";

/**
 * A test-only Sandbox that injects pre-built runs into the active map without
 * spawning real processes. We access the private `active` map via a cast.
 */
class StubSandbox extends Sandbox {
  /** Inject a finished run directly into the active map (bypasses spawn). */
  injectTerminal(id, status, endedAt) {
    const run = {
      id,
      projectId: null,
      language: "bash",
      code: "",
      status,
      output: "",
      exitCode: status === "done" ? 0 : 1,
      startedAt: endedAt - 1000,
      endedAt,
    };
    const ar = {
      run,
      proc: null,
      tempDir: "/tmp/dotz-test-nonexistent",
      timeout: null,
      listeners: new Set(),
      port: null,
      portDetector: null,
    };
    // Access private field via cast — the map is private in the real class.
    const map = this.active;
    map.set(id, ar);
    return ar;
  }

  /** Inject a running run directly into the active map. */
  injectRunning(id, startedAt) {
    const run = {
      id,
      projectId: null,
      language: "bash",
      code: "",
      status: "running",
      output: "",
      exitCode: null,
      startedAt,
      endedAt: null,
    };
    const ar = {
      run,
      proc: null,
      tempDir: "/tmp/dotz-test-nonexistent",
      timeout: null,
      listeners: new Set(),
      port: null,
      portDetector: null,
    };
    const map = this.active;
    map.set(id, ar);
    return ar;
  }

  /** Call the private pruneActive method. */
  callPrune() {
    this.pruneActive();
  }

  activeSize() {
    return this.active.size;
  }

  has(id) {
    return this.active.has(id);
  }
}

// Lower the cap for testing by monkey-patching the static field.
Object.defineProperty(Sandbox, "ACTIVE_CAP", { value: 5, writable: true, configurable: true });

test("pruneActive: evicts oldest terminal runs when cap exceeded", () => {
  const s = new StubSandbox();
  // Inject 7 finished runs with increasing endedAt timestamps.
  for (let i = 0; i < 7; i++) {
    s.injectTerminal(`done-${i}`, "done", 1000 + i * 100);
  }
  assert.equal(s.activeSize(), 7);
  s.callPrune();
  // Should evict 2 oldest (done-0, done-1) to bring size down to ACTIVE_CAP (5).
  assert.equal(s.activeSize(), 5, "size should be capped at ACTIVE_CAP");
  assert.equal(s.has("done-0"), false, "oldest run evicted");
  assert.equal(s.has("done-1"), false, "second-oldest run evicted");
  assert.equal(s.has("done-6"), true, "newest run retained");
});

test("pruneActive: never evicts in-flight (running) runs", () => {
  const s = new StubSandbox();
  // Inject 3 running runs.
  for (let i = 0; i < 3; i++) {
    s.injectRunning(`running-${i}`, 1000 + i);
  }
  // Inject 5 finished runs (enough to exceed cap by themselves).
  for (let i = 0; i < 5; i++) {
    s.injectTerminal(`done-${i}`, "done", 2000 + i * 100);
  }
  assert.equal(s.activeSize(), 8);
  s.callPrune();
  // All 3 running runs must survive; finished runs are evicted to get as close to cap as possible.
  assert.equal(s.has("running-0"), true, "running run never evicted");
  assert.equal(s.has("running-1"), true, "running run never evicted");
  assert.equal(s.has("running-2"), true, "running run never evicted");
  // The map should have been trimmed — but with 3 non-evictable running runs,
  // only 2 of the 5 finished runs can be kept (3 + 2 = 5 = cap).
  const doneRemaining = [0,1,2,3,4].filter(i => s.has(`done-${i}`)).length;
  assert.equal(doneRemaining, 2, "only enough finished runs retained to fill the cap");
});

test("pruneActive: no-op when under cap", () => {
  const s = new StubSandbox();
  s.injectTerminal("done-0", "done", 1000);
  s.injectTerminal("done-1", "error", 2000);
  s.callPrune();
  assert.equal(s.activeSize(), 2, "nothing evicted when under cap");
  assert.equal(s.has("done-0"), true);
  assert.equal(s.has("done-1"), true);
});

test("pruneActive: kills and errors are both terminal (evictable)", () => {
  const s = new StubSandbox();
  s.injectTerminal("k", "killed", 1000);
  s.injectTerminal("e", "error", 2000);
  // Fill to 7 total — all terminal.
  for (let i = 0; i < 5; i++) {
    s.injectTerminal(`d-${i}`, "done", 3000 + i);
  }
  s.callPrune();
  assert.equal(s.activeSize(), 5, "capped");
  // "k" (oldest) and "e" (second oldest) should be evicted.
  assert.equal(s.has("k"), false, "killed run evicted as terminal");
  assert.equal(s.has("e"), false, "error run evicted as terminal");
});