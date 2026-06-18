---
name: ultra-code-review
description: Run a 5-reviewer ultra code review with isolated scorers and confidence filtering on the current diff or whole project
---
You are running a dotz ULTRA CODE REVIEW. Follow this exact procedure:

## Phase 1: 5-reviewer fan-out (parallel)
Dispatch 5 reviewer subagents in PARALLEL via the `subagent` tool (`tasks: [...]`). Each reviewer gets a DISJOINT focus and EXCLUSIVE FILES ownership (no two reviewers look at the same dimension). The 5 dimensions:

1. **CLAUDE.md/AGENTS.md compliance** — does the diff honor the project's doctrine + conventions?
2. **Obvious bugs** — logic errors, null derefs, off-by-ones, race conditions, missing error handling
3. **Git history coherence** — does this diff fit the repo's trajectory, or does it undo prior intent? (`git log`, `git show`)
4. **Prior PRs / context** — does this repeat a previously-rejected approach? Are there related merged PRs?
5. **Code comments + intent** — are comments honest? Does the code do what the comments claim?

Use the `reviewer` agent for each. Select a low-cost model for each reviewer via the `model` parameter.

## Phase 2: Isolated scoring (parallel, separate fan-out)
For EACH finding from Phase 1, dispatch a SCORER subagent (separate from the reviewers). Each scorer gets:
- The finding verbatim
- The rubric: "Score this finding 0-100 for confidence it's a real issue. 0 = noise/already handled, 100 = definitely a bug."
- NO exposure to other findings or other scorers' justifications (anti-bias)

Filter to confidence ≥ 80. Dedupe on `file:line`.

## Phase 3: Synthesize
Produce a numbered report:
```
## Ultra Code Review — <N> findings (confidence ≥ 80)

1. [CRITICAL] file.ts:42 — <finding> (confidence: 95)
   Fix: <specific fix>
2. [WARNING] ...
```

If the user said `--fix`, spawn a fix subagent with strict scope (only the flagged files), capped at 2 attempts, then re-verify by re-running the 5 reviewers.

Use `rsi_baseline` + `rsi_compare` to prove the fix didn't regress typecheck/build/tests.