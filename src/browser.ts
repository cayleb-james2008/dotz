/**
 * Isolated agent-browser controller used by Pi and the monitoring UI.
 *
 * Remote pages never run inside Dotz's Electron renderer and never receive its preload.
 * Each session uses a disposable profile and an explicit navigation allowlist.
 */
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";

const VERSION = 1 as const;
const AGENT_BROWSER_VERSION = "0.27.0";
const MAX_OUTPUT = 50_000;
const REF_PATTERN = /(?:@|\bref=)(e\d+)\b/g;

export type BrowserActionName =
  | "navigate" | "observe" | "back" | "forward" | "reload"
  | "click" | "clickAt" | "type" | "key" | "select" | "scroll" | "wait";

export interface BrowserOwner {
  app: "dotz";
  projectId: string;
  workflowId?: string;
  stepId?: string;
}

export interface BrowserObservation {
  schemaVersion: typeof VERSION;
  sessionId: string;
  seq: number;
  status: "starting" | "ready" | "acting" | "done" | "error" | "stopped";
  owner: BrowserOwner;
  startedAt: string;
  updatedAt: string;
  page: { url: string; title: string; viewport: { width: number; height: number } };
  allowedOrigins: string[];
  refs: string[];
  elements: Array<{ ref: string; role: string; name: string; observationSeq: number }>;
  snapshot: string;
  currentAction?: { name: BrowserActionName; targetRef?: string; summary: string };
  cursor?: { x: number; y: number; kind: string };
  frame?: { seq: number; mime: "image/jpeg"; width: number; height: number; available: true };
  counters: { actions: number; consoleErrors: number; networkErrors: number };
  consoleErrors: string[];
  networkErrors: string[];
  error?: { code: string; message: string; retryable: boolean };
}

export interface BrowserStartInput {
  projectId: string;
  workflowId?: string;
  stepId?: string;
  url: string;
  allowedOrigins?: string[];
  viewport?: { width: number; height: number };
}

export interface BrowserActInput {
  sessionId: string;
  action: BrowserActionName;
  expectedSeq?: number;
  url?: string;
  targetRef?: string;
  text?: string;
  x?: number;
  y?: number;
  key?: string;
  values?: string[];
  direction?: "up" | "down" | "left" | "right";
  pixels?: number;
  milliseconds?: number;
}

interface SessionRecord {
  profileDir: string;
  observation: BrowserObservation;
  frameData?: Buffer;
}

function normalizeOrigin(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("browser URLs must use http or https");
  return url.origin.toLowerCase();
}

function executableCandidates(): string[] {
  const exe = process.platform === "win32" ? "agent-browser-win32-x64.exe" : "agent-browser";
  const candidates = [
    process.env.DOTZ_AGENT_BROWSER,
    process.resourcesPath && path.join(process.resourcesPath, "app", "node_modules", "agent-browser", "bin", exe),
    path.resolve("node_modules", "agent-browser", "bin", exe),
    "agent-browser",
  ];
  return candidates.filter((v): v is string => !!v);
}

function parseJsonOutput(output: string): unknown {
  const trimmed = output.trim();
  if (!trimmed) return undefined;
  for (const line of trimmed.split(/\r?\n/).reverse()) {
    try { return JSON.parse(line); } catch { /* keep looking */ }
  }
  try { return JSON.parse(trimmed); } catch { return trimmed; }
}

function dataValue(value: unknown): unknown {
  if (!value || typeof value !== "object") return value;
  const record = value as Record<string, unknown>;
  return record.data ?? record.result ?? value;
}

function stringValue(value: unknown, key: string): string {
  const data = dataValue(value);
  if (typeof data === "string") return data;
  if (data && typeof data === "object") {
    const raw = (data as Record<string, unknown>)[key];
    if (typeof raw === "string") return raw;
  }
  return "";
}

