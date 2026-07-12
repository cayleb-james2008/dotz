//! House-convention guard: every `Command::new` spawn site in shipped code must be windowless
//! on Windows. The packaged app builds with `windows_subsystem = "windows"`, so any console
//! subprocess spawned without `CREATE_NO_WINDOW` flashes a conhost window over the dashboard
//! (git per workflow step, taskkill per timeout, gh per vcs-panel poll, ...).
//!
//! Enforced as a source scan: a file that spawns processes must reference either the shared
//! `util::no_window` / `util::no_window_tokio` helper or an inline `CREATE_NO_WINDOW` flag.
//! Test code (the `#[cfg(test)]` module at the bottom of each file, and files under `tests/`)
//! is exempt — test spawns run from a console harness where a window cannot flash.

use std::path::{Path, PathBuf};

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_spawn_site_is_windowless() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("dotz-core has a parent dir");
    // Shipped source only: dotz-core/src + the Tauri shell. tests/ dirs are exempt by omission.
    let roots = [manifest.join("src"), workspace.join("src-tauri").join("src")];

    let mut files = Vec::new();
    for root in &roots {
        rs_files(root, &mut files);
    }
    assert!(
        files.len() > 10,
        "guard walked too few source files ({}) — did the src layout move?",
        files.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).unwrap_or_default();
        // Repo convention: the `#[cfg(test)] mod tests` block sits at the bottom of the file.
        // Only shipped code above the first `#[cfg(test)]` is held to the convention.
        let shipped = src.split("#[cfg(test)]").next().unwrap_or(&src);
        if shipped.contains("Command::new")
            && !shipped.contains("CREATE_NO_WINDOW")
            && !shipped.contains("no_window")
        {
            offenders.push(file.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "these files spawn subprocesses without CREATE_NO_WINDOW / util::no_window \
         (house convention — the packaged app flashes a conhost window otherwise):\n{}",
        offenders.join("\n")
    );
}
