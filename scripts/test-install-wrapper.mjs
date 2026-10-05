import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const fixture = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-install-wrapper-policy-"));
const scripts = path.join(fixture, "scripts");
const allowed = path.join(fixture, "allowed-sharp");
const unapproved = path.join(fixture, "unapproved");
fs.mkdirSync(scripts, { recursive: true });
fs.mkdirSync(allowed, { recursive: true });
fs.mkdirSync(unapproved, { recursive: true });
fs.copyFileSync(
  path.join(repoRoot, "scripts", "install-npm-deps.mjs"),
  path.join(scripts, "install-npm-deps.mjs"),
);

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

const allowedMarker = path.join(fixture, "allowed-hook.txt");
const unapprovedMarker = path.join(fixture, "unapproved-hook.txt");
writeJson(path.join(fixture, "package.json"), {
  name: "install-wrapper-policy-fixture",
  version: "1.0.0",
  private: true,
  allowScripts: { "sharp@0.34.5": true },
  scripts: { "install:deps": "node scripts/install-npm-deps.mjs" },
  dependencies: {
    sharp: "file:./allowed-sharp",
    "fixture-unapproved": "file:./unapproved",
  },
});
writeJson(path.join(allowed, "package.json"), {
  name: "sharp",
  version: "0.34.5",
  scripts: { postinstall: "node write-marker.js" },
});
fs.writeFileSync(
  path.join(allowed, "write-marker.js"),
  "import fs from 'node:fs';\nfs.writeFileSync(process.env.ALLOWED_LIFECYCLE_MARKER, `${process.env.SHARP_IGNORE_GLOBAL_LIBVIPS}\\n`);\n",
);
writeJson(path.join(unapproved, "package.json"), {
  name: "fixture-unapproved",
  version: "1.0.0",
  scripts: { postinstall: "node write-marker.js" },
});
fs.writeFileSync(
  path.join(unapproved, "write-marker.js"),
  "import fs from 'node:fs';\nfs.writeFileSync(process.env.UNAPPROVED_LIFECYCLE_MARKER, 'ran\\n');\n",
);

const env = { ...process.env, ALLOWED_LIFECYCLE_MARKER: allowedMarker, UNAPPROVED_LIFECYCLE_MARKER: unapprovedMarker };
delete env.SHARP_IGNORE_GLOBAL_LIBVIPS;
const npmExecPath = process.env.npm_execpath;
const npmCommand = npmExecPath
  ? process.execPath
  : process.platform === "win32"
    ? "npm.cmd"
    : "npm";
const npmArgs = npmExecPath
  ? [npmExecPath, "run", "install:deps", "--", "--no-audit", "--no-fund"]
  : ["run", "install:deps", "--", "--no-audit", "--no-fund"];
const run = spawnSync(npmCommand, npmArgs, {
  cwd: fixture,
  env,
  encoding: "utf8",
  shell: process.platform === "win32" && !npmExecPath,
});
const allowedContent = fs.existsSync(allowedMarker) ? fs.readFileSync(allowedMarker, "utf8") : null;
const result = {
  fixture_root: fixture,
  node_version: process.version,
  npm_execpath_present: Boolean(npmExecPath),
  exit_code: run.status,
  signal: run.signal,
  spawn_error: run.error ? `${run.error.name}: ${run.error.message}` : null,
  approved_sharp_hook_ran: allowedContent !== null,
  approved_hook_sharp_ignore_global_libvips: allowedContent?.trim() ?? null,
  unapproved_hook_ran: fs.existsSync(unapprovedMarker),
  stdout: run.stdout ?? "",
  stderr: run.stderr ?? "",
};
result.passed = result.exit_code === 0
  && result.approved_sharp_hook_ran
  && result.approved_hook_sharp_ignore_global_libvips === "1"
  && !result.unapproved_hook_ran;
const output = `${JSON.stringify(result, null, 2)}\n`;
if (process.env.DOTZ_INSTALL_WRAPPER_TEST_OUT) {
  fs.writeFileSync(process.env.DOTZ_INSTALL_WRAPPER_TEST_OUT, output);
} else {
  process.stdout.write(output);
}
if (!result.passed) process.exitCode = 1;
fs.rmSync(fixture, { recursive: true, force: true });
