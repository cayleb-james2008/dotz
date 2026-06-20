/**
 * dotz sandbox — isolated code execution with live output streaming + visual web preview.
 *
 * The sandbox is the agent's "little box": it spins up a program in its own temp dir and
 * streams stdout/stderr in real time. For web apps, it also exposes the HTTP port so the
 * dotz UI can render the app live in an iframe and overlay the agent's cursor as it
 * controls/tests the frontend.
 *
 * Modes:
 *  - "terminal" — CLI program, output streamed line-by-line.
 *  - "web"      — HTTP server program; the UI loads http://127.0.0.1:<port> in an iframe
 *                 and overlays an animated agent cursor driven by sandbox_cursor events.
 *
 * Cursor interaction model:
 *  The agent (or user) sends cursor events {x, y, action:"move"|"click"|"type", text?} over
 *  the WS. The UI ALWAYS renders a visual cursor at (x,y) overlaid on the iframe — this works for
 *  any preview, including cross-origin pages, because the overlay is a dotz-side element, not part
 *  of the iframe document. For click/type the UI ALSO dispatches synthetic DOM events directly into
 *  the iframe; that DOM injection is SAME-ORIGIN ONLY — on a cross-origin preview the contentDocument
 *  access throws a SecurityError, which the renderer catches and skips, so the visual cursor still
 *  moves but the click/type is a no-op. This is the lean visual bridge — no heavy browser-automation
 *  dependency, just coordinate events + same-origin DOM dispatch.
 */
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import net from "node:net";
import type { SandboxRun } from "./types";

export type SandboxEvent =
  | { type: "sandbox_start"; runId: string; run: SandboxRun }
  | { type: "sandbox_output"; runId: string; stream: "stdout" | "stderr"; line: string }
  | { type: "sandbox_port"; runId: string; port: number }
  | { type: "sandbox_cursor"; runId: string; x: number; y: number; action: "move" | "click" | "type"; text?: string }
  | { type: "sandbox_end"; runId: string; run: SandboxRun };

interface ActiveRun {
  run: SandboxRun;
  proc: ChildProcess | null;
  tempDir: string;
  timeout: NodeJS.Timeout | null;
  listeners: Set<(e: SandboxEvent) => void>;
  port: number | null;
  portDetector: PortDetector | null;
}

/** Language → { filename, command, defaultMode } mapping. */
const LANGUAGES: Record<string, { file: string; cmd: string[]; env?: Record<string, string>; mode?: "terminal" | "web" }> = {
  javascript: { file: "run.mjs", cmd: ["node", "run.mjs"] },
  typescript: { file: "run.ts", cmd: ["npx", "tsx", "run.ts"] },
  python: { file: "run.py", cmd: ["python", "run.py"] },
  bash: { file: "run.sh", cmd: ["bash", "run.sh"] },
  powershell: { file: "run.ps1", cmd: ["powershell", "-NoProfile", "-File", "run.ps1"] },
  shell: { file: "run.sh", cmd: ["sh", "run.sh"] },
};

export const SANDBOX_LANGUAGES = Object.keys(LANGUAGES);

const DEFAULT_TIMEOUT_MS = 30_000;

/**
 * Detects the first TCP port a process listens on by scanning stdout/stderr for port
 * patterns and probing candidate ports. Emits `sandbox_port` via onFound when detected.
 */
class PortDetector {
  private candidates = new Set<number>();
  private found: number | null = null;
  private probeTimer: NodeJS.Timeout | null = null;
  private onFound: (port: number) => void;

  constructor(onFound: (port: number) => void) {
    this.onFound = onFound;
  }

  scan(text: string): void {
    if (this.found) return;
    // Only treat a port as the child's OWN when it appears near a server-up keyword on the same
    // line ("listening on port 3000", "Local: http://localhost:5173", "Serving HTTP on port 8000").
    // This excludes ports the code merely talks to as a CLIENT ("connecting to localhost:6379",
    // "redis on :6379", "fetch http://127.0.0.1:8080") — probing those could attach the preview to
    // an unrelated local service. (\blocal\b matches "Local:" but not "localhost".)
    const portRe = /\b(?:listening|serving|running|started|ready|server|available|local)\b[^\n]{0,40}?(?:port\s+|localhost:|127\.0\.0\.1:|:)(\d{2,5})/gi;
    const matches = text.matchAll(portRe);
    for (const m of matches) {
      const port = Number(m[1]);
      if (port && port > 1024 && port < 65536) this.candidates.add(port);
    }
    this.maybeProbe();
  }

