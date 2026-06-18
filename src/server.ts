/**
 * dotz backend — Fastify serving REST + WebSocket + the static chat UI.
 * The WebSocket streams pi AgentSession events AND sandbox execution events; REST handles
 * session creation, controls, projects, memory, and sandbox runs.
 */
import path from "node:path";
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
import { memoryStore } from "./memory";
import { sandbox, SANDBOX_LANGUAGES, type SandboxEvent } from "./sandbox";
import { PROVIDERS, type Project, type MemoryEntry, type SandboxRun } from "./types";

const HOST = "127.0.0.1";
const PORT = Number(process.env.DOTZ_PORT || 4317);
const SELF = fileURLToPath(import.meta.url);
const WEB_DIR = path.resolve(path.dirname(SELF), "../web");

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

  await app.register(fastifyWebsocket);

  // ---- health + providers ----
  app.get("/api/health", async () => ({ ok: true, sessions: pi.list().length, sandboxRuns: sandbox.list().length }));
  app.get("/api/providers", async () => ({ providers: PROVIDERS }));

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

  // ---- memory ----
  app.get("/api/memory", async (req) => {
    const projectId = (req.query as { projectId?: string }).projectId;
    return { entries: await memoryStore.list(projectId) };
  });
  app.post("/api/memory", async (req, reply) => {
    const body = (req.body ?? {}) as { projectId?: string; key?: string; value?: string; scope?: "project" | "global" };
    if (!body.key || body.value === undefined) {
      reply.code(400).send({ error: "key and value are required" });
      return;
    }
    return memoryStore.create({ projectId: body.projectId || "", key: body.key, value: body.value, scope: body.scope });
  });
  app.patch("/api/memory/:id", async (req, reply) => {
    const e = await memoryStore.update((req.params as { id: string }).id, (req.body ?? {}) as Partial<Pick<MemoryEntry, "key" | "value" | "scope">>);
    if (!e) { reply.code(404).send({ error: "no such memory entry" }); return; }
    return e;
  });
  app.delete("/api/memory/:id", async (req) => ({ ok: await memoryStore.remove((req.params as { id: string }).id) }));

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
    const ref = req.body as ModelRef;
    const m = resolveModel(s, ref);
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
    s.setThinkingLevel((req.body as { level: ThinkingLevel }).level);
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
    s.setActiveToolsByName((req.body as { tools: string[] }).tools ?? []);
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
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "event", sessionId, event }));
    });
    // Fan sandbox events to the same socket so the UI can render live sandbox output inline.
    // Any sandbox run started from this session's context (projectId match) broadcasts here.
    const sandboxSub = (e: SandboxEvent) => {
      if (socket.readyState === socket.OPEN) socket.send(JSON.stringify({ kind: "sandbox", sessionId, event: e }));
    };
    socket.send(JSON.stringify({ kind: "ready", sessionId }));

    socket.on("message", async (raw: Buffer) => {
      let msg: { kind?: string; text?: string; projectId?: string | null; language?: string; code?: string; mode?: "terminal" | "web"; timeoutMs?: number; runId?: string; x?: number; y?: number; action?: "move" | "click" | "type"; cursorText?: string };
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
        } else if (msg.kind === "sandbox.cursor") {
          // Cursor event — the UI renders the agent's cursor over the live web preview iframe.
          sandbox.cursor(msg.runId || "", msg.x || 0, msg.y || 0, msg.action || "move", msg.cursorText);
        }
      } catch (e) {
        socket.send(JSON.stringify({ kind: "error", error: (e as Error).message }));
      }
    });

    socket.on("close", () => offAgent());
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