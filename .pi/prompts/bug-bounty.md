---
name: bug-bounty
description: Project-wide visual E2E bug-bounty loop — drive the real app like a user across a region × lens matrix, adversarially verify, fix in safe order, audit each fix, prove a clean single branch, repeat until stopped. Caps a cycle at 25 confirmed findings or when clean; security-gated before any public push. Use for "run a bug bounty", "find and fix bugs across the app on repeat", "dogfood and auto-fix until I stop you".
---
You are running a dotz BUG-BOUNTY cycle. Repeat the cycle below until the user stops you. This
preset is the dotz-native runtime for the `e2e-bug-bounty-loop` skill — **load the full doctrine
first** and follow it; the phases here map its Scout → Hunt → Verify → Fix → Audit → Prove-clean
loop onto dotz's own tools.

## Phase 0 — Preflight
- Call the **`skill`** tool with name `e2e-bug-bounty-loop` and read the full body — it is the
  source of truth for the matrix, the ABSENT-states hunt, fix-safety order, and the push gate.
- Git safety: `git status` must be clean. If the tree is dirty, **stop and ask** (it is the
  user's in-progress work) before fixing on top of it.
- Read **`agents_md`** (action `read`) for dotz's **intentional-design allow-list** (e.g. "the
  sandbox store is throwaway", "stores stay separate from the agent loop") and pass those into
  every finder so they do NOT flag by-design behaviour — the single biggest false-positive cut.
- Recall the cross-cycle ledger: **`memory_search`** for `bug-bounty findings <project>` so you
  do not re-report issues a prior cycle already skipped.
- Capture the BEFORE gate with **`rsi_baseline`** (typecheck + build + tests). A missing gate is
  itself a finding.
- Launch the app into the **monitored sandbox browser** and confirm it renders before hunting
  (a crash on launch is the worst bug — fix that first, then relaunch).

## Phase 1 — Scout (1 subagent)
Use the **`subagent`** tool (`agentScope: "both"`) with the **`scout`** agent to map the 4–8
user-facing surfaces (regions) and how each opens. These are the rows of the matrix.

## Phase 2 — Hunt (region × lens matrix)
Fan out with the **`subagent`** tool's `tasks` array (parallel; ≤8 per call, so batch regions if
needed). For each region, run the three lenses — **edge-cases**, **user-interaction**, **visual**
(both halves: defects in present states AND the ABSENT empty/loading/error states the skill
mandates you drive *into*). Give each finder the intentional-design allow-list and tell it to
return **at most its 6 strongest findings (empty if none)** against a typed schema.

dotz gives you a **real visual driver**, so do NOT fall back to code-only review when a screen
exists: drive the live app with **`browser_start`** (one isolated session per region — this is
the skill's contention rule, solved natively: regions run in parallel, each on its own session),
**`browser_act`** (navigate/click/type/observe — force each empty/error/loading condition), and
**`browser_stop`**. Capture a concrete repro + observation for every finding; never report a bug
you did not trigger. Optionally run **`design_audit`** on each surface for the UX/accessibility
half.

## Phase 3 — Verify (refute + dedup + confidence gate)
Dedup findings across the whole matrix (merge same file/symbol/route + symptom). Then use the
**`subagent`** **`reviewer`** agent, one per surviving finding, prompted to **refute** it
(re-run the repro, cite the code). Each verdict returns `isReal`, a **confidence 0–100 (default
LOW when uncertain)**, and a **`refinedFix`**. Keep only findings that are **`isReal` AND
confidence ≥ 70**; the refinedFix supersedes the finder's guess. Cap the cycle at **25** confirmed,
most-severe first.

## Phase 4 — Fix (safe order, strict scope)
Use the **`subagent`** **`worker`** agent to fix confirmed findings in the skill's safe order
(mechanical → correctness-with-a-test → security → behavior-frozen simplification → perf), applying
the `refinedFix`. Strict scope per fix: only the implicated files, no refactors/renames/new
features. ≤2 attempts per finding; on the 2nd failure, mark `skipped(<reason>)` and move on. For a
risky/irreversible fix, gate it with **`human_gate`** before applying.

## Phase 5 — Audit each fix (independent fix-breaker)
For every applied fix, run a **fresh `reviewer` subagent whose job is to BREAK it, not re-confirm
it**: feed boundary/empty/malformed inputs, check the exact invariant the bug violated now holds,
and hunt a regression the fix introduced — *executing* inputs where it can. A fix that doesn't
hold is **not done** — rework or revert it. This is separate from (and catches what's missed by)
the gate re-run below.

## Phase 6 — Prove clean + consolidate
- Re-drive each fixed flow through the browser (a fix you didn't re-run is a claim, not a result).
- **`rsi_compare`** for AFTER gates — all must pass, tests not weakened/deleted.
- Consolidate to the single main branch (`master`), `git status` clean, no stray sandbox
  artifacts or scratch branches.

## Phase 7 — Public-push gate (hard stop)
dotz is a **public** repo. Pushing is NEVER automatic. If the user asked to publish/push, follow
the skill's `public-repo-gate`: security-review the diff (secrets/keys/PII/backdoors — block on
any finding), then call **`human_gate`** with a plan that states exactly what would be published
and asks **"Are you 100% comfortable making this code live and public?"**. Push only on an
explicit approval; a private remote gets the security review but not the public go/no-go.

## Phase 8 — Ledger + loop
- **`memory_add`** (scope `project`, category `bug-bounty`) a one-line ledger of found / fixed /
  skipped (and any by-design dismissals) so the next cycle won't re-surface them.
- Report the cycle: found / fixed / skipped / audit-failed / still-open with BEFORE→AFTER gate
  numbers.
- Start the next cycle unless the user stopped you. After two consecutive clean cycles, tell the
  user the project is bounty-clean and ask whether to keep looping or stop.

The dotz WorkflowStore makes each phase's subagent fan-out visible as a node/edge graph in the
UI — the matrix, the live `browserSessionId` per cell, and the fixes are all observable as they
run.
