---
description: Matt Pocock's end-to-end AI coding workflow - grill the user on intent + domain language, write a spec + CONTEXT.md, break into tracer-bullet tickets, implement with TDD at each seam, code-review the diff, then improve codebase architecture. Use for any non-trivial feature or change where alignment + feedback loops matter.
---
Execute Matt Pocock's complete AI coding workflow (mattpocock/skills) for: $@

This is the end-to-end flow: grill → spec → tickets → implement (TDD) → review → improve architecture. Each phase is a subagent in a chain, passing output via `{previous}`. Run it for any change where getting the intent right matters more than speed.

## Phase 1 — Grill (alignment + domain language)

Use the subagent tool with the "planner" agent to interview the user relentlessly about: $@

The grilling must resolve every branch of the decision tree before any code is written. Ask:
- What is the actual problem? (not the assumed solution)
- What modules/files will this touch? Name them.
- What is the domain vocabulary? Capture jargon into a `CONTEXT.md` (create or update it in the project root).
- What are the edge cases? The failure modes? The rollback plan?
- What does "done" look like — the measurable acceptance criteria?

Do NOT skip the grilling. "No-one knows exactly what they want" — the grilling is how the user finds out. If the user says "just build it", push back once, then proceed with the most-likely interpretation and flag the assumption.

Capture durable decisions as ADRs (Architecture Decision Records) in `docs/adr/` or `.ai-agents/adr/` via `living_docs_update`.

## Phase 2 — Spec (synthesize the conversation, no new interview)

Use the subagent tool with the "planner" agent, passing the grilling output via `{previous}`.

Call `openspec_propose` for: $@
The spec synthesizes what was discussed — do NOT re-interview. The spec must declare:
- The modules being touched (deepen them — a lot of behavior behind a small interface, at a clean seam)
- The acceptance criteria (copied from the grilling output)
- The rollback plan

Then call `vcs_branch` with the spec slug + `openspec_apply`.

## Phase 3 — Tickets (tracer-bullet breakdown)

Use the subagent tool with the "planner" agent, passing the spec via `{previous}`.

Break the spec into a set of tracer-bullet tickets. Each ticket:
- Declares its blocking edges (which tickets must land first)
- Is a vertical slice (end-to-end, thin but complete — not a horizontal layer)
- Has its own acceptance criteria + verification gate

Write the tickets to `tasks.md` via `openspec_apply` or `living_docs_update`. Order them by the blocking edges so the executor knows the critical path.

## Phase 4 — Implement (TDD at each seam)

Use the subagent tool with the "worker" agent, passing the tickets via `{previous}`.

For each ticket in dependency order:
1. Write a failing test first (red). Use `sandbox_run` to verify it fails for the right reason.
2. Implement the minimum to make it pass (green). Keep changes scoped to the ticket.
3. Refactor (green stays green). Run the project verification gate.
4. When the gate is green, call `vcs_atomic_commit` for that one ticket.

Drive TDD at pre-agreed seams — the seams the spec named. Not every line needs a test, but every behavior boundary does. If a test is hard to write, that's a signal the interface is wrong — fix the interface, don't weaken the test.

If the code doesn't work, use the "debugger" agent (systematic 4-phase root-cause: understand → reproduce → isolate → fix). Never fix symptoms.

## Phase 5 — Code review (two-axis: standards + spec)

Use the subagent tool with the "reviewer" agent, passing the implementation via `{previous}`.

Run a two-axis review of the diff since the branch point:
- **Standards axis**: does it follow the repo's coding standards (AGENTS.md doctrine)? Run a Fowler smell baseline — long methods, feature envy, primitive obsession, divergent change.
- **Spec axis**: does it faithfully implement the originating spec? Every acceptance criterion from Phase 2 — is it met, partially met, or missing?

Run the two axes as parallel subagent calls (use the `subagent` tool with `parallel: true` if available, or two sequential calls) so neither pollutes the other. The reviewer must cite `file:line` for every finding.

If the review finds blocking issues, loop back to Phase 4 for the specific ticket. Do NOT merge with blocking findings.

## Phase 6 — Improve codebase architecture (deepen the modules)

Use the subagent tool with the "reviewer" agent, passing the review output via `{previous}`.

Scan the codebase touched by this change for deepening opportunities (John Ousterhout, "A Philosophy of Software Design"):
- Are the modules deep (lot of behavior, small interface)? Or shallow (interface as big as implementation)?
- Is there a clean seam, or is the module welded to its callers?
- Can the interface be smaller without losing behavior?

Present the top 3 deepening opportunities. Grill the user on which one to pick (one question, not a full grilling). Implement the chosen deepening as a separate `vcs_atomic_commit` — architecture changes are their own logical unit, never mixed with feature work.

## Phase 7 — Ship

When all phases are green:
1. Call `openspec_verify` then `openspec_sync`.
2. Run the full project verification gate one final time.
3. Call `vcs_pr` to open a PR with the spec + tickets + implementation + review + architecture-deepening as discrete commits.
4. Call `memory_add` to capture durable facts from this workflow (domain vocabulary, architecture decisions, gotchas) so the next session starts with the context.

## When to use this workflow

- Any feature touching >1 module
- Any change where the user isn't 100% sure what they want (the grilling resolves it)
- Any change where feedback loops (tests, types, previews) exist or should exist
- Any change where the codebase architecture matters (not a one-line typo fix)

## When NOT to use this workflow

- Trivial changes (typo, one-line fix) — use `/implement` directly
- Pure research / exploration — use the "scout" agent or `/scout-and-plan`
- Read-only audits — use `/ultra-code-review`

Reference: mattpocock/skills (github.com/mattpocock/skills, MIT, 179k stars). Adapted to dotz's native tool surface (subagent chain, openspec_*, vcs_*, living_docs_*, sandbox_run, memory_*).