  private maybeProbe() {
    if (this.found || this.candidates.size === 0) return;
    if (this.probeTimer) return;
    // Probe on a short cadence so we catch the port shortly after the server starts listening.
    this.probeTimer = setTimeout(async () => {
      this.probeTimer = null;
      for (const port of this.candidates) {
        const open = await isPortOpen(port);
        if (open) { this.found = port; this.onFound(port); return; }
      }
      if (!this.found) this.maybeProbe(); // keep trying remaining/new candidates
    }, 250);
  }

  get port() { return this.found; }

  dispose() { if (this.probeTimer) clearTimeout(this.probeTimer); }
}

function isPortOpen(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const sock = new net.Socket();
    sock.setTimeout(400);
    sock.once("connect", () => { sock.destroy(); resolve(true); });
    sock.once("timeout", () => { sock.destroy(); resolve(false); });
    sock.once("error", () => { sock.destroy(); resolve(false); });
    sock.connect(port, "127.0.0.1");
  });
}

export class Sandbox {
  private active = new Map<string, ActiveRun>();

  private async spawnRun(
    run: SandboxRun,
    code: string,
    language: string,
    timeoutMs: number,
    listeners: Set<(e: SandboxEvent) => void>,
    mode: "terminal" | "web"
  ): Promise<ActiveRun> {
    const lang = LANGUAGES[language];
    if (!lang) throw new Error(`unsupported sandbox language: ${language}. Available: ${SANDBOX_LANGUAGES.join(", ")}`);

    const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-sandbox-"));
    await fs.writeFile(path.join(tempDir, lang.file), code, "utf-8");

    const proc = spawn(lang.cmd[0], lang.cmd.slice(1), {
      cwd: tempDir,
      env: { ...process.env, ...lang.env },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });

    const portDetector = mode === "web" ? new PortDetector((port) => {
      if (ar.port !== null) return;
      ar.port = port;
      run.output += `[dotz] detected web server on port ${port}\n`;
      emit({ type: "sandbox_port", runId: run.id, port });
    }) : null;
    const ar: ActiveRun = { run, proc, tempDir, timeout: null, listeners, port: null, portDetector };

    const emit = (e: SandboxEvent) => {
      for (const l of [...ar.listeners]) {
        try { l(e); } catch { /* listener errors must not break the sandbox loop */ }
      }
    };

    // Per-stream carry buffer: a logical line split across chunk reads is reassembled, not dropped.
    const lineBufs: Record<"stdout" | "stderr", string> = { stdout: "", stderr: "" };
    const flushLine = (stream: "stdout" | "stderr") => {
      const rest = lineBufs[stream];
      if (!rest) return;
      lineBufs[stream] = "";
      run.output += rest + "\n";
      emit({ type: "sandbox_output", runId: run.id, stream, line: rest });
    };

    const finish = (status: "done" | "error" | "killed", exitCode: number | null) => {
      if (ar.timeout) { clearTimeout(ar.timeout); ar.timeout = null; }
      flushLine("stdout"); flushLine("stderr");
      if (portDetector) portDetector.dispose();
      run.status = status;
      run.exitCode = exitCode;
      run.endedAt = Date.now();
      emit({ type: "sandbox_end", runId: run.id, run });
      // NOTE: we intentionally keep the run in `active` after completion so REST
      // (GET /api/sandbox/runs/:id) can still query the final output/exitCode. The
      // process handle is dead; only the run record stays for inspection. Cleanup
      // happens via disposeAll() on shutdown or kill() reclaiming the entry.
      if (ar.proc) { try { ar.proc.removeAllListeners(); } catch { /* */ } ar.proc = null; }
      fs.rm(tempDir, { recursive: true, force: true }).catch(() => {});
    };

    const processOutput = (chunk: Buffer, stream: "stdout" | "stderr") => {
      // Prepend the partial line carried over from the previous chunk so a line split across reads
      // is reassembled before we emit or scan it.
      const text = lineBufs[stream] + chunk.toString();
      const lines = text.split("\n");
      lineBufs[stream] = lines.pop() ?? "";
      for (const line of lines) {
        run.output += line + "\n";
        emit({ type: "sandbox_output", runId: run.id, stream, line });
      }
      // Web mode: scan the reassembled text (incl. the carried partial) so a port banner split
      // across reads still matches. The PortDetector probes the candidate asynchronously and emits
      // sandbox_port via its onFound callback.
      if (portDetector) portDetector.scan(text);
    };

    if (proc.stdout) {
      proc.stdout.on("data", (chunk: Buffer) => processOutput(chunk, "stdout"));
    }
    if (proc.stderr) {
      proc.stderr.on("data", (chunk: Buffer) => processOutput(chunk, "stderr"));
    }

    proc.on("error", (err) => {
      run.output += `\n[spawn error] ${err.message}\n`;
      finish("error", null);
    });
    proc.on("exit", (code, signal) => {
      if (signal === "SIGTERM" || signal === "SIGKILL") finish("killed", null);
      else finish(code === 0 ? "done" : "error", code);
    });

    if (timeoutMs > 0) {
      ar.timeout = setTimeout(() => {
        if (ar.proc && !ar.proc.killed) {
          try { ar.proc.kill("SIGKILL"); } catch { /* already dead */ }
        }
        run.output += `\n[timeout] killed after ${timeoutMs}ms\n`;
      }, timeoutMs);
    }

    return ar;
  }

