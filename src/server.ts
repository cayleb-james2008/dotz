/**
 * dotz backend — Fastify serving REST + WebSocket + the static chat UI.
 * The WebSocket streams pi AgentSession events AND sandbox execution events; REST handles
 * session creation, controls, projects, memory, and sandbox runs.
 */
import path from "node:path";
import fs from "node:fs/promises";
import process from "node:process";
import { fileURLToPath } from "node:url";
import Fastify, { type FastifyInstance } from "fastify";
import fastifyWebsocket from "@fastify/websocket";
import fastifyStatic from "@fastify/static";
import {
  PiSessions,
  listCommands,
  modelInfo,
  resolveModel,
  DEFAULT_MODEL,
  PROFILES,
  profileSummary,
  type AgentSession,
  type CreateOpts,
  type ModelRef,
  type ThinkingLevel,
} from "./pi";
import { projectStore } from "./projects";
import { memoryStore, onMemoryRecall, enableMemoryAutonomy } from "./memory";
import { sandbox, SANDBOX_LANGUAGES, type SandboxEvent } from "./sandbox";
import { skillLoader } from "./skills";
import { workflowStore, WorkflowCycleError } from "./workflows";
import { workflowBridge } from "./workflow-bridge";
import { resolveHumanGate, onGateRequest } from "../.pi/extensions/dotz-tools/index";
import { browserController, type BrowserActInput, type BrowserStartInput } from "./browser";
import { connectionsController } from "./connections";
import { PROVIDERS, PROVIDER_DEFAULTS, type Project, type WorkflowRun } from "./types";
import { loadConfig, getConfig, updateConfig, type DotzConfig } from "./config";

const HOST = "127.0.0.1";
const PORT = Number(process.env.DOTZ_PORT || 4317);
const SELF = fileURLToPath(import.meta.url);
const WEB_DIR = path.resolve(path.dirname(SELF), "../web");
const FILE_TREE_MAX_DEPTH = 3;

/** Validate a project cwd before it is persisted: it must be an ABSOLUTE path that EXISTS and is a
 *  DIRECTORY. Returns an error string for the caller to 400 on, or null when valid. Closes the gap
 *  where a relative ("relative/path") or nonexistent ("C:\\no\\such\\dir") cwd was accepted by the
 *  API and then broke the agent (invalid cwd, empty file tree). */
async function validateCwd(cwd: string): Promise<string | null> {
  if (!path.isAbsolute(cwd)) return "cwd must be an absolute path";
  try {
    const st = await fs.stat(cwd);
    if (!st.isDirectory()) return "cwd is not a directory";
  } catch {
    return "cwd directory does not exist";
  }
  return null;
}

/** Recursive, depth-limited file tree used by `GET /api/projects/:id/files`. */
async function buildFileTree(cwd: string, depth = 0): Promise<Array<{ path: string; type: "file" | "dir"; children?: unknown }>> {
  if (depth >= FILE_TREE_MAX_DEPTH) return [];
  const entries = await fs.readdir(cwd, { withFileTypes: true }).catch(() => [] as import("node:fs").Dirent[]);
  const result: Array<{ path: string; type: "file" | "dir"; children?: unknown }> = [];
  for (const ent of entries) {
    const name = ent.name;
    if (name === "node_modules" || name === ".git") continue;
    const full = path.join(cwd, name);
    if (ent.isDirectory()) {
      const children = await buildFileTree(full, depth + 1);
      result.push({ path: full, type: "dir", children });
    } else if (ent.isFile() || ent.isSymbolicLink()) {
      result.push({ path: full, type: "file" });
    }
  }
  return result;
}

/** Trust-boundary helpers: a request body is untrusted JSON, so a field that the handler will
 *  later .trim()/path.join()/spread MUST be type-checked first — otherwise a non-string truthy
 *  value (number/array/object) reaches a string op and throws an uncaught 500 instead of a 400. */
const isNonEmptyStr = (v: unknown): v is string => typeof v === "string" && v.trim().length > 0;
const isValidModel = (m: unknown): m is ModelRef =>
  !!m && typeof m === "object" &&
  typeof (m as ModelRef).provider === "string" && (m as ModelRef).provider.trim().length > 0 &&
  typeof (m as ModelRef).modelId === "string" && (m as ModelRef).modelId.trim().length > 0;

/** Valid thinking levels — the closed set from types.ts (ThinkingLevel). Used to validate untrusted
 *  request bodies before they reach session.setThinkingLevel(), which may throw or silently accept
 *  garbage for unknown values. Mirrors THINK_LEVELS in web/app.js. */
const VALID_THINKING_LEVELS = new Set<ThinkingLevel>(["off", "minimal", "low", "medium", "high", "xhigh"]);

