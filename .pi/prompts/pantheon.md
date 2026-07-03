---
name: pantheon
description: New Model, New Project — scaffold and build a fresh episode: prove you can ship to GitHub, scaffold via the pantheon generator, then build the app to the ship checklist
---
You are driving a **New Model, New Project** recorded episode. The series hub is
`C:\Users\Cayleb\Desktop\workspace\projects\pantheon` — its `README.md` episode table + codename
pool are the source of truth, and `scripts\new-episode.ps1` is the scaffolding engine (do not
reimplement it). Environment: Windows 11, use `powershell` (PS7/`pwsh` is not installed), git-bash
also available; `git`, `gh` (authed as `cayleb-james2008`), `node`, `npm`, `uv`, `cargo` (+`cargo
tauri`), `ollama` on PATH.

`$@` is the **episode idea** (one sentence). If it's empty, ask me for the idea before doing anything.

Follow these steps in order; STOP and tell me if any gate fails.

## 1. Pick the episode number + codename (from the hub, the source of truth)
- Read `pantheon\README.md`. The next **episode number** = (highest `Ep` in the table) + 1, or **1**
  if the table has no episode rows. Never reuse or skip a number.
- Pick the next **unused** codename from the `## Codenames` **Pool** line (mythological,
  lowercase-kebab, one per episode, never reused).
- Derive a short lowercase-kebab **slug** from the idea, and choose an unused **dev port** (8090+).
- Confirm the number / codename / slug / model / port with me in one line before proceeding.

## 2. Prove you can ship BEFORE building
- Confirm which model you are running as and run `gh auth status`. If either fails, STOP and tell me.
- Smoke test: `gh repo create np-smoke-<random> --public --add-readme`, clone it, add
  `hello.txt`, commit `chore: smoke`, push, confirm the push, then
  `gh repo delete <name> --yes` (if that errors about a missing `delete_repo` scope, note it and
  move on — don't stop). If **create or push** fails, STOP with the exact error.

## 3. Scaffold the episode
Run (fill the values from step 1):
```
powershell -ExecutionPolicy Bypass -File pantheon\scripts\new-episode.ps1 `
  -Codename <codename> -Episode <N> -Model "<model>" -Serve "<serve>" `
  -Slug "<slug>" -Idea "<idea>" -DevPort <port> -UpdateHub
```
This copies `template\`, fills tokens, `git init`s with a repo-local identity, tags `episode/N`,
`gh repo create --public --push`, and appends the episode's row to the hub index. It creates a
sibling folder `day<N>-<model>-<codename>\`.

## 4. Build the app to the ship checklist
`cd` into `day<N>-<model>-<codename>\`, read its `AGENTS.md`, and build to that file's definition of
done: **GitHub-only** (clone-and-run; static site or desktop release — no hosted server, no managed
DB), keep CI green, follow **ponytail** (laziest solution that works; stdlib before deps; shortest
diff), commit with `rsi:` / `fix(scope):`.

## 5. Report
When the ship checklist passes, report the episode repo URL.
