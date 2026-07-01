fn main() {
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
