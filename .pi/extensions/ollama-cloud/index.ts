/**
 * Registers Ollama Cloud (OpenAI-compatible) as the dotz "ollama" provider, so the executive
 * model (glm-5.2) and every dispersed subagent (minimax-m3) can run on Ollama Cloud.
 *
 * Loaded two ways, both via this single file:
 *   - the dotz lead session, via additionalExtensionPaths in src/profiles.ts;
 *   - each spawned subagent `pi` process, via `-e <this file>` (see .pi/extensions/subagent).
 *
 * `apiKey` MUST be the bare env-var NAME — pi's resolveConfigValue replaces a value that names
 * an env var with process.env[value]. Passing "$OLLAMA_API_KEY" would be sent as a literal bearer
 * token and Ollama would 401. The few seeded models act as free-form templates; any other Ollama
 * Cloud model id (kimi-k2.7-code, deepseek-v4-pro, qwen3-coder-next, …) resolves by cloning one.
 */
import { type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const MODEL = (id: string, name: string) => ({
  id,
  name,
  reasoning: false,
  input: ["text"] as string[],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 256000,
  maxTokens: 8192,
});

export default function (pi: ExtensionAPI) {
  pi.registerProvider("ollama", {
    baseUrl: "https://ollama.com/v1",
    apiKey: "OLLAMA_API_KEY",
    api: "openai-completions",
    models: [
      MODEL("glm-5.2", "GLM 5.2"),
      MODEL("minimax-m3", "MiniMax M3"),
      MODEL("kimi-k2.7-code", "Kimi K2.7 Code"),
      MODEL("deepseek-v4-pro", "DeepSeek V4 Pro"),
    ],
  });
}
