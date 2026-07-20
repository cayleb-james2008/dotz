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
    borrow::Cow,
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
/// (pointing at the `.pi` dir itself), else `<cwd>/.pi`, else the workspace root derived from
/// this crate's manifest (used when tests run from the `dotz-core` package dir).
pub fn pi_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DOTZ_PI") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let cwd_pi = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".pi");
    if cwd_pi.exists() {
        return cwd_pi;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join(".pi"))
        .unwrap_or(cwd_pi)
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
///
/// C3 (MCP): a `~/.dotz/mcp-prompts/` root is appended near the end so MCP prompt templates
/// (synced by a future C3 follow-up from `mcp::client::list_prompts` into per-server
/// `SKILL.md` files) are surfaced THROUGH `skills.rs` — the single skill-discovery path.
/// `# ponytail:` full prompt sync (polling connected servers, generating SKILL.md files) is the
/// follow-up; for now we only add the scan root so a manually-populated dir works.
fn scan_roots() -> Vec<(PathBuf, &'static str)> {
    let pi = pi_dir();
    let mut roots: Vec<(PathBuf, &'static str)> = vec![
        (pi.join("design-skills"), "design"),
        (home_join(&[".hermes", "skills"]), "hermes"),
        (
            home_join(&[
                ".codex",
                "plugins",
                "cache",
                "openai-curated",
                "superpowers",
            ]),
            "superpowers",
        ),
        (
            home_join(&[
                ".codex",
                "marketplaces",
                "ecc-local",
                "plugins",
                "ecc",
                "skills",
            ]),
            "ecc",
        ),
        (home_join(&[".codex", "skills"]), "codex"),
        (home_join(&[".claude", "skills"]), "claude"),
        (home_join(&[".config", "opencode", "skills"]), "opencode"),
        (pi.join("skills"), "dotz"),
        (user_skills_dir(), "dotz"),
        // C3: MCP prompt templates live under `~/.dotz/mcp-prompts/` (per-server SKILL.md files
        // generated from `mcp::client::list_prompts`). Surfaces THROUGH skills.rs — not a
        // parallel loader.
        (crate::config::dotz_dir().join("mcp-prompts"), "mcp"),
        // C8: marketplace presets live under `~/.dotz/presets/<name>/` and may ship a
        // `SKILL.md` (skill presets) or nested `skills/<name>/SKILL.md` trees. Surfaced THROUGH
        // skills.rs — the single skill-discovery path — so an installed preset's skills appear
        // in the index + system prompt exactly like a bundled `.pi` skill. Source label
        // "preset" so the UI can distinguish them from bundled/curated pools.
        (crate::config::dotz_dir().join("presets"), "preset"),
        // C4: plugins live under `~/.dotz/plugins/<name>/`. Each plugin dir is a scan root so its
        // `SKILL.md` (the plugin's skill body) is loaded THROUGH skills.rs — the single
        // skill-discovery path. The plugin manifest (`plugin.toml`) is parsed by `plugins.rs`;
        // the `SKILL.md` is parsed by this module. The source is `"plugin"` so the UI can
        // distinguish plugin-sourced skills.
        //
        // We add the individual plugin dirs (not the `plugins/` root) so each plugin's `SKILL.md`
        // is discovered with its parent-dir name as the skill name fallback (matching the other
        // scan roots). `plugins::plugin_dirs()` returns the list of direct child dirs.
        // # ponytail: the plugin scan is a separate root per plugin dir; ceiling = if plugins
        // grow to thousands, batch them under one root + walk. For now, one root per plugin is
        // fine (the typical install has <10 plugins).
    ];
    for plugin_dir in crate::plugins::plugin_dirs() {
        roots.push((plugin_dir, "plugin"));
    }
    // Operator override: DOTZ_SKILLS_PATHS=dir1<sep>dir2 (path.delimiter — ';' on Windows,
    // ':' on Unix). Each existing dir is appended at the end (highest priority).
    if let Ok(extra) = std::env::var("DOTZ_SKILLS_PATHS") {
        roots.extend(extra_skills_roots(&extra));
    }
    roots
}

/// Parse a DOTZ_SKILLS_PATHS value into existing (dir, "dotz") pairs. Uses the OS path-list
/// separator so the same env value works on Windows and Unix CI.
fn extra_skills_roots(raw: &str) -> Vec<(PathBuf, &'static str)> {
    std::env::split_paths(raw)
        .filter(|p| !p.as_os_str().is_empty() && p.exists())
        .map(|p| (p, "dotz"))
        .collect()
}

