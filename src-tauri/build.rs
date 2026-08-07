fn main() {
    // Embed Windows resource info (version, manifest) into the .exe via winres.
    // Only runs on Windows targets; tauri-build handles the app icon via tauri.conf.json.
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set("FileDescription", "dotz — ultra code (Rust/Tauri shell)");
        res.set("ProductName", "dotz");
        res.set("LegalCopyright", "Copyright (c) 2026");
        if let Err(e) = res.compile() {
            // Non-fatal: the build still works without the resource file.
            println!("cargo:warning=winres compile failed: {e}");
        }
    }

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
