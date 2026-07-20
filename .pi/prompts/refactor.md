---
description: Safe behavior-preserving refactoring - snapshot tests, smallest diff, one conceptual change per step, verify between steps. Use for "refactor X", "clean up X", "extract X".
---
Safe, behavior-preserving refactoring of: $@

Behavior is FROZEN. The test suite MUST pass before and after every step. Smallest diff, one conceptual change per step, never mix a refactor with a feature or a fix.

1. SNAPSHOT — run the full verification gate first (`cargo test -p dotz-core -- --test-threads=2` for dotz). If it is not green, stop: you are debugging, not refactoring.
2. SLICE — pick the smallest test-backed slice. If the slice has no test, write one before touching it (`/test <slice>`).
3. CHANGE — one conceptual move per step: extract a function, rename a symbol, inline a wrapper. Use `read` + `grep` to confirm a symbol has no hidden callers before renaming.
4. VERIFY — re-run the gate. Green? `vcs_atomic_commit` so a bad step is one revert away. Red? Revert and re-slice.
5. REPEAT — until the target is fully refactored.

Rules:
- Never delete a test to make the build green — a deleted test is a regression you have already shipped.
- Never mix a refactor with a feature or a bug fix. Open a separate task.
- Use `living_docs_read` / `living_docs_suggest` if the refactor changes a public API that docs reference.

Report: the before/after gate output, the list of commits (one per step), and any behavior surface that changed (there should be none).