/**
 * Unit tests for dotz global config loading/persistence (src/config.ts).
 *
 * Verifies that a corrupt/hand-edited config.json (e.g. an invalid thinkingLevel)
 * is sanitized on load, and that updateConfig rejects invalid thinking levels.
 *
 *   node --test --import tsx scripts/verify-config.ts
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { loadConfig, getConfig, updateConfig } from "../src/config";
import { VALID_THINKING_LEVELS } from "../src/types";

let tmpDir: string;

before(async () => {
  tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-config-"));
  process.env.DOTZ_CONFIG_DIR = tmpDir;
});

after(async () => {
  delete process.env.DOTZ_CONFIG_DIR;
  await fs.rm(tmpDir, { recursive: true, force: true });
});

function configPath() {
  return path.join(tmpDir, "config.json");
}

test("VALID_THINKING_LEVELS contains the expected closed set", () => {
  const expected: string[] = ["off", "minimal", "low", "medium", "high", "xhigh"];
  assert.equal(VALID_THINKING_LEVELS.size, expected.length);
  for (const lvl of expected) {
    assert.ok(VALID_THINKING_LEVELS.has(lvl as import("../src/types").ThinkingLevel), `expected ${lvl}`);
  }
});

test("loadConfig clamps invalid persisted thinkingLevel to default", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ thinkingLevel: "banana", provider: "ollama" }));
  const cfg = await loadConfig();
  assert.equal(cfg.thinkingLevel, "high", "invalid thinkingLevel should clamp to default high");
  assert.equal(cfg.provider, "ollama", "other valid fields are preserved");
});

test("loadConfig ignores garbage thinkingLevel type and keeps default", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ thinkingLevel: 42 }));
  const cfg = await loadConfig();
  assert.equal(cfg.thinkingLevel, "high", "non-string thinkingLevel should keep default");
});

test("updateConfig rejects invalid thinkingLevel", async () => {
  const before = getConfig();
  const cfg = await updateConfig({ thinkingLevel: "banana" as any });
  assert.equal(cfg.thinkingLevel, before.thinkingLevel, "invalid thinkingLevel must not be persisted");
});

test("updateConfig persists valid thinkingLevel", async () => {
  const cfg = await updateConfig({ thinkingLevel: "medium" });
  assert.equal(cfg.thinkingLevel, "medium", "valid thinkingLevel is applied in memory");
  const raw = JSON.parse(await fs.readFile(configPath(), "utf-8"));
  assert.equal(raw.thinkingLevel, "medium", "valid thinkingLevel is persisted to disk");
});

test("loadConfig reads a valid config unchanged", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ provider: "openrouter", executiveModel: "nex-agi/nex-n2-pro:free", subagentModel: "minimax-m3", thinkingLevel: "low" }));
  const cfg = await loadConfig();
  assert.equal(cfg.provider, "openrouter");
  assert.equal(cfg.thinkingLevel, "low");
});
