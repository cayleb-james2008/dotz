import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as runtime from "./windows-installer-runtime.mjs";

for (const name of [
  "createDriverCallError",
  "installedProcessLaunchIdentity",
  "validateNativeCloseTarget",
  "sendGuardedClose",
  "requireVerifiedCloseForRelaunch",
]) {
  assert.equal(typeof runtime[name], "function", `${name} must be exported for native-close acceptance`);
}

const refusalPayload = {
  code: "background_unavailable",
  escalation: { recommended: "foreground", reason: "Retry this action with delivery_mode:\"foreground\"." },
  event_kind: "key_combo",
  target_class: "Tauri Window",
};
const refusalError = () => runtime.createDriverCallError("hotkey", {
  status: 1,
  stdout: "",
  stderr: JSON.stringify(refusalPayload),
});
const identity = {
  ProcessId: 1234,
  ParentProcessId: 1000,
  Name: "dotz.exe",
  ExecutablePath: String.raw`C:\Users\runneradmin\AppData\Local\dotz\dotz.exe`,
  SessionId: 2,
  CreationDate: "2026-10-06T06:00:00.000000Z",
};
const expectedExe = identity.ExecutablePath;
const launchIdentity = runtime.installedProcessLaunchIdentity(identity, expectedExe);
assert.ok(launchIdentity, "verified launch must yield a stable installed-process identity");
const liveState = { classification: "present", probe: { status: 0, stdout: "DOTZ_PROCESS_PRESENT:1234", stderr: "" } };
const windows = [{ app_name: "dotz.exe", pid: 1234, window_id: 5678, title: "dotz" }];
const target = (processInfo = identity, windowList = windows, processState = liveState, windowId = 5678) =>
  runtime.validateNativeCloseTarget({
    pid: 1234,
    windowId,
    expectedExecutable: expectedExe,
    expectedLaunchIdentity: launchIdentity,
    processState,
    processInfo,
    windows: windowList,
  });

// RED/GREEN: only the driver's parsed structured refusal can trigger one foreground retry.
{
  const requests = [];
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => target(),
    sendInput: (request) => {
      requests.push(request);
      if (requests.length === 1) throw refusalError();
      return { accepted: true, effect: "unverifiable" };
    },
  });
  assert.equal(close.ok, true);
  assert.equal(requests.length, 2, "the foreground action is attempted exactly once");
  assert.equal(requests[0].delivery_mode, undefined, "the first request uses driver's default background mode");
  assert.equal(requests[1].delivery_mode, "foreground");
  assert.deepEqual(close.attempts.map((attempt) => attempt.delivery_mode), ["background", "foreground"]);
  assert.equal(close.attempts[0].error.driver_failure.code, "background_unavailable");
  assert.deepEqual(close.attempts[0].error.driver_failure.parsed_error, refusalPayload);
  assert.deepEqual(close.attempts[1].response, { accepted: true, effect: "unverifiable" });
  assert.equal(close.verifications.length, 2, "target identity is revalidated before both input actions");
}

// Other structured errors and malformed JSON are preserved but never escalated.
for (const [label, completed] of [
  ["different code", { status: 1, stdout: "", stderr: JSON.stringify({ code: "stale_element_token", escalation: { recommended: "foreground" } }) }],
  ["malformed JSON", { status: 1, stdout: "", stderr: '{"code":"background_unavailable"' }],
  ["unrecommended refusal", { status: 1, stdout: "", stderr: JSON.stringify({ code: "background_unavailable", escalation: { recommended: "px" } }) }],
]) {
  const calls = [];
  const error = runtime.createDriverCallError("hotkey", completed);
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => target(),
    sendInput: (request) => { calls.push(request); throw error; },
  });
  assert.equal(close.ok, false, `${label} must remain a failed close`);
  assert.equal(calls.length, 1, `${label} must not escalate`);
  assert.equal(close.attempts.length, 1);
  assert.equal(close.attempts[0].error.driver_failure.stderr, completed.stderr);
}

// A stale PID/path/creation identity before the first close refuses all input.
{
  const stalePid = { ...identity, ProcessId: 4321 };
  const stale = target(stalePid);
  assert.equal(stale.valid, false);
  const calls = [];
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => stale,
    sendInput: (request) => calls.push(request),
  });
  assert.equal(close.ok, false);
  assert.equal(close.attempts.length, 0, "stale PID refuses before the initial background action");
  assert.equal(calls.length, 0);
}

