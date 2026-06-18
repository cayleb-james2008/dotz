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