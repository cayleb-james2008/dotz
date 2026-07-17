//! Guard: the `ort` dependency must stay EXACT-pinned until a stable 2.x exists.
//!
//! Why: the shipped installer bundles an ONNX Runtime fetched at build time by ort-sys's
//! `download-binaries` feature. Each ort-sys release hardcodes the runtime version it fetches
//! (rc.12 -> ONNX Runtime 1.24.2, SHA-256 checksummed per target), so the fetch is only
//! deterministic while ort/ort-sys are exact-pinned. The 2.0 rcs also break API between
//! releases. If someone relaxes the `=` pin (Cargo.toml) or the lockfile drifts to a different
//! ort/ort-sys version, a routine `cargo update` could silently swap the embedding runtime in
//! a shipped installer. This test fails loudly instead.
//!
//! On a deliberate bump: update PINNED below, the rationale comment on the `ort` line in
//! dotz-core/Cargo.toml, and the stable-release watch note in README "Notes & caveats";
//! re-verify embed.rs / all-MiniLM-L6-v2 parity (`cargo test -p dotz-core`).

use std::path::Path;

/// The one place the pinned version is spelled out for this guard.
const PINNED: &str = "2.0.0-rc.12";

fn workspace_root() -> &'static Path {
    // CARGO_MANIFEST_DIR = dotz-core; the workspace root (Cargo.lock) is one level up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("dotz-core has a parent workspace dir")
}

/// Cargo.toml must keep the exact (`=`) requirement, not a range.
#[test]
fn cargo_toml_keeps_exact_ort_pin() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read dotz-core/Cargo.toml")
            .replace("\r\n", "\n");
    let want = format!("ort = {{ version = \"={PINNED}\"");
    assert!(
        manifest.contains(&want),
        "dotz-core/Cargo.toml no longer exact-pins ort to ={PINNED}. The `=` pin is what keeps \
         the download-binaries ONNX Runtime deterministic in the shipped installer. If this is a \
         deliberate bump, update PINNED in tests/ort_pin_guard.rs and the rationale comment in \
         Cargo.toml; otherwise restore `ort = {{ version = \"={PINNED}\", ... }}`."
    );
}

/// Cargo.lock must resolve both `ort` and `ort-sys` to exactly the pinned version.
/// ort-sys is the crate whose build script actually downloads the runtime binaries.
#[test]
fn cargo_lock_resolves_ort_and_ort_sys_to_pinned() {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock"))
        .expect("read workspace Cargo.lock")
        .replace("\r\n", "\n");
    for name in ["ort", "ort-sys"] {
        let header = format!("name = \"{name}\"\n");
        let block_start = lock
            .find(&header)
            .unwrap_or_else(|| panic!("`{name}` not found in Cargo.lock"));
        let rest = &lock[block_start..];
        let version_line = rest
            .lines()
            .find(|l| l.starts_with("version = "))
            .unwrap_or_else(|| panic!("no version line for `{name}` in Cargo.lock"));
        let want = format!("version = \"{PINNED}\"");
        assert_eq!(
            version_line, want,
            "Cargo.lock resolves `{name}` to `{version_line}` instead of {PINNED}. \
             download-binaries fetches whatever ONNX Runtime that ort-sys release hardcodes, \
             so lockfile drift silently changes the runtime bundled in the installer. \
             Revert the lock change or do a deliberate bump (see tests/ort_pin_guard.rs header)."
        );
    }
}
