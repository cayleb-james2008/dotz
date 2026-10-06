import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { isSupportedSetupPe, isX64ApplicationPe, readPeInfo } from "./windows-pe-validation.mjs";
import { classifyInitialOnboarding, classifyOnboardingReuse, classifyProcessStateReadback, classifyRestartUiSamples, createDriverCallError, installedProcessLaunchIdentity, isDotzWindowForPid, isInstalledDotzProcess, parseDriverOutput, processStateProbeCommand, requireVerifiedCloseForRelaunch, sendGuardedClose, sleep, uiFailureDetails, validateNativeCloseTarget } from "./windows-installer-runtime.mjs";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..");
const args = parseArgs(process.argv.slice(2));
const outputDir = process.env.DOTZ_ACCEPTANCE_OUT;
if (!outputDir) throw new Error("DOTZ_ACCEPTANCE_OUT must point to this run's evidence directory");
fs.mkdirSync(outputDir, { recursive: true });

const runId = process.env.GITHUB_RUN_ID || `local-${Date.now()}`;
const mode = args.mode;
const userProfile = process.env.USERPROFILE;
if (!userProfile) throw new Error("USERPROFILE is required on the hosted Windows runner");
const dotzProfile = path.join(userProfile, ".dotz");
const projectName = `dotz-native-${mode}-${runId}`;
const projectDir = path.join(process.env.RUNNER_TEMP || outputDir, `dotz-native-${mode}-${runId}-project`);
const memoryText = `dotz Windows installer acceptance ${runId}: this synthetic project memory must persist locally after restart.`;
const memoryCategory = "windows-installer-acceptance";
const RELEASED_ASSET = {
  tag: "v0.2.8",
  tagCommit: "16f27f5878c44cf7aa13ea3719a84dce9b58b0fd",
  name: "dotz_0.2.8_x64-setup.exe",
  url: "https://github.com/cayleb-james2008/dotz/releases/download/v0.2.8/dotz_0.2.8_x64-setup.exe",
  sha256: "64a7e18f6b00d04f7a20533ecd860684bcaf365de11185d032472424cf729642",
  bytes: 100052806,
  publicKeyId: "A55EE27DA4AEE7BB",
  feedBytes: 971,
  feedSha256: "e67d7cf6e12bcfd9c57b3d829416c4c2947b1ebacaa598a6f79cfe3cd100a142",
  signatureBase64Sha256: "4194a09c14c54cc5008db922cafdd5bb51fda0d79e78ed5c2fff3adeefa6f9c8",
  firstRunSource: {
    rawBaseUrl: "https://raw.githubusercontent.com/cayleb-james2008/dotz/16f27f5878c44cf7aa13ea3719a84dce9b58b0fd",
    wizardModuleHttpStatus: 404,
    indexHtmlSha256: "7d5204adad6a67cc8acf007eef3a56076eefdffc86b78537ebbe814fcd157f52",
    indexHtmlWizardReferenceCount: 0,
    serverModuleSha256: "72092052fd3181cf1f7c94cd3e5adea329f11ab9b898f4f9473c69f30b24a9db",
    serverModuleFirstRunRouteReferenceCount: 0,
  },
};
const driverBin = process.env.CUA_DRIVER_BIN || path.join(
  process.env.LOCALAPPDATA || "",
  "Programs",
  "Cua",
  "cua-driver",
  "bin",
  "cua-driver.exe",
);
const driverSession = `dotz-${mode}-${runId}`;
const result = {
  schema_version: 1,
  status: "RUNNING",
  result: "IN_PROGRESS",
  source: mode,
  run_id: runId,
  job: process.env.GITHUB_JOB || null,
  commit: process.env.GITHUB_SHA || null,
  ref: process.env.GITHUB_REF || null,
  runner: {
    os: os.platform(),
    arch: os.arch(),
    release: os.release(),
    node: process.version,
    username: process.env.USERNAME || null,
    session_id: null,
  },
  release: mode === "released" ? {
    ...RELEASED_ASSET,
    expected_sha256: args.expectedSha256 || RELEASED_ASSET.sha256,
    feed_url: "https://github.com/cayleb-james2008/dotz/releases/download/v0.2.8/latest.json",
  } : null,
  installer: null,
  environment: {
    dotz_profile: dotzProfile,
    dotz_config_dir_override: process.env.DOTZ_CONFIG_DIR || null,
    dotz_assets_override: process.env.DOTZ_ASSETS || null,
    dotz_models_override: process.env.DOTZ_MODELS || null,
    webview2_runtime: null,
    host_vc_runtime_files: [],
  },
  installation: null,
  app: null,
  webview2_processes: [],
  onboarding: { status: "NOTRUN", provider_key_entered: false, provider_inference: false },
  project: { name: projectName, cwd: projectDir, status: "NOTRUN" },
  memory: { text: memoryText, category: memoryCategory, scope: "project", status: "NOTRUN" },
  screenshots: [],
  profile_snapshots: [],
  checks: [],
  skips: [],
  raw_counts: { total: 0, passed: 0, failed: 0, skipped: 0 },
  cleanup: { app_stopped: false, cua_daemon_stopped: false, process_stops: [] },
};

let daemonStartedByTest = false;
let driverSessionStarted = false;
let activeAppPid = null;
let activeWindowId = null;
let activeLaunchIdentity = null;
let activeStopAttempted = false;
let fatalError = null;
let currentPhase = "preflight";

function counts() {
  const summary = { total: result.checks.length, passed: 0, failed: 0, skipped: 0 };
  for (const check of result.checks) summary[check.status === "PASS" ? "passed" : check.status === "FAIL" ? "failed" : "skipped"] += 1;
  return summary;
}

function phaseCounts() {
  const phases = {};
  for (const check of result.checks) {
    const phase = check.phase || "unclassified";
    phases[phase] ||= { total: 0, passed: 0, failed: 0, skipped: 0 };
    phases[phase].total += 1;
    phases[phase][check.status === "PASS" ? "passed" : check.status === "FAIL" ? "failed" : "skipped"] += 1;
  }
  return phases;
}

function renderReport() {
  const countsNow = counts();
  const profileRows = result.profile_snapshots.map((snapshot) =>
    `- ${snapshot.stage}: exists=${snapshot.exists}; files=${snapshot.file_count}; sha256=${snapshot.aggregate_sha256}`,
  );
  const screenshotRows = result.screenshots.map((screenshot) =>
    `- ${screenshot.file}: ${screenshot.bytes} bytes; sha256=${screenshot.sha256}`,
  );
  const checkRows = result.checks.map((check) =>
    `- [${check.status}] (${check.phase}) ${check.name}${check.reason ? ` — ${check.reason}` : ""}`,
  );
  const phaseRows = Object.entries(phaseCounts()).map(([phase, value]) =>
    `- ${phase}: total=${value.total}, passed=${value.passed}, failed=${value.failed}, skipped=${value.skipped}`,
  );
  return [
    "# Dotz Windows Installer Acceptance",
    "",
    `- **Status:** ${result.status}`,
    `- **Result:** ${result.result}`,
    `- **Target:** ${mode}`,
    `- **Source commit:** ${result.commit || "unknown"}`,
    `- **Workflow run/job:** ${result.run_id || "local"} / ${result.job || "unknown"}`,
    `- **Runner:** ${result.runner.os} ${result.runner.arch} ${result.runner.release}; interactive session ${result.runner.session_id ?? "unconfirmed"}`,
    `- **Installer:** ${result.installer?.filename || "not checked"}; bytes=${result.installer?.bytes ?? "unknown"}; sha256=${result.installer?.sha256 ?? "unknown"}`,
    `- **Counts:** total=${countsNow.total}, passed=${countsNow.passed}, failed=${countsNow.failed}, skipped=${countsNow.skipped}`,
    "",
    "## Raw counts by phase",
    ...(phaseRows.length ? phaseRows : ["- No checks recorded."]),
    "",
    "## Checks",
    ...(checkRows.length ? checkRows : ["- None recorded."]),
    "",
    "## Skips",
    ...(result.skips.length ? result.skips.map((item) => `- ${item.name}: ${item.reason}`) : ["- None."]),
    "",
    "## Profile snapshots",
    ...(profileRows.length ? profileRows : ["- None captured."]),
    "",
    "## Screenshots",
    ...(screenshotRows.length ? screenshotRows : ["- None captured."]),
    "",
    ...(result.error ? ["## Failure", "", "```text", result.error, "```", ""] : []),
  ].join("\n");
}

function persist() {
  result.raw_counts = { ...counts(), by_phase: phaseCounts() };
  result.phase_counts = phaseCounts();
  result.updated_at = new Date().toISOString();
  const tmp = path.join(outputDir, "result.json.tmp");
  fs.writeFileSync(tmp, `${JSON.stringify(result, null, 2)}\n`, "utf8");
  fs.renameSync(tmp, path.join(outputDir, "result.json"));
  fs.writeFileSync(path.join(outputDir, "REPORT.md"), renderReport(), "utf8");
}

function log(message) {
  const line = `[${new Date().toISOString()}] ${message}`;
  process.stdout.write(`${line}\n`);
  fs.appendFileSync(path.join(outputDir, "raw.log"), `${line}\n`, "utf8");
}

function addCheck(name, ok, detail = {}) {
  const item = { name, phase: currentPhase, status: ok ? "PASS" : "FAIL", detail, at: new Date().toISOString() };
  result.checks.push(item);
  log(`${item.status} ${name} ${JSON.stringify(detail)}`);
  persist();
  return ok;
}

