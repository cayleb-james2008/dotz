---
description: Spec-driven implementation and review - apply spec, worker implements, reviewer audits, worker fixes, verify readiness
---
Use dotz's native OpenSpec and VCS tools before implementation:

1. Call `openspec_status` / `openspec_explore`.
2. If no suitable active change exists, call `openspec_propose` for: $@
3. Call `vcs_branch` with the spec slug, then `openspec_apply`.
4. Use the subagent tool with the chain parameter:
   - worker: implement the spec task, update `tasks.md` and `readiness.md`, and report changed files.
   - reviewer: review the implementation, tests, readiness, security, error handling, and rollback path from the previous step (use `{previous}`).
   - worker: apply required feedback from the review (use `{previous}`).
5. Run tests/build, call `openspec_verify`, then `openspec_sync`.
6. When verification is green, use `vcs_atomic_commit` for one logical task. Use `vcs_pr` only if GitHub CLI status is ready.

Execute the subagent part as a chain, passing output between steps via `{previous}`.