function snapshotElements(snapshot: string, observationSeq: number): BrowserObservation["elements"] {
  const elements: BrowserObservation["elements"] = [];
  for (const line of snapshot.split(/\r?\n/)) {
    const ref = line.match(/(?:@|\bref=)(e\d+)\b/)?.[1];
    if (!ref) continue;
    const descriptor = line.replace(/^\s*[-*]?\s*/, "").replace(/\s*\[(?:ref=)?e\d+\]\s*.*$/, "").trim();
    const match = descriptor.match(/^([^:"']+?)(?:\s+["']([^"']*)["'])?$/);
    elements.push({ ref, role: match?.[1]?.trim() || "element", name: match?.[2] || "", observationSeq });
  }
  return elements;
}

export class BrowserController {
  private readonly sessions = new Map<string, SessionRecord>();
  private readonly events = new EventEmitter();
  private readonly executable?: string;

  constructor(options: { executable?: string } = {}) {
    this.executable = options.executable;
  }

  subscribe(listener: (observation: BrowserObservation) => void): () => void {
    this.events.on("observation", listener);
    return () => this.events.off("observation", listener);
  }

  list(): BrowserObservation[] {
    return [...this.sessions.values()].map((record) => structuredClone(record.observation));
  }

  state(sessionId?: string): BrowserObservation | null {
    const record = sessionId ? this.sessions.get(sessionId) : [...this.sessions.values()].at(-1);
    return record ? structuredClone(record.observation) : null;
  }

  frame(sessionId: string, afterSeq = -1): { seq: number; mime: "image/jpeg"; data: Buffer } | null {
    const record = this.sessions.get(sessionId);
    const seq = record?.observation.frame?.seq;
    if (!record?.frameData || seq === undefined || seq <= afterSeq) return null;
    return { seq, mime: "image/jpeg", data: Buffer.from(record.frameData) };
  }

  async start(input: BrowserStartInput): Promise<BrowserObservation> {
    if (!input.projectId?.trim()) throw new Error("projectId is required");
    const initialOrigin = normalizeOrigin(input.url);
    const allowedOrigins = [...new Set((input.allowedOrigins?.length ? input.allowedOrigins : [initialOrigin]).map(normalizeOrigin))];
    if (!allowedOrigins.includes(initialOrigin)) throw new Error(`initial URL origin ${initialOrigin} is not in the allowlist`);

    const sessionId = `dotz-${randomUUID()}`;
    const viewport = {
      width: Math.max(320, Math.min(2560, input.viewport?.width ?? 1280)),
      height: Math.max(240, Math.min(1600, input.viewport?.height ?? 800)),
    };
    const profileDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-browser-"));
    const now = new Date().toISOString();
    const observation: BrowserObservation = {
      schemaVersion: VERSION,
      sessionId,
      seq: 0,
      status: "starting",
      owner: { app: "dotz", projectId: input.projectId, workflowId: input.workflowId, stepId: input.stepId },
      startedAt: now,
      updatedAt: now,
      page: { url: input.url, title: "", viewport },
      allowedOrigins,
      refs: [],
      elements: [],
      snapshot: "",
      counters: { actions: 0, consoleErrors: 0, networkErrors: 0 },
      consoleErrors: [],
      networkErrors: [],
    };
    const record = { profileDir, observation };
    this.sessions.set(sessionId, record);
    this.emit(record);

    try {
      await this.run(record, ["set", "viewport", String(viewport.width), String(viewport.height)]);
      await this.run(record, ["open", input.url]);
      return await this.observe(record, { name: "navigate", summary: `opened ${input.url}` });
    } catch (error) {
      this.fail(record, error);
      await this.closeProcess(record).catch(() => undefined);
      await fs.rm(profileDir, { recursive: true, force: true }).catch(() => undefined);
      this.sessions.delete(sessionId);
      throw error;
    }
  }

  async act(input: BrowserActInput): Promise<BrowserObservation> {
    const record = this.sessions.get(input.sessionId);
    if (!record) throw new Error("no such browser session");
    if (record.observation.status === "stopped") throw new Error("browser session is stopped");
    const sequenceBound = Boolean(input.targetRef) || input.action === "clickAt" || (input.action === "type" && !input.targetRef);
    if (sequenceBound && input.expectedSeq !== record.observation.seq) {
      throw new Error(`stale browser action: expected observation seq ${record.observation.seq}`);
    }
    if (input.targetRef && !record.observation.refs.includes(input.targetRef.replace(/^@/, ""))) {
      throw new Error(`unknown browser ref: ${input.targetRef}`);
    }

    record.observation.status = "acting";
    record.observation.currentAction = { name: input.action, targetRef: input.targetRef, summary: this.actionSummary(input) };
    record.observation.updatedAt = new Date().toISOString();
    this.emit(record);

    try {
      if (input.targetRef) {
        const ref = input.targetRef.startsWith("@") ? input.targetRef : `@${input.targetRef}`;
        const box = dataValue(await this.run(record, ["get", "box", ref]));
        if (box && typeof box === "object") {
          const value = box as Record<string, unknown>;
          const x = Number(value.x); const y = Number(value.y);
          const width = Number(value.width); const height = Number(value.height);
          if ([x, y, width, height].every(Number.isFinite)) {
            record.observation.cursor = { x: x + width / 2, y: y + height / 2, kind: input.action };
          }
        }
      } else if (input.action === "clickAt" && Number.isFinite(input.x) && Number.isFinite(input.y)) {
        record.observation.cursor = { x: Number(input.x), y: Number(input.y), kind: input.action };
      }
      if (input.action === "clickAt") {
        const x = Math.round(Number(input.x));
        const y = Math.round(Number(input.y));
        const viewport = record.observation.page.viewport;
        if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error("clickAt requires finite x and y coordinates");
        if (x < 0 || y < 0 || x > viewport.width || y > viewport.height) {
          throw new Error(`clickAt coordinates must be inside ${viewport.width}x${viewport.height}`);
        }
        await this.run(record, ["mouse", "move", String(x), String(y)]);
        await this.run(record, ["mouse", "down", "left"]);
        await this.run(record, ["mouse", "up", "left"]);
      } else {
        const args = this.actionArgs(record, input);
        if (args) await this.run(record, args);
      }
      record.observation.counters.actions += 1;
      return await this.observe(record, record.observation.currentAction);
    } catch (error) {
      this.fail(record, error);
      throw error;
    }
  }

  async stop(sessionId: string): Promise<BrowserObservation> {
    const record = this.sessions.get(sessionId);
    if (!record) throw new Error("no such browser session");
    await this.closeProcess(record).catch(() => undefined);
    record.observation.status = "stopped";
    record.observation.seq += 1;
    record.observation.updatedAt = new Date().toISOString();
    delete record.observation.currentAction;
    delete record.frameData;
    delete record.observation.frame;
    this.emit(record);
    const stopped = structuredClone(record.observation);
    await fs.rm(record.profileDir, { recursive: true, force: true }).catch(() => undefined);
    this.sessions.delete(sessionId);
    return stopped;
  }

  async disposeAll(): Promise<void> {
    await Promise.all([...this.sessions.keys()].map((id) => this.stop(id).catch(() => undefined)));
  }

  private actionArgs(record: SessionRecord, input: BrowserActInput): string[] | null {
    const ref = input.targetRef?.startsWith("@") ? input.targetRef : input.targetRef ? `@${input.targetRef}` : "";
    switch (input.action) {
      case "observe": return null;
      case "back": return ["back"];
      case "forward": return ["forward"];
      case "reload": return ["reload"];
      case "navigate": {
        if (!input.url) throw new Error("navigate requires url");
        const origin = normalizeOrigin(input.url);
        if (!record.observation.allowedOrigins.includes(origin)) throw new Error(`navigation origin ${origin} is not in the allowlist`);
        return ["open", input.url];
      }
      case "click": if (!ref) throw new Error("click requires targetRef"); return ["click", ref];
      case "clickAt": return null; // handled as explicit Windows-safe mouse commands in act()
      case "type": {
        if (input.text === undefined) throw new Error("type requires text");
        return ref ? ["fill", ref, input.text] : ["keyboard", "inserttext", input.text];
      }
      case "key": if (!input.key) throw new Error("key requires key"); return ["press", input.key];
      case "select": if (!ref || !input.values?.length) throw new Error("select requires targetRef and values"); return ["select", ref, ...input.values];
      case "scroll": return ["scroll", input.direction ?? "down", String(Math.max(1, input.pixels ?? 500))];
      case "wait": return ["wait", String(Math.max(0, Math.min(30_000, input.milliseconds ?? 500)))];
    }
  }

  private actionSummary(input: BrowserActInput): string {
    if (input.action === "navigate") return `navigate ${input.url ?? ""}`;
    if (input.action === "clickAt") return `click (${Math.round(Number(input.x))}, ${Math.round(Number(input.y))})`;
    if (input.action === "type" && !input.targetRef) return "type into focused element";
    if (input.targetRef) return `${input.action} ${input.targetRef}`;
    return input.action;
  }

  private async observe(record: SessionRecord, action?: BrowserObservation["currentAction"]): Promise<BrowserObservation> {
    const [urlResult, titleResult, snapshotResult, consoleResult, errorResult] = await Promise.all([
      this.run(record, ["get", "url"]),
      this.run(record, ["get", "title"]),
      this.run(record, ["snapshot", "-i", "-c"]),
      this.run(record, ["console"]),
      this.run(record, ["errors"]),
    ]);
    const snapshot = stringValue(snapshotResult, "snapshot") ||
      (typeof dataValue(snapshotResult) === "string" ? String(dataValue(snapshotResult)) : JSON.stringify(dataValue(snapshotResult) ?? ""));
    const consoleText = typeof dataValue(consoleResult) === "string" ? String(dataValue(consoleResult)) : JSON.stringify(dataValue(consoleResult) ?? "");
    const errorText = typeof dataValue(errorResult) === "string" ? String(dataValue(errorResult)) : JSON.stringify(dataValue(errorResult) ?? "");
    const framePath = path.join(record.profileDir, "frame.jpg");
    await this.run(record, ["screenshot", framePath, "--screenshot-format", "jpeg", "--screenshot-quality", "72"]);
    const frame = await fs.readFile(framePath).catch(() => null);

    record.observation.seq += 1;
    record.observation.status = "ready";
    record.observation.updatedAt = new Date().toISOString();
    record.observation.page.url = stringValue(urlResult, "url") || record.observation.page.url;
    if (!record.observation.allowedOrigins.includes(normalizeOrigin(record.observation.page.url))) {
      throw new Error(`browser navigated outside the allowlist: ${record.observation.page.url}`);
    }
    record.observation.page.title = stringValue(titleResult, "title");
    record.observation.snapshot = snapshot;
    record.observation.refs = [...new Set([...snapshot.matchAll(REF_PATTERN)].map((match) => match[1]))];
    record.observation.elements = snapshotElements(snapshot, record.observation.seq);
    record.observation.currentAction = action;
    record.observation.consoleErrors = consoleText ? consoleText.split(/\r?\n/).filter((line) => /error|exception|failed/i.test(line)).slice(-50) : [];
    record.observation.networkErrors = errorText ? errorText.split(/\r?\n/).filter(Boolean).slice(-50) : [];
    record.observation.counters.consoleErrors = record.observation.consoleErrors.length;
    record.observation.counters.networkErrors = record.observation.networkErrors.length;
    delete record.observation.error;
    if (frame) {
      record.frameData = frame;
      record.observation.frame = {
        seq: record.observation.seq,
        mime: "image/jpeg",
        width: record.observation.page.viewport.width,
        height: record.observation.page.viewport.height,
        available: true,
      };
    }
    this.emit(record);
    return structuredClone(record.observation);
  }

  private async run(record: SessionRecord, command: string[]): Promise<unknown> {
    const executable = await this.resolveExecutable();
    const domains = record.observation.allowedOrigins.map((origin) => new URL(origin).hostname).join(",");
    const args = [
      "--session", record.observation.sessionId,
      "--profile", record.profileDir,
      "--allowed-domains", domains,
      "--content-boundaries",
      "--max-output", String(MAX_OUTPUT),
      "--confirm-actions", "eval,download,upload,clipboard",
      "--json",
      ...command,
    ];
    const nonce = randomUUID();
    const stdoutPath = path.join(record.profileDir, `command-${nonce}.out`);
    const stderrPath = path.join(record.profileDir, `command-${nonce}.err`);
    const stdoutFile = await fs.open(stdoutPath, "w+");
    const stderrFile = await fs.open(stderrPath, "w+");
    let child: ReturnType<typeof spawn> | undefined;
    try {
      child = spawn(executable, args, {
        windowsHide: true,
        stdio: ["ignore", stdoutFile.fd, stderrFile.fd],
        env: { ...process.env, AGENT_BROWSER_HEADED: "false" },
      });
      const code = await new Promise<number | null>((resolve, reject) => {
        const timeout = setTimeout(() => {
          child?.kill();
          reject(new Error("agent-browser command timed out"));
        }, 35_000);
        child!.once("error", (error) => { clearTimeout(timeout); reject(error); });
        child!.once("exit", (value) => { clearTimeout(timeout); resolve(value); });
      });
      await new Promise((resolve) => setTimeout(resolve, 100));
      await stdoutFile.close(); await stderrFile.close();
      const [stdout, stderr] = await Promise.all([
        fs.readFile(stdoutPath, "utf8").catch(() => ""),
        fs.readFile(stderrPath, "utf8").catch(() => ""),
      ]);
      if (code !== 0) throw new Error((stderr || stdout || `agent-browser exited ${code}`).trim().slice(0, 1000));
      return parseJsonOutput(stdout);
    } finally {
      await stdoutFile.close().catch(() => undefined);
      await stderrFile.close().catch(() => undefined);
      await Promise.all([fs.rm(stdoutPath, { force: true }), fs.rm(stderrPath, { force: true })]).catch(() => undefined);
    }
  }

  private async resolveExecutable(): Promise<string> {
    for (const candidate of this.executable ? [this.executable] : executableCandidates()) {
      if (candidate === "agent-browser") return candidate;
      try { await fs.access(candidate); return candidate; } catch { /* continue */ }
    }
    throw new Error(`agent-browser ${AGENT_BROWSER_VERSION} is not installed or packaged`);
  }

  private async closeProcess(record: SessionRecord): Promise<void> {
    await this.run(record, ["close"]);
  }

  private fail(record: SessionRecord, error: unknown): void {
    record.observation.status = "error";
    record.observation.seq += 1;
    record.observation.updatedAt = new Date().toISOString();
    record.observation.error = { code: "BROWSER_ACTION_FAILED", message: (error as Error).message, retryable: true };
    this.emit(record);
  }

  private emit(record: SessionRecord): void {
    this.events.emit("observation", structuredClone(record.observation));
  }
}

export const browserController = new BrowserController();
