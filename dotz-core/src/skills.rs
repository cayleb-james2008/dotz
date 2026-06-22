//! dotz unified skills loader — port of src/skills.ts.
//!
//! Discovers `SKILL.md` files across the shared skill pools (design, hermes, superpowers, ecc,
//! codex, claude, opencode, plus the bundled `.pi` skills and the per-user `~/.dotz/ai-agents/skills`),
//! parses each file's frontmatter once, dedupes by name (later scan roots overwrite earlier
//! same-named skills — the roots are ordered LOW→HIGH priority), platform-filters to the host
//! (windows), and exposes the two `/api/skills*` endpoints.
//!
//! Frontmatter dialects handled (we read `name`, `description`, `tags`, `platforms`, and the
//! Hermes nesting `metadata.hermes.tags`). A malformed-frontmatter parse falls back to `{}` so the
//! skill still loads by its parent-directory name. Umbrella detection mirrors the JS heuristic: the
//! first 400 chars of the body (after frontmatter) contain "Class-level umbrella" or "umbrella skill"
//! (case-insensitive).
//!
//! Self-contained: no AppState, no axum State — a module-owned `OnceLock<Mutex<...>>` caches the
//! deduped index (built on first request, idempotent), exactly like the Node `SkillLoader.load()`
//! one-shot. The `.pi` location is resolved the same way `design.rs` does (a `DOTZ_PI` override,
//! else `<cwd>/.pi`).
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

/// Host platform token used by the platform filter. This crate ships for Windows; the JS code maps
/// `process.platform === "win32"` → "windows". A skill declaring `platforms: [...]` is kept only
/// when that list contains this token.
const HOST_PLATFORM: &str = "windows";

/// A discovered skill, after frontmatter parse + platform filter. `path` is kept for `load_body`;
/// the serialized index (`SkillView`) drops it.
#[derive(Clone, Debug)]
struct Skill {
    name: String,
    description: String,
    path: PathBuf,
    source: &'static str,
    tags: Option<Vec<String>>,
    platforms: Option<Vec<String>>,
    is_umbrella: bool,
}

/// One row in the `GET /api/skills` response. Field order + names mirror the Node object and the
/// captured oracle (`skills.index.json`): `name, description, source, tags?, isUmbrella`. `tags` is
/// omitted when absent; `isUmbrella` is always present.
#[derive(Serialize)]
struct SkillView {
    name: String,
    description: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<Vec<String>>,
    #[serde(rename = "isUmbrella")]
    is_umbrella: bool,
}

/// Resolve the bundled `.pi` dir the way `design.rs` / the Node code does: a `DOTZ_PI` override
/// (pointing at the `.pi` dir itself), else `<cwd>/.pi`.
fn pi_dir() -> PathBuf {
    std::env::var("DOTZ_PI")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".pi")
        })
}

/// `~/.dotz/ai-agents/skills` — the per-user dotz skill root, honoring `DOTZ_CONFIG_DIR` via
/// `crate::config::dotz_dir()` (same as the Node `userSkillsDir()`).
fn user_skills_dir() -> PathBuf {
    crate::config::dotz_dir().join("ai-agents").join("skills")
}

/// `$HOME/<rest...>` for the `~`-anchored roots (uses `dirs::home_dir()`).
fn home_join(rest: &[&str]) -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    for seg in rest {
        p.push(seg);
    }
    p
}

/// Ordered scan roots, LOWEST priority first (later roots overwrite earlier same-named skills).
/// Mirrors `scanRoots()` in skills.ts exactly, including the `DOTZ_SKILLS_PATHS` override appended
/// at the end (highest priority). Read fresh each load so env overrides take effect.
fn scan_roots() -> Vec<(PathBuf, &'static str)> {
    let pi = pi_dir();
    let mut roots: Vec<(PathBuf, &'static str)> = vec![
        (pi.join("design-skills"), "design"),
        (home_join(&[".hermes", "skills"]), "hermes"),
        (
            home_join(&[".codex", "plugins", "cache", "openai-curated", "superpowers"]),
            "superpowers",
        ),
        (
            home_join(&[".codex", "marketplaces", "ecc-local", "plugins", "ecc", "skills"]),
            "ecc",
        ),
        (home_join(&[".codex", "skills"]), "codex"),
        (home_join(&[".claude", "skills"]), "claude"),
        (home_join(&[".config", "opencode", "skills"]), "opencode"),
        (pi.join("skills"), "dotz"),
        (user_skills_dir(), "dotz"),
    ];
    // Operator override: DOTZ_SKILLS_PATHS=dir1<sep>dir2 (path.delimiter — ';' on Windows). Each
    // existing dir is appended at the end (highest priority).
    if let Ok(extra) = std::env::var("DOTZ_SKILLS_PATHS") {
        for d in extra.split(';') {
            let d = d.trim();
            if d.is_empty() {
                continue;
            }
            let p = PathBuf::from(d);
            if p.exists() {
                roots.push((p, "dotz"));
            }
        }
    }
    roots
}