function addSkip(name, reason) {
  const item = { name, phase: currentPhase, status: "SKIP", reason, at: new Date().toISOString() };
  result.checks.push(item);
  result.skips.push(item);
  log(`SKIP ${name}: ${reason}`);
  persist();
}

function parseArgs(argv) {
  const parsed = { mode: null, installer: null, expectedSha256: null, metadata: null };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    if (flag === "--mode") parsed.mode = argv[++i];
    else if (flag === "--installer") parsed.installer = argv[++i];
    else if (flag === "--expected-sha256") parsed.expectedSha256 = argv[++i];
    else if (flag === "--metadata") parsed.metadata = argv[++i];
    else throw new Error(`Unknown argument: ${flag}`);
  }
  if (!new Set(["released", "candidate"]).has(parsed.mode)) throw new Error("--mode must be released or candidate");
  if (!parsed.installer) throw new Error("--installer is required");
  if (!parsed.metadata) throw new Error("--metadata is required; it must identify this exact downloaded or built installer");
  if (parsed.expectedSha256 && !/^[a-f0-9]{64}$/i.test(parsed.expectedSha256)) {
    throw new Error("--expected-sha256 must be exactly 64 hexadecimal characters");
  }
  return parsed;
}

function loadAndValidateMetadata(installerPath, installerHash, installerBytes) {
  const metadata = JSON.parse(fs.readFileSync(path.resolve(args.metadata), "utf8"));
  result.artifact_metadata = metadata;
  if (metadata.mode !== mode) throw new Error(`metadata mode ${metadata.mode} does not match ${mode}`);
  if (path.basename(installerPath) !== metadata.asset_name) {
    throw new Error(`installer name ${path.basename(installerPath)} does not match metadata asset ${metadata.asset_name}`);
  }
  const expected = mode === "released" ? RELEASED_ASSET.sha256 : String(metadata.sha256 || "").toLowerCase();
  if (!/^[a-f0-9]{64}$/.test(expected)) throw new Error("metadata contains no valid 64-character installer SHA-256");
  if (installerHash.toLowerCase() !== expected || String(metadata.sha256 || "").toLowerCase() !== expected) {
    throw new Error(`installer SHA-256 mismatch: actual=${installerHash} expected=${expected} metadata=${metadata.sha256}`);
  }
  if (installerBytes !== metadata.asset_bytes) {
    throw new Error(`installer byte size mismatch: actual=${installerBytes} metadata=${metadata.asset_bytes}`);
  }
  if (args.expectedSha256 && args.expectedSha256.toLowerCase() !== expected) {
    throw new Error(`--expected-sha256 ${args.expectedSha256} does not match trusted metadata ${expected}`);
  }
  if (mode === "released") {
    if (metadata.tag !== RELEASED_ASSET.tag || metadata.tag_commit !== RELEASED_ASSET.tagCommit ||
        metadata.asset_name !== RELEASED_ASSET.name || metadata.asset_url !== RELEASED_ASSET.url ||
        String(metadata.sha256 || "").toLowerCase() !== RELEASED_ASSET.sha256 || metadata.asset_bytes !== RELEASED_ASSET.bytes) {
      throw new Error("public v0.2.8 release metadata does not match the pinned tag, x64 NSIS asset, size, and SHA-256");
    }
    const feedPath = path.join(outputDir, "latest.json");
    if (!fs.existsSync(feedPath)) throw new Error("raw public updater feed latest.json is missing from the evidence directory");
    const feedBytes = fs.readFileSync(feedPath);
    const feedHash = crypto.createHash("sha256").update(feedBytes).digest("hex");
    const feed = JSON.parse(feedBytes.toString("utf8"));
    const feedPlatform = "windows-x86_64";
    if (feed.version !== "0.2.8" || feed.platforms?.[feedPlatform]?.url !== RELEASED_ASSET.url ||
        feedHash !== RELEASED_ASSET.feedSha256 || feedHash !== metadata.feed_sha256 || feedBytes.length !== RELEASED_ASSET.feedBytes || feedBytes.length !== metadata.feed_bytes ||
        metadata.target_platform !== feedPlatform || metadata.target_arch !== "x64" || !metadata.source_bundle_targets?.includes("nsis")) {
      throw new Error("raw public update-feed bytes or tag metadata do not identify the pinned v0.2.8 Windows x64 NSIS package");
    }
    const signatureText = feed.platforms[feedPlatform].signature;
    const signatureBytes = Buffer.from(signatureText, "base64");
    if (!signatureBytes.length || signatureBytes.toString("base64") !== signatureText) {
      throw new Error("updater-feed signature is not canonical base64 Minisign data");
    }
    const signatureFile = path.join(outputDir, "latest.json.sig");
    fs.writeFileSync(signatureFile, signatureBytes);
    const signatureBase64Sha256 = crypto.createHash("sha256").update(signatureText).digest("hex");
    if (signatureBase64Sha256 !== RELEASED_ASSET.signatureBase64Sha256 || metadata.signature_sha256 !== signatureBase64Sha256) {
      throw new Error("updater-feed signature does not match the pinned public v0.2.8 signature");
    }
    const minisign = metadata.minisign;
    if (minisign?.status === "PASS") {
      if (minisign.public_key_id !== RELEASED_ASSET.publicKeyId || minisign.positive_exit_code !== 0 || !Number.isInteger(minisign.tamper_negative_exit_code) || minisign.tamper_negative_exit_code <= 0) {
        throw new Error("Minisign public-key identity or positive/tampered verification result is invalid");
      }
      addCheck("public v0.2.8 Minisign signature verified; tampering was rejected", true, minisign);
    } else {
      addCheck("public v0.2.8 Minisign verification is mandatory before setup.exe execution", false, minisign || { status: "missing" });
      throw new Error(`Refusing to run the public installer without successful Minisign positive/tamper-negative verification: ${JSON.stringify(minisign)}`);
    }
  } else {
    if (metadata.source_sha !== process.env.GITHUB_SHA || metadata.branch !== "diagnostic/windows-installer-acceptance") {
      throw new Error("candidate artifact manifest does not match this workflow HEAD and diagnostic branch");
    }
    if (metadata.runner_arch !== "X64" || metadata.target_triple !== "x86_64-pc-windows-msvc" ||
        metadata.target_arch !== "x64" || String(metadata.bundle_target || "").toLowerCase() !== "nsis") {
      throw new Error("candidate artifact manifest does not prove an x64 Tauri NSIS build target");
    }
  }
  result.artifact_metadata = metadata;
  return expected;
}

function runSync(command, commandArgs, options = {}) {
  const completed = spawnSync(command, commandArgs, {
    encoding: "utf8",
    windowsHide: true,
    timeout: options.timeoutMs ?? 120_000,
    input: options.input,
    env: options.env ?? process.env,
  });
  if (completed.error) throw new Error(`${command} ${commandArgs.join(" ")} failed to spawn: ${completed.error.message}`);
  return completed;
}

async function sha256File(filePath) {
  const hash = crypto.createHash("sha256");
  for await (const chunk of fs.createReadStream(filePath)) hash.update(chunk);
  return hash.digest("hex");
}

async function snapshotProfile(stage) {
  const files = [];
  if (fs.existsSync(dotzProfile)) {
    const pending = [{ absolute: dotzProfile, relative: "", depth: 0 }];
    while (pending.length) {
      const current = pending.pop();
      if (current.depth > 12) continue;
      for (const entry of fs.readdirSync(current.absolute, { withFileTypes: true })) {
        const absolute = path.join(current.absolute, entry.name);
        const relative = path.join(current.relative, entry.name).replaceAll(path.sep, "/");
        if (entry.isDirectory()) pending.push({ absolute, relative, depth: current.depth + 1 });
        else if (entry.isFile()) {
          const stat = fs.statSync(absolute);
          files.push({ path: relative, bytes: stat.size, sha256: await sha256File(absolute) });
        }
      }
    }
  }
  files.sort((a, b) => a.path.localeCompare(b.path));
  const canonical = files.map((file) => `${file.path}\0${file.bytes}\0${file.sha256}\n`).join("");
  const profileHash = crypto.createHash("sha256").update(canonical).digest("hex");
  const snapshot = {
    stage,
    exists: fs.existsSync(dotzProfile),
    file_count: files.length,
    aggregate_sha256: profileHash,
    files,
    captured_at: new Date().toISOString(),
  };
  result.profile_snapshots.push(snapshot);
  fs.writeFileSync(path.join(outputDir, `profile-${stage}.json`), `${JSON.stringify(snapshot, null, 2)}\n`, "utf8");
  persist();
  return snapshot;
}

async function captureScreenshot(name, state) {
  const destination = path.join(outputDir, `${name}.png`);
  const saved = call("get_window_state", {
    pid: activeAppPid,
    window_id: activeWindowId,
    screenshot_out_file: destination,
  });
  if (!fs.existsSync(destination)) throw new Error(`Native screenshot was not written: ${destination}; driver=${JSON.stringify(saved)}`);
  const detail = { file: path.basename(destination), bytes: fs.statSync(destination).size, sha256: await sha256File(destination) };
  result.screenshots.push(detail);
  if (state) saveUiSnapshot(name, state);
  log(`SCREENSHOT ${JSON.stringify(detail)}`);
  persist();
}

