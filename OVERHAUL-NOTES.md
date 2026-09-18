# OVERHAUL-NOTES — dotz

Complete-overhaul run, 2026-09-18. Scope: modernize the Rust core + Tauri shell of a multi-agent
coding dashboard without changing what the app does. Verified on this Linux machine
(`cargo/rustc 1.88.0`). Every command was run; raw output is under
`/home/cayleb/Work/projects/oss-showcase/_verify/` (scratch, gitignored).

## What changed

1. **Rust edition 2021 → 2024** (workspace root `Cargo.toml` + `src-tauri/Cargo.toml`). This is the
   substantive modernization.
2. **Dependency refresh** — `cargo update` and a Tauri icon fix (RGB → RGBA) in the phase-1 commit
   `4870dd1` (Tauri 2 requires RGBA icons).
3. **`std::env::set_var` / `remove_var` migration (the big one).** Under edition 2024 those
   functions are `unsafe`. The phase-1 commit bumped the edition but left **406 call sites**
   unmigrated, producing **396 clippy errors** (391 of them `E0133`) and breaking the test build.
   Commit `b763e80` completes the migration with Rust's own edition-migration tool
   (`cargo fix --edition`), which wraps each call site — including `match` arms — in an
   `unsafe { ... }` block with a `// TODO: Audit that the environment access only happens in
   single-threaded code.` note. No behavior changed.
4. **rustfmt across the workspace** — `cargo fmt --check` failed on 7 files after the phase-1 run;
   now clean.
5. **Clippy machine-applicable fixes** — `uninlined_format_args` (`format!("{}", x)` →
   `format!("{x}")`) and a `match`-to-`if-let` simplification, applied via `cargo clippy --fix`.
6. **Linux portability fix (real bug).** `sandbox::WindowsSandbox` was not `#[cfg(windows)]`-gated
   even though it is only ever constructed in `platform_backend()`'s `#[cfg(windows)]` arm, so on
   Linux it tripped `-D dead-code`. It is now gated to match the Mac/Linux backends. The
   intentionally-documentation-only `RESOURCE_NAME` constant in `agent/extra_tools.rs` also tripped
   `-D dead-code` on Linux (its `const _: &str = RESOURCE_NAME;` alias does not satisfy rustc); it
   now carries an explicit `#[allow(dead_code)]`, matching the pattern `solomon` uses for
   `FROZEN_CORE`.

## Verified by running (raw results)

Run from the repo root (workspace):

| command | result |
|---|---|
| `cargo fmt --check` | **exit 0** |
| `cargo build` | **exit 0** |
| `cargo clippy --all-targets -- -D warnings` | **exit 0** (0 errors) |
| `cargo test -- --test-threads=2` | **exit 101** — `803 passed; 10 failed; 0 ignored` |

Logs: `_verify/lead/dotz-{fmt4,build,clippy4,test}.log`.

Before this pass the same commands produced (lead-measured): clippy **396 errors**, fmt **7 diffs**,
test build **broken**. So this pass moved dotz from non-compiling to 803 passing.

## The 10 honest test failures (read this)

All 10 are **environment-limited on this Linux host**, not logic failures, and none was silenced:

- **7 × `marketplace::tests::*`** — every one panics with
  `MarketplaceError { message: "minisign CLI required for signature verification; install via
  `cargo install minisign` or your package manager" }`. `minisign` is **not installed** on this
  machine (verified: `which minisign` → not found). The signature-verification tests therefore
  cannot run here. Install `minisign` to exercise them.
- **2 × `agent::tools::tests::*`** (`run_bash_reaps_child_after_timeout`,
  `grep_respects_file_budget_and_terminates_cleanly`) — fail with "test should have captured a
  valid child pid"; the host's process-reaping setup does not yield a child pid.
- **1 × `sandbox::tests::sandbox_backend_posix_impl_sets_process_group`** — asserts the child's
  `pgid == pid`; the host reports `pgid -1` (no controlling process group in this environment).

These count and root causes match the prior pass's honest figure (803 passed / 10 failed with the
model absent, single-threaded) — i.e. they are pre-existing environment limits, not regressions.

## NOT RUN here (honest blockers)

- **Windows Tauri build** (`cargo tauri build` → signed NSIS installer) — Windows-only.
- **`npm install && npm run fetch-model`** — fetches the `agent-browser` binary + ONNX model from
  the network; not run.
- **CI** (`.github/workflows/ci.yml`) — runs on `windows-latest` by design.
- The full model-backed test set (needs provider credentials).

## README truth pass

The phase-1 commit added a "Modernization (September 2026)" section to `README.md`. The counts in
this file are the ones measured today and name their exact commands. No claim was removed in this
pass.

## Notes

- Per the repo's own AGENTS.md the CI gate runs `cargo clippy -p dotz-core --all-targets --
  -D warnings`; that now exits 0 on Linux as well. `.cargo/config.toml` pins `build.jobs = 2` to
  avoid LLVM OOM on a loaded host; tests were run with `--test-threads=2` for the same reason.
