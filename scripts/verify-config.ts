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
import { loadConfig, getConfig, updateConfig, executiveModelRef } from "../src/config";
import { PROVIDERS, PROVIDER_DEFAULTS, VALID_THINKING_LEVELS } from "../src/types";

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

test("loadConfig clamps unknown provider to default", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ provider: "banana", executiveModel: "glm-5.2", subagentModel: "minimax-m3", thinkingLevel: "low" }));
  const cfg = await loadConfig();
  assert.equal(cfg.provider, "ollama", "unknown provider should fall back to default");
});

test("loadConfig clamps empty/whitespace model ids to defaults", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ provider: "openrouter", executiveModel: "   ", subagentModel: "", thinkingLevel: "low" }));
  const cfg = await loadConfig();
  assert.equal(cfg.provider, "openrouter", "valid provider is kept");
  assert.equal(cfg.executiveModel, PROVIDER_DEFAULTS.ollama.executive, "empty executiveModel falls back");
  assert.equal(cfg.subagentModel, PROVIDER_DEFAULTS.ollama.subagent, "empty subagentModel falls back");
});

test("updateConfig ignores unknown provider", async () => {
  const before = getConfig();
  const cfg = await updateConfig({ provider: "not-a-provider" as any });
  assert.equal(cfg.provider, before.provider, "unknown provider must not change current provider");
});

test("updateConfig trims and lowercases provider, trims model ids", async () => {
  const cfg = await updateConfig({ provider: "  OpenRouter  ", executiveModel: "  some/model  ", subagentModel: " other " });
  assert.equal(cfg.provider, "openrouter", "provider is trimmed and lowercased");
  assert.equal(cfg.executiveModel, "some/model", "executiveModel is trimmed");
  assert.equal(cfg.subagentModel, "other", "subagentModel is trimmed");
  const raw = JSON.parse(await fs.readFile(configPath(), "utf-8"));
  assert.equal(raw.provider, "openrouter");
  assert.equal(raw.executiveModel, "some/model");
});

test("executiveModelRef returns a valid model ref after sanitization", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ provider: "bad-provider", executiveModel: "", subagentModel: "  ", thinkingLevel: "medium" }));
  await loadConfig();
  const ref = executiveModelRef();
  assert.ok(ref.provider && PROVIDERS.some((p) => p.id === ref.provider), "ref provider is known");
  assert.ok(ref.modelId, "ref modelId is non-empty");
});

test("loadConfig reads a valid config unchanged", async () => {
  await fs.writeFile(configPath(), JSON.stringify({ provider: "openrouter", executiveModel: "nex-agi/nex-n2-pro:free", subagentModel: "minimax-m3", thinkingLevel: "low" }));
  const cfg = await loadConfig();
  assert.equal(cfg.provider, "openrouter");
  assert.equal(cfg.thinkingLevel, "low");
});
