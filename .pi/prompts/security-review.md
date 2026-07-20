---
description: Security audit - threat model, input validation, secret handling, dependency vuln scan. Use for "security review X", "audit X for vulns", "is X safe to ship".
---
Security audit of: $@

Threat-model first, then hunt. Find real vulns; do not generate generic checklist churn.

1. THREAT MODEL — name the trust boundaries (where untrusted input enters, where secrets live, where auth is checked). `read` the target and list: entry points, privileged operations, secret storage, and any `unsafe`/`exec`/`eval`/`Command::new`/`innerHTML`. Use `memory_search` for prior security findings on this codebase.
2. INPUT VALIDATION — at every trust boundary, confirm input is validated BEFORE use. Flag: string-concatenated SQL, shell commands with user input, deserialized user JSON, path joins with user input, regex with user input.
3. SECRET HANDLING — keys/tokens must come from env vars or `~/.pi/agent/auth.json`, never literals. Confirm they are never logged, never sent to telemetry, never echoed in error messages. (dotz's rule: see `docs/provider-setup.md`.)
4. DEPENDENCIES — `cargo audit` (or the project's native scanner) for known CVEs. Flag any pinned dep that is pinned for a reason that has since been fixed.
5. OUTPUT — for each finding: file:line, severity (CRITICAL/HIGH/MEDIUM/LOW), the concrete attack, and the fix. Do not edit files in this pass — return the findings.

Rules:
- Do not flag by-design behavior. Read `agents_md` first for the intentional-design allow-list (e.g. "the sandbox store is throwaway", "keys are read at request time only").
- Do not flag `.env.example` values (names only, not real keys), test credentials clearly marked, or public API keys meant to be public.
- A finding without a concrete attack is a suggestion, not a vuln. Label it as such.

Report: the threat model (one paragraph), the findings table (file:line, severity, attack, fix), and a one-line ship/no-ship recommendation.