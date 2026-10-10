# Provider setup

I usually start with one provider, then add others as needed. Adapter support is not evidence of
live inference: check the selected endpoint, model availability, and account limits separately.

For `$VAR` / `${VAR}` key references, dotz resolves credentials in this order:

1. A non-empty environment variable, such as `OLLAMA_API_KEY` or `OPENROUTER_API_KEY`.
2. The matching entry in `~/.pi/agent/auth.json` when that variable is unset or empty.

`DOTZ_PI_AGENT_DIR` overrides the auth directory (the file is `<DOTZ_PI_AGENT_DIR>/auth.json`).
This is separate from `DOTZ_CONFIG_DIR`, which controls dotz's configuration/memory directory.

## Option A — In-app key entry

1. Open the **Connections** panel.
2. Select a provider, paste your key, and **Save**.
3. dotz writes the key under its environment-variable name in `auth.json` and refreshes the
   process's auth cache. A non-empty environment override still wins.

The key-status API reports whether a key is set, not its value. Saved credentials persist across
app restarts. An OS reinstall or deletion of `auth.json` requires restoring your own backup or
entering the keys again. This is a local credential file, not cross-device key synchronization.

## Option B — Environment variables

Set variables in the shell that launches dotz. The repository's `.env.example` lists settings;
copying it to `.env` alone is not a documented loading mechanism. Export variables explicitly or
use the in-app key entry. Replace the placeholders below without posting real keys in screenshots,
logs, issues, or shell transcripts.

PowerShell:

```powershell
$env:OLLAMA_API_KEY = "<your-key>"
cargo run -p dotz-core --bin serve
```

macOS / Linux:

```bash
export OLLAMA_API_KEY="<your-key>"
cargo run -p dotz-core --bin serve
```

Launch a desktop app from the same environment if you want it to inherit these values. A GUI
launcher may not inherit variables exported in a different shell.

## Option C — Edit `auth.json` directly

Create a JSON object mapping environment-variable names to key strings:

```json
{
  "OLLAMA_API_KEY": "<your-key>",
  "OPENROUTER_API_KEY": "<your-key>"
}
```

Keep the file private and outside version control. Restart after a manual edit to reload the cache;
in-app writes refresh it immediately. `auth.rs` attempts owner-only file permissions, but failures
are warnings, so verify permissions on shared machines rather than assuming an encrypted vault.

## Provider reference

| Provider | Key variable | Adapter / defaults |
|----------|--------------|--------------------|
| Ollama Cloud | `OLLAMA_API_KEY` | OpenAI-compatible; primary defaults: executive `glm-5.2`, subagent `minimax-m3` |
| OpenRouter | `OPENROUTER_API_KEY` | OpenAI-compatible; configured fallback `nex-agi/nex-n2-pro:free` |
| OpenAI | `OPENAI_API_KEY` | OpenAI-compatible |
| Anthropic | `ANTHROPIC_API_KEY` | Native Messages API |
| Google | `GEMINI_API_KEY` | Native Gemini API |
| Groq | `GROQ_API_KEY` | OpenAI-compatible |
| Mistral | `MISTRAL_API_KEY` | OpenAI-compatible |
| xAI | `XAI_API_KEY` | OpenAI-compatible |
| DeepSeek | `DEEPSEEK_API_KEY` | OpenAI-compatible, with `reasoning_content` support |
| Cohere | `COHERE_API_KEY` | OpenAI-compatible endpoint |
| NVIDIA NIM | `NVIDIA_API_KEY` | OpenAI-compatible endpoint |
| Local | `DOTZ_LOCAL_API_KEY` | Optional key; default base `http://localhost:11434/v1`, override with `DOTZ_LOCAL_BASE_URL` |

Ollama, OpenRouter, and Local expose free-form model-id inputs. `resolveModel` can clone a
same-provider template for an unknown id; that does not prove the upstream endpoint serves it or
that it is free. Inspect the response and current provider terms before relying on cost labels.

In the inspected source, Local selects the literal placeholder `local` unless a non-empty
`DOTZ_LOCAL_API_KEY` environment variable is set. Do not rely on a Local key saved only in
`auth.json` taking effect on that path; export it explicitly for a local server that needs auth.

## Request headers and credential safety

- A non-empty environment value wins over `auth.json`. If a saved key seems ignored, inspect the
  launching environment without printing its value (and note the Local exception above).
- A variable reference still unresolved after both lookups fails with a variable-named error.
- OpenAI-compatible adapters use `Authorization: Bearer …`. Native Anthropic uses `x-api-key`;
  native Google uses `x-goog-api-key` rather than putting the key in the URL.
- Credentials go to the selected/configured endpoint. If you override an endpoint or use a gateway,
  you are trusting that service with the request and credential; do not assume it is the provider's
  own host. The key store/status routes are designed not to echo key values, but that is not a
  blanket privacy guarantee for tool output or content you paste yourself.

## Memory endpoints and privacy

Memory storage and ONNX sentence embeddings run locally, but automatic fact extraction has its
own endpoint selection; it does not simply follow the current chat provider:

- `DOTZ_MEMORY_BASE_URL`: defaults to `https://ollama.com/v1` (OpenAI-compatible chat API).
- `DOTZ_MEMORY_MODEL`: defaults to the configured executive model, otherwise `glm-5.2`.
- `DOTZ_MEMORY_API_KEY`: a non-empty environment value wins; otherwise dotz resolves
  `OLLAMA_API_KEY` through the environment/auth-file rule above.
- `DOTZ_MEMORY_TIMEOUT_MS`: defaults to 30 seconds, clamped to 1 second–5 minutes.

Extraction sends the user/assistant exchange to that endpoint. It is best-effort: no key or an
endpoint/parse failure can yield no captured facts. Selecting Local for chat does not by itself
move extraction off Ollama Cloud. Configure memory separately before using sensitive text; local
embeddings do not mean an offline application.

Setting `DOTZ_COGNEE_URL` enables an optional memory service that can receive recall queries and
captured facts. Its `DOTZ_COGNEE_API_KEY` and service-side model/auth configuration are separate.
Leave it unset if you do not want that extra service boundary. See
[the memory API contract](api-contract.md#memory-local-storage-and-embeddings-best-effort-capture)
for storage paths and explicit memory controls.
