import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

export { sleep };

export function processStateProbeCommand(pid) {
  if (!Number.isSafeInteger(pid) || pid < 1) throw new TypeError("pid must be a positive safe integer");
  return `$ErrorActionPreference = 'Stop'\ntry {\n  $process = Get-CimInstance -ClassName Win32_Process -Filter 'ProcessId = ${pid}' -ErrorAction Stop\n  if ($null -eq $process) { [Console]::Out.WriteLine('DOTZ_PROCESS_ABSENT'); exit 0 }\n  [Console]::Out.WriteLine('DOTZ_PROCESS_PRESENT:' + $process.ProcessId)\n  exit 0\n} catch {\n  [Console]::Error.WriteLine($_.Exception.ToString())\n  exit 1\n}`;
}

export function isProcessAbsentReadback(probe) {
  return probe?.status === 0
    && String(probe.stdout || "").trim() === "DOTZ_PROCESS_ABSENT"
    && !String(probe.stderr || "").trim();
}

export function classifyProcessStateReadback(probe, pid) {
  if (!Number.isSafeInteger(pid) || pid < 1) throw new TypeError("pid must be a positive safe integer");
  if (isProcessAbsentReadback(probe)) return "absent";
  if (probe?.status === 0
    && String(probe.stdout || "").trim() === `DOTZ_PROCESS_PRESENT:${pid}`
    && !String(probe.stderr || "").trim()) return "present";
  return "unknown";
}

export function classifyInitialOnboarding(mode, text) {
  if (/WELCOME TO dotz|STEP [1-4]/i.test(String(text || ""))) return "wizard-visible";
  if (mode === "released") return "released-wizard-missing";
  if (mode === "candidate") return "candidate-wizard-wait";
  throw new TypeError(`unsupported acceptance mode: ${mode}`);
}

export function classifyOnboardingReuse(mode, status) {
  if (mode !== "released" && mode !== "candidate") throw new TypeError(`unsupported acceptance mode: ${mode}`);
  if (status === "COMPLETED_IN_NATIVE_UI") return "assert-reuse";
  if (mode === "released" && status === "NOT_PRESENT_IN_RELEASE_TAG") return "skip-release-reuse";
  return "fail-incomplete-reuse";
}

export function isInstalledDotzProcess(processInfo, expectedExe) {
  const actualPath = String(processInfo?.ExecutablePath || "");
  const expectedPath = String(expectedExe || "");
  if (String(processInfo?.Name || "").toLowerCase() !== "dotz.exe" || !actualPath || !expectedPath) return false;
  if (!path.win32.isAbsolute(actualPath) || !path.win32.isAbsolute(expectedPath)) return false;
  return path.win32.normalize(actualPath).toLowerCase() === path.win32.normalize(expectedPath).toLowerCase();
}

export function isDotzWindowForPid(window, pid) {
  const windowPid = Number(window?.pid);
  const windowId = Number(window?.window_id);
  return Number.isSafeInteger(pid) && pid > 0
    && String(window?.app_name || "").toLowerCase() === "dotz.exe"
    && windowPid === pid
    && Number.isSafeInteger(windowId) && windowId > 0;
}

export function classifyRestartUiSamples(sampleTexts) {
  if (!Array.isArray(sampleTexts) || sampleTexts.length === 0) return "unsettled";
  const samples = sampleTexts.map((text) => String(text || ""));
  if (samples.some((text) => /WELCOME TO dotz|STEP [1-4]/i.test(text))) return "wizard";
  const commandCenterVisible = (text) => /NO PROJECT/i.test(text) && /\bSEND\b/i.test(text);
  const recent = samples.slice(-4);
  if (recent.length === 4 && recent.every(commandCenterVisible)) return "dashboard";
  return "unsettled";
}

export function uiFailureDetails(error, failureUi = {}) {
  return {
    error: error instanceof Error ? error.message : String(error),
    text_sample: failureUi.text_sample ?? null,
    ui_read_error: failureUi.ui_read_error ?? null,
    screenshot_error: failureUi.screenshot_error ?? null,
    label_error: failureUi.label_error ?? null,
  };
}

