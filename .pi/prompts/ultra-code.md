---
description: Canonical Ultra Code execution loop - load ultra-code first, then use dotz adapters for spec, workflow, subagents, review, and verification
---
Use Ultra Code as the canonical harness for: $@

1. Call the `skill` tool for `ultra-code` before planning or editing.
2. Restate the goal, inspect current state, and choose the smallest safe plan.
3. Use dotz-native adapters under Ultra Code:
   - OpenSpec for non-trivial product/source changes.
   - Workflow presets and subagents for decomposable work.
   - Sandbox/browser/design/memory/RSI tools when relevant.
4. Implement with reversible edits and preserve credentials, auth state, caches, runtime DBs, and history.
5. Review, verify with fresh project-native evidence, run skeptic/audit when material, and report in past tense.
