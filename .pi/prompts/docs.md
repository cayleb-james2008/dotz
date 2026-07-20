---
description: Author and maintain documentation - living-docs, accurate examples that compile, audience-aware tone, AGENTS.md doctrine. Use for "document X", "write docs for X", "update the readme".
---
Author or maintain documentation for: $@

Living docs, accurate examples, audience-aware tone. No marketing fluff.

1. AUDIENCE — decide first: operator (how do I run this?), contributor (how do I change this?), or end user (what does this do?). Tone and depth follow from that.
2. ACCURATE — every code snippet in the doc MUST compile or run. Verify each snippet with `sandbox_run` (terminal) before committing it. A snippet that does not is worse than no snippet — it teaches the wrong thing.
3. LIVING — use `living_docs_read` and `living_docs_suggest` to keep docs in sync with code. Use `agents_md` (action `read` then `write`) to keep project `AGENTS.md` doctrine current. Update the doc as part of the change that made it stale, not in a separate pass.
4. CONCISE — prefer deletion over rot. A deleted stale doc cannot mislead. Do not add generic documentation churn.

Rules:
- Use `memory_search` for prior decisions about the topic so the doc reflects the chosen path, not a superseded one.
- Link to the authoritative source (file path + line range, or upstream doc URL) instead of restating it.
- For API surfaces, match `docs/api-contract.md` — do not invent a contract the code does not implement.

Report: the files written/updated, the audience chosen, and a one-line note per snippet confirming it was verified to run.