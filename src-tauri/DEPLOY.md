# dotz — build and updater distribution

I keep build instructions separate from release acceptance. Source and releases live in the public
[`cayleb-james2008/dotz`](https://github.com/cayleb-james2008/dotz) repository; the default branch is
`main`. The old private-source/separate-release-repo topology is historical.

## What is wired, and what has been checked

- `src-tauri/tauri.conf.json` enables NSIS, DMG, AppImage, and DEB targets plus updater artifacts.
  A configured target is not proof of a working native build on that OS.
- The updater endpoint is
  `https://github.com/cayleb-james2008/dotz/releases/latest/download/latest.json`.
- `latest.json` is a JSON manifest containing platform download URLs and artifact signatures.
  The updater verifies downloaded artifacts against `plugins.updater.pubkey`; calling the JSON
  itself “minisign-signed” obscures what is actually verified.
- `src-tauri/src/main.rs` wires check/apply commands. The apply path calls `download_and_install`
  and requests a restart. A source path is not an exercised native UI/update journey.
- Read-only, unauthenticated GETs on **2026-10-10 at 18:07 UTC** returned HTTP 200 for both the
  latest-release page and manifest. The page redirected to `v0.2.8`; the parsed manifest reported
  version `0.2.8` with only `windows-x86_64`. The 2026-09-18 HTTP 404 result is an older observation.
  No installer was executed and no new signature or build-identity verification was done in this
  documentation lane.

Updater/Minisign signatures are **not Windows Authenticode**. Neither a reachable manifest nor a
signature field establishes signature validity, a candidate-to-release source identity, native
Windows startup, or a completed update/install/relaunch. Keep each claim tied to its own receipt.

## Local build

Use a fresh checkout of the exact revision you intend to test. Start from `main` for development;
an unmerged candidate must be identified separately. Install Rust, Node.js/npm, the Tauri 2 CLI,
and the [platform prerequisites](https://v2.tauri.app/start/prerequisites/) first.

Run these commands **from the repository root**, not `src-tauri/`:

```text
npm run install:deps
npm run fetch-model
cargo tauri build
```

`install:deps` is present in `package.json` and calls `scripts/install-npm-deps.mjs`. The wrapper
installs with scripts disabled, then rebuilds only present packages at exact approved versions.
It sets `SHARP_IGNORE_GLOBAL_LIBVIPS=1` for both phases. Use it instead of raw `npm install` when
relying on that lifecycle policy. Network access is required for dependency/runtime/model downloads;
a policy fixture does not establish a successful fresh online install.

With Cargo's default target directory, bundles are under the workspace-root `target/release/bundle/`:

- Windows NSIS: `target/release/bundle/nsis/` (requires a Windows build environment).
- Linux: `target/release/bundle/deb/` or `target/release/bundle/appimage/`.
- macOS: `target/release/bundle/dmg/` and the app bundle output.

An explicit Rust target or `CARGO_TARGET_DIR` changes the output location. Read the build output
rather than assuming a file name. Building without an updater signing key is not proof of a
signed distributable; inspect actual artifacts and failures.

## Signing secrets

For updater-signed artifacts, supply `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` through a secure environment/secret store before building.
The release workflow reads repository Actions secrets with those names; the public verification
key belongs in `src-tauri/tauri.conf.json`.

Do not commit private keys or passwords, print them into logs, or depend on a particular operator's
home-directory files. Key rotation requires deliberate client/public-key compatibility handling;
this document does not certify any local key's identity or availability.

## Repository release workflow

[`.github/workflows/release.yml`](../.github/workflows/release.yml) is configured to run on a `v*`
tag on `windows-latest`, run the two npm setup commands above, and invoke
`tauri-apps/tauri-action` with `projectPath: src-tauri`, `releaseDraft: true`, and
`prerelease: false`. A tag starts a build/draft-release job; it does not automatically accept or
publish a tested release.

Before tagging/publishing, reconcile versions in `src-tauri/tauri.conf.json` and
`src-tauri/Cargo.toml`, use the deliberately accepted exact commit from `main`, and obtain the
required exact-head CI and independent review. Then inspect the job's actual bundle/signature and
manifest artifacts, verify their source/version/platform identity, and exercise a fresh native
install plus a real upgrade/relaunch on the claimed platform before calling it accepted.

There is **no repository-owned `build-latest-json.mjs` helper** in the inspected source. The old
command pointing into `~/.claude/dotz-rust/` is not a portable release procedure and is intentionally
omitted. Use the checked-in workflow's actual outputs; if it does not produce the intended feed,
repair and verify that separately rather than inventing a helper or publishing a guessed manifest.

## Bundled runtime

The resource map includes `web/`, `.pi/`, `assets/` (including fetched embedding-model files), and
`node_modules/agent-browser/bin/`. The shell resolves those resources, starts the in-process axum
server on `127.0.0.1:4317`, and opens the shared UI in the platform webview (WebView2 on Windows).
The resource map and a readiness banner do not establish complete packaging or working native
bridges. Keep Linux package receipts, Windows/macOS journeys, live-provider tests, signature
verification, and update acceptance distinct; see [README status](../README.md#what-works-today).
