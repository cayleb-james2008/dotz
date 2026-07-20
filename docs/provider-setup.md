# Provider setup

dotz is multi-provider. Set a key for **one** provider to start; add more later. Keys are resolved
in this order (highest precedence first):

1. **Environment variables** — `OLLAMA_API_KEY`, `OPENROUTER_API_KEY`, etc. (see the table below).
2. **`~/.pi/agent/auth.json`** — written by the in-UI Connections panel; the fallback when the
   matching env var is unset.

Keys are **never logged and never sent to telemetry**. The Connections panel shows a key only while
you are entering it; after Save it is stored in `auth.json` and never displayed again.

## Option A — In-UI (recommended)

1. Open the **Connections** panel in dotz.
2. Select a provider, paste your key, and **Save**.
3. The key is written to `~/.pi/agent/auth.json` and used on every subsequent launch.

This is the easiest path and the only one that survives an OS reinstall of your env files.

## Option B — Environment variables

Set the env var(s) before launching dotz. A `.env.example` is committed at the repo root; copy it
to `.env` (or export the vars in your shell). Env vars **take precedence over `auth.json`** —
use this when you want to override the saved key (e.g. a CI run, a second account).

```bash
# Windows PowerShell
$env:OLLAMA_API_KEY = "sk-..."
cargo run -p dotz-core --bin serve

# macOS / Linux
export OLLAMA_API_KEY=sk-...
cargo run -p dotz-core --bin serve
```

## Option C — `~/.pi/agent/auth.json` directly

Create the file manually with a JSON object mapping env-var names to keys. Same effect as Option A,
just without the UI:

```json
{
  "OLLAMA_API_KEY": "sk-...",
  "OPENROUTER_API_KEY": "sk-or-..."
}
```

## Provider env-var reference

| Provider     | Env var              | Notes                                                    |
|--------------|----------------------|----------------------------------------------------------|
| Ollama Cloud | `OLLAMA_API_KEY`     | Primary. Executive `glm-5.2`, subagent `minimax-m3`.     |
| OpenRouter   | `OPENROUTER_API_KEY` | Free fallback `nex-agi/nex-n2-pro:free`.                 |
| OpenAI       | `OPENAI_API_KEY`     | Free-form model id.                                      |
| Anthropic    | `ANTHROPIC_API_KEY`  | Native Messages API adapter. Free-form model id.         |
| Google       | `GEMINI_API_KEY`     | Native Gemini API adapter. Free-form model id.           |
| Groq         | `GROQ_API_KEY`       | OpenAI-compatible. Free-form model id.                   |
| Mistral      | `MISTRAL_API_KEY`    | OpenAI-compatible. Free-form model id.                   |
| xAI          | `XAI_API_KEY`        | OpenAI-compatible. Free-form model id.                   |
| DeepSeek     | `DEEPSEEK_API_KEY`   | OpenAI-compatible; reasoning in `reasoning_content`.     |
| Cohere       | `COHERE_API_KEY`     | OpenAI-compatible. Free-form model id.                   |
| NVIDIA NIM   | `NVIDIA_API_KEY`     | OpenAI-compatible; free preview models via an nvapi key. |
| Local        | `DOTZ_LOCAL_API_KEY` | Optional. Defaults to `http://localhost:11434/v1` (no auth). Override the base with `DOTZ_LOCAL_BASE_URL`. |

OpenRouter, Ollama, and Local are **free-form model-id inputs** (not dropdowns) — `resolveModel`
clones any same-provider template for unknown ids, so an upstream-valid id just works.

## Precedence and safety

- Env vars win over `auth.json`. If a key seems "stuck", check the environment first.
- A `$VAR` / `${VAR}` reference that resolves to empty fails fast with a clear, variable-named
  error (an unset `OLLAMA_API_KEY` would otherwise send an empty Bearer and 401 silently).
- Keys are read at request time and sent only to the provider's own API as a Bearer token. They
  are not written to logs, run records, telemetry, or the workflow graph.