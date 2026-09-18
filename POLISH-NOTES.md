# dotz — polish notes (2026-09-18, Linux host)

## What changed and why

- README badges: removed the live-fact shields.io badges (CI status, release version,
  download count). All three point at `https://github.com/cayleb-james2008/dotz`, which
  returned HTTP 404 from an unauthenticated `curl` check on 2026-09-18, so they would
  render as error badges. Kept static badges only (license, platform, built-with, Rust
  edition) and added a repo-internal release statement pointing at
  `.github/workflows/release.yml`.
- README "Cross-device Auto-update": scoped the self-update claim. The updater wiring
  exists in code (`tauri-plugin-updater` in `src-tauri/Cargo.toml`, endpoint + pubkey in
  `src-tauri/tauri.conf.json`), but the release-feed URL 404s, so no feed could be
  verified. The feed goes live with the first `v*` release.
- README sandbox claims (Highlights + Safety Patterns): removed the false
  "allowlist-only execution model / Bash tools only run allowlisted commands" statements.
  The `bash` tool (`dotz-core/src/agent/tools.rs`, `BashTool::execute`) runs the given
  command via `sh -c` in the session cwd with a wall-clock timeout — no command
  allowlist. Allowlists exist at the boundaries (profile tool gating via `PLAN_TOOLS`,
  in-app browser origin allowlist in `dotz-core/src/browser.rs`, loopback Origin/Host
  guard returning 403 in `dotz-core/src/server/guard.rs`), and the README now says that
  with file references.
- README: added "What works today" section (verified-in-code list with file references,
  verified-by-running results, not-run list with reasons).
- Added `SECURITY.md` (was missing): supported versions (latest 0.2.x only), private
  reporting via GitHub Security Advisory link (no invented email), response expectations,
  scope notes pointing at the real secret paths and trust boundaries.
- Code fix: `dotz-core/src/sandbox.rs` test helpers (2 sites) called
  `child.id().expect(...)`. `std::process::Child::id()` returns plain `u32` on every
  platform (`Option<u32>` is the tokio Child API); the `#[cfg(not(windows))]` branch did
  not compile on Linux/macOS, so the documented `cargo test -p dotz-core` gate and the
  CI ubuntu/macos matrix jobs could not build. Replaced with `let pid = child.id();`
  (comment explains the std/tokio mix-up). No test semantics changed, no assertion
  touched, no check weakened.
- Deliberately kept: `.pi/` (154 design-system dirs / 150 with DESIGN.md, 160
  design-skill dirs / 156 with SKILL.md) — read at runtime by `dotz-core/src/design.rs`
  and `dotz-core/src/skills.rs` via `<cwd>/.pi` (`DOTZ_PI` override); moving it would
  break the app. `.claude/` (7 files: `launch.json` + ponytail skills) — referenced by
  `AGENTS.md` as the code-style guide; the app's `~/.claude/skills` scan root is the home
  dir, not this folder, so it is docs-only and harmless.

## Verified by running (exact command + result)

- `curl -sS -o /dev/null -w '%{http_code}' https://github.com/cayleb-james2008/dotz` → `404`
  (same for `/releases/latest`). Unauthenticated check from this machine, 2026-09-18.
- `python3 -c "import yaml; ..."` on `.github/workflows/ci.yml` and `release.yml` → both
  valid YAML; every referenced path exists (`scripts/fetch-embed-model.mjs`,
  `docs/perf-baseline.json` with `cold_start_ms: 526` measured 2026-07-20,
  `dotz-core/benches/cold_start.rs` for `cargo bench --bench cold_start`).
- `flock /tmp/oss-showcase-heavy.lock cargo fmt --all -- --check` → exit 0 (before and
  after the sandbox.rs fix).
