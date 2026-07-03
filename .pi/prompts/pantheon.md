---
name: pantheon
description: New Model, New Project — scaffold and build a fresh episode as a required capability spine, each phase its own subagent node, shipped to GitHub with an icon and a beautified repo
---
You are driving a **New Model, New Project** recorded episode. The series hub is
`C:\Users\Cayleb\Desktop\workspace\projects\pantheon` — its `README.md` episode table + codename
pool are the source of truth, and `scripts\new-episode.ps1` is the scaffolding engine (do not
reimplement it). Environment: Windows 11, use `powershell` (PS7/`pwsh` is not installed), git-bash
also available; `git`, `gh` (authed as `cayleb-james2008`), `node`, `npm`, `uv`, `cargo` (+`cargo
tauri`), `ollama` on PATH.

`$@` is the **episode idea** (one sentence). If it's empty, ask me for the idea before doing anything.

**You are the ORCHESTRATOR.** Every phase below is its OWN named `subagent` so it shows as a distinct
node in the live workflow graph, and each subagent's tool calls stream onto that node as sub-nodes.
Building a phase inline (no subagent) is a FAILED build even if the code works. **Every phase is
REQUIRED**; skip one only when genuinely impossible, with an explicit logged line
`skipped <phase>: <reason>` (e.g. browser-verify for a headless library). Give EVERY subagent
`cwd: "<codename>"` — a relative cwd resolves against the session dir `pantheon\projects`, so
`"<codename>"` puts the subagent inside `pantheon\projects\<codename>` (the EPISODE repo, not the hub).
STOP and tell me if any gate fails.

## 0. Recall, pick the episode, prove you can ship
- `scout` subagent: `memory_search` prior episodes/conventions. Read `pantheon\README.md`: next
  **episode N** = (highest `Ep` in the table) + 1, or **1** if none. Pick the next **unused**
  mythological **codename** (Pool line, lowercase-kebab, never reused), a kebab **slug**, and an
  unused **dev port** (8090+). Read your executive model from `bash`: `cat ~/.dotz/config.json`
  → `executiveModel` (that is YOU; `DOTZ_SUBAGENT_MODEL` is the workers'). Confirm
  number / codename / slug / model / port with me in one line.
- Confirm `gh auth status` works. Smoke-test shipping: `gh repo create np-smoke-<random> --public
  --add-readme`, clone it, add `hello.txt`, commit, push, confirm, then `gh repo delete <name> --yes`
  (if that errors about a missing `delete_repo` scope, note it and move on). If **create or push**
  fails, STOP with the exact error.

## 1. Scaffold
```
powershell -ExecutionPolicy Bypass -File pantheon\scripts\new-episode.ps1 `
  -Codename <codename> -Episode <N> -Model "<exec-model>" -Serve "Ollama Cloud" `
  -Slug "<slug>" -Idea "<idea>" -DevPort <port> -UpdateHub
```
It copies `template\`, fills tokens (incl. the committed `.github/workflows/ci.yml`), `git init`s,
tags `episode/N`, `gh repo create --public --push`, and appends the episode row to the hub. It prints
the full path of `pantheon\projects\<codename>\`.

## 2. Build to the ship checklist — the capability spine (one subagent per phase)
`cd` into `pantheon\projects\<codename>\`, read its `AGENTS.md`, then dispatch, in order:

1. **DESIGN** — `ui-ux-pro`: `design_use` to pick + load an Open Design system and honor its tokens.
   MUST produce an **app icon** (favicon.svg/.ico for web/SPA, or an app/desktop icon) committed to
   the repo, wired into the app, and embedded in the README.
2. **SPEC** — `spec-owner`: `openspec_propose` the change mapped to the build units, `openspec_verify`.
3. **SKILLS** — `skill-agent-builder`: create a reusable skill/agent ONLY for a real recurring gap;
   else log `skipped skills: no capability gap`.
4. **BUILD** — fan out one `worker` per independent unit (each crate/module, frontend, CI, tests) in
   PARALLEL, each `cwd: "<codename>"` with the file paths it owns. You conduct; you do NOT write large
   source files inline. Replace the generic CI with real build/test for the stack. Follow **ponytail**
   (laziest solution that works; stdlib before deps; shortest diff). Integrate + gate results.
5. **SANDBOX-VERIFY** — `sandbox-runner`: `sandbox_run` (terminal) the REAL build/test in the episode
   cwd; failures are required work (hand back, re-run until green). This local green is the
   **authoritative** ship gate.
6. **E2E & BUG-BOUNTY** — `browser-operator`: `sandbox_run` (mode "web") to launch the app, capture its
   local url/port, then `browser_start`/`browser_act` to drive the REAL FRONTEND (click/scroll/type/
   screenshot), loading `@e2e-test` + `@bug-bounty`. Bugs are required work — fix + re-verify before
   done. (Skip only for a headless CLI/library, logged.)
7. **DOCS & BEAUTIFY** — `docs-maintainer`: `living_docs_update` + `agents_md`, and beautify the repo
   README (title, one-line description, CI + license badges, screenshot, clone-and-run, icon embedded).

## 3. Ship (honest CI — never fake or wait forever)
`platform-operator`: `vcs_atomic_commit` (`rsi:` / `fix(scope):`), push to origin, then set metadata
via `bash`: `gh repo edit <owner>/<codename> --description "<one-liner>" --add-topic <slug> --add-topic
<lang>`. Then verify CI with a BOUNDED check: `gh run list -R <owner>/<codename> --limit 1` up to 3
times (~10s apart). If a run appears → `gh run watch -R <owner>/<codename> --exit-status` and keep it
green. If none appears → `gh api repos/<owner>/<codename>/actions/permissions --jq .enabled`; if Actions
is disabled, log EXACTLY `skipped CI: Actions disabled at account level` and continue on the
authoritative local + E2E gate. NEVER invent "propagation delay", NEVER push empty commits to
retrigger, NEVER claim CI is green when it is not. Confirm the live URL + a fresh-clone quickstart.

## 4. Self-improve + score
`self-improvement-reviewer`: `rsi_baseline`/`rsi_compare` the gate. Then spawn a scoring `subagent`
(cwd `<codename>`) with a FIXED judge model (not your own; same judge every episode) to rate
`difficulty` (1-5) and `quality` (0-100) with a one-line note. Then:
```
powershell -ExecutionPolicy Bypass -File C:\Users\Cayleb\Desktop\workspace\projects\pantheon\scripts\score-episode.ps1 `
  -Codename <codename> -Difficulty <d> -Quality <q> -JudgeNote "<note>"
```
It auto-harvests build time / CI / commits and refreshes `pantheon\leaderboard.html`.

## Notes
- AUTH: every subagent runs on the Ollama Cloud sub-model — `OLLAMA_API_KEY` must be set (an empty key
  401s; the provider fails fast with a missing-key error). A long build phase can exceed the 5-min
  subagent timeout — set `DOTZ_SUBAGENT_TIMEOUT_MS` higher (e.g. 900000) before dispatching workers.
- When the ship checklist passes, report the episode repo URL and the new leaderboard standing.