export function parseDriverOutput(tool, stdout) {
  const text = String(stdout || "").trim();
  if (!text) throw new Error(`cua-driver call ${tool} returned empty output`);

  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error(`cua-driver call ${tool} returned non-JSON output: ${text}`);
  }

  if (parsed?.isError) throw new Error(`cua-driver call ${tool} failed: ${parsed.error || JSON.stringify(parsed)}`);
  return parsed;
}

function parseStructuredDriverError(stdout, stderr) {
  for (const candidate of [stderr, stdout]) {
    const text = String(candidate || "").trim();
    if (!text) continue;
    try {
      const parsed = JSON.parse(text);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) return parsed;
    } catch {
      // A code appearing in malformed/plain text is not a structured refusal.
    }
  }
  return null;
}

/** Keep the CLI's original streams and parsed JSON together for auditable retries. */
export function createDriverCallError(tool, completed) {
  const stdout = String(completed?.stdout || "");
  const stderr = String(completed?.stderr || "");
  const parsedError = parseStructuredDriverError(stdout, stderr);
  const detail = (stderr || stdout).trim();
  const exitCode = completed?.status ?? null;
  const error = new Error(`cua-driver call ${tool} exited ${exitCode}: ${detail}`);
  error.name = "CuaDriverCallError";
  error.driverFailure = {
    tool,
    exit_code: exitCode,
    stdout,
    stderr,
    parsed_error: parsedError,
    code: typeof parsedError?.code === "string" ? parsedError.code : null,
    escalation_recommended: typeof parsedError?.escalation?.recommended === "string"
      ? parsedError.escalation.recommended
      : null,
  };
  return error;
}

/** Capture PID, executable, creation time, and session for this exact launch. */
export function installedProcessLaunchIdentity(processInfo, expectedExecutable) {
  if (!isInstalledDotzProcess(processInfo, expectedExecutable)) return null;
  const pid = Number(processInfo?.ProcessId);
  const sessionId = Number(processInfo?.SessionId);
  const creationDate = String(processInfo?.CreationDate || "").trim();
  if (!Number.isSafeInteger(pid) || pid < 1 || !Number.isSafeInteger(sessionId) || sessionId < 0 || !creationDate) return null;
  return {
    pid,
    executable_path: path.win32.normalize(String(processInfo.ExecutablePath)).toLowerCase(),
    creation_date: creationDate,
    session_id: sessionId,
  };
}

/** Recheck liveness, the same installed process instance, and its exact native window. */
export function validateNativeCloseTarget({
  pid,
  windowId,
  expectedExecutable,
  expectedLaunchIdentity,
  processState,
  processInfo,
  windows,
}) {
  const reasons = [];
  const live = processState?.classification === "present";
  if (!live) reasons.push(`installed PID ${pid} is not confirmed present`);

  const actualLaunchIdentity = installedProcessLaunchIdentity(processInfo, expectedExecutable);
  const launchMatches = Number.isSafeInteger(pid) && pid > 0
    && expectedLaunchIdentity?.pid === pid
    && actualLaunchIdentity?.pid === pid
    && actualLaunchIdentity.executable_path === expectedLaunchIdentity.executable_path
    && actualLaunchIdentity.creation_date === expectedLaunchIdentity.creation_date
    && actualLaunchIdentity.session_id === expectedLaunchIdentity.session_id;
  if (!launchMatches) reasons.push("current PID/executable/creation-time/session does not match the installed dotz launch identity");

  const windowList = Array.isArray(windows) ? windows : [];
  const associatedWindow = windowList.find((window) => Number(window?.window_id) === windowId) || null;
  const windowMatches = Number.isSafeInteger(windowId) && windowId > 0
    && associatedWindow !== null
    && isDotzWindowForPid(associatedWindow, pid)
    && Number(associatedWindow.window_id) === windowId;
  if (!windowMatches) reasons.push("the exact native dotz window is not associated with the verified PID");

  return {
    valid: reasons.length === 0,
    pid,
    window_id: windowId,
    process_state: processState || null,
    expected_launch_identity: expectedLaunchIdentity || null,
    actual_launch_identity: actualLaunchIdentity,
    process_info: processInfo || null,
    associated_window: associatedWindow,
    reasons,
  };
}

