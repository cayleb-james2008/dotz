# Security Policy

## Supported Versions

dotz is pre-1.0 (`0.2.x`). Only the latest `0.2.x` release receives security
fixes. Older tags are unsupported — upgrade to the newest release.

| Version  | Supported          |
| -------- | ------------------ |
| 0.2.x (latest) | Yes          |
| < 0.2.0  | No                 |

## Reporting a Vulnerability

**Do not open a public issue for a security vulnerability.**

Report privately via a
[GitHub Security Advisory](https://github.com/cayleb-james2008/dotz/security/advisories/new)
against this repository. Include:

- What the vulnerability is and where it lives (file / endpoint / commit).
- Steps to reproduce or a minimal proof of concept.
- What you think the impact is (what an attacker could do with it).

## Response Expectations

- Acknowledgement of your report within 7 days.
- A fix or mitigation plan for confirmed issues affecting the latest `0.2.x`
  release, prioritised by severity. There is no paid bounty programme.
- Public disclosure only after a fix is available, coordinated with you.

## Scope Notes

The threat model that matters most for dotz: it runs LLM-driven code execution
(a `bash` tool, a sandbox, and an in-app browser) on your own machine behind a
loopback server (`127.0.0.1:4317`) guarded by an origin/host allowlist
(`dotz-core/src/server/guard.rs`). Treat API keys in `.env` /
`~/.pi/agent/auth.json` as secrets, review agent-proposed commands before they
run in `terminal` mode, and only point `DOTZ_SKILLS_PATHS` at directories you
trust (their `SKILL.md` text is injected into the agent's system prompt).