async function captureFailureUi(name) {
  let state = null;
  let uiReadError = null;
  let screenshotError = null;
  let labelError = null;
  try {
    state = getState(false);
  } catch (error) {
    uiReadError = error instanceof Error ? error.message : String(error);
  }
  try {
    await captureScreenshot(name, state);
  } catch (error) {
    screenshotError = error instanceof Error ? error.message : String(error);
  }
  let textSample = null;
  try {
    if (state) textSample = labels(state).slice(0, 2_000);
  } catch (error) {
    labelError = error instanceof Error ? error.message : String(error);
  }
  return {
    state,
    text_sample: textSample,
    ui_read_error: uiReadError,
    screenshot_error: screenshotError,
    label_error: labelError,
  };
}

function saveUiSnapshot(name, state) {
  const sanitized = {
    pid: state.pid,
    window_id: state.window_id,
    title: state.title,
    screenshot_width: state.screenshot_width,
    screenshot_height: state.screenshot_height,
    elements: (state.elements || []).map((element) => ({
      role: element.role,
      label: element.label,
      frame: element.frame,
      enabled: element.enabled,
      has_element_token: Boolean(element.element_token),
    })),
    tree_markdown: state.tree_markdown || "",
  };
  fs.writeFileSync(path.join(outputDir, `uia-${name}.json`), `${JSON.stringify(sanitized, null, 2)}\n`, "utf8");
}

function invokeDriver(tool, payload = {}, timeoutMs = 120_000) {
  const withSession = { ...payload, session: payload.session || driverSession };
  const completed = runSync(driverBin, ["call", tool], { input: JSON.stringify(withSession), timeoutMs });
  if (completed.status !== 0) throw createDriverCallError(tool, completed);
  return parseDriverOutput(tool, completed.stdout);
}

function call(tool, payload = {}, timeoutMs) {
  return invokeDriver(tool, payload, timeoutMs);
}

function getState(includeScreenshot = false) {
  return call("get_window_state", {
    pid: activeAppPid,
    window_id: activeWindowId,
    include_screenshot: includeScreenshot,
  });
}

function labels(state) {
  return (state.elements || []).map((item) => `${item.role || ""} ${item.label || ""}`).join("\n") + `\n${state.tree_markdown || ""}`;
}

async function withUiFailureCheck(checkName, evidenceName, operation) {
  try {
    return await operation();
  } catch (error) {
    const failureUi = await captureFailureUi(evidenceName);
    addCheck(checkName, false, uiFailureDetails(error, failureUi));
    throw error;
  }
}

function waitForTextChecked(checkName, evidenceName, text, timeoutMs = 30_000) {
  return withUiFailureCheck(checkName, evidenceName, () => waitForText(text, timeoutMs));
}

function clickByTextChecked(checkName, evidenceName, text, options = {}) {
  return withUiFailureCheck(checkName, evidenceName, () => clickByText(text, options));
}

function typeIntoChecked(checkName, evidenceName, text, value, options = {}) {
  return withUiFailureCheck(checkName, evidenceName, () => typeInto(text, value, options));
}

function getStateChecked(checkName, evidenceName, includeScreenshot = false) {
  return withUiFailureCheck(checkName, evidenceName, () => getState(includeScreenshot));
}

function findElement(state, { text, role } = {}) {
  const needle = String(text || "").toLowerCase();
  return (state.elements || []).find((element) => {
    if (role && String(element.role || "").toLowerCase() !== role.toLowerCase()) return false;
    return String(element.label || "").toLowerCase().includes(needle);
  });
}

async function waitForText(text, timeoutMs = 30_000) {
  const start = Date.now();
  let latest = null;
  while (Date.now() - start < timeoutMs) {
    latest = getState(false);
    if (labels(latest).toLowerCase().includes(String(text).toLowerCase())) return latest;
    await sleep(400);
  }
  const visible = (latest?.elements || []).map((element) => `${element.role}:${element.label}`).join(" | ");
  throw new Error(`Timed out waiting for native UI text ${JSON.stringify(text)}; visible=${visible}`);
}

async function waitForFirstRunBranch(timeoutMs = 30_000) {
  const start = Date.now();
  let latest = null;
  while (Date.now() - start < timeoutMs) {
    latest = getState(false);
    if (/WELCOME TO dotz|STEP [1-4]|NO PROJECT/i.test(labels(latest))) return latest;
    await sleep(400);
  }
  throw new Error(`Timed out waiting for native first-run wizard or dashboard; visible=${labels(latest).slice(0, 2_000)}`);
}

async function waitForFirstRunWizard(timeoutMs = 45_000) {
  const start = Date.now();
  let latest = null;
  while (Date.now() - start < timeoutMs) {
    latest = getState(false);
    if (/WELCOME TO dotz|STEP [1-4]/i.test(labels(latest))) return latest;
    await sleep(400);
  }
  throw new Error(`Timed out waiting for native first-run wizard; visible=${labels(latest).slice(0, 2_000)}`);
}

async function clickByText(text, { role, timeoutMs = 30_000 } = {}) {
  const state = await waitForText(text, timeoutMs);
  const element = findElement(state, { text, role });
  if (!element) throw new Error(`UIA element not found for click text=${JSON.stringify(text)}, role=${role || "any"}`);
  if (element.element_token) {
    return call("click", { pid: activeAppPid, window_id: activeWindowId, element_token: element.element_token });
  }
  const fresh = getState(true);
  const target = findElement(fresh, { text, role });
  if (!target?.frame) throw new Error(`UIA element has no token/frame for pixel click: ${text}`);
  const windows = call("list_windows", { pid: activeAppPid });
  const window = (windows.windows || windows._legacy_windows || []).find((item) => item.window_id === activeWindowId);
  if (!window?.bounds || !fresh.screenshot_width || !fresh.screenshot_height) {
    throw new Error(`Cannot map UIA frame to a native window screenshot for ${text}`);
  }
  const frame = target.frame;
  const ox = window.bounds.x + (window.bounds.width - fresh.screenshot_width) / 2;
  const oy = window.bounds.y + (window.bounds.height - fresh.screenshot_height) / 2;
  const x = Math.round(frame.x + frame.w / 2 - ox);
  const y = Math.round(frame.y + frame.h / 2 - oy);
  return call("click", { pid: activeAppPid, window_id: activeWindowId, x, y });
}

async function typeInto(text, value, { role = "Edit", timeoutMs = 30_000 } = {}) {
  const state = await waitForText(text, timeoutMs);
  const element = findElement(state, { text, role });
  if (!element) throw new Error(`UIA input not found: ${text}`);
  if (!element.element_token) throw new Error(`UIA input has no ValuePattern token: ${text}`);
  return call("type_text", {
    pid: activeAppPid,
    window_id: activeWindowId,
    element_token: element.element_token,
    text: value,
  });
}

async function waitForAppWindow(pid, timeoutMs = 30_000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const response = call("list_windows", { pid });
    const windows = response.windows || response._legacy_windows || [];
    const window = windows.find((item) => item.pid === pid);
    if (window) return window;
    await sleep(300);
  }
  throw new Error(`Timed out waiting for a native dotz window owned by pid ${pid}`);
}

function startCuaDaemon() {
  const status = runSync(driverBin, ["status"]);
  if (/daemon is running/i.test(status.stdout || "")) {
    log("Reusing pre-existing CUA daemon (unexpected on a fresh hosted runner)");
    return;
  }
  const child = spawn(driverBin, ["serve", "--permission-mode", "standard"], {
    detached: true,
    stdio: "ignore",
    windowsHide: true,
    env: process.env,
  });
  child.unref();
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const probe = runSync(driverBin, ["status"]);
    if (/daemon is running/i.test(probe.stdout || "")) {
      daemonStartedByTest = true;
      return;
    }
    sleepSync(250);
  }
  throw new Error("cua-driver daemon did not become ready after 40 status probes");
}

function stopCuaDaemon() {
  if (driverSessionStarted) {
    let ended = false;
    try {
      call("end_session", {}, 20_000);
      ended = true;
    } catch (error) {
      log(`CUA end_session failed: ${error instanceof Error ? error.message : String(error)}`);
    }
    driverSessionStarted = false;
    result.cleanup.cua_session_ended = ended;
    addCheck("cua-driver session ended cleanly", ended, {});
  }
  if (!daemonStartedByTest) return;
  const stopped = runSync(driverBin, ["stop"], { timeoutMs: 30_000 });
  log(`CUA stop exit=${stopped.status} stdout=${(stopped.stdout || "").trim()} stderr=${(stopped.stderr || "").trim()}`);
  result.cleanup.cua_daemon_stopped = stopped.status === 0 || /not running/i.test(`${stopped.stdout} ${stopped.stderr}`);
  addCheck("test-owned cua-driver daemon stopped cleanly", result.cleanup.cua_daemon_stopped, { exit_code: stopped.status });
}

function sleepSync(ms) {
  const end = Date.now() + ms;
  while (Date.now() < end) { /* bounded wait for synchronous CUA polling */ }
}

function runPowerShellJson(command) {
  const completed = runSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", command], { timeoutMs: 60_000 });
  if (completed.status !== 0) throw new Error(`PowerShell diagnostic exited ${completed.status}: ${completed.stderr || completed.stdout}`);
  const text = (completed.stdout || "").trim();
  if (!text) return null;
  return JSON.parse(text);
}

