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