/// Recursively collect files named (case-insensitively) `skill.md` under `root`. Unreadable dirs
/// are skipped silently. Mirrors `findSkillFiles()`.
fn find_skill_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    walk(root, &mut out);
    out
}

fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let ft = match ent.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let full = ent.path();
        if ft.is_dir() {
            walk(&full, out);
        } else if ft.is_file()
            && ent
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                == "skill.md"
        {
            out.push(full);
        }
    }
}

/// Parent-directory name of a SKILL.md file — the `path.basename(path.dirname(file))` fallback.
fn parent_dir_name(file: &std::path::Path) -> String {
    file.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Split a YAML value coerced to a string: a string OR a number → its string form; anything else
/// (null/seq/map/bool) → `None`. Mirrors the JS `typeof === "string" || typeof === "number"` guard.
fn scalar_str(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A YAML sequence filtered to its string elements only (non-strings dropped). A non-sequence → None.
/// Mirrors `Array.isArray(x) ? x.filter(typeof === "string") : undefined`.
fn string_array(v: &serde_yaml::Value) -> Option<Vec<String>> {
    match v {
        serde_yaml::Value::Sequence(seq) => Some(
            seq.iter()
                .filter_map(|x| match x {
                    serde_yaml::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Collapse every run of ASCII/Unicode whitespace to a single space and trim — mirrors JS
/// `String(x).replace(/\s+/g, " ").trim()`.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split off a leading frontmatter block: between a leading `---\n` and the next `\n---`. Returns
/// `(Some(yaml), body)` when present, else `(None, whole)`. Mirrors gray-matter's leading-fence split
/// (it requires the `---` fence at the very start of the file).
fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    // The opening fence must be at the start: "---\n" or "---\r\n".
    let after_open = if let Some(rest) = raw.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = raw.strip_prefix("---\r\n") {
        rest
    } else {
        return (None, raw);
    };
    // Find the closing fence: a line that is exactly "---" (preceded by a newline). We search for
    // "\n---" and require it to be followed by end-of-string or a newline.
    let mut search_from = 0usize;
    while let Some(rel) = after_open[search_from..].find("\n---") {
        let idx = search_from + rel; // position of the '\n' before the closing ---
        let after_dashes = idx + 4; // just past "\n---"
        let tail = &after_open[after_dashes..];
        // Valid closer iff the "---" is the whole line: end-of-file, or next char starts a newline.
        if tail.is_empty() || tail.starts_with('\n') || tail.starts_with('\r') {
            let yaml = &after_open[..idx];
            // Body starts after the closing fence's own line terminator.
            let body = if let Some(b) = tail.strip_prefix("\r\n") {
                b
            } else if let Some(b) = tail.strip_prefix('\n') {
                b
            } else {
                tail // closer at EOF, empty body
            };
            return (Some(yaml), body);
        }
        search_from = idx + 1;
    }
    // Opening fence but no valid closer — gray-matter would treat the whole thing as body.
    (None, raw)
}

/// Parse one SKILL.md into a `Skill` (without loading/keeping the full body). Returns None only when
/// no resolvable name exists (parent dir name is empty) — mirrors the JS `if (!name) return null`.
fn parse_skill_file(file: &std::path::Path, source: &'static str) -> Option<Skill> {
    let raw = std::fs::read_to_string(file).ok()?;
    let (fm_src, body) = split_frontmatter(&raw);

    // Parse the frontmatter; on any error fall back to an empty mapping (dirname name fallback).
    let fm: serde_yaml::Value = fm_src
        .and_then(|y| serde_yaml::from_str::<serde_yaml::Value>(y).ok())
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));

    let get = |k: &str| fm.get(k);

    let dir_name = parent_dir_name(file);
    let name = get("name")
        .and_then(scalar_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir_name.clone());
    if name.is_empty() {
        return None;
    }

    let description = get("description")
        .and_then(scalar_str)
        .map(|s| collapse_ws(&s))
        .unwrap_or_default();

    let mut tags = get("tags").and_then(string_array);
    let platforms = get("platforms").and_then(string_array);

    // Hermes nests extra tags under metadata.hermes.tags — concat onto whatever flat tags exist.
    if let Some(hermes_tags) = get("metadata")
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.get("tags"))
        .and_then(string_array)
    {
        let mut merged = tags.take().unwrap_or_default();
        merged.extend(hermes_tags);
        tags = Some(merged);
    }

    // Umbrella heuristic: first 400 chars of the body, case-insensitive.
    let head: String = body.chars().take(400).collect();
    let head_lower = head.to_lowercase();
    let is_umbrella =
        head_lower.contains("class-level umbrella") || head_lower.contains("umbrella skill");

    Some(Skill {
        name,
        description,
        path: file.to_path_buf(),
        source,
        tags,
        platforms,
        is_umbrella,
    })
}

/// Platform filter: no `platforms` (or empty) → keep; otherwise keep only if it includes the host.
fn platforms_ok(s: &Skill) -> bool {
    match &s.platforms {
        None => true,
        Some(p) if p.is_empty() => true,
        Some(p) => p.iter().any(|x| x == HOST_PLATFORM),
    }
}

/// Build the deduped, platform-filtered, name-sorted index. Scan roots low→high so a later root
/// overwrites an earlier same-named skill (BTreeMap insert replaces the value, keeping sorted order).
fn build_index() -> BTreeMap<String, Skill> {
    let mut map: BTreeMap<String, Skill> = BTreeMap::new();
    for (dir, source) in scan_roots() {
        for file in find_skill_files(&dir) {
            if let Some(skill) = parse_skill_file(&file, source) {
                if platforms_ok(&skill) {
                    map.insert(skill.name.clone(), skill);
                }
            }
        }
    }
    map
}

/// Module-owned cache of the deduped index, built once on first access (idempotent — mirrors the
/// Node `SkillLoader.load()` one-shot guard).
fn index() -> &'static Mutex<BTreeMap<String, Skill>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Skill>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(build_index()))
}

/// GET /api/skills — `{ "skills": [{ name, description, source, tags?, isUmbrella }] }`, sorted by
/// name (the BTreeMap iterates in key order, which matches the JS `localeCompare` for these ASCII
/// names). `tags` omitted when absent; `isUmbrella` always present.
async fn list_skills() -> Json<serde_json::Value> {
    let guard = index().lock().unwrap();
    let skills: Vec<SkillView> = guard
        .values()
        .map(|s| SkillView {
            name: s.name.clone(),
            description: s.description.clone(),
            source: s.source,
            tags: s.tags.clone(),
            is_umbrella: s.is_umbrella,
        })
        .collect();
    Json(json!({ "skills": skills }))
}

/// GET /api/skills/{name} — `{ "name", "body" }` where `body` is the file contents with a leading
/// frontmatter block stripped (`^---...---\n`). 404 `{ "error": "no such skill" }` when unknown or
/// the body can't be loaded — mirrors server.ts (a falsy body → 404).
async fn get_skill(Path(name): Path<String>) -> Response {
    let path = {
        let guard = index().lock().unwrap();
        guard.get(&name).map(|s| s.path.clone())
    };
    let Some(path) = path else {
        return not_found();
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let (_, body) = split_frontmatter(&raw);
            body.to_string()
        }
        Err(_) => return not_found(),
    };
    Json(json!({ "name": name, "body": body })).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such skill" })),
    )
        .into_response()
}

/// Stateless router for the unified skills endpoints.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/skills", get(list_skills))
        .route("/api/skills/{name}", get(get_skill))
}
