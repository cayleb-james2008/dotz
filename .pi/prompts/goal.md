---
description: Run the goal-driven workflow end to end — audit, plan, execute, verify, review, and report
---
Run the full goal loop on: $@

1. Restate the goal in one line.
2. Audit the project first — use the `subagent` tool with the "project-auditor" agent (docs, rules, scripts, manifests, tests, source, existing skills/agents, recent git state).
3. Define success criteria and scope. Load `capability-routing` with the `skill` tool and select skills/agents/tools by category relevance before creating anything new.
4. For non-trivial work, DISPERSE via the `subagent` tool — chain scout → planner → worker, then a "reviewer" subagent to verify adversarially. Prefer the `/implement` or `/implement-and-review` presets.
5. Verify with fresh evidence (test output, build result, file readback) before any success claim.
6. Update docs via the "docs-maintainer" subagent when setup, commands, architecture, or conventions changed.
7. Report in past tense: what changed, what was verified, any remaining user-only step, plus the capability-evidence block (`SKILLS_USED` / `AGENTS_USED` / `CAPABILITY_GAPS` / `CAPABILITIES_CREATED` / `LEARNING_REVIEW`).

If no target was given, use the current working directory. Execute fully — do not stop unless a safety boundary is hit or the user says stop.