function discoverInstallEntry() {
  const command = `
$roots = @('HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall','HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall','HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall')
$items = @()
foreach ($root in $roots) {
  if (Test-Path -LiteralPath $root) {
    foreach ($key in (Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue)) {
      $item = Get-ItemProperty -LiteralPath $key.PSPath -ErrorAction SilentlyContinue
      if ($item.DisplayName -and $item.DisplayName -like 'dotz*') {
        $items += [pscustomobject]@{ display_name=$item.DisplayName; install_location=$item.InstallLocation; uninstall_string=$item.UninstallString; registry_key=$key.Name }
      }
    }
  }
}
$items | ConvertTo-Json -Depth 4 -Compress
`;
  const data = runPowerShellJson(command);
  if (data == null) return [];
  return Array.isArray(data) ? data : [data];
}

function findInstalledExe(installEntries) {
  const roots = new Set();
  for (const entry of installEntries) if (entry.install_location) roots.add(entry.install_location);
  roots.add(path.join(process.env.LOCALAPPDATA || "", "dotz"));
  roots.add(path.join(process.env.LOCALAPPDATA || "", "Programs", "dotz"));
  roots.add(process.env.LOCALAPPDATA || "");
  const found = [];
  for (const root of roots) {
    if (!root || !fs.existsSync(root)) continue;
    const pending = [{ dir: root, depth: 0 }];
    while (pending.length) {
      const current = pending.pop();
      if (current.depth > 6) continue;
      let entries;
      try { entries = fs.readdirSync(current.dir, { withFileTypes: true }); }
      catch { continue; }
      for (const entry of entries) {
        const absolute = path.join(current.dir, entry.name);
        if (entry.isFile() && entry.name.toLowerCase() === "dotz.exe") found.push(absolute);
        else if (entry.isDirectory() && !["node_modules", "cache", "temp"].includes(entry.name.toLowerCase())) {
          pending.push({ dir: absolute, depth: current.depth + 1 });
        }
      }
      if (found.length > 1) break;
    }
    if (found.length > 1) break;
  }
  return [...new Set(found)];
}

function inspectRuntimePrerequisites() {
  const systemRoot = process.env.SYSTEMROOT || "C:\\Windows";
  const hostVcruntimeFiles = ["msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll"]
    .map((name) => ({ name, present: fs.existsSync(path.join(systemRoot, "System32", name)) }));
  result.environment.host_vc_runtime_files = hostVcruntimeFiles;
  const programFilesX86 = process.env["ProgramFiles(x86)"] || "C:\\Program Files (x86)";
  const webviewRoot = path.join(programFilesX86, "Microsoft", "EdgeWebView", "Application");
  let webview2 = null;
  if (fs.existsSync(webviewRoot)) {
    const versions = fs.readdirSync(webviewRoot, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => ({ version: entry.name, exe: path.join(webviewRoot, entry.name, "msedgewebview2.exe") }))
      .filter((entry) => fs.existsSync(entry.exe))
      .sort((a, b) => b.version.localeCompare(a.version, undefined, { numeric: true }));
    if (versions.length) webview2 = versions[0];
  }
  result.environment.webview2_runtime = webview2;
  return { hostVcruntimeFiles, webview2 };
}

function webviewChildren(pid) {
  const command = `
$target = ${Number(pid)}
$rows = @(Get-CimInstance Win32_Process | Select-Object Name,ProcessId,ParentProcessId,ExecutablePath)
$ids = [System.Collections.Generic.HashSet[int]]::new()
[void]$ids.Add($target)
do {
  $added = 0
  foreach ($row in $rows) {
    $rowPid = [int]$row.ProcessId
    $parentPid = [int]$row.ParentProcessId
    if ($ids.Contains($parentPid) -and -not $ids.Contains($rowPid)) { [void]$ids.Add($rowPid); $added++ }
  }
} while ($added -gt 0)
$matches = @($rows | Where-Object { $_.Name -eq 'msedgewebview2.exe' -and $ids.Contains([int]$_.ProcessId) } | Select-Object Name,ProcessId,ParentProcessId,ExecutablePath)
$matches | ConvertTo-Json -Depth 3 -Compress
`;
  const data = runPowerShellJson(command);
  if (data == null) return [];
  return Array.isArray(data) ? data : [data];
}

async function modelResources(installDir) {
  const candidates = [
    path.join(installDir, "resources", "assets", "models", "Xenova", "all-MiniLM-L6-v2"),
    path.join(installDir, "assets", "models", "Xenova", "all-MiniLM-L6-v2"),
    path.join(installDir, "resources", "Xenova", "all-MiniLM-L6-v2"),
  ];
  const seen = new Set();
  for (const base of candidates) {
    if (seen.has(base)) continue;
    seen.add(base);
    const tokenizer = path.join(base, "tokenizer.json");
    const model = path.join(base, "onnx", "model.onnx");
    if (fs.existsSync(tokenizer) && fs.existsSync(model)) {
      return {
        base,
        tokenizer: { path: path.relative(installDir, tokenizer), bytes: fs.statSync(tokenizer).size, sha256: await sha256File(tokenizer) },
        model: { path: path.relative(installDir, model), bytes: fs.statSync(model).size, sha256: await sha256File(model) },
      };
    }
  }
  return null;
}

function appProcessInfo(pid) {
  const command = `Get-CimInstance Win32_Process -Filter 'ProcessId = ${Number(pid)}' | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,SessionId,CreationDate | ConvertTo-Json -Depth 3 -Compress`;
  const data = runPowerShellJson(command);
  return Array.isArray(data) ? data[0] || null : data;
}

async function launchNativeApp(appExe) {
  const launched = call("launch_app", { path: appExe });
  const pid = Number(launched.pid);
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error(`cua-driver launch_app returned no valid process id: ${JSON.stringify(launched)}`);
  let processInfo;
  try {
    processInfo = appProcessInfo(pid);
  } catch (error) {
    result.app = { pid, executable: appExe, process_query_error: error instanceof Error ? error.message : String(error) };
    addCheck("launched PID is verified as the installed dotz.exe", false, { pid, expected_executable: appExe, process_query_error: result.app.process_query_error });
    throw new Error(`refusing to track or stop unverified launch pid ${pid}: ${result.app.process_query_error}`);
  }
  const processOwned = isInstalledDotzProcess(processInfo, appExe);
  activeLaunchIdentity = installedProcessLaunchIdentity(processInfo, appExe);
  result.app = { pid, window_id: null, title: null, app_name: null, executable: appExe, process: processInfo, launch_identity: activeLaunchIdentity };
  addCheck("launched PID is verified as the installed dotz.exe", processOwned, { pid, expected_executable: appExe, process: processInfo });
  if (!processOwned) throw new Error(`refusing to track or stop pid ${pid}: process identity does not match installed ${appExe}`);
  activeAppPid = pid;
  activeStopAttempted = false;
  const stableLaunchIdentity = activeLaunchIdentity?.pid === pid;
  addCheck("launched installed dotz PID has a stable process-instance identity", stableLaunchIdentity, { pid, launch_identity: activeLaunchIdentity, required_fields: ["ProcessId", "ExecutablePath", "CreationDate", "SessionId"] });
  if (!stableLaunchIdentity) throw new Error(`refusing to close/restart pid ${pid}: launch identity lacks a stable creation-time/session record`);

  let window;
  try {
    window = await waitForAppWindow(pid);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    result.app.window_query_error = message;
    addCheck("native dotz window becomes available for the verified installed process", false, { pid, error: message });
    throw error;
  }
  const windowOwned = isDotzWindowForPid(window, pid);
  addCheck("native dotz window is bound to the verified installed process", windowOwned, { pid, window });
  if (!windowOwned) throw new Error(`native window returned for pid ${pid} is not the installed dotz.exe window`);
  activeWindowId = window.window_id;
  result.app.window_id = activeWindowId;
  result.app.title = window.title || null;
  result.app.app_name = window.app_name || null;
  const children = webviewChildren(pid);
  result.webview2_processes = children;
  addCheck("WebView2 child process is present under installed dotz", children.some((child) => child.Name?.toLowerCase() === "msedgewebview2.exe"), { count: children.length, processes: children });
  let state = getState(true);
  const width = state.screenshot_width || 1200;
  const height = state.screenshot_height || 800;
  // Follow the proven Sophos WebView2 UIA flow: one inert center click changes the native WebView2 tree from an opaque pane to DOM controls.
  call("click", { pid, window_id: activeWindowId, x: Math.round(width / 2), y: Math.round(height / 2) });
  await sleep(500);
  state = getState(false);
  if (!(state.elements || []).length && !state.tree_markdown) throw new Error("Native WebView2 UIA tree remained empty after the accessibility-enabling click");
  return state;
}

function readProcessState(pid) {
  const probe = runSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", processStateProbeCommand(pid)], { timeoutMs: 10_000 });
  return { classification: classifyProcessStateReadback(probe, pid), probe };
}

