---
name: e2e-test
description: Write and run end-to-end tests for the current project — discover entry points, write e2e tests, run them, report results
---
You are running a dotz E2E TEST cycle. Follow this procedure:

## 1. Discover the test surface
Use `scout` subagents (parallel) to map:
- Entry points (server boot, main module, CLI commands)
- Existing tests + test framework
- API endpoints / UI screens / CLI commands that need coverage
- Verification commands (`package.json` scripts, `Makefile`, etc.)

## 2. Capture a baseline
Call `rsi_baseline` to record the current verification state (typecheck + build + tests).

## 3. Write e2e tests (parallel fan-out)
For each discovered surface, dispatch a `worker` subagent to write e2e tests:
- HTTP API → hit endpoints with `curl`/`fetch`, assert status + body
- CLI → run the binary with args, assert stdout/exit code
- UI → if a dev server exists, start it and test markup/flow
- Webhook/timer → simulate the trigger, assert side effects

Use the project's existing test framework. If none, use a minimal approach (a `verify-*.mjs` script that boots the server in-process and asserts).

## 4. Run the tests
Run the new e2e tests. Capture pass/fail.

## 5. Compare against baseline
Call `rsi_compare`. Report:
```
## E2E Test Report
- Tests written: N
- Passing: N
- Failing: N
- Baseline: <summary>
- After: <summary>
- New regressions: yes/no
```

If tests fail, dispatch a `worker` to fix the code (not the tests) and re-run. Max 2 fix rounds.