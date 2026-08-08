# Contributing to dotz

Thank you for your interest in contributing to dotz! dotz is an open-source, MIT-licensed
in-process multi-agent coding dashboard built in Rust.

## Getting Started

1. **Fork** the repository and clone your fork.
2. **Install prerequisites**:
   - [Rust](https://rustup.rs/) (stable, edition 2021)
   - [Node.js](https://nodejs.org/) (for the agent-browser binary + ONNX model fetch)
   - [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) (WebView2 on Windows)
3. **Set up the workspace**:
   ```bash
   npm install
   npm run fetch-model    # downloads the all-MiniLM-L6-v2 ONNX model
   ```
4. **Verify the build**:
   ```bash
   cargo build -p dotz-core
   cargo run -p dotz-core --bin serve    # headless backend on http://127.0.0.1:4317
   ```

## Development Workflow

### Run in development

```bash
# Headless backend (browser dev loop)
cargo run -p dotz-core --bin serve

# Native desktop window (Tauri / WebView2)
cargo tauri dev
```

### Before you push — run the gates

CI runs these three commands on every push and PR. Run them locally before pushing:

```bash
cargo fmt --all -- --check
cargo clippy -p dotz-core --all-targets -- -D warnings
cargo test -p dotz-core
```

> **LLVM OOM note:** On memory-constrained hosts, bound parallelism:
> `cargo test -p dotz-core -- --test-threads=2`. If a build dies with
> `STATUS_STACK_BUFFER_OVERRUN` or exit 1455, re-run once before treating the
> gate as red.

### Tests

Integration tests live under `dotz-core/tests/`:

| Test file | What it guards |
|-----------|---------------|
| `e2e.rs` | Backend + static-frontend e2e: spawns the real `serve` binary, exercises REST + WebSocket. |
| `windowless_guard.rs` | Every `Command::new` spawn in shipped code must be windowless on Windows. |
| `ort_pin_guard.rs` | The `ort` dependency must stay exact-pinned until a stable 2.x exists. |

The embed tests need the bundled all-MiniLM-L6-v2 model files (`npm run fetch-model` first).

## Code Style — Ponytail

Follow the ponytail principle:

- **YAGNI** — don't build what you don't need yet.
- **Stdlib first** — native platform features before dependencies.
- **One line over fifty** — prefer a single clear line to clever cleverness.
- **Shortest working diff** — deletion over addition.
- **Never simplify away** input validation at trust boundaries, error/data-loss handling, security,
  accessibility, or tests.
- Mark a deliberate shortcut with a `# ponytail:` comment naming its ceiling and the upgrade path.

## Architecture Quick Reference

| Component | Path | Role |
|-----------|------|------|
| `dotz-core` | `dotz-core/` | Pure library + headless bins: agent runtime, providers, memory, workflow DAG, sandbox, browser, server. |
| `src-tauri` | `src-tauri/` | Thin Tauri 2 shell: boots core, opens WebView2 window, wires updater. |
| `web/` | `web/` | Vanilla HTML/CSS/JS bento dashboard — no build step, no framework. |
| `.pi/` | `.pi/` | Bundled agent resources: profiles, prompts, design systems, design skills. |

Key conventions:

- The workflow graph is the **single source of truth** — execution and observability read the same
  `WorkflowStep` nodes. Keep `subagent.rs` the sole emitter of `step_tool` + `step_thinking` events.
- `sandbox.rs` owns process lifecycle for both `terminal` and `web` runs — never spawn sandbox
  processes directly from `server`.
- `memory.rs` is the on-device memory store. `MEMORY.md` is the git-committable source of truth;
  the sqlite vector index is a derived cache — don't hand-edit it.
- `skills.rs` is the single skill-discovery path — don't add a second skill loader.
- **Don't reintroduce the Electron/TypeScript/pi-SDK/mem0 stack** — the native-Rust workspace is
  the product.
- Frontend panels use class-scoped selectors or per-panel `querySelector` lookups (panel templates
  are cloned, so avoid global `getElementById` for panel internals).

## Opening a Pull Request

1. Create a branch from `main` with a descriptive name.
2. Make your changes — keep diffs minimal.
3. Run the gates (fmt, clippy, test).
4. Write a clear PR description: what changed, why, and any testing notes.
5. Reference any related issues.

## Reporting Issues

Use the [GitHub issue templates](.github/ISSUE_TEMPLATE/) to report bugs or request features.
Include as much context as possible: OS, Rust version, steps to reproduce, expected vs. actual
behavior.

## Community Standards

All participants in the dotz community are expected to follow our
[Code of Conduct](CODE_OF_CONDUCT.md). Be respectful, constructive, and inclusive in all
interactions — issues, PRs, and discussions.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).