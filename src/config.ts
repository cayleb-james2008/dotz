/**
 * dotz global config — the operator's provider, executive model, subagent model, and reasoning
 * defaults, persisted to ~/.dotz/config.json so new sessions (and new chats) inherit them.
 * The subagent model is exported to the bundled subagent extension via the DOTZ_SUBAGENT_MODEL
 * env var, so every dispersed subagent runs on it by default (minimax-m3 on Ollama Cloud).
 */
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { PROVIDER_DEFAULTS, DEFAULT_PROVIDER, VALID_THINKING_LEVELS, type ModelRef, type ThinkingLevel } from "./types";

// DOTZ_CONFIG_DIR overrides the config location (operator relocation + test isolation so verify
// scripts never read or mutate the operator's real ~/.dotz/config.json). Resolved lazily so a
// caller can set the env var before the first loadConfig() (ESM hoists imports, so a module-load
// constant would be fixed before a script's body runs).
const dotzDir = () => process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz");
const configFile = () => path.join(dotzDir(), "config.json");

export interface DotzConfig {
  provider: string;
  executiveModel: string;
  subagentModel: string;
  thinkingLevel: ThinkingLevel;
}

function defaults(): DotzConfig {
  const d = PROVIDER_DEFAULTS[DEFAULT_PROVIDER];
  return { provider: DEFAULT_PROVIDER, executiveModel: d.executive, subagentModel: d.subagent, thinkingLevel: "high" };
}

let cache: DotzConfig | null = null;

/** The bundled subagent extension reads this to pin every dispersed subagent's model. */
function applyEnv(c: DotzConfig): void {
  process.env.DOTZ_SUBAGENT_MODEL = `${c.provider}/${c.subagentModel}`;
}

export async function loadConfig(): Promise<DotzConfig> {
  try {
    const raw = await fs.readFile(configFile(), "utf-8");
    const parsed = JSON.parse(raw) as Partial<DotzConfig>;
    // Guard a hand-edited or corrupt config.json so an invalid thinkingLevel doesn't propagate
    // to session.setThinkingLevel() and cause every new session to fail.
    if (parsed.thinkingLevel !== undefined && !VALID_THINKING_LEVELS.has(parsed.thinkingLevel)) {
      parsed.thinkingLevel = defaults().thinkingLevel;
    }
    cache = { ...defaults(), ...parsed };
  } catch {
    cache = defaults();
  }
  applyEnv(cache);
  return cache;
}

export function getConfig(): DotzConfig {
  if (!cache) {
    cache = defaults();
    applyEnv(cache);
  }
  return cache;
}

export async function updateConfig(patch: Partial<DotzConfig>): Promise<DotzConfig> {
  // Allow-list known string fields so a junk key or a non-string value (e.g. an array `provider`)
  // can't be persisted and then corrupt DOTZ_SUBAGENT_MODEL via applyEnv.
  const clean: Partial<DotzConfig> = {};
  if (typeof patch.provider === "string") clean.provider = patch.provider;
  if (typeof patch.executiveModel === "string") clean.executiveModel = patch.executiveModel;
  if (typeof patch.subagentModel === "string") clean.subagentModel = patch.subagentModel;
  if (typeof patch.thinkingLevel === "string" && VALID_THINKING_LEVELS.has(patch.thinkingLevel)) {
    clean.thinkingLevel = patch.thinkingLevel;
  }
  const next = { ...getConfig(), ...clean };
  cache = next;
  applyEnv(next);
  await fs.mkdir(dotzDir(), { recursive: true });
  await fs.writeFile(configFile(), JSON.stringify(next, null, 2), "utf-8");
  return next;
}

/** The executive (lead) model ref the next session should start on. */
export function executiveModelRef(): ModelRef {
  const c = getConfig();
  return { provider: c.provider, modelId: c.executiveModel };
}
