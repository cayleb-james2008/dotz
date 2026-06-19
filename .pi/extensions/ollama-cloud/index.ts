/**
 * Registers Ollama Cloud (OpenAI-compatible) as the dotz "ollama" provider, so the executive
 * model (glm-5.2) and every dispersed subagent (minimax-m3) can run on Ollama Cloud.
 *
 * Loaded two ways, both via this single file:
 *   - the dotz lead session, via additionalExtensionPaths in src/profiles.ts;
 *   - each spawned subagent `pi` process, via `-e <this file>` (see .pi/extensions/subagent).
 *
 * `apiKey` MUST be "$OLLAMA_API_KEY" (the env-var reference, with the leading `$`). pi's
 * resolveConfigValue only interpolates a value from the environment when it contains a `$ENV_VAR`
 * (or `${ENV_VAR}`) reference; a BARE "OLLAMA_API_KEY" is treated as a literal and sent verbatim as
 * the bearer token, which Ollama rejects with 401 (pi then swallows it into an empty reply). The `$`
 * form still keeps the secret out of source (resolved from process.env at request time). The seeded
 * models act as free-form templates; any other Ollama Cloud model id resolves by cloning one.
 */
import { type ExtensionAPI } from "@earendil-works/pi-coding-agent";

// Per-model metadata. These Ollama Cloud models are ALL reasoning ("thinking") models — declaring
// reasoning:false (the old generic default) made pi treat them as plain completion models, so the
// model's answer landed in the separate `reasoning` channel and `content` came back empty: that is
// the root cause of subagents returning "(no output)" AND of the reasoning control showing nothing.
// contextWindow/maxTokens are the real per-model values from Ollama's /api/show (not a flat 256k/8k).
interface ModelOpts {
  reasoning?: boolean;
  contextWindow?: number;
  maxTokens?: number;
  input?: string[];
}
const MODEL = (id: string, name: string, opts: ModelOpts = {}) => ({
  id,
  name,
  reasoning: opts.reasoning ?? true,
  input: opts.input ?? (["text"] as string[]),
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: opts.contextWindow ?? 256000,
  maxTokens: opts.maxTokens ?? 32768,
});

export default function (pi: ExtensionAPI) {
  pi.registerProvider("ollama", {
    baseUrl: "https://ollama.com/v1",
    apiKey: "$OLLAMA_API_KEY",
    api: "openai-completions",
    models: [
      MODEL("glm-5.2", "GLM 5.2", { reasoning: true, contextWindow: 1_000_000, maxTokens: 32768 }),
      MODEL("minimax-m3", "MiniMax M3", { reasoning: true, contextWindow: 524_288, maxTokens: 32768 }),
      MODEL("kimi-k2.7-code", "Kimi K2.7 Code", { reasoning: true, contextWindow: 262_144, maxTokens: 32768 }),
      MODEL("deepseek-v4-pro", "DeepSeek V4 Pro", { reasoning: true, contextWindow: 524_288, maxTokens: 32768 }),
    ],
  });
}