async function stopApp() {
  if (!activeAppPid) return;
  if (activeStopAttempted) throw new Error(`native close was already attempted for installed dotz pid ${activeAppPid}; refusing duplicate input`);
  activeStopAttempted = true;
  const pid = activeAppPid;
  const stopRecord = { phase: currentPhase, pid, requested_at: new Date().toISOString() };
  result.cleanup.process_stops ||= [];
  result.cleanup.process_stops.push(stopRecord);

  if (!Number.isSafeInteger(activeWindowId) || activeWindowId < 1) {
    const reason = "no verified native dotz window ID is available";
    stopRecord.close_refused_reason = reason;
    result.cleanup.stop_refused = true;
    addCheck("native close target has a verified dotz window ID", false, { pid, window_id: activeWindowId, reason });
    throw new Error(`refusing to send Alt+F4 to pid ${pid}: ${reason}`);
  }

  const verifyCloseTarget = (stage) => {
    const errors = [];
    let processState = null;
    let processInfo = null;
    let windows = [];
    try { processState = readProcessState(pid); }
    catch (error) { errors.push({ probe: "process_state", error: error instanceof Error ? error.message : String(error) }); }
    try { processInfo = appProcessInfo(pid); }
    catch (error) { errors.push({ probe: "process_identity", error: error instanceof Error ? error.message : String(error) }); }
    try {
      const response = call("list_windows", { pid });
      windows = response.windows || response._legacy_windows || [];
    } catch (error) { errors.push({ probe: "native_window_association", error: error instanceof Error ? error.message : String(error) }); }
    const proof = validateNativeCloseTarget({
      pid,
      windowId: activeWindowId,
      expectedExecutable: result.installation.app_exe,
      expectedLaunchIdentity: activeLaunchIdentity,
      processState,
      processInfo,
      windows,
    });
    if (errors.length) {
      proof.valid = false;
      proof.reasons.push("one or more exact process/window identity readbacks failed");
      proof.readback_errors = errors;
    }
    proof.stage = stage;
    addCheck(`exact installed process and native window identity verified ${stage}`, proof.valid, proof);
    return proof;
  };

  const closeOutcome = sendGuardedClose({
    pid,
    windowId: activeWindowId,
    keys: ["alt", "f4"],
    verifyTarget: verifyCloseTarget,
    sendInput: (request) => call("hotkey", request),
  });
  stopRecord.close_target_verifications = closeOutcome.verifications;
  stopRecord.cua_alt_f4_attempts = closeOutcome.attempts;
  stopRecord.cua_alt_f4_response = closeOutcome.attempts.at(-1)?.response || null;
  stopRecord.cua_alt_f4_error = closeOutcome.ok ? null : closeOutcome.reason;
  result.cleanup.alt_f4_close_attempts = closeOutcome.attempts;
  result.cleanup.alt_f4_close_response = stopRecord.cua_alt_f4_response;
  result.cleanup.alt_f4_close_error = stopRecord.cua_alt_f4_error;
  addCheck("cua-driver Alt+F4 close request succeeded after identity-guarded delivery", closeOutcome.ok, {
    pid,
    window_id: activeWindowId,
    keys: ["alt", "f4"],
    attempts: closeOutcome.attempts,
    target_verifications: closeOutcome.verifications,
    error: closeOutcome.reason,
  });

  const waitForExit = async (timeoutMs) => {
    const start = Date.now();
    let latest = null;
    while (Date.now() - start < timeoutMs) {
      latest = readProcessState(pid);
      if (latest.classification === "absent") return { exited: true, latest };
      await sleep(250);
    }
    return { exited: false, latest };
  };
  const exitResult = await waitForExit(15_000);
  const exited = exitResult.exited;
  stopRecord.after_close_process_state = exitResult.latest;
  stopRecord.stop_method = "cua-driver hotkey alt+f4; identity-guarded foreground retry only after structured background_unavailable; no forced PID termination";
  stopRecord.exited = exited;
  result.cleanup.cua_alt_f4_exit_observed = closeOutcome.ok && exited;
  result.cleanup.stop_method = stopRecord.stop_method;
  addCheck("installed app process exited after guarded native Alt+F4", exited, {
    pid,
    timeout_ms: 15_000,
    method: stopRecord.stop_method,
    close_request_error: closeOutcome.reason,
    final_process_state: exitResult.latest,
    forced_termination: null,
  });

  try {
    requireVerifiedCloseForRelaunch(closeOutcome, exitResult);
  } catch (error) {
    stopRecord.relaunch_refused_reason = error instanceof Error ? error.message : String(error);
    const failureUi = await captureFailureUi(`native-close-failure-${currentPhase}`);
    stopRecord.failure_ui = {
      text_sample: failureUi.text_sample,
      ui_read_error: failureUi.ui_read_error,
      screenshot_error: failureUi.screenshot_error,
      label_error: failureUi.label_error,
    };
    throw error;
  }

  activeAppPid = null;
  activeWindowId = null;
  activeLaunchIdentity = null;
  result.cleanup.app_stopped = true;
}


async function createProjectInUi() {
  let state = await getStateChecked("fresh native UI state is readable before project creation", "project-create-initial-state-failed");
  const unexpectedProjectVisible = labels(state).includes(projectName);
  addCheck("run-specific project is absent before native creation", !unexpectedProjectVisible, { project: projectName, visible: unexpectedProjectVisible });
  if (unexpectedProjectVisible) throw new Error(`Unexpected project name already visible before create: ${projectName}`);
  await clickByTextChecked("project selector opens before new-project creation", "project-create-selector-failed", "NO PROJECT", { role: "Button" });
  await clickByTextChecked("new-project form is requested through the selector", "project-create-form-button-failed", "+ NEW PROJECT", { role: "Button" });
  state = await waitForTextChecked("new-project form becomes visible", "project-create-form-timeout", "project name");
  try {
    fs.mkdirSync(projectDir, { recursive: true });
  } catch (error) {
    addCheck("synthetic project workspace directory is created", false, { cwd: projectDir, error: error instanceof Error ? error.message : String(error) });
    throw error;
  }
  addCheck("synthetic project workspace directory is created", true, { cwd: projectDir });
  await typeIntoChecked("native project form accepts run-specific name", "project-name-entry-failed", "project name", projectName);
  await typeIntoChecked("native project form accepts synthetic workspace path", "project-cwd-entry-failed", "cwd (absolute path)", projectDir);
  await withUiFailureCheck("native project form screenshot is captured", "project-form-screenshot-failed", () => captureScreenshot("project-form", getState(false)));
  await clickByTextChecked("native CREATE & OPEN action completes", "project-create-action-failed", "CREATE & OPEN", { role: "Button" });
  state = await waitForTextChecked("new project appears open in the native UI", "project-open-timeout", projectName, 45_000);
  result.project.status = "CREATED_AND_OPENED_IN_NATIVE_UI";
  addCheck("project created and opened through native UI", labels(state).includes(projectName), { name: projectName, cwd: projectDir });
  await withUiFailureCheck("native opened-project screenshot is captured", "project-open-screenshot-failed", () => captureScreenshot("project-open", state));
}

async function ensureMemoryPanel() {
  let state = await getStateChecked("native UI state is readable before adding Memory panel", "memory-panel-initial-state-failed");
  if (labels(state).toLowerCase().includes("+ add")) return state;
  await clickByTextChecked("panel palette opens through native UI", "memory-panel-palette-button-failed", "+ PANELS", { role: "Button" });
  state = await waitForTextChecked("panel palette becomes visible", "memory-panel-palette-timeout", "ADD PANEL");
  await withUiFailureCheck("panel palette screenshot is captured", "memory-panel-palette-screenshot-failed", () => captureScreenshot("panel-palette", state));
  await clickByTextChecked("Memory panel is selected from the native palette", "memory-panel-select-failed", "MEMORY");
  state = await waitForTextChecked("Memory panel exposes its add action", "memory-panel-add-timeout", "+ ADD", 30_000);
  addCheck("Memory panel opened through native panel palette", labels(state).toLowerCase().includes("+ add"), {});
  return state;
}

async function saveSyntheticMemory() {
  let state = await ensureMemoryPanel();
  await clickByTextChecked("memory form opens from the native Memory panel", "memory-add-button-failed", "+ ADD", { role: "Button" });
  state = await waitForTextChecked("synthetic-memory entry form becomes visible", "memory-form-timeout", "a durable fact to remember");
  await typeIntoChecked("native Memory form accepts synthetic text", "memory-text-entry-failed", "a durable fact to remember", memoryText, { role: "Edit" });
  await typeIntoChecked("native Memory form accepts the synthetic category", "memory-category-entry-failed", "category (convention", memoryCategory, { role: "Edit" });
  state = await getStateChecked("native Memory form state is readable before save", "memory-form-state-failed");
  const projectScopeVisible = labels(state).toLowerCase().includes("project");
  addCheck("memory form exposes the intended project scope", projectScopeVisible, { scope: "project", text_sample: labels(state).slice(0, 2_000) });
  if (!projectScopeVisible) throw new Error("Memory form did not expose its default project scope");
  await withUiFailureCheck("synthetic-memory form screenshot is captured", "memory-form-screenshot-failed", () => captureScreenshot("memory-form", state));
  await clickByTextChecked("synthetic Memory SAVE action completes", "memory-save-action-failed", "SAVE", { role: "Button" });
  state = await waitForTextChecked("synthetic memory appears in the native Memory panel", "memory-saved-list-timeout", memoryText, 45_000);
  result.memory.status = "SAVED_THROUGH_NATIVE_UI";
  addCheck("synthetic project memory saved and listed in native UI", labels(state).includes(memoryText), { category: memoryCategory, scope: "project" });
  await withUiFailureCheck("saved-memory screenshot is captured", "memory-saved-screenshot-failed", () => captureScreenshot("memory-saved", state));
}

