fn main() {
    // tauri-build produces the Windows VERSION resource for this executable.
    // A second winres-generated VERSIONINFO conflicts with it under current MSVC
    // linkers (CVTRES CVT1100: duplicate type VERSION, name 1, language 0x0409).
    // Declare the app's custom `bridge` command so Tauri generates an `allow-bridge` permission for
    // it. The main window loads a REMOTE http origin (127.0.0.1:4317), and app-defined commands are
    // not ACL-allowed for remote origins by default — without a generated permission the capability
    // can't grant `bridge`, so every invoke('bridge', …) (updater status/apply) fails with
    // "bridge not allowed. Plugin not found".
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["bridge"])),
    )
    .expect("failed to run tauri-build");
}
