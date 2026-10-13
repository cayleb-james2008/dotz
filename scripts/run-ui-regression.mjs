#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const candidates = process.platform === "win32"
  ? [{ command: "py", prefix: ["-3"] }, { command: "python", prefix: [] }, { command: "python3", prefix: [] }]
  : [{ command: "python3", prefix: [] }, { command: "python", prefix: [] }];

function runPython(args) {
  for (const candidate of candidates) {
    const result = spawnSync(candidate.command, [...candidate.prefix, ...args], {
      cwd: root,
      env: process.env,
      stdio: "inherit",
      shell: false,
    });
    if (result.error?.code === "ENOENT") continue;
    if (result.error) {
      console.error(`Could not start ${candidate.command}: ${result.error.message}`);
      return 1;
    }
    return result.status ?? 1;
  }
  console.error("Python 3 is required; install it or add py/python/python3 to PATH.");
  return 1;
}

const unitStatus = runPython([
  "-m", "unittest", "discover", "-s", "scripts", "-p", "test_ui_regression_platform.py", "-v",
]);
if (unitStatus !== 0) process.exit(unitStatus);
process.exit(runPython(["scripts/ui_init_regression.py"]));