// A stale window association after a structured refusal prevents foreground input.
{
  let verification = 0;
  const calls = [];
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => ++verification === 1 ? target() : target(identity, windows, liveState, 9999),
    sendInput: (request) => {
      calls.push(request);
      throw refusalError();
    },
  });
  assert.equal(close.ok, false);
  assert.equal(calls.length, 1, "foreground escalation is refused for a stale window association");
  assert.equal(close.attempts.length, 1);
  assert.equal(close.verifications.length, 2);
  assert.equal(close.verifications[1].valid, false);
}

// PID reuse/creation-time drift after background refusal prevents foreground input.
{
  let verification = 0;
  const calls = [];
  const replacement = { ...identity, CreationDate: "2026-10-06T06:01:00.000000Z" };
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => ++verification === 1 ? target() : target(replacement),
    sendInput: (request) => {
      calls.push(request);
      throw refusalError();
    },
  });
  assert.equal(close.ok, false);
  assert.equal(calls.length, 1, "PID reuse is refused before foreground input");
  assert.equal(close.verifications[1].valid, false);
  assert.ok(close.verifications[1].reasons.some((reason) => /launch identity/.test(reason)));
}

// A second foreground refusal must not cause a third attempt.
{
  const calls = [];
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => target(),
    sendInput: (request) => { calls.push(request); throw refusalError(); },
  });
  assert.equal(close.ok, false);
  assert.equal(calls.length, 2, "foreground escalation is bounded to one attempt");
  assert.equal(close.attempts.length, 2);
  assert.equal(close.attempts[1].error.driver_failure.code, "background_unavailable");
}

// Foreground failure is still a failure; process exit alone never authorizes relaunch.
{
  const calls = [];
  const foregroundFailure = runtime.createDriverCallError("hotkey", {
    status: 1,
    stdout: "",
    stderr: JSON.stringify({ code: "foreground_input_failed", target_class: "Tauri Window" }),
  });
  const close = runtime.sendGuardedClose({
    pid: 1234,
    windowId: 5678,
    keys: ["alt", "f4"],
    verifyTarget: () => target(),
    sendInput: (request) => {
      calls.push(request);
      if (calls.length === 1) throw refusalError();
      throw foregroundFailure;
    },
  });
  assert.equal(close.ok, false);
  assert.equal(close.attempts.length, 2);
  assert.equal(close.attempts[1].error.driver_failure.code, "foreground_input_failed");
  assert.throws(() => runtime.requireVerifiedCloseForRelaunch(close, { exited: true }), /close|input|foreground/i);
}

// Neither a successful input response without confirmed process exit nor a failed input with exit permits relaunch.
{
  const succeeded = { ok: true, attempts: [{ delivery_mode: "foreground", response: { accepted: true } }] };
  assert.throws(() => runtime.requireVerifiedCloseForRelaunch(succeeded, { exited: false }), /exit|closed/i);
  assert.equal(runtime.requireVerifiedCloseForRelaunch(succeeded, { exited: true }), true);
  assert.throws(() => runtime.requireVerifiedCloseForRelaunch({ ok: false, reason: "driver refused" }, { exited: true }), /driver refused|close/i);
}

const acceptanceSource = readFileSync(new URL("./windows-installer-acceptance.mjs", import.meta.url), "utf8");
assert.match(acceptanceSource, /if \(completed\.status !== 0\) throw createDriverCallError\(tool, completed\)/, "native close must retain the CLI's original stderr/stdout JSON error fields");
assert.match(acceptanceSource, /sendGuardedClose\(\{[\s\S]*?verifyTarget:\s*verifyCloseTarget[\s\S]*?sendInput:\s*\(request\)\s*=>\s*call\("hotkey", request\)/, "the real stopApp path must use the tested guarded close helper");
assert.match(acceptanceSource, /Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,SessionId,CreationDate/, "the native process probe must capture launch identity against PID reuse");
assert.match(acceptanceSource, /list_windows", \{ pid \}\)/, "the exact target window association must be re-read before close input");
assert.match(acceptanceSource, /requireVerifiedCloseForRelaunch\(closeOutcome, exitResult\)/, "relaunch requires both a successful input action and observed process exit");
assert.match(acceptanceSource, /if \(activeAppPid && !activeStopAttempted\) await stopApp\(\)/, "cleanup must not repeat a previously attempted Alt+F4 action");
const restartBoundary = acceptanceSource.indexOf("await stopApp();\n  await sleep(1_200);\n  state = await launchNativeApp(appExe);");
assert.ok(restartBoundary >= 0, "native relaunch and persistence reads remain strictly after stopApp succeeds");
console.log("windows installer guarded native-close tests passed (all assertions)");
