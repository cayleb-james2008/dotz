/**
 * dotz metrics — baseline + compare for the recursive self-improvement (RSI) loop.
 * Captures a project's verification state (typecheck, tests, build) as a baseline,
 * then re-measures after an improvement to prove the needle moved.
 *
 * Anti-gaming checks: tests must not be deleted, must still pass (or the count must not
 * drop precipitously). This mirrors the recursive-self-improvement skill's guardrails.
 *
 * This is a pure measurement layer — no agent runtime. The RSI brain (a pi tool registered
 * by dotz-tools) drives the loop; this module provides the evidence.
 */
import { exec } from "node:child_process";
import { promisify } from "node:util";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";

const execAsync = promisify(exec);

export interface MetricsBaseline {
  id: string;
  cwd: string;
  timestamp: number;
  typecheck: { ok: boolean; output: string } | null;
  build: { ok: boolean; output: string } | null;
  tests: { ok: boolean; passed: number; failed: number; output: string } | null;
  fileCount: number;
  testFiles: number;
}

export interface MetricsCompare {
  baseline: MetricsBaseline;
  after: MetricsBaseline;
  typecheckFixed: boolean;
  buildFixed: boolean;
  testsImproved: boolean;
  fileCountDelta: number;
  /** Anti-gaming: did the test count drop suspiciously? */
  testsDeleted: boolean;
  summary: string;
}

// Respect DOTZ_CONFIG_DIR for operator relocation + test isolation (same as config.ts/memory.ts).
const rsiDir = () => {
  const base = process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
  return path.join(base, "ai-agents", "rsi");
};

/** Run a command in a cwd with a timeout; returns {ok, output}. */
async function runCmd(cmd: string, cwd: string, timeoutMs = 60000): Promise<{ ok: boolean; output: string }> {
  try {
    const { stdout, stderr } = await execAsync(cmd, { cwd, timeout: timeoutMs, maxBuffer: 1024 * 1024, windowsHide: true });
    return { ok: true, output: (stdout + stderr).slice(-2000) };
  } catch (e) {
    const err = e as { stdout?: string; stderr?: string; message: string };
    return { ok: false, output: ((err.stdout || "") + (err.stderr || "") + err.message).slice(-2000) };
  }
}

