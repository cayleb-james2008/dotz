/**
 * Registers a LOCAL OpenAI-compatible provider as the dotz "local" provider, so the
 * "Local (Ollama/LM Studio)" option in the UI actually routes to a server on this machine
 * instead of falling through to a relabeled OpenRouter model (which would send local prompts
 * to openrouter.ai — see src/pi.ts resolveModel).
 *
 * Defaults to Ollama's local OpenAI endpoint (http://localhost:11434/v1). LM Studio users set
 * DOTZ_LOCAL_BASE_URL=http://localhost:1234/v1 (or whatever port their server uses).
 *
 * Auth: local servers (Ollama, LM Studio) ignore the bearer token, so a literal placeholder keeps
 * pi's hasConfiguredAuth("local") happy without a real secret. If your local server DOES require a
 * key, set DOTZ_LOCAL_API_KEY and it is read from the environment at request time (the `$` form,
 * like the Ollama Cloud extension — a bare literal would be sent verbatim).
 *
 * The seeded models are free-form TEMPLATES: any local model id you type clones one (src/pi.ts:169),
 * so you are not limited to this list. reasoning:true is the safe default — a reasoning model needs
 * it (else its answer lands in the reasoning channel and content comes back empty), while a plain
 * completion model simply emits no reasoning and is unaffected.
 */
import { type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const BASE_URL = process.env.DOTZ_LOCAL_BASE_URL || "http://localhost:11434/v1";
// $-form when a key is provided (resolved from env at request time); literal placeholder otherwise.
const API_KEY = process.env.DOTZ_LOCAL_API_KEY ? "$DOTZ_LOCAL_API_KEY" : "local";

const MODEL = (id: string, name: string) => ({
  id,
  name,
  reasoning: true,
  input: ["text"] as string[],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 32768,
  maxTokens: 8192,
});

export default function (pi: ExtensionAPI) {
  pi.registerProvider("local", {
    baseUrl: BASE_URL,
    apiKey: API_KEY,
    api: "openai-completions",
    models: [
      MODEL("qwen2.5-coder", "Qwen2.5 Coder (local)"),
      MODEL("llama3.1", "Llama 3.1 (local)"),
    ],
  });
}
