import { setTimeout as sleep } from "node:timers/promises";

export { sleep };

export function parseDriverOutput(tool, stdout) {
  const text = String(stdout || "").trim();
  if (!text) throw new Error(`cua-driver call ${tool} returned empty output`);

  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    // cua-driver 0.34.0's kill_app tool returns this documented-positive
    // human-readable message; stopApp still verifies process exit separately.
    const termination = tool === "kill_app" ? text.match(/^(?:✅\s*)?Terminated pid (\d+)\.$/) : null;
    if (termination) {
      return { status: "terminated", pid: Number(termination[1]), raw_output: text };
    }
    throw new Error(`cua-driver call ${tool} returned non-JSON output: ${text}`);
  }

  if (parsed?.isError) throw new Error(`cua-driver call ${tool} failed: ${parsed.error || JSON.stringify(parsed)}`);
  return parsed;
}
