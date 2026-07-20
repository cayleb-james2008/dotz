---
description: Migration assistance - map old to new, preserve behavior, incremental test-backed steps. Use for "migrate X to Y", "upgrade from A to B", "port X to Y".
---
Migrate: $@

Preserve behavior across the migration. Incremental, test-backed steps. Never a big-bang rewrite.

1. MAP — `read` the old surface and the new surface. Produce a table: old API -> new API, with the behavior each preserves. Use `memory_search` for prior migration notes on this codebase.
2. TESTS — characterize the CURRENT behavior with tests (`/test <target>`) before changing anything. These tests are the migration contract: they must pass before and after.
3. INCREMENTAL — migrate one slice at a time. After each slice: tests green, `vcs_atomic_commit`, next slice. A failed slice is one revert away.
4. WRAP — where the old API must stay during the migration, wrap it (delegate to the new API) rather than duplicating logic. Mark the wrapper with a `# migration:` comment naming the removal step.
5. REMOVE — once the old API has no callers, delete it and its tests-against-the-old-shape. Keep the behavior tests.

Rules:
- Never mix a migration with a feature or a refactor. Open a separate task.
- If the new API cannot preserve a behavior, that is a breaking change — stop and propose it explicitly; do not silently drop behavior.
- Use `living_docs_suggest` if the migration changes a documented API.

Report: the old->new map, the before/after gate output, the list of commits, and any behavior that could not be preserved (there should be none).