/** Public-facing snapshot of a session's control state. */
function sessionSummary(id: string, s: AgentSession, profileId?: string | null, projectId?: string | null) {
  const m = s.model as (typeof s.model & { provider?: string; reasoning?: boolean }) | undefined;
  return {
    sessionId: id,
    profileId: profileId || null,
    projectId: projectId || null,
    model: m ? { provider: m.provider, modelId: m.id, name: m.name, reasoning: !!m.reasoning } : null,
    thinkingLevel: s.thinkingLevel,
    supportsThinking: s.supportsThinking(),
    availableThinkingLevels: s.getAvailableThinkingLevels(),
    tools: s.getActiveToolNames(),
  };
}

export async function buildServer(): Promise<{ app: FastifyInstance; pi: PiSessions }> {
  const app = Fastify({ logger: false });
  const pi = new PiSessions();

  // Load the operator's persisted provider/model/reasoning defaults (sets DOTZ_SUBAGENT_MODEL).
  await loadConfig();

  await app.register(fastifyWebsocket);

  // Human-gate: when the agent calls the `human_gate` tool, forward the gate request to all
  // open WS sockets so the UI can render an approval card. The UI replies via gate.approve/reject.
  const wsSockets = new Set<{ readyState: number; send: (data: string) => void; OPEN: number; ping: () => void; terminate: () => void }>();

  // WebSocket heartbeat: ping every 30s and terminate sockets that don't pong back within one
  // interval. Without this, a half-open connection (laptop sleep, network change, crashed client)
  // lingers forever in wsSockets — leaking memory, sending broadcasts into the void, and never
  // firing the socket's `close` handler (so sandboxOffs/gate listeners for that socket leak too).
  const HEARTBEAT_MS = 30_000;
  const wsAlive = new Set<typeof wsSockets extends Set<infer T> ? T : never>();
  const heartbeat = setInterval(() => {
    for (const s of wsSockets) {
      if (!wsAlive.has(s)) { s.terminate(); wsSockets.delete(s); wsAlive.delete(s); continue; }
      wsAlive.delete(s);
      try { s.ping(); } catch { /* socket may have closed between checks */ }
    }
  }, HEARTBEAT_MS);
  onGateRequest((gateId, plan) => {
    for (const s of wsSockets) {
      if (s.readyState === s.OPEN) s.send(JSON.stringify({ kind: "gate", gateId, plan }));
    }
  });
  const offBrowserBroadcast = browserController.subscribe((observation) => {
    for (const s of wsSockets) {
      if (s.readyState === s.OPEN) s.send(JSON.stringify({ kind: "browser", event: observation }));
    }
  });
  // Enable autonomous memory (capture / recall / consolidation) — ONLY in this main server
  // process. Spawned subagents never call buildServer, so they never enable autonomy and never
  // churn the shared memory store.
  enableMemoryAutonomy();
  // Legacy migration runs once inside engine() on first memory use; a separate eager call here would
  // re-enter importLegacy before the marker is written and double-import every legacy entry.
  // Forward pre-task memory recall to the UI so the operator can see which memories were injected.
  const offMemoryRecall = onMemoryRecall((e) => {
    for (const s of wsSockets) {
      if (s.readyState === s.OPEN) s.send(JSON.stringify({ kind: "memory_recall", cwd: e.cwd, query: e.query, items: e.items }));
    }
  });
  app.addHook("onClose", async () => {
    clearInterval(heartbeat);
    offBrowserBroadcast();
    offMemoryRecall();
    await browserController.disposeAll();
    connectionsController.disposeAll();
  });

  // ---- health + providers ----
  app.get("/api/health", async () => ({ ok: true, sessions: pi.list().length, sandboxRuns: sandbox.list().length }));
  app.get("/api/providers", async () => ({ providers: PROVIDERS }));

  // ---- global config: provider + executive/subagent model + reasoning defaults ----
  app.get("/api/config", async () => ({ config: getConfig(), providerDefaults: PROVIDER_DEFAULTS, providers: PROVIDERS }));
  app.post("/api/config", async (req) => {
    const patch = (req.body ?? {}) as Partial<DotzConfig>;
    const config = await updateConfig(patch);
    return { config };
  });

  // ---- profiles ----
  app.get("/api/profiles", async () => ({ profiles: PROFILES.map(profileSummary), default: "workflow" }));

  // ---- projects ----
  app.get("/api/projects", async () => ({ projects: await projectStore.list() }));
  app.post("/api/projects", async (req, reply) => {
    const body = (req.body ?? {}) as { name?: string; cwd?: string; profileId?: string; model?: ModelRef; thinkingLevel?: ThinkingLevel; appUrl?: string; gateCommand?: string };
    if (!isNonEmptyStr(body.name) || !isNonEmptyStr(body.cwd)) {
      reply.code(400).send({ error: "name and cwd are required (non-empty strings)" });
      return;
    }
    if (body.model !== undefined && !isValidModel(body.model)) { reply.code(400).send({ error: "model must be { provider, modelId }" }); return; }
    // The remaining optional fields must be strings — projectStore.create later does appUrl?.trim()/
    // gateCommand?.trim() (a non-string 500s), and a non-string profileId/thinkingLevel persists a
    // corrupt project. Keep this in lockstep with the PATCH handler below.
    const b = body as Record<string, unknown>;
    for (const k of ["profileId", "thinkingLevel", "appUrl", "gateCommand"]) {
      if (b[k] !== undefined && typeof b[k] !== "string") { reply.code(400).send({ error: `${k} must be a string` }); return; }
    }
    if (body.thinkingLevel !== undefined && !VALID_THINKING_LEVELS.has(body.thinkingLevel)) { reply.code(400).send({ error: `thinkingLevel must be one of: ${[...VALID_THINKING_LEVELS].join(", ")}` }); return; }
    const cwdErr = await validateCwd(body.cwd);
    if (cwdErr) { reply.code(400).send({ error: cwdErr }); return; }
    return projectStore.create({ name: body.name, cwd: body.cwd, profileId: body.profileId, model: body.model, thinkingLevel: body.thinkingLevel, appUrl: body.appUrl, gateCommand: body.gateCommand });
  });
  app.get("/api/projects/:id", async (req, reply) => {
    const p = await projectStore.get((req.params as { id: string }).id);
    if (!p) { reply.code(404).send({ error: "no such project" }); return; }
    return p;
  });
  app.patch("/api/projects/:id", async (req, reply) => {
    const raw = (req.body ?? {}) as Record<string, unknown>;
    // Allow-list the patchable fields and type-check each — never spread the raw body into the
    // persisted record. Closes mass-assignment (arbitrary keys persisted) AND the prior gap where a
    // non-string cwd skipped validateCwd and persisted a corrupt project that then broke the agent.
    const patch: Partial<Project> = {};
    if (raw.name !== undefined) { if (!isNonEmptyStr(raw.name)) { reply.code(400).send({ error: "name must be a non-empty string" }); return; } patch.name = raw.name.trim(); }
    if (raw.cwd !== undefined) {
      if (typeof raw.cwd !== "string") { reply.code(400).send({ error: "cwd must be a string" }); return; }
      const cwdErr = await validateCwd(raw.cwd);
      if (cwdErr) { reply.code(400).send({ error: cwdErr }); return; }
      patch.cwd = raw.cwd;
    }
    if (raw.profileId !== undefined) { if (typeof raw.profileId !== "string") { reply.code(400).send({ error: "profileId must be a string" }); return; } patch.profileId = raw.profileId; }
    if (raw.model !== undefined) { if (!isValidModel(raw.model)) { reply.code(400).send({ error: "model must be { provider, modelId }" }); return; } patch.model = raw.model; }
    if (raw.thinkingLevel !== undefined) { if (typeof raw.thinkingLevel !== "string") { reply.code(400).send({ error: "thinkingLevel must be a string" }); return; } if (!VALID_THINKING_LEVELS.has(raw.thinkingLevel as ThinkingLevel)) { reply.code(400).send({ error: `thinkingLevel must be one of: ${[...VALID_THINKING_LEVELS].join(", ")}` }); return; } patch.thinkingLevel = raw.thinkingLevel as ThinkingLevel; }
    if (raw.appUrl !== undefined) { if (typeof raw.appUrl !== "string") { reply.code(400).send({ error: "appUrl must be a string" }); return; } patch.appUrl = raw.appUrl; }
    if (raw.gateCommand !== undefined) { if (typeof raw.gateCommand !== "string") { reply.code(400).send({ error: "gateCommand must be a string" }); return; } patch.gateCommand = raw.gateCommand; }
    const p = await projectStore.update((req.params as { id: string }).id, patch);
    if (!p) { reply.code(404).send({ error: "no such project" }); return; }
    return p;
  });
  app.delete("/api/projects/:id", async (req) => ({ ok: await projectStore.remove((req.params as { id: string }).id) }));
  app.get("/api/projects/:id/files", async (req, reply) => {
    const p = await projectStore.get((req.params as { id: string }).id);
    if (!p) { reply.code(404).send({ error: "no such project" }); return; }
    return { tree: await buildFileTree(p.cwd) };
  });

  // ---- memory (mem0-backed; see src/memory.ts) ----
  const cwdForProject = async (projectId?: string | null): Promise<string | null> => {
    if (!projectId) return null;
    const p = await projectStore.get(projectId);
    return p ? p.cwd : null;
  };
  app.get("/api/memory", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return { entries: await memoryStore.list(await cwdForProject(projectId)) };
  });
  app.post("/api/memory", async (req, reply) => {
    const body = (req.body ?? {}) as { projectId?: string; text?: string; value?: string; category?: string; folder?: string; scope?: "project" | "global" };
    const rawText = body.text ?? body.value;
    if (typeof rawText !== "string" || !rawText.trim()) { reply.code(400).send({ error: "text (or value) is required" }); return; }
    const text = rawText.trim();
    for (const k of ["category", "folder"]) {
      const v = (body as Record<string, unknown>)[k];
      if (v !== undefined && typeof v !== "string") { reply.code(400).send({ error: `${k} must be a string` }); return; }
    }
    const cwd = await cwdForProject(body.projectId);
    const scope = body.scope ?? (body.projectId ? "project" : "global");
    return memoryStore.create({ text, category: body.category, folder: body.folder, scope, projectCwd: cwd });
  });
  app.patch("/api/memory/:id", async (req, reply) => {
    const body = (req.body ?? {}) as { text?: string; value?: string; projectId?: string };
    const rawText = body.text ?? body.value;
    if (typeof rawText !== "string" || !rawText.trim()) { reply.code(400).send({ error: "text (or value) is required" }); return; }
    const text = rawText.trim();
    const e = await memoryStore.update((req.params as { id: string }).id, text, await cwdForProject(body.projectId));
    if (!e) { reply.code(404).send({ error: "no such memory entry" }); return; }
    return e;
  });
  app.delete("/api/memory/:id", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return { ok: await memoryStore.remove((req.params as { id: string }).id, await cwdForProject(projectId)) };
  });
  // Manual semantic search (recall observability + agent-independent lookup).
  app.post("/api/memory/search", async (req, reply) => {
    const body = (req.body ?? {}) as { query?: string; projectId?: string; threshold?: number; topK?: number; folder?: string; scope?: "project" | "global"; category?: string };
    if (body.query !== undefined && typeof body.query !== "string") { reply.code(400).send({ error: "query must be a string" }); return; }
    const results = await memoryStore.search(body.query ?? "", {
      projectCwd: await cwdForProject(body.projectId), threshold: body.threshold, topK: body.topK, folder: body.folder, scope: body.scope, category: body.category,
    });
    return { results };
  });
  // Operator-triggered consolidation (the automatic pass also runs on a capture threshold).
  app.post("/api/memory/consolidate", async (req) => {
    const body = (req.body ?? {}) as { projectId?: string };
    return memoryStore.consolidate(await cwdForProject(body.projectId));
  });
  // ---- skills (unified pool) ----
  app.get("/api/skills", async () => {
    await skillLoader.load();
    return { skills: skillLoader.list().map((s) => ({ name: s.name, description: s.description, source: s.source, tags: s.tags, isUmbrella: s.isUmbrella })) };
  });
  app.get("/api/skills/:name", async (req, reply) => {
    await skillLoader.load();
    const name = (req.params as { name: string }).name;
    const body = await skillLoader.loadBody(name);
    if (!body) { reply.code(404).send({ error: "no such skill" }); return; }
    return { name, body };
  });

  // ---- workflows ----
  app.get("/api/workflows", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return { runs: await workflowStore.listHistory(projectId) };
  });
  app.get("/api/workflows/active", async () => ({ runs: workflowStore.list() }));
  app.get("/api/workflows/:id", async (req, reply) => {
    const run = workflowStore.get((req.params as { id: string }).id)
      ?? (await workflowStore.listHistory()).find((r) => r.id === (req.params as { id: string }).id);
    if (!run) { reply.code(404).send({ error: "no such workflow run" }); return; }
    return run;
  });
  app.post("/api/workflows", async (req, reply) => {
    const body = (req.body ?? {}) as { projectId?: string | null; sessionId?: string | null; label?: string; origin?: string; steps: Array<{ agent: string; task: string; parents?: string[] }> };
    if (!Array.isArray(body.steps) || body.steps.length === 0) {
      reply.code(400).send({ error: "steps (non-empty array) is required" });
      return;
    }
    // Validate each step's SHAPE — the store later reads s.agent/s.task and iterates s.parents, so a
    // null/number/string element or a non-array `parents` would throw an uncaught 500 (or persist a
    // corrupt agent-less step) instead of a clean 400.
    for (const s of body.steps) {
      if (!s || typeof s !== "object" || typeof s.agent !== "string" || typeof s.task !== "string") {
        reply.code(400).send({ error: "each step needs a string agent and task" });
        return;
      }
      if (s.parents !== undefined && !Array.isArray(s.parents)) {
        reply.code(400).send({ error: "step parents must be an array" });
        return;
      }
    }
    let run;
    try {
      run = await workflowStore.create({
        projectId: body.projectId ?? null,
        sessionId: body.sessionId ?? null,
        label: body.label || "untitled workflow",
        origin: body.origin,
        steps: body.steps,
      });
    } catch (err) {
      if (err instanceof WorkflowCycleError) { reply.code(400).send({ error: err.message }); return; }
      throw err;
    }
    workflowStore.start(run.id);
    return run;
  });
  app.post("/api/workflows/:id/step", async (req, reply) => {
    const run = workflowStore.get((req.params as { id: string }).id);
    if (!run) { reply.code(404).send({ error: "no such workflow run" }); return; }
    // A finished run is immutable: a late step update would leave the run done/error/aborted while a
    // step flips to running/error (a contradictory graph). Reject with 409 (the in-process workflow
    // bridge calls stepState directly and is unaffected).
    if (run.status === "done" || run.status === "error" || run.status === "aborted") {
      reply.code(409).send({ error: `run is ${run.status} — its steps can no longer be updated` });
      return;
    }
    const body = (req.body ?? {}) as { stepId: string; status: WorkflowRun["steps"][number]["status"]; output?: string; error?: string; usage?: WorkflowRun["steps"][number]["usage"] };
    if (typeof body.stepId !== "string" || !body.stepId) { reply.code(400).send({ error: "stepId is required" }); return; }
    if (!run.steps.some((s) => s.id === body.stepId)) { reply.code(404).send({ error: "no such step" }); return; }
    const ALLOWED = ["pending", "ready", "running", "done", "error", "skipped"];
    if (body.status !== undefined && !ALLOWED.includes(body.status)) { reply.code(400).send({ error: "invalid status" }); return; }
    await workflowStore.stepState(run.id, body.stepId, { status: body.status, output: body.output, error: body.error, usage: body.usage });
    return workflowStore.get(run.id);
  });
  app.post("/api/workflows/:id/abort", async (req, reply) => {
    const id = (req.params as { id: string }).id;
    // 404 for an unknown run, matching the GET/:id and /step routes — abort() is a silent no-op on a
    // missing run, so without this the client gets a misleading {ok:true} for a run that never existed.
    if (!workflowStore.get(id)) { reply.code(404).send({ error: "no such workflow run" }); return; }
    await workflowStore.abort(id);
    return { ok: true };
  });

  // ---- isolated Pi browser controller ----
  app.get("/api/browser/state", async (req) => {
    const sessionId = (req.query as { sessionId?: string }).sessionId;
    return { available: true, observation: browserController.state(sessionId), sessions: browserController.list() };
  });
  app.get("/api/browser/frame", async (req, reply) => {
    const query = req.query as { sessionId?: string; afterSeq?: string };
    if (!query.sessionId) { reply.code(400).send({ error: "sessionId required" }); return; }
    const frame = browserController.frame(query.sessionId, Number(query.afterSeq ?? -1));
    if (!frame) { reply.code(204).send(); return; }
    reply.header("content-type", frame.mime);
    reply.header("cache-control", "no-store");
    reply.header("x-dotz-frame-seq", String(frame.seq));
    return reply.send(frame.data);
  });
  app.post("/api/browser/start", async (req, reply) => {
    try { return await browserController.start((req.body ?? {}) as BrowserStartInput); }
    catch (error) { reply.code(400).send({ error: (error as Error).message }); }
  });
  app.post("/api/browser/act", async (req, reply) => {
    try { return await browserController.act((req.body ?? {}) as BrowserActInput); }
    catch (error) {
      const message = (error as Error).message;
      // 409 (retryable conflict) for the staleness cases — a stale expectedSeq OR a ref from an
      // older observation; the client refetches state and retries. Other errors (unknown action,
      // bad coords, no session) are 400. The actual thrown text is "unknown browser ref:…".
      reply.code(/stale browser action|unknown browser ref/i.test(message) ? 409 : 400).send({ error: message });
    }
  });
  app.post("/api/browser/stop", async (req, reply) => {
    const sessionId = ((req.body ?? {}) as { sessionId?: string }).sessionId;
    if (!sessionId) { reply.code(400).send({ error: "sessionId required" }); return; }
    try { return await browserController.stop(sessionId); }
    catch (error) { reply.code(404).send({ error: (error as Error).message }); }
  });

  // ---- local connections (each provider's own browser-CLI login on this machine) ----
  app.get("/api/connections", async () => ({ connections: await connectionsController.status() }));
  app.post("/api/connections/:provider/login", async (req, reply) => {
    try { return connectionsController.login((req.params as { provider: string }).provider); }
    catch (error) { reply.code(400).send({ error: (error as Error).message }); }
  });
  app.get("/api/connections/:provider/login", async (req, reply) => {
    try { return connectionsController.loginState((req.params as { provider: string }).provider); }
    catch (error) { reply.code(404).send({ error: (error as Error).message }); }
  });
  app.post("/api/connections/:provider/logout", async (req, reply) => {
    try { return await connectionsController.logout((req.params as { provider: string }).provider); }
    catch (error) { reply.code(400).send({ error: (error as Error).message }); }
  });

  // ---- sandbox ----
  app.get("/api/sandbox/languages", async () => ({ languages: SANDBOX_LANGUAGES }));
  app.get("/api/sandbox/runs", async () => ({ runs: sandbox.list() }));
  app.get("/api/sandbox/runs/:id", async (req, reply) => {
    const r = sandbox.get((req.params as { id: string }).id);
    if (!r) { reply.code(404).send({ error: "no such sandbox run" }); return; }
    return r;
  });
  app.post("/api/sandbox/runs", async (req, reply) => {
    const body = (req.body ?? {}) as { projectId?: string; language?: string; code?: string; timeoutMs?: number; mode?: "terminal" | "web" };
    if (typeof body.language !== "string" || typeof body.code !== "string") {
      reply.code(400).send({ error: "language and code are required (strings)" });
      return;
    }
    if (!SANDBOX_LANGUAGES.includes(body.language)) {
      reply.code(400).send({ error: `unsupported language: ${body.language}. Available: ${SANDBOX_LANGUAGES.join(", ")}` });
      return;
    }
    const run = await sandbox.start(body.projectId || null, body.language, body.code, { timeoutMs: body.timeoutMs, mode: body.mode });
    return run;
  });
  app.post("/api/sandbox/runs/:id/kill", async (req) => ({ ok: await sandbox.kill((req.params as { id: string }).id) }));
  app.get("/api/sandbox/runs/:id/port", async (req, reply) => {
    const port = sandbox.port((req.params as { id: string }).id);
    if (port === null) { reply.code(404).send({ error: "no web port detected (terminal run or not yet listening)" }); return; }
    return { port };
  });

  // ---- sessions ----
  app.post("/api/sessions", async (req, reply) => {
    const body = (req.body ?? {}) as CreateOpts;
    // Validate the model shape like the project routes do — otherwise { provider } with no modelId
    // flows into resolveModel and pins the session to a structurally broken provider/undefined model
    // that setModel silently accepts (every later prompt then sends model=undefined upstream).
    if (body.model !== undefined && !isValidModel(body.model)) { reply.code(400).send({ error: "model must be { provider, modelId }" }); return; }
    // Validate thinkingLevel against the known set — an invalid value (e.g. 123, "banana") would
    // pass through to session.setThinkingLevel(), which may throw or silently corrupt the session's
    // reasoning state. Same allow-list the UI uses (THINK_LEVELS in app.js).
    if (body.thinkingLevel !== undefined && !VALID_THINKING_LEVELS.has(body.thinkingLevel)) {
      reply.code(400).send({ error: `thinkingLevel must be one of: ${[...VALID_THINKING_LEVELS].join(", ")}` }); return;
    }
    try {
      const entry = await pi.create(body);
      return sessionSummary(entry.id, entry.session, entry.profile.id, entry.projectId);
    } catch (err) {
      // pi.create() can throw for auth failures (no provider key), bad model resolution, or SDK
      // init errors — surface these as a clean 503 instead of Fastify's generic 500 so the UI can
      // show a helpful message (e.g. "configure your Ollama API key") instead of a stack trace.
      reply.code(503).send({ error: "session creation failed", detail: (err as Error).message });
    }
  });

  app.get("/api/sessions", async () => pi.list().map((e) => sessionSummary(e.id, e.session, e.profile.id, e.projectId)));

  // Resolve :id → session entry, or 404. Returns the entry (session + profile).
  const need = (id: string, reply: import("fastify").FastifyReply): SessionEntry | null => {
    const e = pi.get(id);
    if (!e) {
      reply.code(404).send({ error: "no such session" });
      return null;
    }
    return e;
  };
  type SessionEntry = { id: string; session: AgentSession; profile: { id: string }; projectId: string | null };

  app.get("/api/sessions/:id", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    return { ...sessionSummary(e.session.sessionId, e.session, e.profile.id, e.projectId), stats: e.session.getSessionStats() };
  });

  app.delete("/api/sessions/:id", async (req) => {
    pi.dispose((req.params as { id: string }).id);
    return { ok: true };
  });

  // Models: available list + distinct providers + dotz default. All providers are surfaced now;
  // providers that support free-form input (openrouter, local) accept any model-id string.
  app.get("/api/sessions/:id/models", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    const available = s.modelRegistry.getAvailable().map(modelInfo);
    const providers = [...new Set(available.map((m) => m.provider))].sort();
    return {
      current: s.model ? modelInfo(s.model) : null,
      default: DEFAULT_MODEL,
      providers,
      available,
      providerMeta: PROVIDERS,
    };
  });

  app.post("/api/sessions/:id/model", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    const ref = (req.body ?? {}) as Partial<ModelRef>;
    // trim-check: a whitespace-only modelId would pass a bare truthiness guard, then resolveModel
    // clones a template with that garbage id and setModel accepts it (provider-only auth check).
    if (!ref.provider?.trim() || !ref.modelId?.trim()) {
      reply.code(400).send({ error: "provider and modelId are required" });
      return;
    }
    const m = resolveModel(s, ref as ModelRef);
    if (!m) {
      reply.code(404).send({ error: `model not found: ${ref.provider}/${ref.modelId}` });
      return;
    }
    try {
      await s.setModel(m);
    } catch (err) {
      reply.code(400).send({ error: (err as Error).message });
      return;
    }
    return sessionSummary(s.sessionId, s, e.profile.id, e.projectId);
  });

  app.post("/api/sessions/:id/thinking", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    const level = ((req.body ?? {}) as { level: ThinkingLevel }).level;
    // Validate against the known set — an invalid level (number, typo, null) reaches setThinkingLevel
    // which may throw or silently accept garbage, corrupting the session's reasoning state.
    if (typeof level !== "string" || !VALID_THINKING_LEVELS.has(level as ThinkingLevel)) {
      reply.code(400).send({ error: `level must be one of: ${[...VALID_THINKING_LEVELS].join(", ")}` }); return;
    }
    s.setThinkingLevel(level as ThinkingLevel);
    return {
      thinkingLevel: s.thinkingLevel,
      supportsThinking: s.supportsThinking(),
      availableThinkingLevels: s.getAvailableThinkingLevels(),
    };
  });

  app.get("/api/sessions/:id/tools", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    return { active: s.getActiveToolNames(), all: s.getAllTools().map((t) => t.name) };
  });

  app.post("/api/sessions/:id/tools", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    // tools is fed to setActiveToolsByName which iterates it — a non-array (e.g. a number) would throw
    // an uncaught 500; require an array of strings -> clean 400.
    const tools = ((req.body ?? {}) as { tools?: unknown }).tools ?? [];
    if (!Array.isArray(tools) || !tools.every((t) => typeof t === "string")) {
      reply.code(400).send({ error: "tools must be an array of strings" });
      return;
    }
    s.setActiveToolsByName(tools);
    return { active: s.getActiveToolNames(), all: s.getAllTools().map((t) => t.name) };
  });

  app.get("/api/sessions/:id/commands", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    return { commands: listCommands(e.session) };
  });

  app.post("/api/sessions/:id/abort", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    await e.session.abort();
    return { ok: true };
  });

  // Rebuild a session so its system prompt re-injects fresh project memory + the latest skill index
  // + AGENTS.md (all read once at session-build time, so mid-session edits to memory/AGENTS.md never
  // reach a live session). Reuses create/dispose: the new session keeps the same project/profile and
  // the current model + reasoning effort, but starts a FRESH conversation (history resets — the
  // caller should re-bind to the returned sessionId).
  app.post("/api/sessions/:id/reload-context", async (req, reply) => {
    const e = need((req.params as { id: string }).id, reply);
    if (!e) return;
    const s = e.session;
    const m = s.model as { provider?: string; id?: string } | undefined;
    const opts: CreateOpts = {
      projectId: e.projectId ?? undefined,
      profileId: e.profile.id,
      model: m && m.provider && m.id ? { provider: m.provider, modelId: m.id } : undefined,
      thinkingLevel: s.thinkingLevel as ThinkingLevel,
    };
    // Create the fresh session FIRST (it can throw — bad model/auth, init error); only dispose the
    // old one once the replacement is live, so a failed reload doesn't strand the client on a dead id.
    let fresh;
    try { fresh = await pi.create(opts); }
    catch (err) { reply.code(500).send({ error: "reload failed", detail: String(err) }); return; }
    pi.dispose(s.sessionId);
    return sessionSummary(fresh.id, fresh.session, fresh.profile.id, fresh.projectId);
  });

  // WebSocket: stream a session's agent events AND sandbox events; accept prompt/steer/followUp/abort.
  app.get("/ws", { websocket: true }, (socket, req) => {
    const sessionId = new URL(req.url, `http://${HOST}`).searchParams.get("sessionId") ?? "";
    const entry = pi.get(sessionId);
    if (!entry) {
      socket.send(JSON.stringify({ kind: "error", error: "no such session" }));
      socket.close();
      return;
    }

    // Fan agent events to the socket.
    const offAgent = pi.subscribe(sessionId, (event) => {
      // Bridge: synthesize workflow runs from subagent tool calls so the graph auto-populates.
      try { workflowBridge.handleEvent(sessionId, entry.projectId, event); } catch { /* best-effort */ }
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "event", sessionId, event }));
    });
    // Fan sandbox events to the same socket so the UI can render live sandbox output inline.
    // Any sandbox run started from this session's context (projectId match) broadcasts here.
    const sandboxSub = (e: SandboxEvent) => {
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "sandbox", sessionId, event: e }));
    };
    // Unsubscribe handles for every run this socket attaches to — runs are kept in `active` after
    // they finish (by design, for REST inspection), so without this the closed socket's listener
    // would leak in each run's listener Set forever. Drained in socket.on("close").
    const sandboxOffs = new Set<() => void>();
    // Fan workflow events (step state transitions, run lifecycle) to the same socket so the UI
    // can render the live workflow graph. All active workflow runs broadcast here; the UI filters
    // by sessionId/projectId as needed.
    const offWorkflow = workflowStore.onEvent((runId, event) => {
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "workflow", sessionId, runId, event }));
    });
    socket.send(JSON.stringify({ kind: "ready", sessionId }));
    wsSockets.add(socket);
    // Heartbeat: mark alive on pong so the interval knows this socket is responsive.
    wsAlive.add(socket);
    (socket as { on?: (ev: string, cb: () => void) => void }).on?.("pong", () => { wsAlive.add(socket); });

    socket.on("message", async (raw: Buffer) => {
      let msg: { kind?: string; text?: string; projectId?: string | null; language?: string; code?: string; mode?: "terminal" | "web"; timeoutMs?: number; runId?: string; x?: number; y?: number; action?: "move" | "click" | "type"; cursorText?: string; gateId?: string; feedback?: string };
      try {
        msg = JSON.parse(raw.toString());
      } catch {
        return;
      }
      const s = entry.session;
      try {
        if (msg.kind === "prompt") {
          if (typeof msg.text !== "string" || !msg.text.trim()) return;
          await s.prompt(msg.text, s.isStreaming ? { streamingBehavior: "followUp" } : undefined);
        }
        else if (msg.kind === "steer") {
          if (typeof msg.text !== "string" || !msg.text.trim()) return;
          await s.steer(msg.text);
        }
        else if (msg.kind === "followUp") {
          if (typeof msg.text !== "string" || !msg.text.trim()) return;
          await s.followUp(msg.text);
        }
        else if (msg.kind === "abort") await s.abort();
        else if (msg.kind === "sandbox.start") {
          const run = await sandbox.start(msg.projectId || entry.projectId || null, msg.language || "bash", msg.code || "", {
            timeoutMs: msg.timeoutMs,
            mode: msg.mode,
          });
          // Subscribe AFTER start (not via onEvent) so we get an unsubscribe handle to clean up on
          // close; the single sandbox_start is sent explicitly below (avoids a duplicate emit).
          sandboxOffs.add(sandbox.subscribe(run.id, sandboxSub));
          if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "sandbox", sessionId, event: { type: "sandbox_start", runId: run.id, run } }));
        } else if (msg.kind === "sandbox.kill") {
          await sandbox.kill(msg.runId || "");
        }         else if (msg.kind === "sandbox.cursor") {
          // Cursor event — the UI renders the agent's cursor over the live web preview iframe.
          sandbox.cursor(msg.runId || "", msg.x || 0, msg.y || 0, msg.action || "move", msg.cursorText);
        } else if (msg.kind === "gate.approve") {
          // Human-gate approval — resolves the human_gate tool's awaiting Promise.
          resolveHumanGate(msg.gateId || "", true, msg.feedback);
        } else if (msg.kind === "gate.reject") {
          resolveHumanGate(msg.gateId || "", false, msg.feedback);
        }
      } catch (e) {
        // Guard: the socket may have closed while the async handler was running (e.g. a long
        // s.prompt() that threw after the client disconnected). send() on a non-OPEN socket
        // throws, which would be an unhandled rejection.
        if (socket.readyState === socket.OPEN) {
          try { socket.send(JSON.stringify({ kind: "error", error: (e as Error).message })); } catch { /* socket closed */ }
        }
      }
    });

    socket.on("close", () => { offAgent(); offWorkflow(); for (const off of sandboxOffs) off(); sandboxOffs.clear(); wsSockets.delete(socket); wsAlive.delete(socket); });
  });

  // Static chat UI last, so explicit /api and /ws routes win.
  await app.register(fastifyStatic, { root: WEB_DIR, prefix: "/" });

  return { app, pi };
}