function readEmbeddingFromDatabase(stage) {
  const dbPath = path.join(dotzProfile, "ai-agents", "memory.db");
  const helper = path.join(scriptDir, "verify-windows-memory-embedding.py");
  try {
    const completed = runSync(process.env.DOTZ_PYTHON || "python", [helper, "--db", dbPath, "--text", memoryText, "--cwd", projectDir, "--category", memoryCategory], { timeoutMs: 60_000 });
    fs.writeFileSync(path.join(outputDir, `memory-${stage}-raw.stdout.json`), completed.stdout || "", "utf8");
    fs.writeFileSync(path.join(outputDir, `memory-${stage}-raw.stderr.log`), completed.stderr || "", "utf8");
    if (completed.status !== 0) throw new Error(`SQLite memory/embedding probe exited ${completed.status}: ${completed.stderr || completed.stdout}`);
    const proof = JSON.parse(completed.stdout);
    fs.writeFileSync(path.join(outputDir, `memory-${stage}.json`), `${JSON.stringify(proof, null, 2)}\n`, "utf8");
    addCheck(`local memory/embedding proof read succeeds (${stage})`, true, { database: dbPath, dimensions: proof.dimensions, embedding_sha256: proof.embedding_sha256 });
    return proof;
  } catch (error) {
    addCheck(`local memory/embedding proof read succeeds (${stage})`, false, { database: dbPath, error: error instanceof Error ? error.message : String(error) });
    throw error;
  }
}

