---
description: Performance investigation - profile, find the hot path, optimize with evidence, measure before and after. Use for "X is slow", "optimize X", "perf X".
---
Performance investigation of: $@

Profile first, optimize second. No optimization without a measurement that proves it matters.

1. REPRODUCE — establish a stable workload. For dotz cold start: `cargo bench --bench cold_start`. For a request path: a loop of `curl` against `cargo run -p dotz-core --bin serve`. A flaky workload produces flaky numbers; fix the workload first.
2. PROFILE — find the hot path, not the suspected path. Use `sandbox_run` to run the profiler (e.g. `cargo flamegraph`, `hyperfine`) against the repro. Read the top 3 frames; those are your budget.
3. HYPOTHESIZE — one sentence: "X% of time is in <frame> because <reason>." If you cannot name the reason, you have not isolated the cause.
4. OPTIMIZE — the smallest change that removes the hot frame. Smallest diff, one change per step.
5. MEASURE — re-run the SAME workload. Before/after numbers, same machine, same load. No measurement, no merge.

Rules:
- Never optimize without a before number. "It feels faster" is not a measurement.
- Never trade correctness for speed. A faster wrong answer is a regression.
- Use `memory_add` (scope `project`, category `perf`) to record the before/after so the next investigation starts from a known baseline.
- A micro-optimization that does not move the macro benchmark is noise; do not commit it.

Report: the before/after numbers, the hot frame that was removed, the change (file:line), and the measurement command so it can be re-run.