export async function start(): Promise<void> {
  const { app, pi } = await buildServer();
  try {
    await app.listen({ host: HOST, port: PORT });
  } catch (err) {
    const code = (err as { code?: string }).code;
    if (code === "EADDRINUSE") {
      console.error(`dotz: port ${PORT} is already in use — another dotz instance may be running. Set DOTZ_PORT to use a different port.`);
      process.exit(1);
    }
    throw err;
  }
  console.log(`dotz server → http://${HOST}:${PORT}`);
  const shutdown = async () => {
    sandbox.disposeAll();
    pi.disposeAll();
    // Timeout backstop: app.close() runs async onClose hooks (browserController.disposeAll, etc).
    // If one stalls, force exit after 8s so Ctrl+C always terminates the server.
    const forceExit = setTimeout(() => process.exit(0), 8_000);
    try { await app.close(); } catch { /* best-effort */ }
    clearTimeout(forceExit);
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

// Start only when run directly (tsx src/server.ts), not when imported by the Electron main.
const entry = process.argv[1] ? path.resolve(process.argv[1]) : "";
if (!process.versions.electron && (entry === SELF || entry === SELF.replace(/\.ts$/, ".js"))) {
  start().catch((e) => {
    console.error("dotz failed to start:", e);
    process.exit(1);
  });
}
