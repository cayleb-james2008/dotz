/**
 * Kill a spawned child process AND its descendants, so a force-stop never leaks a grandchild
 * (a launcher that forked the real server, or a `shell:true` cmd.exe wrapper).
 *   - win32: `taskkill /PID <pid> /T /F` reaps the whole tree (child.kill only reaps the wrapper).
 *   - posix "group": the child was spawned `detached` (a process-group leader), so SIGKILL the group.
 *   - posix "single" (default): the child is not a group leader — kill it directly.
 */
import { execFile, type ChildProcess } from "node:child_process";

export function killTree(proc: ChildProcess | null | undefined, posix: "group" | "single" = "single"): void {
  const pid = proc?.pid;
  if (!pid || !proc || proc.killed) return;
  if (process.platform === "win32") {
    try { execFile("taskkill", ["/PID", String(pid), "/T", "/F"], { windowsHide: true }, () => { /* best-effort */ }); } catch { /* ignore */ }
  } else if (posix === "group") {
    try { process.kill(-pid, "SIGKILL"); } catch { try { proc.kill("SIGKILL"); } catch { /* already dead */ } }
  } else {
    try { proc.kill(); } catch { /* already gone */ }
  }
}
