//! House-convention guard: every `Command::new` spawn site in shipped code must be windowless
//! on Windows. The packaged app builds with `windows_subsystem = "windows"`, so any console
//! subprocess spawned without `CREATE_NO_WINDOW` flashes a conhost window over the dashboard
//! (git per workflow step, taskkill per timeout, gh per vcs-panel poll, ...).
//!
//! Enforced as a per-spawn-site source scan: each `Command::new` occurrence in shipped code
//! must have a windowing marker (`creation_flags` / `CREATE_NO_WINDOW` / the shared
//! `util::no_window` / `util::no_window_tokio` helpers) inside its enclosing function (or,
//! for spawns outside any function, within the next 10 lines). Exempt code is stripped by
//! brace tracking — not by truncating at the first `#[cfg(test)]`, which used to silently
//! skip every shipped spawn below a mid-file test-only item:
//!   - `#[cfg(test)]` items/blocks (test spawns run from a console harness — no window flash),
//!   - `#[cfg(not(windows))]` / `#[cfg(unix)]` items/blocks (no conhost off Windows),
//!   - files under `tests/` (exempt by omission from the scan roots).

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

/// Attributes whose following item/block is exempt from the windowing convention.
const EXEMPT_ATTRS: [&str; 3] = ["#[cfg(test)]", "#[cfg(not(windows))]", "#[cfg(unix)]"];

/// A marker that proves the spawn site suppresses the console window on Windows.
fn has_marker(text: &str) -> bool {
    text.contains("creation_flags")
        || text.contains("CREATE_NO_WINDOW")
        || text.contains("no_window")
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//")
}

/// Does this line start a `fn` item (free function or method)?
fn is_fn_item(line: &str) -> bool {
    if is_comment(line) {
        return false;
    }
    let t = line.trim_start();
    if !t.contains("fn ") {
        return false;
    }
    t.starts_with("fn ")
        || ((t.starts_with("pub")
            || t.starts_with("async ")
            || t.starts_with("unsafe ")
            || t.starts_with("const ")
            || t.starts_with("extern "))
            && t.contains("fn "))
}

/// Net brace tracking, line by line. Good enough for this repo's formatted (rustfmt-shaped)
/// source: `{`/`}` inside string literals appear as balanced `{}` format pairs and cancel out.
fn brace_delta(line: &str) -> (i64, bool) {
    let mut delta = 0i64;
    let mut opened = false;
    // Ignore trailing line comments so a `// ... {` doc note cannot skew the count.
    let code = line.split("//").next().unwrap_or(line);
    for ch in code.chars() {
        match ch {
            '{' => {
                delta += 1;
                opened = true;
            }
            '}' => delta -= 1,
            _ => {}
        }
    }
    (delta, opened)
}

/// Blank out (preserving line numbers) every item or block attached to an exempt attribute.
/// The item is consumed by brace tracking: it ends when the brace depth opened after the
/// attribute returns to zero, or at a `;` before any brace opens (e.g. `static X: T = init();`).
fn blank_exempt_blocks(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if !EXEMPT_ATTRS.iter().any(|a| t.starts_with(a)) {
            i += 1;
            continue;
        }
        keep[i] = false;
        i += 1;
        // Skip any further attributes / doc comments between the cfg attribute and the item.
        while i < lines.len() {
            let t = lines[i].trim_start();
            if t.starts_with("#[") || t.starts_with("//") {
                keep[i] = false;
                i += 1;
            } else {
                break;
            }
        }
        // Consume the attached item or block.
        let mut depth = 0i64;
        let mut opened = false;
        while i < lines.len() {
            keep[i] = false;
            let (delta, line_opened) = brace_delta(lines[i]);
            depth += delta;
            opened |= line_opened;
            let done = (opened && depth <= 0) || (!opened && lines[i].contains(';'));
            i += 1;
            if done {
                break;
            }
        }
    }
    lines
        .iter()
        .enumerate()
        .map(|(n, l)| {
            if keep[n] {
                (*l).to_string()
            } else {
                String::new()
            }
        })
        .collect()
}

/// The scan scope for a spawn at `line_idx`: the enclosing `fn` item's full body, or (if the
/// spawn is not inside a detected function) the spawn line plus the following 10 lines.
fn scan_scope(lines: &[String], line_idx: usize) -> String {
    // Nearest `fn` header at or above the spawn line is the innermost enclosing candidate.
    let fn_start = (0..=line_idx).rev().find(|&j| is_fn_item(&lines[j]));
    if let Some(start) = fn_start {
        // Body ends when the depth opened at/after the header returns to zero.
        let mut depth = 0i64;
        let mut opened = false;
        let mut end = start;
        for (j, line) in lines.iter().enumerate().skip(start) {
            let (delta, line_opened) = brace_delta(line);
            depth += delta;
            opened |= line_opened;
            end = j;
            if opened && depth <= 0 {
                break;
            }
        }
        if line_idx <= end {
            return lines[start..=end].join("\n");
        }
    }
    let window_end = (line_idx + 11).min(lines.len());
    lines[line_idx..window_end].join("\n")
}

#[test]
fn every_spawn_site_is_windowless() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("dotz-core has a parent dir");
    // Shipped source only: dotz-core/src + the Tauri shell. tests/ dirs are exempt by omission.
    let roots = [
        manifest.join("src"),
        workspace.join("src-tauri").join("src"),
    ];

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
    let mut spawn_sites = 0usize;
    for file in &files {
        let src = std::fs::read_to_string(file).unwrap_or_default();
        let shipped = blank_exempt_blocks(&src);
        for (idx, line) in shipped.iter().enumerate() {
            if !line.contains("Command::new") || is_comment(line) {
                continue;
            }
            spawn_sites += 1;
            if !has_marker(&scan_scope(&shipped, idx)) {
                offenders.push(format!("{}:{}", file.display(), idx + 1));
            }
        }
    }
    assert!(
        spawn_sites > 10,
        "guard found too few spawn sites ({spawn_sites}) — did the scan or src layout break?"
    );
    assert!(
        offenders.is_empty(),
        "these spawn sites lack CREATE_NO_WINDOW / creation_flags / util::no_window in their \
         enclosing function (house convention — the packaged app flashes a conhost window \
         otherwise):\n{}",
        offenders.join("\n")
    );
}