function serializeCloseError(error) {
  return {
    name: error?.name || "Error",
    message: error instanceof Error ? error.message : String(error),
    driver_failure: error?.driverFailure || null,
  };
}

function structuredCloseSignal(attempt) {
  const payload = attempt.error?.driver_failure?.parsed_error || attempt.response;
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) return null;
  return {
    code: typeof payload.code === "string" ? payload.code : null,
    recommended: typeof payload.escalation?.recommended === "string" ? payload.escalation.recommended : null,
  };
}

/**
 * Send a default-background close. Retry only a parsed background_unavailable
 * that recommends foreground, after revalidating the exact launch/window.
 * Control flow permits at most one foreground action.
 */
export function sendGuardedClose({ pid, windowId, keys, verifyTarget, sendInput }) {
  if (!Number.isSafeInteger(pid) || pid < 1) throw new TypeError("close PID must be a positive safe integer");
  if (!Number.isSafeInteger(windowId) || windowId < 1) throw new TypeError("close window ID must be a positive safe integer");
  if (!Array.isArray(keys) || keys.length === 0) throw new TypeError("close keys must be a non-empty array");
  if (typeof verifyTarget !== "function" || typeof sendInput !== "function") throw new TypeError("close target verifier and input sender are required");

  const attempts = [];
  const verifications = [];
  const verify = (stage) => {
    let value;
    try {
      value = verifyTarget(stage);
    } catch (error) {
      value = { valid: false, error: serializeCloseError(error) };
    }
    const snapshot = { stage, ...(value && typeof value === "object" ? value : { valid: false }) };
    verifications.push(snapshot);
    return snapshot;
  };
  const act = (deliveryMode) => {
    const request = { pid, window_id: windowId, keys };
    if (deliveryMode === "foreground") request.delivery_mode = "foreground";
    const attempt = { number: attempts.length + 1, delivery_mode: deliveryMode, request, response: null, error: null };
    try {
      attempt.response = sendInput(request);
    } catch (error) {
      attempt.error = serializeCloseError(error);
    }
    attempts.push(attempt);
    return attempt;
  };
  const failure = (reason) => ({ ok: false, reason, attempts, verifications });

  const beforeBackground = verify("before-background-close");
  if (beforeBackground.valid !== true) return failure("exact installed process/window identity was not valid before background close");
  const background = act("background");
  const backgroundSignal = structuredCloseSignal(background);
  if (!background.error && backgroundSignal?.code !== "background_unavailable") {
    return { ok: true, reason: null, attempts, verifications };
  }
  if (backgroundSignal?.code !== "background_unavailable") {
    return failure(background.error?.message || "background close returned an unrecognized error");
  }
  if (backgroundSignal.recommended !== "foreground") {
    return failure("background_unavailable did not carry escalation.recommended=foreground");
  }

  const beforeForeground = verify("before-foreground-retry");
  if (beforeForeground.valid !== true) return failure("exact installed process/window identity was not valid before foreground retry");
  const foreground = act("foreground");
  if (foreground.error) return failure(foreground.error.message);
  const foregroundSignal = structuredCloseSignal(foreground);
  if (foregroundSignal?.code === "background_unavailable") {
    return failure("foreground close returned background_unavailable; retry limit reached");
  }
  return { ok: true, reason: null, attempts, verifications };
}

/** A relaunch is forbidden unless the close action succeeded and CIM observed exit. */
export function requireVerifiedCloseForRelaunch(closeOutcome, exitObservation) {
  if (closeOutcome?.ok !== true) {
    throw new Error(closeOutcome?.reason || "native close action did not succeed; refusing app relaunch and persistence assertions");
  }
  if (exitObservation?.exited !== true) {
    throw new Error("installed dotz process exit was not observed; refusing app relaunch and persistence assertions");
  }
  return true;
}
