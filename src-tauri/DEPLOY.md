# dotz — build + cross-device auto-update

dotz ships as a signed NSIS installer that self-updates via `tauri-plugin-updater`. As of 2026-06-24
the project is a **single public repo**: source + releases both live in `cayleb-james2008/dotz`
(public), so the release assets the unauthenticated updater fetches are publicly downloadable. (The
old private-source + separate `dotz-releases` split was retired.)

## Topology
- **Repo (public):** `cayleb-james2008/dotz` — source + releases (signed installer + `latest.json`).
- `tauri.conf.json → plugins.updater.endpoints` = `https://github.com/cayleb-james2008/dotz/releases/latest/download/latest.json`.
- `latest.json` is minisign-signed; clients verify it against `plugins.updater.pubkey` before installing.

## Signing key
- Private key: `~/.claude/dotz-rust/dotz-updater-v2.key` (gitignored / never committed). Its password is
  in `~/.claude/dotz-rust/dotz-updater-v2.key.password` — **store the password in a password manager /
  CI secret; never commit it.** (The original key was rotated on going public; the old one is retired.)
- Public key is `plugins.updater.pubkey` in `tauri.conf.json`.

## Cut a new release (manual)
```
# 1. bump version in src-tauri/tauri.conf.json + src-tauri/Cargo.toml, commit + push to master
# 2. build the signed installer (from a clean checkout):
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.claude/dotz-rust/dotz-updater-v2.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(sed 's/^PASSWORD=//' ~/.claude/dotz-rust/dotz-updater-v2.key.password)"
npm install && npm run fetch-model       # agent-browser binary + ONNX model into assets/models/
cargo tauri build
# 3. build latest.json + publish to the dotz repo:
VER=<ver> NSIS_DIR="$(pwd)/target/release/bundle/nsis" node ~/.claude/dotz-rust/build-latest-json.mjs
gh release create v<ver> --repo cayleb-james2008/dotz --target master \
  --title "dotz v<ver>" --notes "…" \
  target/release/bundle/nsis/dotz_<ver>_x64-setup.exe \
  target/release/bundle/nsis/latest.json
```
Installed clients then see "Update available" (Settings → Check for updates) → one click installs + relaunches.

## Optional: automate via CI
`.github/workflows/release.yml` can run `tauri-apps/tauri-action` on a `v*` tag, publishing to this
same repo. It needs two repo secrets — `TAURI_SIGNING_PRIVATE_KEY` (contents of
`~/.claude/dotz-rust/dotz-updater-v2.key`) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (its password).
Add them in Settings → Secrets → Actions. (The private signing key + password are added by you — never committed.)

## Architecture
The installer bundles the dotz-core Rust server + `web/` UI + `.pi/` + `assets/` (ONNX embedding model)
+ the `agent-browser` engine. On launch the Tauri shell starts the axum server on 127.0.0.1:4317 and
opens a WebView2 window on it — the vanilla `web/` UI runs unchanged over `location.host`.
