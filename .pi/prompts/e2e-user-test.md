---
name: e2e-user-test
description: Drive THIS project's app end-to-end the way a real user would — launch it, click/type through its real UI (or run its real CLI), watch for errors and rough edges, and report what a user would actually hit
---
You are running a dotz E2E USER-STYLE TEST: exercise the project's app exactly as a human user would, in its real running form — NOT by reading code or writing unit tests. The goal is to find what a user actually experiences: crashes, broken flows, dead buttons, errors, and ugly/confusing UI.

## 1. Figure out how a user runs this app (scout fan-out)
Dispatch `scout` subagents in PARALLEL (the `subagent` tool with `tasks: [...]`) to map:
- How the app starts: `package.json` scripts (`dev`/`start`), a server entry + port, a CLI binary, an Electron/desktop launch, or a built artifact.
- What KIND of surface it is: web app (HTTP + UI), HTTP API, CLI, TUI, or desktop app.
- The 3-7 primary user flows worth testing (the things a user does most).
- Any README "Run" / "Usage" / "Getting started" section.

Let each scout run on the default subagent model — do NOT pass a `model` override.

## 2. Launch it for real (sandbox)
Use the `sandbox` tool to actually run the app:
- Web app / HTTP API → start it in **`web` mode** (long-lived process bound to a local port).
- CLI → run it in **`terminal` mode** with real arguments.
Wait until it is actually up (poll the port, or watch for the ready log). A crash on launch is the single worst user bug — capture it verbatim and report it.

## 3. Drive it as a user
**Web app / anything with a URL** — use the in-app browser tools, which render the real page:
- `browser_start` at the app's URL.
- Walk each primary flow with `browser_act` (navigate / click an `@ref` / type / select / scroll), reading the returned snapshot + screenshot after each step to confirm what the user actually sees.
- Cover the obvious happy paths AND a few edge paths (empty input, an invalid value, a back-navigation, a double-click).

**CLI / API** — run the real commands or hit the real endpoints (happy path + a bad-input path) and assert on stdout / status / exit code.

For every step, note: did it work? was it clear? any console or network errors? any visual breakage?

## 4. Watch for what a user would hit (the whole point)
Record each problem WITH the exact step to reproduce it:
- Errors / crashes / blank screens / dead controls / clicks that do nothing.
- Confusing or ugly UI: misalignment, unreadable or low-contrast text, missing feedback, no loading / empty / error states.
- Behavior that contradicts what the UI implies.
- Steps slow enough that a user would notice.
Capture a screenshot as evidence for every visual finding.

## 5. Report
```
## E2E User-Style Test — <project>
Surface: <web app | API | CLI | desktop>   Launched via: <how>   Flows tested: <N>

### Blocking (a user cannot get through this)
1. [where] <what happened> — repro: <exact steps> — evidence: <screenshot / console line>

### Rough edges (works, but a user would notice)
1. ...

### Worked cleanly
- ...
```
Severity order: blocking > broken-but-recoverable > visual/UX > nit. Cite the exact repro for EVERY finding — never report a problem you did not actually trigger. If the app would not launch at all, say so plainly and stop: that is the finding.

If the user passed `--fix`, dispatch a `worker` subagent per blocking finding (strict scope: only the implicated files), then re-launch and re-run the affected flow to confirm the fix. Max 2 fix rounds.
