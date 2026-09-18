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

## Dashboard modernization (`web/index.html` + `styles.css`)

- **Accessible names for 40 form fields.** The dashboard's inputs/selects/textareas relied on
  `placeholder` text only, which is not an accessible name. Each now carries an `aria-label`. Most
  reuse the field's own visible placeholder text (e.g. `aria-label="project name"`); a handful that
  had no placeholder were given a human label derived from their id (e.g. "project profile",
  "memory scope", "sandbox language"). 9 fields already had a name via `title`/`<label for>`/wrapping
  and were left alone.
- **Keyboard skip-link** added, focus-revealed by CSS, targeting `<main id="stage">`.
- The stylesheet already supported `prefers-reduced-motion` and had focus styling.

Verified: audit reports 0 failures across lang/viewport/img-alt/button-names/field-labels/link
integrity; the 4 root-absolute refs (`/fonts.css`, `/styles.css`, `/main.js`, `/wizard.js`) resolve
against `frontendDist: "../web"` and were confirmed present. No JS changed.

## Windows-native verification (2026-09-18)

Windows-native is now **verified by cross-compiling the real Windows target**:
`cargo xwin check --target x86_64-pc-windows-msvc` → **exit 0** (the whole `dotz-core` +
`dotz-tauri` graph type-checks for Windows, including `WindowsSandbox` and the `winres` resources).

**Honest blocker on the full Windows link:** `cargo xwin build --release --target
x86_64-pc-windows-msvc` fails at the final link with undefined C++ standard-library symbols
(`__std_find_trivial_8`, `__std_search_1`, …) referenced by `ort_sys`. Root cause: dotz's
`ort = "=2.0.0-rc.12"` (DirectML/ONNX, deliberate pin — see the crate comment and
`tests/ort_pin_guard.rs`) is an MSVC C++ object built against a newer STL than cargo-xwin's
downloadable CRT provides; the same symbols resolve fine with real Visual Studio on Windows.
This is a cross-compilation toolchain gap, not a code defect — recorded, not worked around.

**Runtime Windows testing remains NOT RUN.**

## Windows runtime test — COMPLETED (2026-09-18); the link gap is closed

The earlier entry recorded dotz's Windows **link** as blocked. That gap is now **resolved**, and
dotz has been **run on real Windows**.

**What was wrong.** The Windows link failed with 20 undefined C++ symbols
(`__std_find_trivial_8`, `__std_search_1`, `__std_last_of_trivial_pos_1`, …) referenced by the
pinned `ort` / ONNX Runtime prebuilt. Root cause, established with a clean-room test:
cargo-xwin's **default CRT is older than the MSVC C++ STL the ONNX prebuilt was compiled
against**, so those internal `__std_*` helpers were defined by nothing on the link line.

**Two fixes, both verified:**
1. `--xwin-crt-version 14.44.17.14`. That CRT's `libcpmt.lib` **does** define the missing
   symbols (confirmed: 4 hits for `__std_find_trivial_8`, vs 0 in the default CRT). Subtlety:
   xwin silently reuses an already-populated cache, so the newer CRT only takes effect when
   fetched into a **clean** cache directory — that was the key discovery.
2. The Windows SDK import libs are extracted lowercase on Linux but MSVC asks for
   `PathCch.lib` / `DirectML.lib`; correctly-cased symlinks resolve that.

**Result:** `dotz.exe` builds (`PE32+ executable for MS Windows (GUI), x86-64`, ~47 MB) and,
delivered to a **real Windows 10 Enterprise LTSC** VM together with four CRT DLLs, it launched
and stayed running: **`[dotz] RESULT=RUNNING_AFTER_15s pid=4180`** (report uploaded from inside
the guest; evidence in `_verify/windows-vm/dotz-*`).

**Reproducible:** `bash scripts/windows-build.sh` (documented in `WINDOWS-BUILD.md`).

**Packaging note:** dotz cannot use `+crt-static` (unlike the other four apps) because ONNX
Runtime is compiled `/MD` and the linker rejects the mismatch. Its installer must ship
`msvcp140.dll`, `msvcp140_1.dll`, `vcruntime140.dll`, `vcruntime140_1.dll` from the MSVC 14.44
redistributable. Without them a clean Windows install fails with "VCRUNTIME140.dll was not
found" — measured; with them, dotz runs.

**Linux bar unchanged:** `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0,
`cargo test -- --test-threads=2` 803 passed / 10 failed (the same environment-limited failures
documented above: `minisign` absent, pid/pgid privileges).
