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

Use the `reviewer` agent for each. Do NOT pass a `model` override — the reviewers run on the configured low-cost subagent model by default (inventing a model id breaks the fan-out with auth/credit errors).

## Phase 2: Score + filter (do this yourself — no scorer fan-out)
You now hold every finding the 5 reviewers returned. Score them YOURSELF — do NOT dispatch a scorer
subagent per finding (that spawns dozens of extra subagents and stalls the run for many minutes for
no quality gain). For each finding, assign a 0-100 confidence it's a real issue (0 = noise/already
handled, 100 = definitely a bug), judging it against the actual code you and the reviewers read.
Filter to confidence ≥ 80 and dedupe on `file:line`. Then go straight to Phase 3.

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