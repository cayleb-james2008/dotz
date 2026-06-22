/**
 * Unit + portability tests for src/metrics.ts.
 *
 * - countFiles must work on this repo WITHOUT relying on Windows-only `findstr`.
 * - parseTestCounts must extract pass/fail counts from node:test / vitest / jest output.
 *
 * Uses DOTZ_CONFIG_DIR isolation so any baseline files land in a temp dir.
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { countFiles, parseTestCounts, captureBaseline } from "../src/metrics";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

let tmpDir: string;
let originalConfigDir: string | undefined;

before(async () => {
  tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-metrics-test-"));
  originalConfigDir = process.env.DOTZ_CONFIG_DIR;
  process.env.DOTZ_CONFIG_DIR = tmpDir;
});

after(async () => {
  if (originalConfigDir !== undefined) process.env.DOTZ_CONFIG_DIR = originalConfigDir;
  else delete process.env.DOTZ_CONFIG_DIR;
  await fs.rm(tmpDir, { recursive: true, force: true });
});

test("countFiles: returns positive counts for the dotz repo", async () => {
  const { fileCount, testFiles } = await countFiles(REPO_ROOT);
  assert.ok(fileCount > 10, `expected tracked files, got ${fileCount}`);
  assert.ok(testFiles > 0, `expected at least one test file, got ${testFiles}`);
});

test("countFiles: excludes build/dependency trees", async () => {
  const { fileCount } = await countFiles(REPO_ROOT);
  // node_modules and dist are gitignored but also explicitly filtered; a naïve run without
  // filtering would blow up the count. This repo should have < 500 tracked source files.
  assert.ok(fileCount < 500, `expected <500 tracked files, got ${fileCount} — filter may be missing build dirs`);
});

test("parseTestCounts: node:test TAP output", () => {
  const out = `
TAP version 13
# Subtest: first
ok 1 - first
not ok 2 - second
1..2
# pass 1
# fail 1
`;
  assert.deepEqual(parseTestCounts(out), { passed: 1, failed: 1 });
});

test("parseTestCounts: vitest output (prefers final test count over file count)", () => {
  // Vitest prints "Test Files 3 passed" before "Tests 12 passed"; we must pick the latter.
  const out = `
Test Files 3 passed (3)
Tests 12 passed (12)
`;
  assert.deepEqual(parseTestCounts(out), { passed: 12, failed: 0 });
});

test("parseTestCounts: jest output", () => {
  const out = `Tests: 14 passed, 2 failed, 16 total`;
  assert.deepEqual(parseTestCounts(out), { passed: 14, failed: 2 });
});

test("parseTestCounts: returns zeros for empty/unparseable output", () => {
  assert.deepEqual(parseTestCounts(""), { passed: 0, failed: 0 });
  assert.deepEqual(parseTestCounts("random log line"), { passed: 0, failed: 0 });
});

test("captureBaseline: captures the dotz repo state with file counts", async () => {
  const baseline = await captureBaseline(REPO_ROOT);
  assert.ok(baseline.fileCount > 0, "baseline should count tracked files");
  assert.ok(baseline.testFiles > 0, "baseline should count test files");
  assert.equal(typeof baseline.timestamp, "number");
  assert.ok(baseline.typecheck !== null || baseline.build !== null || baseline.tests !== null, "at least one metric captured");
});
