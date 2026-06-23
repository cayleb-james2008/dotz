# dotz — build + cross-device auto-update

dotz ships as a signed NSIS installer that self-updates via `tauri-plugin-updater`. The **source
repo stays private**; releases are hosted on a **separate public repo** because GitHub release
assets on a *private* repo are NOT publicly downloadable (verified — the updater is unauthenticated).

## Topology
- **Source (private):** `cayleb-james2008/dotz` — code only.
- **Releases (public):** `cayleb-james2008/dotz-releases` — hosts the signed installer + `latest.json`.
- `tauri.conf.json → plugins.updater.endpoints` = `https://github.com/cayleb-james2008/dotz-releases/releases/latest/download/latest.json`.
- `latest.json` is minisign-signed; clients verify it against `plugins.updater.pubkey` before installing.

## Auto-update is ACTIVE (v0.2.3 published)
Any dotz **≥ 0.2.3** (built with the `dotz-releases` endpoint) already checks that public endpoint.
Install 0.2.3 once on each device → every later release self-updates. (Older 0.2.0–0.2.2 builds pointed
at the private repo and won't update — install 0.2.3 to bootstrap.)

## Cut a new release (manual — what's wired today)
```
# 1. bump version in src-tauri/tauri.conf.json + src-tauri/Cargo.toml (e.g. 0.2.4), commit
# 2. build the signed installer locally:
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.claude/dotz-rust/dotz-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="dotz-updater-key-2026"
cargo tauri build
# 3. build latest.json + publish to the PUBLIC releases repo:
VER=0.2.4 node ~/.claude/dotz-rust/build-latest-json.mjs
gh release create v0.2.4 --repo cayleb-james2008/dotz-releases --target main \
  --title "dotz v0.2.4" --notes "…" \
  target/release/bundle/nsis/dotz_0.2.4_x64-setup.exe \
  target/release/bundle/nsis/latest.json
```
Installed clients then see "Update available" (Settings → Check for updates) → one click installs + relaunches.

## Optional: automate via CI
`.github/workflows/release.yml` (in the private source repo) can run `tauri-apps/tauri-action` on a
`v*` tag, but it must publish to the **public `dotz-releases`** repo (set `owner`/`repo` accordingly)
and needs two repo secrets — `TAURI_SIGNING_PRIVATE_KEY` (contents of `~/.claude/dotz-rust/dotz-updater.key`)
and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Add those in the source repo's Settings → Secrets → Actions.
(The private signing key must be added by you — never committed.)

## Architecture
The installer bundles the dotz-core Rust server + `web/` UI + `.pi/` + `assets/` (ONNX embedding model)
+ the `agent-browser` engine. On launch the Tauri shell starts the axum server on 127.0.0.1:4317 and
opens a WebView2 window on it — the vanilla `web/` UI runs unchanged over `location.host`.