async function main() {
  const runnerIsWindowsX64 = process.platform === "win32" && os.arch() === "x64";
  addCheck("acceptance runner is native Windows x64", runnerIsWindowsX64, { platform: process.platform, arch: os.arch() });
  if (!runnerIsWindowsX64) throw new Error("actual installer acceptance requires a native Windows x64 runner");

  const installerPath = path.resolve(args.installer);
  const stat = fs.statSync(installerPath);
  const pe = readPeInfo(installerPath);
  const installerHash = await sha256File(installerPath);
  const trustedSha = loadAndValidateMetadata(installerPath, installerHash, stat.size);
  result.installer = {
    path: installerPath,
    filename: path.basename(installerPath),
    bytes: stat.size,
    sha256: installerHash,
    expected_sha256: trustedSha,
    pe,
    payload_target_arch: result.artifact_metadata.target_arch,
    artifact_type: mode === "released" ? "public GitHub v0.2.8 Tauri NSIS setup.exe (x64 payload target)" : "candidate Tauri NSIS setup.exe (x64 payload target)",
    nsis_evidence: "The pinned Tauri bundle/feed target is windows-x86_64 and NSIS; a PE32 x86 NSIS bootstrapper is allowed, while the installed dotz.exe is checked separately as AMD64 PE32+.",
  };
  const validInstaller = stat.isFile() && path.extname(installerPath).toLowerCase() === ".exe" &&
    isSupportedSetupPe(pe, result.artifact_metadata.target_arch);
  addCheck("installer is a valid PE container for the pinned x64 NSIS package", validInstaller, {
    bytes: stat.size,
    payload_target_arch: result.artifact_metadata.target_arch,
    pe,
    expected_sha256: trustedSha,
  });
  if (!validInstaller) throw new Error("refusing to run a malformed setup PE or one without independently verified x64 package metadata");
  addCheck("installer SHA-256 matches pinned release/build metadata", installerHash.toLowerCase() === trustedSha, { actual_sha256: installerHash, expected_sha256: trustedSha });
  if (installerHash.toLowerCase() !== trustedSha) throw new Error("installer bytes do not match the verified public release or exact candidate build artifact");

  const prereqs = inspectRuntimePrerequisites();
  addCheck("WebView2 runtime prerequisite was discovered before silent install", Boolean(prereqs.webview2), { runtime: prereqs.webview2 });
  if (!prereqs.webview2) throw new Error("WebView2 Evergreen runtime was not discovered before install; do not run the NSIS installer without a known prerequisite state");
  addCheck("MSVC runtime inventory was captured before silent install", prereqs.hostVcruntimeFiles.length === 4, { files: prereqs.hostVcruntimeFiles });
  const cleanProfile = !fs.existsSync(dotzProfile);
  addCheck("fresh disposable Windows user profile has no existing dotz data", cleanProfile, { profile_exists: !cleanProfile, path: dotzProfile });
  if (!cleanProfile) throw new Error(`refusing to install over pre-existing user Dotz data at ${dotzProfile}`);
  addCheck("production profile is not redirected by DOTZ_CONFIG_DIR", !process.env.DOTZ_CONFIG_DIR, { override: process.env.DOTZ_CONFIG_DIR || null });
  addCheck("model resources are not redirected away from the installed app", !process.env.DOTZ_ASSETS && !process.env.DOTZ_MODELS, { dotz_assets: process.env.DOTZ_ASSETS || null, dotz_models: process.env.DOTZ_MODELS || null });
  if (process.env.DOTZ_CONFIG_DIR || process.env.DOTZ_ASSETS || process.env.DOTZ_MODELS) {
    throw new Error("Unexpected DOTZ_* override would invalidate fresh installed-app acceptance");
  }

  const install = runSync(installerPath, ["/S"], { timeoutMs: 10 * 60_000 });
  result.installation = { silent_flag: "/S", exit_code: install.status, stdout: install.stdout || "", stderr: install.stderr || "" };
  fs.writeFileSync(path.join(outputDir, "installer.stdout.log"), install.stdout || "", "utf8");
  fs.writeFileSync(path.join(outputDir, "installer.stderr.log"), install.stderr || "", "utf8");
  addCheck("NSIS documented /S per-user installation exited zero", install.status === 0, { exit_code: install.status, signal: install.signal || null });
  if (install.status !== 0) throw new Error(`NSIS installer /S exited ${install.status}`);

  const entries = discoverInstallEntry();
  const exePaths = findInstalledExe(entries);
  result.installation.registry_entries = entries;
  result.installation.installed_exe_candidates = exePaths;
  addCheck("installer registered a dotz installation", entries.length > 0 || exePaths.length > 0, { entries, exe_paths: exePaths });
  if (exePaths.length !== 1) throw new Error(`Expected exactly one installed dotz.exe; discovered ${JSON.stringify(exePaths)}`);
  const appExe = exePaths[0];
  result.installation.app_exe = appExe;
  const appPe = readPeInfo(appExe);
  result.installation.app_exe_pe = appPe;
  const installedAppIsX64 = isX64ApplicationPe(appPe);
  addCheck("installed dotz.exe is AMD64 PE32+ x64 application binary", installedAppIsX64, { app_exe: appExe, pe: appPe });
  if (!installedAppIsX64) throw new Error(`Installed dotz.exe is not an AMD64 PE32+ x64 binary: ${JSON.stringify(appPe)}`);
  const installDir = path.dirname(appExe);
  result.installation.install_dir = installDir;
  const localAppDataRoot = path.resolve(process.env.LOCALAPPDATA || "");
  const relativeToLocalAppData = localAppDataRoot ? path.relative(localAppDataRoot, installDir) : "";
  const installedForCurrentUser = Boolean(localAppDataRoot) && relativeToLocalAppData !== ".." &&
    !relativeToLocalAppData.startsWith(`..${path.sep}`) && !path.isAbsolute(relativeToLocalAppData);
  result.installation.install_scope = installedForCurrentUser ? "current-user LOCALAPPDATA" : "outside current-user LOCALAPPDATA";
  addCheck("NSIS install scope matches Tauri current-user default", installedForCurrentUser, { local_app_data: localAppDataRoot, install_dir: installDir, scope: result.installation.install_scope });
  if (!installedForCurrentUser) throw new Error(`Expected Tauri default current-user install under LOCALAPPDATA; installed at ${installDir}`);
  const runtimeDllNames = ["msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll"];
  const bundledRuntime = runtimeDllNames.map((name) => ({ name, present: fs.existsSync(path.join(installDir, name)) }));
  result.installation.bundled_vc_runtime = bundledRuntime;
  const hostRuntimeNames = new Set(result.environment.host_vc_runtime_files.filter((item) => item.present).map((item) => item.name.toLowerCase()));
  const hostHasVCRuntime = hostRuntimeNames.has("vcruntime140.dll") && hostRuntimeNames.has("msvcp140.dll");
  const packageHasVCRuntime = bundledRuntime.some((item) => item.name.toLowerCase() === "vcruntime140.dll" && item.present) &&
    bundledRuntime.some((item) => item.name.toLowerCase() === "msvcp140.dll" && item.present);
  addCheck("MSVC runtime dependency is available from the host or app-local package", hostHasVCRuntime || packageHasVCRuntime, { host_system32_runtime: result.environment.host_vc_runtime_files, bundled_runtime: bundledRuntime });
  if (!hostHasVCRuntime && !packageHasVCRuntime) throw new Error("required MSVC runtime DLLs were not found on the host or inside the installed package");
  const model = await modelResources(installDir);
  result.installation.model_resources = model;
  addCheck("installed package includes real local all-MiniLM-L6-v2 model resources", Boolean(model && model.model.bytes > 1_000_000 && model.tokenizer.bytes > 1_000), { model });

  const version = runPowerShellJson(`(Get-Item -LiteralPath '${appExe.replaceAll("'", "''")}').VersionInfo | Select-Object ProductName,ProductVersion,FileVersion | ConvertTo-Json -Compress`);
  result.installation.binary_version = version;
  addCheck("installed dotz executable version metadata is readable", Boolean(version), { version });
  if (fs.existsSync(dotzProfile)) throw new Error(`Expected a fresh Dotz profile before first launch, but ${dotzProfile} already exists`);
  const freshProfile = await snapshotProfile("fresh-before-launch");
  addCheck("prelaunch profile snapshot is empty", freshProfile.file_count === 0, { aggregate_sha256: freshProfile.aggregate_sha256, file_count: freshProfile.file_count });

  if (!fs.existsSync(driverBin)) throw new Error(`cua-driver missing at ${driverBin}`);
  const versionProbe = runSync(driverBin, ["--version"]);
  fs.writeFileSync(path.join(outputDir, "cua-driver-version.log"), `${versionProbe.stdout || ""}${versionProbe.stderr || ""}`, "utf8");
  addCheck("cua-driver is installed on the hosted Windows runner", versionProbe.status === 0, { output: (versionProbe.stdout || versionProbe.stderr || "").trim() });
  if (versionProbe.status !== 0) throw new Error("cua-driver --version failed");
  const doctor = runSync(driverBin, ["doctor"], { timeoutMs: 60_000 });
  const doctorText = `${doctor.stdout || ""}${doctor.stderr || ""}`;
  fs.writeFileSync(path.join(outputDir, "cua-driver-doctor.log"), doctorText, "utf8");
  const sessionMatch = doctorText.match(/interactive session[^\n]*session\s+(\d+)/i);
  if (sessionMatch) result.runner.session_id = Number(sessionMatch[1]);
  addCheck("hosted Windows desktop and UI Automation are available", doctor.status === 0 && /attached interactive desktop/i.test(doctorText) && /UI Automation.*succeeded/i.test(doctorText), { exit_code: doctor.status, session_id: result.runner.session_id, output: doctorText.trim() });
  if (doctor.status !== 0 || !/attached interactive desktop/i.test(doctorText) || !/UI Automation.*succeeded/i.test(doctorText)) {
    throw new Error("cua-driver doctor did not prove an attached interactive desktop and UI Automation");
  }

  startCuaDaemon();
  call("start_session", {}, 30_000);
  driverSessionStarted = true;
  addCheck("cua-driver native UI session started", true, { session: driverSession });
  currentPhase = "first-run";
  let state = await launchNativeApp(appExe);
  await withUiFailureCheck("fresh native launch screenshot is captured", "fresh-native-launch-screenshot-failed", () => captureScreenshot("fresh-native-launch-initial", state));
  let firstLaunchText = labels(state);
  if (!/WELCOME TO dotz|STEP [1-4]|NO PROJECT/i.test(firstLaunchText)) {
    try {
      state = await waitForFirstRunBranch();
      firstLaunchText = labels(state);
    } catch (error) {
      result.onboarding.status = "FAILED";
      const message = error instanceof Error ? error.message : String(error);
      const failureUi = await captureFailureUi("first-run-branch-timeout");
      addCheck("fresh native launch reaches first-run wizard or dashboard", false, {
        wait_error: message,
        text_sample: failureUi.text_sample,
        ui_read_error: failureUi.ui_read_error,
        screenshot_error: failureUi.screenshot_error,
        label_error: failureUi.label_error,
      });
      throw error;
    }
  }
  await withUiFailureCheck("settled first-run/dashboard screenshot is captured", "fresh-native-launch-settled-screenshot-failed", () => captureScreenshot("fresh-native-launch-settled", state));
  const initialOnboarding = classifyInitialOnboarding(mode, firstLaunchText);
  const markerPath = path.join(dotzProfile, "first-run-done");
  if (mode === "released" && initialOnboarding === "wizard-visible") {
    addCheck("released v0.2.8 runtime agrees with its source-confirmed wizard absence", false, {
      tag: RELEASED_ASSET.tag,
      tag_commit: RELEASED_ASSET.tagCommit,
      first_run_source: RELEASED_ASSET.firstRunSource,
      text_sample: firstLaunchText.slice(0, 2_000),
    });
  }
  if (initialOnboarding === "released-wizard-missing") {
    result.onboarding.status = "NOT_PRESENT_IN_RELEASE_TAG";
    addCheck("public v0.2.8 ships the required native first-run onboarding", false, {
      tag: RELEASED_ASSET.tag,
      tag_commit: RELEASED_ASSET.tagCommit,
      dashboard_visible: firstLaunchText.toLowerCase().includes("no project"),
      first_run_source: RELEASED_ASSET.firstRunSource,
      text_sample: firstLaunchText.slice(0, 2_000),
    });
    addSkip("released provider/key onboarding interaction", "v0.2.8 source does not ship web/wizard.js or its index.html reference");
    addSkip("released embedding/browser onboarding interaction", "v0.2.8 source does not ship the first-run wizard");
    addSkip("released optional project onboarding interaction", "v0.2.8 source does not ship the first-run wizard");
    addSkip("released Finish marker interaction", "v0.2.8 source does not ship the first-run wizard");
  } else {
    if (initialOnboarding === "candidate-wizard-wait") {
      try {
        state = await waitForFirstRunWizard();
      } catch (error) {
        result.onboarding.status = "FAILED";
        const message = error instanceof Error ? error.message : String(error);
        const failureUi = await captureFailureUi("candidate-first-run-wizard-timeout");
        addCheck("fresh native WebView2 first-run wizard is visible", false, {
          wait_error: message,
          text_sample: failureUi.text_sample,
          ui_read_error: failureUi.ui_read_error,
          screenshot_error: failureUi.screenshot_error,
          label_error: failureUi.label_error,
        });
        throw error;
      }
    }
    const wizardVisible = /WELCOME TO dotz|STEP [1-4]/i.test(labels(state));
    addCheck("fresh native WebView2 first-run wizard is visible", wizardVisible, { text_sample: labels(state).slice(0, 2_000) });
    if (!wizardVisible) throw new Error(`candidate first-run wizard did not appear in native WebView2: ${labels(state).slice(0, 2_000)}`);
    await withUiFailureCheck("onboarding step-1 screenshot is captured", "onboarding-step1-screenshot-failed", () => captureScreenshot("first-run-step-1", state));
    result.onboarding.status = "IN_PROGRESS";

    await clickByTextChecked("onboarding step 1 Skip interaction completes", "onboarding-step1-skip-failed", "SKIP", { role: "Button" });
    state = await waitForTextChecked("candidate onboarding reaches step 2", "onboarding-step2-timeout", "STEP 2", 30_000);
    await withUiFailureCheck("onboarding step-2 screenshot is captured", "onboarding-step2-screenshot-failed", () => captureScreenshot("first-run-step-2", state));
    const modelStatusVisible = /model files present/i.test(labels(state));
    addCheck("onboarding sees bundled model resources", modelStatusVisible, { text_sample: labels(state).slice(0, 1_500) });
    await clickByTextChecked("onboarding step 2 Next interaction completes", "onboarding-step2-next-failed", "NEXT", { role: "Button" });
    state = await waitForTextChecked("candidate onboarding reaches optional project step 3", "onboarding-step3-timeout", "STEP 3", 30_000);
    const projectStepReached = /STEP 3/i.test(labels(state));
    addCheck("native onboarding reached its optional project step", projectStepReached, {});
    if (!projectStepReached) throw new Error("native onboarding did not expose optional project step 3");
    await clickByTextChecked("onboarding optional project step is skipped without user data", "onboarding-step3-skip-failed", "SKIP", { role: "Button" });
    state = await waitForTextChecked("candidate onboarding reaches final step 4", "onboarding-step4-timeout", "STEP 4", 30_000);
    await withUiFailureCheck("onboarding step-4 screenshot is captured", "onboarding-step4-screenshot-failed", () => captureScreenshot("first-run-step-4", state));
    await clickByTextChecked("onboarding Finish interaction completes", "onboarding-finish-action-failed", "FINISH", { role: "Button" });
    state = await waitForTextChecked("candidate onboarding returns to command center", "onboarding-home-timeout", "NO PROJECT", 30_000);
    addCheck("onboarding Finish returned to native command center", /NO PROJECT/i.test(labels(state)), { text_sample: labels(state).slice(0, 1_500) });
    addCheck("onboarding Finish wrote the first-run marker", fs.existsSync(markerPath), { marker: markerPath });
    result.onboarding.status = fs.existsSync(markerPath) ? "COMPLETED_IN_NATIVE_UI" : "FAILED";
    await withUiFailureCheck("first-run home UI state and screenshot are captured", "first-run-home-screenshot-failed", async () => {
      state = getState(false);
      await captureScreenshot("first-run-home", state);
    });
    const firstRunProfile = await snapshotProfile("first-run-complete");
    addCheck("first-run created a persisted Dotz profile", firstRunProfile.exists && firstRunProfile.file_count > 0, { file_count: firstRunProfile.file_count, aggregate_sha256: firstRunProfile.aggregate_sha256 });
  }

  await createProjectInUi();
  await ensureMemoryPanel();
  await saveSyntheticMemory();
  const memoryDbPath = path.join(dotzProfile, "ai-agents", "memory.db");
  const firstEmbedding = readEmbeddingFromDatabase("first-run");
  result.memory.first_run = firstEmbedding;
  addCheck("saved memory has a local 384-dimensional normalized ONNX embedding", firstEmbedding.dimensions === 384 && firstEmbedding.embedding_bytes === 1536 && firstEmbedding.finite === true && Math.abs(firstEmbedding.l2_norm - 1) < 0.0001, { dimensions: firstEmbedding.dimensions, embedding_bytes: firstEmbedding.embedding_bytes, l2_norm: firstEmbedding.l2_norm, embedding_sha256: firstEmbedding.embedding_sha256, database: memoryDbPath });
  addCheck("saved memory belongs to the synthetic project scope", firstEmbedding.scope === "project" && firstEmbedding.user_id === firstEmbedding.expected_user_id, { scope: firstEmbedding.scope, user_id: firstEmbedding.user_id, expected_user_id: firstEmbedding.expected_user_id });
  const projectMemoryProfile = await snapshotProfile("project-memory-first-run");
  addCheck("first-run project/memory state changed the on-disk profile", projectMemoryProfile.file_count > freshProfile.file_count && projectMemoryProfile.aggregate_sha256 !== freshProfile.aggregate_sha256, { fresh_file_count: freshProfile.file_count, memory_file_count: projectMemoryProfile.file_count, fresh_sha256: freshProfile.aggregate_sha256, memory_sha256: projectMemoryProfile.aggregate_sha256 });

  currentPhase = "reuse-after-restart";
  await stopApp();
  await sleep(1_200);
  state = await launchNativeApp(appExe);
  const restartUiSamples = [labels(state)];
  for (let sample = 1; sample < 8; sample += 1) {
    await sleep(500);
    state = getState(false);
    restartUiSamples.push(labels(state));
  }
  // The UI deliberately boots to its command center and resets the selector label to "NO PROJECT";
  // persisted project-list membership is asserted below by opening the selector after this stable state.
  const restartUiClass = classifyRestartUiSamples(restartUiSamples);
  const restartSampleSummary = restartUiSamples.map((text, index) => ({
    index,
    wizard_visible: /WELCOME TO dotz|STEP [1-4]/i.test(text),
    command_center_visible: /NO PROJECT/i.test(text) && /\bSEND\b/i.test(text),
  }));
  await withUiFailureCheck("restart UIA screenshot is captured", "restart-ui-screenshot-failed", () => captureScreenshot("restart-ui-settled-samples", state));
  const reuseDecision = classifyOnboardingReuse(mode, result.onboarding.status);
  const commandCenterSettled = restartUiClass === "dashboard";
  addCheck("same-profile restart settles on command center across consecutive UIA samples", commandCenterSettled, {
    classification: restartUiClass,
    sample_count: restartUiSamples.length,
    samples: restartSampleSummary,
  });
  if (reuseDecision === "assert-reuse") {
    addCheck("first-run wizard does not return on same-profile restart", commandCenterSettled, {
      classification: restartUiClass,
      marker_exists: fs.existsSync(markerPath),
      samples: restartSampleSummary,
    });
  } else if (reuseDecision === "skip-release-reuse" && restartUiClass !== "wizard") {
    addSkip("first-run wizard reuse assertion after restart", "the released v0.2.8 tag did not ship a first-run wizard");
  } else if (reuseDecision === "skip-release-reuse") {
    addCheck("released v0.2.8 runtime remains consistent with source-confirmed wizard absence after restart", false, {
      source: RELEASED_ASSET.firstRunSource,
      samples: restartSampleSummary,
    });
  } else {
    addCheck("first-run completion state was available for reuse assertion", false, { mode, onboarding_status: result.onboarding.status, classification: restartUiClass, marker_exists: fs.existsSync(markerPath), samples: restartSampleSummary });
  }
  if (!commandCenterSettled) throw new Error(`same-profile restart did not settle on the native command center; observed ${restartUiClass}`);
  try {
    await clickByText("NO PROJECT", { role: "Button" });
    state = await waitForText("+ NEW PROJECT", 10_000);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const failureUi = await captureFailureUi("reuse-project-selector-open-failed");
    addCheck("native project selector menu opens after restart", false, {
      selector_button: "NO PROJECT",
      error: message,
      text_sample: failureUi.text_sample,
      ui_read_error: failureUi.ui_read_error,
      screenshot_error: failureUi.screenshot_error,
      label_error: failureUi.label_error,
    });
    addCheck("run-specific project appears in the native project selector after restart", false, { project: projectName, selector_open_error: message });
    throw error;
  }
  const selectorMenuVisible = labels(state).includes("+ NEW PROJECT");
  addCheck("native project selector menu opens after restart", selectorMenuVisible, {
    selector_button: "NO PROJECT",
    text_sample: labels(state).slice(0, 2_000),
  });
  if (!selectorMenuVisible) throw new Error("project selector click did not expose its + NEW PROJECT menu action");
  await withUiFailureCheck("open restart project selector screenshot is captured", "reuse-project-selector-screenshot-failed", () => captureScreenshot("reuse-project-selector-open-after-restart", state));
  try {
    state = await waitForText(projectName, 30_000);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const failureUi = await captureFailureUi("reuse-project-not-listed-after-restart");
    addCheck("run-specific project appears in the native project selector after restart", false, {
      project: projectName,
      wait_error: message,
      text_sample: failureUi.text_sample,
      ui_read_error: failureUi.ui_read_error,
      screenshot_error: failureUi.screenshot_error,
      label_error: failureUi.label_error,
    });
    throw error;
  }
  const persistedProjectVisible = labels(state).includes(projectName);
  addCheck("run-specific project appears in the native project selector after restart", persistedProjectVisible, {
    project: projectName,
    text_sample: labels(state).slice(0, 2_000),
  });
  if (!persistedProjectVisible) throw new Error(`project selector did not expose persisted project ${projectName}`);
  await withUiFailureCheck("persisted project-list screenshot is captured", "reuse-project-list-screenshot-failed", () => captureScreenshot("reuse-project-list-after-restart", state));
  try {
    await clickByText(projectName, { role: "Button" });
    state = await waitForText(projectName, 30_000);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const failureUi = await captureFailureUi("reuse-project-open-failed-after-restart");
    addCheck("persisted project reopens from the native selector", false, {
      project: projectName,
      error: message,
      text_sample: failureUi.text_sample,
      ui_read_error: failureUi.ui_read_error,
      screenshot_error: failureUi.screenshot_error,
      label_error: failureUi.label_error,
    });
    throw error;
  }
  addCheck("persisted project reopens from the native selector", labels(state).includes(projectName), {
    project: projectName,
    text_sample: labels(state).slice(0, 2_000),
  });
  result.project.reuse_status = "OPENED_FROM_PERSISTED_NATIVE_SELECTOR";
  await withUiFailureCheck("reopened project screenshot is captured", "reuse-project-open-screenshot-failed", () => captureScreenshot("reuse-project-open-after-restart", state));
  addCheck("project is present after app restart", labels(state).includes(projectName), { project: projectName, visible: labels(state).includes(projectName) });
  if (!labels(state).toLowerCase().includes("+ add")) await ensureMemoryPanel();
  state = await waitForTextChecked("synthetic memory remains listed in the native panel after restart", "reuse-memory-list-timeout", memoryText, 45_000);
  addCheck("synthetic memory is listed in the native Memory panel after restart", labels(state).includes(memoryText), { memory: memoryText });
  await withUiFailureCheck("reused-memory screenshot is captured", "reuse-memory-screenshot-failed", () => captureScreenshot("reuse-memory-after-restart", state));
  const reusedEmbedding = readEmbeddingFromDatabase("reuse-after-restart");
  result.memory.reuse = reusedEmbedding;
  addCheck("384-dimensional memory embedding persists unchanged after restart", reusedEmbedding.dimensions === 384 && reusedEmbedding.embedding_sha256 === firstEmbedding.embedding_sha256 && reusedEmbedding.embedding_bytes === firstEmbedding.embedding_bytes, { first_run_sha256: firstEmbedding.embedding_sha256, reuse_sha256: reusedEmbedding.embedding_sha256, first_run_bytes: firstEmbedding.embedding_bytes, reuse_bytes: reusedEmbedding.embedding_bytes });
  await snapshotProfile("reuse-after-restart");
  const childrenAfterRestart = webviewChildren(activeAppPid);
  result.webview2_processes_after_restart = childrenAfterRestart;
  addCheck("restarted installed app has native WebView2 child processes", childrenAfterRestart.length > 0, { count: childrenAfterRestart.length, processes: childrenAfterRestart });
  await withUiFailureCheck("restarted native window screenshot is captured", "reuse-window-screenshot-failed", () => captureScreenshot("reuse-window-after-restart", getState(false)));
  result.memory.status = "PERSISTED_AFTER_RESTART";
  result.project.status = "PERSISTED_AFTER_RESTART";
}