  /** Start a sandbox run. Returns the initial SandboxRun; output streams to listeners. */
  async start(
    projectId: string | null,
    language: string,
    code: string,
    opts: { timeoutMs?: number; mode?: "terminal" | "web"; onEvent?: (e: SandboxEvent) => void } = {}
  ): Promise<SandboxRun> {
    const mode = opts.mode || "terminal";
    const run: SandboxRun = {
      id: randomUUID(),
      projectId,
      language,
      code,
      status: "running",
      output: "",
      exitCode: null,
      startedAt: Date.now(),
      endedAt: null,
    };
    const listeners = new Set<(e: SandboxEvent) => void>();
    if (opts.onEvent) listeners.add(opts.onEvent);
    const ar = await this.spawnRun(run, code, language, opts.timeoutMs ?? DEFAULT_TIMEOUT_MS, listeners, mode);
    this.active.set(run.id, ar);
    for (const l of [...ar.listeners]) l({ type: "sandbox_start", runId: run.id, run });
    return run;
  }

  /** Subscribe to a run's live events (output, port detection, cursor, end). */
  subscribe(runId: string, listener: (e: SandboxEvent) => void): () => void {
    const ar = this.active.get(runId);
    if (!ar) return () => {};
    ar.listeners.add(listener);
    return () => ar.listeners.delete(listener);
  }

  /** Emit a cursor event for a run. The UI always renders the visual cursor at (x,y) over the
   *  iframe (works cross-origin); click/type DOM injection into the iframe is same-origin only. */
  cursor(runId: string, x: number, y: number, action: "move" | "click" | "type", text?: string): void {
    const ar = this.active.get(runId);
    if (!ar) return;
    for (const l of [...ar.listeners]) {
      try { l({ type: "sandbox_cursor", runId, x, y, action, text }); } catch { /* */ }
    }
  }

  get(runId: string): ActiveRun["run"] | undefined {
    return this.active.get(runId)?.run;
  }

  /** Get the detected web port for a run (null for terminal runs or before detection). */
  port(runId: string): number | null {
    return this.active.get(runId)?.port ?? null;
  }

  list(): SandboxRun[] {
    return [...this.active.values()].map((ar) => ar.run);
  }

  async kill(runId: string): Promise<boolean> {
    const ar = this.active.get(runId);
    if (!ar || !ar.proc || ar.proc.killed) return false;
    try { ar.proc.kill("SIGKILL"); return true; } catch { return false; }
  }

  /** Kill all active runs — called on server shutdown so dotz never leaks child processes. */
  disposeAll(): void {
    for (const ar of [...this.active.values()]) {
      if (ar.timeout) clearTimeout(ar.timeout);
      if (ar.portDetector) ar.portDetector.dispose();
      if (ar.proc && !ar.proc.killed) {
        try { ar.proc.kill("SIGKILL"); } catch { /* already dead */ }
      }
      fs.rm(ar.tempDir, { recursive: true, force: true }).catch(() => {});
    }
    this.active.clear();
  }
}

export const sandbox = new Sandbox();