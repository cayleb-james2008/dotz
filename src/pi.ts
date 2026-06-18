/**
 * dotz session manager — embeds the pi.dev SDK (@earendil-works/pi-coding-agent).
 * The dotz server *is* the pi agent: this owns AgentSession lifecycle and fans agent
 * events out to subscribers (WebSocket connections). Import only from the top-level
 * package — pi-ai / pi-agent-core are nested and not resolvable from the project root.
 */
import { createAgentSession, type AgentSession } from "@earendil-works/pi-coding-agent";
import { buildResourceLoader, getProfile, PROFILES, profileSummary, type Profile } from "./profiles";
import { projectStore } from "./projects";

export type { AgentSession };

export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";

export interface ModelRef {
  provider: string;
  modelId: string;
}

export interface CreateOpts {
  cwd?: string;
  model?: ModelRef;
  thinkingLevel?: ThinkingLevel;
  tools?: string[];
  /** dotz profile id (workflow | solo | plan | frontend | backend). Defaults to "workflow". */
  profileId?: string;
  /** Bind the session to a persistent dotz project — injects its cwd, profile, model, and memory. */
  projectId?: string;
}

export { PROFILES, profileSummary, type Profile };

/** dotz default: OpenRouter custom model id (Cayleb's preference), a free model. */
export const DEFAULT_MODEL: ModelRef = { provider: "openrouter", modelId: "nex-agi/nex-n2-pro:free" };

type Listener = (event: unknown) => void;

export interface SessionEntry {
  id: string;
  session: AgentSession;
  profile: Profile;
  projectId: string | null;
  listeners: Set<Listener>;
  unsubscribe: () => void;
}

export class PiSessions {
  private entries = new Map<string, SessionEntry>();

  async create(opts: CreateOpts = {}): Promise<SessionEntry> {
    // If a project is bound, its cwd/profile/model/thinking override the bare opts — the project
    // is the user's pinned workspace intent, so it wins over ad-hoc defaults.
    let cwd = opts.cwd || process.cwd();
    let profileId = opts.profileId;
    let modelRef = opts.model;
    let thinkingLevel = opts.thinkingLevel;
    let projectId: string | null = null;
    if (opts.projectId) {
      const project = await projectStore.get(opts.projectId);
      if (project) {
        projectId = project.id;
        cwd = project.cwd;
        profileId = project.profileId;
        modelRef = project.model;
        thinkingLevel = project.thinkingLevel;
      }
    }
    const profile = getProfile(profileId);
    // Load the bundled .pi (subagent extension, skills, workflow prompts) + inject the profile's
    // operating doctrine as an appendSystemPrompt. When a project is bound, its persistent memory
    // entries are also injected so the agent carries durable context across sessions.
    const resourceLoader = await buildResourceLoader(cwd, profile, { projectId });
    const { session } = await createAgentSession({ cwd, resourceLoader });

    // A profile pins a default model + thinking level, but an explicit opts override wins
    // (opts only wins when there is no project binding — project wins by design above).
    const model = modelRef ?? profile.model;
    const resolved = session.modelRegistry.find(model.provider, model.modelId);
    if (resolved) {
      try {
        await session.setModel(resolved as Parameters<AgentSession["setModel"]>[0]);
      } catch {
        /* model has no configured auth — keep the session's existing default */
      }
    }
    if (thinkingLevel && session.supportsThinking()) session.setThinkingLevel(thinkingLevel);
    else if (session.supportsThinking()) session.setThinkingLevel(profile.thinkingLevel);
    if (opts.tools) session.setActiveToolsByName(opts.tools);
    else if (profile.tools) session.setActiveToolsByName(profile.tools);

    const listeners = new Set<Listener>();
    const unsubscribe = session.subscribe((event) => {
      for (const l of [...listeners]) {
        try {
          l(event);
        } catch {
          /* never let a slow/broken listener break the agent event loop */
        }
      }
    });

    const entry: SessionEntry = { id: session.sessionId, session, profile, projectId, listeners, unsubscribe };
    this.entries.set(entry.id, entry);
    return entry;
  }

  get(id: string): SessionEntry | undefined {
    return this.entries.get(id);
  }

  list(): SessionEntry[] {
    return [...this.entries.values()];
  }

  subscribe(id: string, listener: Listener): () => void {
    const e = this.entries.get(id);
    if (!e) throw new Error(`no such session: ${id}`);
    e.listeners.add(listener);
    return () => e.listeners.delete(listener);
  }

  dispose(id: string): void {
    const e = this.entries.get(id);
    if (!e) return;
    e.unsubscribe();
    e.session.dispose();
    e.listeners.clear();
    this.entries.delete(id);
  }

  disposeAll(): void {
    for (const id of [...this.entries.keys()]) this.dispose(id);
  }
}

// ---- control helpers (operate on a live AgentSession) ----

export interface ModelInfo {
  provider: string;
  modelId: string;
  name: string;
  reasoning: boolean;
  contextWindow?: number;
}

type PiModel = Parameters<AgentSession["setModel"]>[0];

export function modelInfo(m: unknown): ModelInfo {
  const x = m as { provider: string; id: string; name: string; reasoning?: boolean; contextWindow?: number };
  return { provider: x.provider, modelId: x.id, name: x.name, reasoning: !!x.reasoning, contextWindow: x.contextWindow };
}

/**
 * Resolve a model by provider+id. For providers that support free-form model-id input
 * (OpenRouter, local), if the id isn't in the built-in catalog, clone an existing model
 * from that provider as a template so any valid model id works. For catalog-only providers
 * (anthropic, openai, etc.), only registered models resolve.
 */
export function resolveModel(session: AgentSession, ref: ModelRef): PiModel | undefined {
  const reg = session.modelRegistry;
  const found = reg.find(ref.provider, ref.modelId);
  if (found) return found as PiModel;
  const freeForm = ref.provider === "openrouter" || ref.provider === "local";
  if (freeForm) {
    const tmpl = (reg.getAll() as Array<{ provider: string }>).find((m) => m.provider === ref.provider);
    if (tmpl) return { ...(tmpl as object), id: ref.modelId, name: ref.modelId } as PiModel;
  }
  return undefined;
}

export interface SlashCommand {
  name: string;
  description: string;
  source: "extension" | "prompt" | "skill";
}

/** Aggregate invocable commands exactly like pi's RPC get_commands: extensions, prompt templates, skills. */
export function listCommands(session: AgentSession): SlashCommand[] {
  const s = session as unknown as {
    extensionRunner: { getRegisteredCommands(): Array<{ invocationName: string; description: string }> };
    promptTemplates: ReadonlyArray<{ name: string; description: string }>;
    resourceLoader: { getSkills(): { skills: Array<{ name: string; description: string }> } };
  };
  const out: SlashCommand[] = [];
  for (const c of s.extensionRunner.getRegisteredCommands()) out.push({ name: c.invocationName, description: c.description, source: "extension" });
  for (const t of s.promptTemplates) out.push({ name: t.name, description: t.description, source: "prompt" });
  for (const sk of s.resourceLoader.getSkills().skills) out.push({ name: `skill:${sk.name}`, description: sk.description, source: "skill" });
  return out;
}
