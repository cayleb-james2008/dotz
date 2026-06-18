---
name: self-improve
description: Run a recursive self-improvement cycle — measure baseline, research, pick the highest-leverage improvement, plan (human gate), implement, review, simplify, verify
---
You are running a dotz RECURSIVE SELF-IMPROVEMENT cycle. Follow this exact 7-phase procedure:

## Phase 0: Orient + Baseline
- Git safety: confirm we're on a clean branch (or create one: `rsi/cycle-<timestamp>`)
- Call `rsi_baseline` to capture typecheck + build + tests state
- Identify a hard-to-game needle metric (not just "test count" — something that proves real value)

## Phase 1: Research (parallel fan-out)
Dispatch scout subagents (parallel) to audit dimensions:
- Correctness (bugs, error handling)
- Performance (hot paths, N+1 queries, bundle size)
- Test gaps (untested critical paths)
- Security (input validation, auth boundaries)
- Tech debt (duplication, stale deps, TODOs)
- DX (setup friction, unclear docs)
- Features (missing UX, incomplete flows)

## Phase 2: Select + Plan → HUMAN GATE
Pick the SINGLE highest-leverage improvement (the one that moves the needle most per unit effort).
Write a concrete plan (files to touch, approach, verification).
**Call `human_gate` with the plan. STOP and wait for approval.** Do NOT implement until approved.

## Phase 3: Implement (TDD + checkpoint commits)
After approval:
- Dispatch worker subagents per task (fresh subagent per task, full task text in context)
- TDD: write the test first, watch it fail, write minimal code to pass
- Checkpoint commit after each task passes

## Phase 4: Review (5-reviewer ultra fan-out)
Run the ultra-code-review procedure (5 parallel reviewers → isolated scorers → confidence ≥ 80 filter).
Treat findings as required work. Fix, re-review.

## Phase 5: Simplify
Look for over-engineering, dead code, unnecessary abstraction introduced. Remove it. Re-run tests.

## Phase 6: Verify + Report
- Call `rsi_compare` to re-measure against the Phase 0 baseline
- Confirm: tests still green (not deleted/weakened), typecheck clean, build passes
- Write a report:
```
## RSI Cycle Report
- Baseline: <summary>
- After: <summary>
- Improvement: <what changed>
- Evidence: <metric movement>
- Next opportunity: <what the next cycle should tackle>
```

If the project has its OWN self-improvement system, switch to META-IMPROVER mode: improve THAT system instead of the product. Never touch frozen-core paths. Never weaken the safety ladder.