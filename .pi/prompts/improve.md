---
description: Audit the codebase and write prioritized self-contained implementation plans for other agents to execute
---
Load the `improve` skill with the `skill` tool, then run it (read-only senior advisor) on: $@

The `improve` skill only writes plans — it never edits source code. Use scout subagents in parallel (via the `subagent` tool) for the recon/audit phase, then have it produce prioritized, self-contained implementation plans.

If no arguments are given, run a full audit of the current working directory.
