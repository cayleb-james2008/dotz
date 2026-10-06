import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { classifyInitialOnboarding, classifyOnboardingReuse, classifyProcessStateReadback, classifyRestartUiSamples, isDotzWindowForPid, isInstalledDotzProcess, isProcessAbsentReadback, parseDriverOutput, processStateProbeCommand, sleep, uiFailureDetails } from "./windows-installer-runtime.mjs";

const launch = parseDriverOutput("launch_app", '{"pid":7308}');
assert.deepEqual(launch, { pid: 7308 });
assert.throws(() => parseDriverOutput("get_window_state", "Ready"), /non-JSON output/);
assert.ok(sleep(0) instanceof Promise, "sleep returns an awaitable promise");
await sleep(1);
assert.equal(classifyInitialOnboarding("released", "Button:NO PROJECT"), "released-wizard-missing");
assert.equal(classifyInitialOnboarding("candidate", "Button:NO PROJECT"), "candidate-wizard-wait");
assert.equal(classifyInitialOnboarding("candidate", "Text:WELCOME TO dotz"), "wizard-visible");
assert.equal(classifyOnboardingReuse("candidate", "COMPLETED_IN_NATIVE_UI"), "assert-reuse");
assert.equal(classifyOnboardingReuse("released", "COMPLETED_IN_NATIVE_UI"), "assert-reuse");
assert.equal(classifyOnboardingReuse("released", "NOT_PRESENT_IN_RELEASE_TAG"), "skip-release-reuse");
assert.equal(classifyOnboardingReuse("candidate", "FAILED"), "fail-incomplete-reuse");
assert.equal(classifyOnboardingReuse("released", "FAILED"), "fail-incomplete-reuse");
assert.equal(isProcessAbsentReadback({ status: 0, stdout: "DOTZ_PROCESS_ABSENT", stderr: "" }), true);
assert.equal(isProcessAbsentReadback({ status: 0, stdout: "", stderr: "" }), false);
assert.equal(isProcessAbsentReadback({ status: 0, stdout: "DOTZ_PROCESS_ABSENT", stderr: "CIM query failed" }), false);
assert.equal(isProcessAbsentReadback({ status: 1, stdout: "DOTZ_PROCESS_ABSENT", stderr: "" }), false);
assert.equal(isProcessAbsentReadback({ status: 0, stdout: "DOTZ_PROCESS_PRESENT:1364", stderr: "" }), false);
assert.equal(classifyProcessStateReadback({ status: 0, stdout: "DOTZ_PROCESS_ABSENT", stderr: "" }, 1364), "absent");
assert.equal(classifyProcessStateReadback({ status: 0, stdout: "DOTZ_PROCESS_PRESENT:1364", stderr: "" }, 1364), "present");
assert.equal(classifyProcessStateReadback({ status: 0, stdout: "DOTZ_PROCESS_PRESENT:999", stderr: "" }, 1364), "unknown");
assert.equal(classifyProcessStateReadback({ status: 1, stdout: "", stderr: "Access denied" }, 1364), "unknown");
const installedExe = String.raw`C:\Users\runneradmin\AppData\Local\dotz\dotz.exe`;
assert.equal(isInstalledDotzProcess({ Name: "dotz.exe", ExecutablePath: installedExe }, installedExe), true);
assert.equal(isInstalledDotzProcess({ Name: "dotz.exe", ExecutablePath: String.raw`C:\Windows\System32\dotz.exe` }, installedExe), false);
assert.equal(isInstalledDotzProcess({ Name: "msedgewebview2.exe", ExecutablePath: installedExe }, installedExe), false);
assert.equal(isInstalledDotzProcess({ Name: "dotz.exe", ExecutablePath: "" }, installedExe), false);
assert.equal(isDotzWindowForPid({ app_name: "dotz.exe", pid: 1364, window_id: 196944 }, 1364), true);
assert.equal(isDotzWindowForPid({ app_name: "dotz.exe", pid: 999, window_id: 196944 }, 1364), false);
assert.equal(isDotzWindowForPid({ app_name: "chrome.exe", pid: 1364, window_id: 196944 }, 1364), false);
assert.equal(classifyRestartUiSamples(["NO PROJECT", "NO PROJECT", "NO PROJECT", "NO PROJECT"]), "unsettled");
assert.equal(classifyRestartUiSamples(["NO PROJECT SEND ▹", "NO PROJECT SEND ▹", "NO PROJECT SEND ▹", "NO PROJECT SEND ▹"]), "dashboard");
assert.equal(classifyRestartUiSamples(["PROJECT dotz-run-123", "PROJECT dotz-run-123", "PROJECT dotz-run-123", "PROJECT dotz-run-123"]), "unsettled");
assert.equal(classifyRestartUiSamples(["NO PROJECT SEND ▹", "NO PROJECT SEND ▹", "Loading", "NO PROJECT SEND ▹"]), "unsettled");
assert.equal(classifyRestartUiSamples(["NO PROJECT SEND ▹", "NO PROJECT SEND ▹", "STEP 3", "NO PROJECT SEND ▹"]), "wizard");
assert.equal(classifyRestartUiSamples(["Loading", "", "Loading", "Loading"]), "unsettled");
assert.equal(classifyRestartUiSamples(["NO PROJECT", "NO PROJECT", "NO PROJECT"]), "unsettled");
assert.deepEqual(uiFailureDetails(new Error("step timed out"), { text_sample: null, ui_read_error: "UIA unavailable", screenshot_error: "screenshot failed", label_error: null }), {
  error: "step timed out",
  text_sample: null,
  ui_read_error: "UIA unavailable",
  screenshot_error: "screenshot failed",
  label_error: null,
});
assert.deepEqual(uiFailureDetails("selector failed", { text_sample: "fresh selector UI", ui_read_error: null, screenshot_error: null, label_error: null }), {
  error: "selector failed",
  text_sample: "fresh selector UI",
  ui_read_error: null,
  screenshot_error: null,
  label_error: null,
});
const processProbe = processStateProbeCommand(1364);
assert.ok(processProbe.includes("Get-CimInstance -ClassName Win32_Process -Filter 'ProcessId = 1364' -ErrorAction Stop"));
assert.ok(processProbe.includes("DOTZ_PROCESS_ABSENT"));
assert.ok(processProbe.includes("[Console]::Error.WriteLine"));
assert.throws(() => processStateProbeCommand("1364; exit 0"), /positive safe integer/);
const acceptanceSource = readFileSync(new URL("./windows-installer-acceptance.mjs", import.meta.url), "utf8");
const memorySave = acceptanceSource.match(/async function saveSyntheticMemory\(\) \{([\s\S]*?)\n\}/);
assert.ok(memorySave, "native acceptance must define the synthetic Memory save journey");
assert.match(memorySave[1], /typeIntoChecked\(\s*"native Memory form accepts the synthetic category"/, "category entry must acquire a fresh UIA state/token after the text field changes the snapshot");
assert.doesNotMatch(memorySave[1], /findElement\(state,\s*\{\s*text:\s*"category/, "category entry must not reuse a token from the pre-text-entry UIA state");
assert.match(acceptanceSource, /sendGuardedClose\(\{[\s\S]*?windowId: activeWindowId,[\s\S]*?keys: \["alt", "f4"\],[\s\S]*?verifyTarget: verifyCloseTarget,[\s\S]*?sendInput: \(request\) => call\("hotkey", request\)/, "Alt+F4 must use the identity-guarded fallback with the exact native dotz window");
const stopApp = acceptanceSource.match(/async function stopApp\(\) \{([\s\S]*?)\n\}/);
assert.ok(stopApp, "native cleanup must define stopApp");
assert.match(stopApp[1], /if \(!Number\.isSafeInteger\(activeWindowId\) \|\| activeWindowId < 1\) \{[\s\S]*?stopRecord\.close_refused_reason[\s\S]*?stop_refused[\s\S]*?throw new Error[\s\S]*?\}/, "cleanup must refuse Alt+F4 if launch never verified a native window ID");
assert.ok(stopApp[1].indexOf("if (!Number.isSafeInteger(activeWindowId)") < stopApp[1].indexOf("sendGuardedClose({"), "window-ID guard must run before any native close input");

if (process.platform === "win32") {
  const runProbe = (pid) => spawnSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", processStateProbeCommand(pid)], {
    encoding: "utf8",
    windowsHide: true,
    timeout: 15_000,
  });
  const absentProbe = runProbe(2_147_483_647);
  assert.equal(absentProbe.error, undefined, `absent-PID CIM probe failed to launch: ${absentProbe.error || ""}`);
  assert.equal(classifyProcessStateReadback(absentProbe, 2_147_483_647), "absent", `absent PID must be explicit: ${JSON.stringify(absentProbe)}`);
  const currentProcessProbe = runProbe(process.pid);
  assert.equal(currentProcessProbe.status, 0, `live-PID CIM probe failed: ${JSON.stringify(currentProcessProbe)}`);
  assert.equal(String(currentProcessProbe.stdout || "").trim(), `DOTZ_PROCESS_PRESENT:${process.pid}`);
  assert.equal(classifyProcessStateReadback(currentProcessProbe, process.pid), "present");
  console.log("Windows CIM process readback integration passed (5 assertions)");
} else {
  console.log(`Windows CIM process readback integration skipped on ${process.platform} host`);
}
console.log("windows installer runtime helper tests passed (47 assertions)");
