# dotz — build the exe + cross-device auto-update

The dotz Rust/Tauri app ships as a single signed NSIS installer (`dotz_<ver>_x64-setup.exe`) that
self-updates across devices via `tauri-plugin-updater`. The source repo stays **private** — only the
release *assets* are public, which is all the updater needs.

## How auto-update works (Solomon pattern)
- `tauri.conf.json → plugins.updater.endpoints` points at
  `https://github.com/cayleb-james2008/dotz/releases/latest/download/latest.json`.
- GitHub Releases **assets are downloadable even when the repo is private** → any device fetches
  `latest.json` + the installer with no auth/credentials.
- `latest.json` is signed (minisign); the client verifies it against `plugins.updater.pubkey` before
  installing. Tampered/unsigned updates are rejected.

## One-time setup
1. **Signing key** — already generated at `~/.claude/dotz-rust/dotz-updater.key` (+ `.password`).
   Public key is already in `tauri.conf.json`. Add two repo secrets (Settings → Secrets → Actions):
   - `TAURI_SIGNING_PRIVATE_KEY` = the contents of `dotz-updater.key`
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = the password in `dotz-updater.key.password`
   (Keep the private key secret; never commit it.)
2. `.github/workflows/release.yml` is in place (uses `tauri-apps/tauri-action`).

## Cut a release (any version bump → all devices can update)
```
# bump version in src-tauri/tauri.conf.json (e.g. 0.2.1), commit
git tag v0.2.1 && git push origin v0.2.1
```
CI builds the signed installer + `latest.json` and uploads them to a **draft** release. Publish it
from the GitHub Releases UI. Installed clients then see "Update available" (Settings → Check for
updates) → one click downloads, installs (NSIS), and relaunches.

## Build the exe locally
```
# from the repo root (worktree). Model + resources must be present:
npm run fetch-model                  # populates assets/models (gitignored)
# sign locally too (so the local build emits updater artifacts):
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.claude/dotz-rust/dotz-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="dotz-updater-key-2026"
cargo tauri build
# → src-tauri/target/release/bundle/nsis/dotz_0.2.0_x64-setup.exe  (+ .sig + latest.json bits)
```

## Architecture
The installer bundles the dotz-core Rust server + `web/` UI + `.pi/` + `assets/` (incl. the ONNX
embedding model) as resources. On launch, the Tauri shell starts the axum server on 127.0.0.1:4317
and opens a WebView2 window on it — the vanilla `web/` UI runs unchanged over `location.host`.
Single-instance: relaunching focuses the running window. Native folder picker via the dialog plugin.