- `flock /tmp/oss-showcase-heavy.lock cargo test -p dotz-core` (all unpiped, EXIT is real) →
  BEFORE the fix: EXIT=101, `error[E0599]: no method named 'expect' found for type 'u32'`
  at `dotz-core/src/sandbox.rs:2232` and `:2499` (log: `cargo-test.log`).
  AFTER the fix, model absent: EXIT=101, 786 passed / 27 failed (log: `cargo-test2.log`).
  The failures were dominated by the missing ONNX model (`assets/models/` gitignored).
- `npm install --ignore-scripts` → EXIT=0 (51 packages; plain `npm install` fails on
  `sharp@0.34.5` node-gyp build: `npm error sharp: Please add node-addon-api to your
  dependencies`, Node v26.7.0 — log: `fetch-model.log`). `--ignore-scripts` skips the
  native build; the model fetcher does not need it. Then `npm run fetch-model` → EXIT=0,
  `Xenova/all-MiniLM-L6-v2 bundled (dim 384)` (log: `fetch-model2.log`).
- AFTER the fix, model present: EXIT=101, 796 passed / 17 failed (log: `cargo-test3.log`).
  All `embed::*` / `memory::*` model tests pass with the model.
- Single-threaded rerun `cargo test -p dotz-core --lib -- --test-threads=1` → EXIT=101,
  803 passed / 10 failed (log: `cargo-test4.log`). The 5 `server::*` + 1
  `agent::provider::*` failures from the parallel run pass single-threaded — they race
  on shared global config/HOME state, not on code. Remaining 10, by cause:
  - 7 `marketplace::*`: `minisign CLI required for signature verification` — the
    `minisign` binary is not installed on this host (environment).
  - `agent::tools::grep_respects_file_budget_and_terminates_cleanly`: assumes directory
    walk order (`z_match.txt` found despite budget=1) — fails on this filesystem.
  - `agent::tools::run_bash_reaps_child_after_timeout`: `test should have captured a
    valid child pid` — fails in this environment, cause not diagnosed.
  - `sandbox::backend_posix_impl_sets_process_group`: `got pgid -1` — `getpgid` fails
    in this environment.
  None of the 10 were edited, weakened, or skipped. The suite is expected to go further
  on a machine with the `minisign` CLI and on Windows (the platform target).
- `cargo fmt --all -- --check` → exit 0 (before and after the fix).
- `cargo clippy -p dotz-core --all-targets -- -D warnings` → NOT RUN (time/lock budget).
- Stray zero-byte `dotz-core/nul` (created by a hooks test redirecting to `> nul`, the
  Windows null device) deleted; `**/nul` added to `.gitignore` with a comment so the
  tree stays clean after test runs. `node_modules/` and `assets/models/` (both
  gitignored) left on disk for future runs, never committed.

## Remains untested and why

- `cargo clippy -p dotz-core --all-targets -- -D warnings` — NOT RUN (time/lock budget).
- `e2e_live_prompt` and other live-provider paths — NOT RUN (need API keys).
- `cargo bench --bench cold_start` — NOT RUN here (Windows-only CI gate).
- Desktop shell + installer (`cargo tauri dev`/`build`, NSIS) — needs Windows.
- GPU/DirectML path for `ort` — needs Windows; CPU only here.
- Live-provider tests (`DOTZ_E2E_LIVE=1`) — need API keys.
- `cargo bench --bench cold_start` — Windows-only CI gate.

## Claims removed or softened

- Removed: CI-status / release-version / download-count badges (unverifiable, 404).
- Removed: "Bash tools only run allowlisted commands — no arbitrary shell execution"
  (false; refuted by reading `BashTool::execute`).
- Softened: "deterministic tool sandbox" → "managed tool sandbox" with the real run
  lifecycle; "signed, self-updating Windows app" kept for the mechanism, qualified with
  the unverified feed; "platform-Windows" badge → "Windows-first" (CI also targets
  ubuntu/macos; core is cross-platform, shell/installer/GPU paths are Windows-first).
- Not changed: version skew `tauri.conf.json 0.2.8` vs workspace `0.2.0` — left alone
  (bumping could break the signing flow); flagged for the author.
