---
description: Spec-first scout and plan - explores context, may write OpenSpec artifacts, no product-code edits
---
Use dotz's native OpenSpec tools and the subagent tool with the chain parameter:

1. Call `openspec_status` / `openspec_explore`.
2. If no suitable active change exists, call `openspec_propose` for: $@. This workflow may write spec artifacts only.
3. Use the "scout" agent to find all code relevant to the spec/task.
4. Use the "planner" agent to create an implementation plan for "$@" using the context from the previous step (use `{previous}` placeholder). The plan must include readiness gates, verification, VCS branch/commit flow, and rollback.

Execute the subagent part as a chain, passing output between steps via `{previous}`. Do NOT edit product/source code - return the plan and spec artifact paths.
