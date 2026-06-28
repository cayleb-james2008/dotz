---
description: Spec-driven implementation workflow - propose/apply spec, scout gathers context, planner creates plan, worker implements, verify readiness
---
Use dotz's native OpenSpec tools before implementation:

1. Call `openspec_status` / `openspec_explore`.
2. If no suitable active change exists, call `openspec_propose` for: $@
3. Call `vcs_branch` with the spec slug, then `openspec_apply`.
4. Use the subagent tool with the chain parameter:
   - scout: find all code relevant to the spec/task.
   - planner: create an implementation plan using the scout output (use `{previous}`).
   - worker: implement the plan, update `tasks.md` and `readiness.md`, and keep changes scoped.
5. Run the project verification gate, call `openspec_verify`, then `openspec_sync`.
6. When verification is green, use `vcs_atomic_commit` for one logical task.

Execute the subagent part as a chain, passing output between steps via `{previous}`.