/** Count files (non-node_modules, non-dist, non-.git) + test-file count. */
async function countFiles(cwd: string): Promise<{ fileCount: number; testFiles: number }> {
  try {
    const { stdout } = await execAsync(
      `git ls-files --cached --others --exclude-standard | findstr /v /b "node_modules dist .git release" || echo ""`,
      { cwd, timeout: 15000, maxBuffer: 1024 * 1024, windowsHide: true }
    ).catch(() => ({ stdout: "" }));
    const files = stdout.split("\n").map((s) => s.trim()).filter(Boolean);
    const testFiles = files.filter((f) => /\.(test|spec)\.[cm]?[jt]sx?$/i.test(f) || /(^|\/)(?:tests?|__tests__)\//i.test(f)).length;
    return { fileCount: files.length, testFiles };
  } catch {
    return { fileCount: 0, testFiles: 0 };
  }
}

/** Parse pass/fail counts from a test runner's output (vitest / jest / mocha / node:test TAP). */
function parseTestCounts(output: string): { passed: number; failed: number } {
  // Use the LAST "(N) passed/failed" match, not the first: vitest prints "Test Files X passed"
  // BEFORE the real "Tests Y passed", so the first match is the file count, not the test count.
  const last = (re: RegExp): number => { const ms = [...output.matchAll(re)]; return ms.length ? Number(ms[ms.length - 1][1]) : 0; };
  const passed = last(/(\d+)\s+pass(?:ed|ing)\b/gi) || last(/#\s*pass\s+(\d+)/gi);
  const failed = last(/(\d+)\s+fail(?:ed|ing)\b/gi) || last(/#\s*fail\s+(\d+)/gi);
  return { passed, failed };
}

/** Capture a baseline for a project cwd. */
export async function captureBaseline(cwd: string, gateCommand?: string): Promise<MetricsBaseline> {
  const [typecheck, build, files] = await Promise.all([
    runCmd("npx tsc --noEmit", cwd, 90000),
    runCmd("npm run build", cwd, 90000).catch(() => ({ ok: false, output: "build not configured" })),
    countFiles(cwd),
  ]);
  // tests: prefer the project's configured gate command (so the gate works on NON-Node projects —
  // e.g. a python venv's pytest); otherwise fall back to the common Node test runners. Non-fatal if
  // none runs. A configured gate is the cure for "0 tests / wrong python" on Python/other stacks.
  const testCmd = gateCommand?.trim()
    ? gateCommand
    : "npm test -- --passWithNoTests 2>&1 || npx vitest run --passWithNoTests 2>&1 || npx jest --passWithNoTests 2>&1";
  const tests = await runCmd(testCmd, cwd, 90000).catch(() => null);
  const baseline: MetricsBaseline = {
    id: randomUUID(),
    cwd,
    timestamp: Date.now(),
    typecheck,
    build,
    tests: tests ? { ok: tests.ok, ...parseTestCounts(tests.output), output: tests.output.slice(-1000) } : null,
    fileCount: files.fileCount,
    testFiles: files.testFiles,
  };
  await fs.mkdir(rsiDir(), { recursive: true });
  await fs.writeFile(path.join(rsiDir(), `baseline-${baseline.id}.json`), JSON.stringify(baseline, null, 2), "utf-8");
  return baseline;
}

/** Compare a new measurement against a baseline. */
export async function compare(baseline: MetricsBaseline, afterCwd?: string, gateCommand?: string): Promise<MetricsCompare> {
  const after = await captureBaseline(afterCwd || baseline.cwd, gateCommand);
  const typecheckFixed = !!(!baseline.typecheck?.ok && after.typecheck?.ok);
  const buildFixed = !!(!baseline.build?.ok && after.build?.ok);
  // Require a real prior failure so a null/absent baseline can't be reported as RED → GREEN.
  const testsImproved = !!after.tests && !!baseline.tests && !baseline.tests.ok && after.tests.ok;
  const fileCountDelta = after.fileCount - baseline.fileCount;
  // anti-gaming: detect tests deleted/gutted to flip the suite GREEN.
  const baseParsed = (baseline.tests?.passed ?? 0) + (baseline.tests?.failed ?? 0) > 0;
  const afterParsed = (after.tests?.passed ?? 0) + (after.tests?.failed ?? 0) > 0;
  const testsDeleted =
    // (a) test FILES dropped >20% — the reliable signal that test files were removed.
    (baseline.testFiles > 0 && after.testFiles < baseline.testFiles * 0.8) ||
    // (b) passing count dropped >20% — applied whenever BOTH runs parsed a real count (so an
    //     UNPARSEABLE after-run, passed=0, isn't mis-flagged as a deletion), regardless of whether
    //     the baseline was green or red. Catches gutting most of a suite's passing assertions.
    (baseParsed && afterParsed && (after.tests!.passed) < (baseline.tests!.passed) * 0.8) ||
    // (c) the suite was gamed RED → GREEN while now reporting ZERO passing tests though the baseline
    //     had test files — i.e. emptied to pass via --passWithNoTests.
    (testsImproved && (after.tests?.passed ?? 0) === 0 && baseline.testFiles > 0);
  const parts: string[] = [];
  if (typecheckFixed) parts.push("typecheck: RED → GREEN");
  if (buildFixed) parts.push("build: RED → GREEN");
  if (testsImproved) parts.push("tests: RED → GREEN");
  if (baseline.typecheck?.ok && !after.typecheck?.ok) parts.push("⚠ typecheck regressed");
  if (baseline.build?.ok && !after.build?.ok) parts.push("⚠ build regressed");
  if (baseline.tests?.ok && after.tests && !after.tests.ok) parts.push("⚠ tests regressed");
  if (testsDeleted) parts.push("⚠ tests removed/disabled (anti-gaming)");
  if (parts.length === 0) parts.push("no metric movement detected");
  const summary = parts.join("; ");
  return { baseline, after, typecheckFixed, buildFixed, testsImproved, fileCountDelta, testsDeleted, summary };
}

/** Render a baseline for the agent's context. */
export function renderBaseline(b: MetricsBaseline): string {
  const lines = [
    `Baseline captured ${new Date(b.timestamp).toISOString()}`,
    `  typecheck: ${b.typecheck?.ok ? "✓ pass" : "✕ fail"}`,
    `  build: ${b.build?.ok ? "✓ pass" : "✕ fail"}`,
    `  tests: ${b.tests?.ok ? `✓ ${b.tests.passed} pass` : b.tests ? `✕ ${b.tests.failed} fail` : "—"}`,
    `  files: ${b.fileCount}${b.testFiles ? ` (${b.testFiles} test)` : ""}`,
  ];
  return lines.join("\n");
}