/// Test-only public accessor for [`scan_roots`], so the marketplace integration test can assert
/// the presets root is included without duplicating the root-resolution logic. Hidden from
/// production callers via the `#[cfg(test)]` gate.
#[cfg(test)]
pub fn scan_roots_public() -> Vec<(PathBuf, &'static str)> {
    scan_roots()
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
            && ent.file_name().to_string_lossy().to_ascii_lowercase() == "skill.md"
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
fn scalar_str(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A YAML sequence filtered to its string elements only (non-strings dropped). A non-sequence → None.
/// Mirrors `Array.isArray(x) ? x.filter(typeof === "string") : undefined`.
fn string_array(v: &serde_yaml_ng::Value) -> Option<Vec<String>> {
    match v {
        serde_yaml_ng::Value::Sequence(seq) => Some(
            seq.iter()
                .filter_map(|x| match x {
                    serde_yaml_ng::Value::String(s) => Some(s.clone()),
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

/// Split off a leading frontmatter block between a leading `---` fence and a matching closing
/// `---` fence. Returns `(Some(yaml), body)` when present, else `(None, whole)`. Mirrors
/// gray-matter's leading-fence split (the opening `---` must be at the very start of the file).
fn split_frontmatter(raw: &str) -> (Option<Cow<'_, str>>, &str) {
    // The opening fence must be at the start: "---\n" or "---\r\n".
    let after_open = if let Some(rest) = raw.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = raw.strip_prefix("---\r\n") {
        rest
    } else {
        return (None, raw);
    };

    // Body starts after the closing fence's own line terminator.
    fn strip_body_newline(tail: &str) -> &str {
        if let Some(b) = tail.strip_prefix("\r\n") {
            b
        } else if let Some(b) = tail.strip_prefix('\n') {
            b
        } else {
            tail // closer at EOF, empty body
        }
    }

    // The closing fence may be the very next line (empty frontmatter). Without this guard the
    // "\n---" search would miss a fence at position 0 of `after_open`.
    if after_open.starts_with("---\n") || after_open.starts_with("---\r\n") || after_open == "---" {
        return (
            Some(Cow::Borrowed("")),
            strip_body_newline(&after_open[3..]),
        );
    }

    // Find the closing fence: a line that is exactly "---" (preceded by a newline). We search for
    // "\n---" and require it to be followed by end-of-string or a newline.
    let mut search_from = 0usize;
    while let Some(rel) = after_open[search_from..].find("\n---") {
        let idx = search_from + rel; // position of the '\n' before the closing ---
        let after_dashes = idx + 4; // just past "\n---"
        let tail = &after_open[after_dashes..];
        // Valid closer iff the "---" is the whole line: end-of-file, or next char starts a newline.
        if tail.is_empty() || tail.starts_with('\n') || tail.starts_with('\r') {
            let mut yaml_end = idx;
            if idx > 0 && after_open.as_bytes()[idx - 1] == b'\r' {
                yaml_end -= 1; // don't leave a dangling \r before the closing fence
            }
            let yaml = &after_open[..yaml_end];
            // Normalize CRLF / lone CR line endings in the frontmatter content so the YAML parser
            // sees clean LF endings instead of embedded \r characters.
            let yaml = if yaml.contains('\r') {
                Cow::Owned(yaml.replace("\r\n", "\n").replace('\r', "\n"))
            } else {
                Cow::Borrowed(yaml)
            };
            return (Some(yaml), strip_body_newline(tail));
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
    let fm: serde_yaml_ng::Value = fm_src
        .and_then(|y| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&y).ok())
        .unwrap_or(serde_yaml_ng::Value::Mapping(Default::default()));

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
///
/// The walk (directory listing) is sequential and cheap; the per-file cost is read-to-string + YAML
/// parse, which is embarrassingly parallel. We collect every (file, source) in priority order, parse
/// them across the available cores with `std::thread::scope` (stdlib — no rayon dep), then insert in
/// the original order so the dedupe priority is byte-identical to the sequential version.
fn build_index() -> BTreeMap<String, Skill> {
    build_index_from(scan_roots())
}

/// Build the skill index from a pre-collected set of roots. Splitting this out of `build_index`
/// lets the parallel-vs-sequential test compare both scans over the *same* root snapshot, so a
/// concurrent `DOTZ_PI`/`DOTZ_SKILLS_PATHS` mutation by another test can't make the two diverge
/// and poison the shared `TEST_LOCK`. Production `build_index()` still resolves roots itself.
fn build_index_from(roots: Vec<(PathBuf, &'static str)>) -> BTreeMap<String, Skill> {
    // 1. Collect candidate files in priority order (low→high). The walk is fast; parsing isn't.
    let mut files: Vec<(PathBuf, &'static str)> = Vec::new();
    for (dir, source) in roots {
        for file in find_skill_files(&dir) {
            files.push((file, source));
        }
    }
    let n = files.len();
    let mut parsed: Vec<Option<Skill>> = (0..n).map(|_| None).collect();

    // 2. Parse in parallel, each thread owning a disjoint slice of inputs+outputs (no locking).
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(8);
    let chunk = n.div_ceil(threads.max(1)).max(1);
    if n > 0 {
        std::thread::scope(|s| {
            for (in_c, out_c) in files.chunks(chunk).zip(parsed.chunks_mut(chunk)) {
                s.spawn(move || {
                    for (i, (file, source)) in in_c.iter().enumerate() {
                        if let Some(skill) = parse_skill_file(file, source) {
                            if platforms_ok(&skill) {
                                out_c[i] = Some(skill);
                            }
                        }
                    }
                });
            }
        });
    }

    // 3. Insert in priority order — later (higher-priority) roots overwrite earlier same-named skills.
    let mut map: BTreeMap<String, Skill> = BTreeMap::new();
    for skill in parsed.into_iter().flatten() {
        map.insert(skill.name.clone(), skill);
    }
    map
}

/// Module-owned cache of the deduped index, built once on first access (idempotent — mirrors the
/// Node `SkillLoader.load()` one-shot guard).
static CACHE: OnceLock<Mutex<BTreeMap<String, Skill>>> = OnceLock::new();

fn index() -> &'static Mutex<BTreeMap<String, Skill>> {
    CACHE.get_or_init(|| Mutex::new(build_index()))
}

/// Lock the skill index, recovering from a poisoned mutex. A panic while building the index
/// (e.g. inside `parse_skill_file` or `build_index`) must not permanently brick the skills REST
/// endpoints or system-prompt injection.
fn index_guard() -> std::sync::MutexGuard<'static, BTreeMap<String, Skill>> {
    index()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Force a rebuild of the cached skill index so newly-created or removed skills are visible
/// immediately. Callers (e.g. `create_skill`) can invoke this after mutating the on-disk skill pool.
pub fn reload_index() {
    let mut guard = index_guard();
    *guard = build_index();
}

/// GET /api/skills — `{ "skills": [{ name, description, source, tags?, isUmbrella }] }`, sorted by
/// name (the BTreeMap iterates in key order, which matches the JS `localeCompare` for these ASCII
/// names). `tags` omitted when absent; `isUmbrella` always present.
async fn list_skills() -> Json<serde_json::Value> {
    let guard = index_guard();
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
        let guard = index_guard();
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

/// Max skills listed in the system-prompt index. Mirrors `INDEX_CAP` in skills.ts — a large pool
/// (96+ skills here) must NOT dump every full description into every turn's prompt; the overflow is
/// summarized and any skill is still loadable by name via the `skill` tool.
const INDEX_CAP: usize = 80;

/// Skill rows formatted for the composer slash-palette (`GET /api/sessions/:id/commands`).
/// Each row is `{ name, description, kind: "skill" }` so the UI can mix presets and skills.
pub fn command_views() -> Vec<serde_json::Value> {
    let guard = index_guard();
    guard
        .values()
        .map(|s| {
            json!({
                "name": s.name.clone(),
                "description": s.description.clone(),
                "kind": "skill",
            })
        })
        .collect()
}

/// Render the skill index (names + truncated descriptions) for system-prompt injection. Byte-faithful
/// to skillLoader.renderIndex(): capped at `INDEX_CAP` rows, each description clipped to 160 chars,
/// with an overflow footer + count in the heading. Empty string when no skills.
pub fn render_index() -> String {
    let guard = index_guard();
    let total = guard.len();
    if total == 0 {
        return String::new();
    }
    let mut lines: Vec<String> = guard
        .values()
        .take(INDEX_CAP)
        .map(|s| {
            let desc: String = s.description.chars().take(160).collect();
            format!("- {}: {}", s.name, desc)
        })
        .collect();
    if total > INDEX_CAP {
        lines.push(format!(
            "- …and {} more — call the `skill` tool by name, or GET /api/skills to browse/filter the full pool.",
            total - INDEX_CAP
        ));
    }
    let heading = if total > INDEX_CAP {
        format!("{total} skills, showing first {INDEX_CAP}")
    } else {
        format!("{total} skills")
    };
    format!(
        "\n# dotz unified skill index ({heading})\nInvoke a skill's full instructions by calling the `skill` tool with its name. Skills are auto-discovered from opencode, claude, codex, ecc, superpowers, hermes, and bundled .pi pools.\n{}\n",
        lines.join("\n")
    )
}

/// Load a skill's SKILL.md body (frontmatter stripped) by name — backs the `skill` tool's execute.
/// None when the skill is unknown or the file can't be read. Mirrors skillLoader.loadBody().
pub fn load_body(name: &str) -> Option<String> {
    let path = {
        let guard = index_guard();
        guard.get(name).map(|s| s.path.clone())
    }?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let (_, body) = split_frontmatter(&raw);
    Some(body.to_string())
}

/// Stateless router for the unified skills endpoints.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/skills", get(list_skills))
        .route("/api/skills/{name}", get(get_skill))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    /// Serialize tests that mutate the process-global `DOTZ_PI` / `DOTZ_SKILLS_PATHS` env vars
    /// so concurrent index builds don't see each other's isolated directories. Recover from
    /// poison (matching `index_guard`) so a single panicking sibling can't brick the group —
    /// this is what kept the cascade going once `parallel_index_matches_sequential` panicked.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire `TEST_LOCK`, surviving a poison left by a sibling test that panicked while holding
    /// it. Poison is sticky on the mutex itself, so without recovery every later skills test
    /// would fail with `PoisonError` even though they did nothing wrong.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// DOTZ_SKILLS_PATHS must be parsed with the OS path-list separator so a multi-dir override
    /// works on both Windows (`;`) and Unix (`:`) without hand-rolling the delimiter.
    #[test]
    fn extra_skills_paths_uses_os_path_delimiter() {
        let _guard = test_lock();
        let dir1 = std::env::temp_dir().join(format!("dotz-skills-a-{}", uuid::Uuid::new_v4()));
        let dir2 = std::env::temp_dir().join(format!("dotz-skills-b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::create_dir_all(&dir2).unwrap();

        let joined = std::env::join_paths([&dir1 as &std::path::Path, &dir2]).unwrap();
        let raw = joined.to_string_lossy().to_string();

        let extra = extra_skills_roots(&raw);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);

        assert_eq!(
            extra.len(),
            2,
            "both existing override dirs should be parsed"
        );
        assert_eq!(extra[0].0, dir1);
        assert_eq!(extra[1].0, dir2);
        assert!(extra.iter().all(|(_, source)| *source == "dotz"));
    }

    /// The parallel `build_index()` MUST produce the exact same deduped (name → path) mapping a plain
    /// sequential parse of the same roots produces — same priority order, same platform filter. This
    /// is the runnable guard for the `std::thread::scope` parallelization. Run with `--nocapture` to
    /// also see the sequential-vs-parallel timing over the real on-disk skill pool.
    #[test]
    fn parallel_index_matches_sequential() {
        let _guard = test_lock();
        // Snapshot the env-controlled roots ONCE so the sequential reference and the parallel
        // build see the exact same root set. Without this, a concurrently-running test that
        // mutates DOTZ_PI/DOTZ_SKILLS_PATHS can flip the roots between the two scans, making them
        // diverge and poisoning TEST_LOCK — flaking the whole skills test group in the full suite.
        let roots = scan_roots();
        let t0 = Instant::now();
        let mut seq: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (dir, source) in &roots {
            for file in find_skill_files(dir) {
                if let Some(s) = parse_skill_file(&file, source) {
                    if platforms_ok(&s) {
                        seq.insert(s.name.clone(), s.path.clone());
                    }
                }
            }
        }
        let seq_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let par = build_index_from(roots.clone());
        let par_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let par_paths: BTreeMap<String, PathBuf> = par
            .iter()
            .map(|(k, v)| (k.clone(), v.path.clone()))
            .collect();
        assert_eq!(
            seq, par_paths,
            "parallel build_index diverged from the sequential reference"
        );

        eprintln!(
            "skills index over {} skills: sequential {seq_ms:.1}ms, parallel {par_ms:.1}ms",
            par.len()
        );
    }

    /// `reload_index()` must rebuild the cached skill index after a new SKILL.md is written to a
    /// scan root. Without it, `load_body` stays stale until the process restarts.
    #[test]
    fn reload_index_picks_up_newly_created_skill() {
        let _guard = test_lock();
        let dir = std::env::temp_dir().join(format!("dotz-skills-reload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Point DOTZ_PI at an empty tree so the only scanned skills come from our override dir.
        let pi = std::env::temp_dir().join(format!("dotz-pi-reload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(pi.join("design-systems")).unwrap();

        let prev_pi = std::env::var("DOTZ_PI").ok();
        let prev_paths = std::env::var("DOTZ_SKILLS_PATHS").ok();
        std::env::set_var("DOTZ_PI", &pi);
        std::env::set_var("DOTZ_SKILLS_PATHS", &dir);

        // Create the first skill and prime the cache.
        let alpha = dir.join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::write(
            alpha.join("SKILL.md"),
            "---\nname: alpha-skill\ndescription: Alpha\n---\nbody alpha\n",
        )
        .unwrap();

        // The cache may already be initialized by earlier tests; force a rebuild with our
        // isolated environment before asserting on the new skill.
        reload_index();

        assert_eq!(load_body("alpha-skill").as_deref(), Some("body alpha\n"));

        // Add a second skill after the cache is already built.
        let beta = dir.join("beta");
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(
            beta.join("SKILL.md"),
            "---\nname: beta-skill\ndescription: Beta\n---\nbody beta\n",
        )
        .unwrap();

        assert!(
            load_body("beta-skill").is_none(),
            "stale index must not see a skill created after the first load"
        );

        reload_index();

        assert_eq!(
            load_body("beta-skill").as_deref(),
            Some("body beta\n"),
            "reload_index must surface the newly-created skill"
        );

        // Cleanup.
        match prev_pi {
            Some(p) => std::env::set_var("DOTZ_PI", p),
            None => std::env::remove_var("DOTZ_PI"),
        }
        match prev_paths {
            Some(p) => std::env::set_var("DOTZ_SKILLS_PATHS", p),
            None => std::env::remove_var("DOTZ_SKILLS_PATHS"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&pi);
    }

    /// A panic while holding the skill-index mutex (e.g. inside `build_index` or a parallel parse)
    /// must not permanently brick the skills REST endpoints or system-prompt injection. With poison
    /// recovery, lookups, list, and reload keep working after a previous lock owner panicked.
    #[test]
    fn index_guard_recovers_from_poisoned_mutex() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Ensure the cache is initialized.
        drop(index().lock().unwrap());

        let m = index();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("intentional skills index mutex poison");
        }));
        assert!(poisoned.is_err(), "skills index mutex should be poisoned");

        // index_guard must recover and return a usable guard.
        {
            let guard = index_guard();
            let _ = guard.len();
        }

        // reload_index must also recover and complete without panicking.
        reload_index();
    }

    /// `split_frontmatter` must recognize a closing fence on the very next line (empty frontmatter).
    /// Without this, a SKILL.md like `---\n---\nbody` is treated as having no frontmatter and the
    /// body starts with the closing fence line, breaking skill name/description extraction.
    #[test]
    fn split_frontmatter_handles_empty_frontmatter() {
        let (yaml, body) = split_frontmatter("---\n---\nbody");
        assert_eq!(
            yaml.as_deref(),
            Some(""),
            "empty frontmatter should yield empty yaml"
        );
        assert_eq!(body, "body", "body should follow the closing fence");

        let (yaml, body) = split_frontmatter("---\r\n---\r\nbody");
        assert_eq!(
            yaml.as_deref(),
            Some(""),
            "empty CRLF frontmatter should yield empty yaml"
        );
        assert_eq!(body, "body", "CRLF body should follow the closing fence");

        let (yaml, body) = split_frontmatter("---\n---");
        assert_eq!(
            yaml.as_deref(),
            Some(""),
            "EOF closing fence should yield empty yaml"
        );
        assert_eq!(body, "", "no body when closing fence is EOF");
    }

    /// Non-empty frontmatter must still parse correctly after the empty-frontmatter fix.
    #[test]
    fn split_frontmatter_preserves_nonempty_frontmatter() {
        let (yaml, body) = split_frontmatter("---\nname: alpha\ndescription: A\n---\nbody alpha\n");
        assert_eq!(yaml.as_deref(), Some("name: alpha\ndescription: A"));
        assert_eq!(body, "body alpha\n");

        let (yaml, body) = split_frontmatter("---\r\nname: beta\r\n---\r\nbody beta\r\n");
        assert_eq!(yaml.as_deref(), Some("name: beta"));
        assert_eq!(body, "body beta\r\n");
    }
}
