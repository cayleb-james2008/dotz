/** Shared value/type constants (kept separate so pi.ts and profiles.ts don't import each other). */
export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";

export interface ModelRef {
  provider: string;
  modelId: string;
}

/** dotz default: OpenRouter custom model id (Cayleb's preference), a free model. */
export const DEFAULT_MODEL: ModelRef = { provider: "openrouter", modelId: "nex-agi/nex-n2-pro:free" };

/** Known provider display metadata — keeps the UI readable without hard-coding catalog logic. */
export interface ProviderMeta {
  id: string;
  label: string;
  /** Whether this provider supports free-form model-id input (vs. a fixed catalog). */
  freeForm?: boolean;
}

export const PROVIDERS: ProviderMeta[] = [
  { id: "openrouter", label: "OpenRouter", freeForm: true },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "google", label: "Google" },
  { id: "groq", label: "Groq" },
  { id: "mistral", label: "Mistral" },
  { id: "xai", label: "xAI" },
  { id: "deepseek", label: "DeepSeek" },
  { id: "cohere", label: "Cohere" },
  { id: "local", label: "Local (Ollama/LM Studio)" },
];

/** Project definition — persistent workspace with its own cwd, profile, model, and memory. */
export interface Project {
  id: string;
  name: string;
  cwd: string;
  profileId: string;
  model: ModelRef;
  thinkingLevel: ThinkingLevel;
  createdAt: number;
  updatedAt: number;
}

/** A persistent memory entry attached to a project (injected into agent context). */
export interface MemoryEntry {
  id: string;
  projectId: string;
  key: string;
  value: string;
  scope: "project" | "global";
  createdAt: number;
  updatedAt: number;
}

/** A sandbox run — isolated code execution with captured output. */
export interface SandboxRun {
  id: string;
  projectId: string | null;
  language: string;
  code: string;
  status: "pending" | "running" | "done" | "error" | "killed";
  output: string;
  exitCode: number | null;
  startedAt: number;
  endedAt: number | null;
}

/** A unified skill discovered across the opencode/claude/codex/ecc/superpowers pools + bundled .pi. */
export interface Skill {
  name: string;
  description: string;
  /** Absolute path to the SKILL.md body. */
  path: string;
  /** Which pool this skill was discovered in. */
  source: "opencode" | "claude" | "codex" | "ecc" | "superpowers" | "hermes" | "dotz";
  /** Raw body (loaded lazily — only present after loadBody() is called). */
  body?: string;
  /** Optional tags from frontmatter. */
  tags?: string[];
  /** Compatibility flag (e.g. "opencode"). */
  compatibility?: string;
  /** Platforms this skill is valid on (from Hermes-style frontmatter). */
  platforms?: string[];
  /** Related skill names (from Hermes/Claude frontmatter). */
  relatedSkills?: string[];
  /** Is this an umbrella skill that routes to leaf SKILL.md files? */
  isUmbrella?: boolean;
}

/** A node in a workflow run's DAG — one agent executing one task. */
export interface WorkflowStep {
  id: string;
  /** Agent name (from .pi/agents or discovered user agents). */
  agent: string;
  /** The task text given to the agent. */
  task: string;
  /** Step state. */
  status: "pending" | "ready" | "running" | "done" | "error" | "skipped";
  /** Parent step ids — steps that must complete before this one becomes ready. */
  parents: string[];
  /** Child step ids — steps waiting on this one. */
  children: string[];
  /** Batch label — steps in the same batch were dispatched together (parallel fan-out). */
  batch?: string;
  /** Captured output (filled when status → done). */
  output?: string;
  /** Error message (filled when status → error). */
  error?: string;
  /** Usage stats from the subagent run. */
  usage?: { input?: number; output?: number; cost?: number; turns?: number };
  startedAt?: number;
  endedAt?: number;
}

/** A workflow run — a DAG of steps executed via the subagent extension, observable by the UI. */
export interface WorkflowRun {
  id: string;
  projectId: string | null;
  sessionId: string | null;
  /** Human label for the run (e.g. "/implement-and-review: add login flow"). */
  label: string;
  /** The steps, in dispatch order. */
  steps: WorkflowStep[];
  /** Run state. */
  status: "pending" | "running" | "done" | "error" | "aborted";
  /** Which workflow preset or prompt spawned this run (if any). */
  origin?: string;
  createdAt: number;
  updatedAt: number;
  startedAt?: number;
  endedAt?: number;
}