persist();
log(`START mode=${mode} installer=${path.resolve(args.installer)} output=${outputDir}`);
try {
  await main();
} catch (error) {
  fatalError = error instanceof Error ? error.stack || error.message : String(error);
  log(`FATAL ${fatalError}`);
  result.error = fatalError;
} finally {
  try { if (activeAppPid && !activeStopAttempted) await stopApp(); }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    log(`CLEANUP_APP_ERROR ${message}`);
    result.cleanup.app_error = message;
    addCheck("installed app cleanup completed without error", false, { error: message, pid: activeAppPid });
    fatalError = fatalError ? `${fatalError}\nCLEANUP_APP_ERROR ${message}` : `CLEANUP_APP_ERROR ${message}`;
    result.error = fatalError;
  }
  try { stopCuaDaemon(); }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    log(`CLEANUP_CUA_ERROR ${message}`);
    result.cleanup.cua_error = message;
    addCheck("cua-driver cleanup completed without error", false, { error: message });
    fatalError = fatalError ? `${fatalError}\nCLEANUP_CUA_ERROR ${message}` : `CLEANUP_CUA_ERROR ${message}`;
    result.error = fatalError;
  }
  result.raw_counts = { ...counts(), by_phase: phaseCounts() };
  result.phase_counts = phaseCounts();
  result.status = result.raw_counts.failed === 0 && !fatalError ? "COMPLETE" : "FAILED";
  result.result = result.raw_counts.failed === 0 && !fatalError ? "PASS" : "FAIL";
  result.finished_at = new Date().toISOString();
  persist();
}

if (result.result !== "PASS") process.exitCode = 1;
else log(`ACCEPTANCE PASS mode=${mode} counts=${JSON.stringify(result.raw_counts)}`);
