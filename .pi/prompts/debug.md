---
description: Systematic root-cause debugging - understand, reproduce, isolate, fix, then write a regression test first. Use for "debug X", "fix this bug", "why does X happen".
---
Systematic root-cause debugging for: $@

Run the 4-phase root-cause loop. Never patch a symptom — a symptom patch is a second bug.

1. UNDERSTAND — `read` the code around the report; form a hypothesis about the cause. Use `memory_search` for prior fixes to the same symbol/file so you do not re-derive a solved problem.
2. REPRODUCE — write a failing test (or capture exact repro steps) before touching code. If you cannot reproduce it, you cannot verify the fix.
3. ISOLATE — bisect to the smallest input that triggers it. Drop irrelevant variables until the failure is the only thing left.
4. FIX — change the actual cause. Watch the regression test fail first, then pass with the fix. Run the project verification gate (`cargo test -p dotz-core -- --test-threads=2` for dotz).

Rules:
- The regression test is written FIRST, watched to fail, then made to pass. A fix without a failing test is a claim, not a result.
- Use `vcs_atomic_commit` between the failing test and the fix only if the tree is otherwise clean; otherwise commit the test + fix together.
- If a fix would change behavior elsewhere, that is a refactor, not a debug fix — stop and propose it separately.

Report: the root cause (one line), the failing repro, the fix (file:line), and fresh gate output.