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
import { workflowStore, WorkflowCycleError, type WorkflowEvent } from "./workflows";
import { workflowBridge } from "./workflow-bridge";
import { resolveHumanGate, onGateRequest } from "../.pi/extensions/dotz-tools/index";
import { browserController, type BrowserActInput, type BrowserStartInput } from "./browser";
import { connectionsController } from "./connections";
import { PROVIDERS, PROVIDER_DEFAULTS, type Project, type SandboxRun, type WorkflowRun } from "./types";
import { loadConfig, getConfig, updateConfig, type DotzConfig } from "./config";

const HOST = "127.0.0.1";
const PORT = Number(process.env.DOTZ_PORT || 4317);
const SELF = fileURLToPath(import.meta.url);
const WEB_DIR = path.resolve(path.dirname(SELF), "../web");
const FILE_TREE_MAX_DEPTH = 3;

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
  const wsSockets = new Set<{ readyState: number; send: (data: string) => void; OPEN: number }>();
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
    const body = (req.body ?? {}) as { name?: string; cwd?: string; profileId?: string; model?: ModelRef; thinkingLevel?: ThinkingLevel };
    if (!body.name || !body.cwd) {
      reply.code(400).send({ error: "name and cwd are required" });
      return;
    }
    return projectStore.create({ name: body.name, cwd: body.cwd, profileId: body.profileId, model: body.model, thinkingLevel: body.thinkingLevel });
  });
  app.get("/api/projects/:id", async (req, reply) => {
    const p = await projectStore.get((req.params as { id: string }).id);
    if (!p) { reply.code(404).send({ error: "no such project" }); return; }
    return p;
  });
  app.patch("/api/projects/:id", async (req, reply) => {
    const p = await projectStore.update((req.params as { id: string }).id, (req.body ?? {}) as Partial<Project>);
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
    const text = (body.text ?? body.value ?? "").trim();
    if (!text) { reply.code(400).send({ error: "text (or value) is required" }); return; }
    const cwd = await cwdForProject(body.projectId);
    const scope = body.scope ?? (body.projectId ? "project" : "global");
    return memoryStore.create({ text, category: body.category, folder: body.folder, scope, projectCwd: cwd });
  });
  app.patch("/api/memory/:id", async (req, reply) => {
    const body = (req.body ?? {}) as { text?: string; value?: string; projectId?: string };
    const text = (body.text ?? body.value ?? "").trim();
    if (!text) { reply.code(400).send({ error: "text (or value) is required" }); return; }
    const e = await memoryStore.update((req.params as { id: string }).id, text, await cwdForProject(body.projectId));
    if (!e) { reply.code(404).send({ error: "no such memory entry" }); return; }
    return e;
  });
  app.delete("/api/memory/:id", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return { ok: await memoryStore.remove((req.params as { id: string }).id, await cwdForProject(projectId)) };
  });
  // Manual semantic search (recall observability + agent-independent lookup).
  app.post("/api/memory/search", async (req) => {
    const body = (req.body ?? {}) as { query?: string; projectId?: string; threshold?: number; topK?: number; folder?: string; scope?: "project" | "global"; category?: string };
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
  // Observable entity/relationship graph (global + current project).
  app.get("/api/memory/graph", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return memoryStore.graphFor(await cwdForProject(projectId));
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
    const body = (req.body ?? {}) as { projectId?: string | null; sessionId?: string | null; label?: string; origin?: string; steps: Array<{ agent: string; task: string; parents?: string[]; batch?: string }> };
    if (!Array.isArray(body.steps) || body.steps.length === 0) {
      reply.code(400).send({ error: "steps (non-empty array) is required" });
      return;
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
    const body = (req.body ?? {}) as { stepId: string; status: WorkflowRun["steps"][number]["status"]; output?: string; error?: string; usage?: WorkflowRun["steps"][number]["usage"] };
    if (!run.steps.some((s) => s.id === body.stepId)) { reply.code(404).send({ error: "no such step" }); return; }
    const ALLOWED = ["pending", "ready", "running", "done", "error", "skipped"];
    if (body.status !== undefined && !ALLOWED.includes(body.status)) { reply.code(400).send({ error: "invalid status" }); return; }
    await workflowStore.stepState(run.id, body.stepId, { status: body.status, output: body.output, error: body.error, usage: body.usage });
    return workflowStore.get(run.id);
  });
  app.post("/api/workflows/:id/abort", async (req) => {
    await workflowStore.abort((req.params as { id: string }).id);
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
      reply.code(/stale browser (?:ref|action)/i.test(message) ? 409 : 400).send({ error: message });
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
    if (!body.language || body.code === undefined) {
      reply.code(400).send({ error: "language and code are required" });
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
  app.post("/api/sessions", async (req) => {
    const body = (req.body ?? {}) as CreateOpts;
    const entry = await pi.create(body);
    return sessionSummary(entry.id, entry.session, entry.profile.id, entry.projectId);
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
    if (!ref.provider || !ref.modelId) {
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
    s.setThinkingLevel(((req.body ?? {}) as { level: ThinkingLevel }).level);
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
    s.setActiveToolsByName(((req.body ?? {}) as { tools?: string[] }).tools ?? []);
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
    // Fan workflow events (step state transitions, run lifecycle) to the same socket so the UI
    // can render the live workflow graph. All active workflow runs broadcast here; the UI filters
    // by sessionId/projectId as needed.
    const offWorkflow = workflowStore.onEvent((runId, event) => {
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "workflow", sessionId, runId, event }));
    });
    socket.send(JSON.stringify({ kind: "ready", sessionId }));
    wsSockets.add(socket);

    socket.on("message", async (raw: Buffer) => {
      let msg: { kind?: string; text?: string; projectId?: string | null; language?: string; code?: string; mode?: "terminal" | "web"; timeoutMs?: number; runId?: string; x?: number; y?: number; action?: "move" | "click" | "type"; cursorText?: string; gateId?: string; feedback?: string };
      try {
        msg = JSON.parse(raw.toString());
      } catch {
        return;
      }
      const s = entry.session;
      try {
        if (msg.kind === "prompt") await s.prompt(msg.text ?? "", s.isStreaming ? { streamingBehavior: "followUp" } : undefined);
        else if (msg.kind === "steer") await s.steer(msg.text ?? "");
        else if (msg.kind === "followUp") await s.followUp(msg.text ?? "");
        else if (msg.kind === "abort") await s.abort();
        else if (msg.kind === "sandbox.start") {
          const run = await sandbox.start(msg.projectId || entry.projectId || null, msg.language || "bash", msg.code || "", {
            timeoutMs: msg.timeoutMs,
            mode: msg.mode,
            onEvent: sandboxSub,
          });
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
        socket.send(JSON.stringify({ kind: "error", error: (e as Error).message }));
      }
    });

    socket.on("close", () => { offAgent(); offWorkflow(); wsSockets.delete(socket); });
  });

  // Static chat UI last, so explicit /api and /ws routes win.
  await app.register(fastifyStatic, { root: WEB_DIR, prefix: "/" });

  return { app, pi };
}

export async function start(): Promise<void> {
  const { app, pi } = await buildServer();
  await app.listen({ host: HOST, port: PORT });
  console.log(`dotz server → http://${HOST}:${PORT}`);
  const shutdown = async () => {
    sandbox.disposeAll();
    pi.disposeAll();
    await app.close();
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
