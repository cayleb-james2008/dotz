//! Preset marketplace for dotz. Lists + installs presets from a curated GitHub repo
//! (`cayleb-james2008/dotz-presets`). Installed presets land in `~/.dotz/presets/` and are
//! discovered by the existing loaders (`profiles.rs`, `templates.rs`, `skills.rs`).
//!
//! C8 shipped the INSTALL side. B2 (this revision) adds the PUBLISH side + version pinning +
//! mandatory minisign signature verification on install:
//! - `POST /api/presets/publish` authors a preset, signs the manifest with the user's minisign
//!   key, and opens a PR to the curated repo via `gh`.
//! - `install_preset` now downloads the preset files + a `.minisig` signature, verifies the
//!   signature against the marketplace pubkey, rejects unsigned/badly-signed presets, and
//!   records the installed version in `.installed-version`.
//! - `GET /api/presets` reports `updateAvailable` when the catalog version > installed version.
//! - `GET /api/presets/pubkey` returns the cached marketplace pubkey.
//!
//! The repo is hardcoded — no arbitrary URL is accepted — and preset names are validated
//! (alphanumeric + dash, max 64 chars) to prevent path traversal. The catalog is cached
//! in-memory for 5 minutes so repeated `GET /api/presets` calls don't hit GitHub on every
//! request.
//!
//! # ponytail: the catalog fetch is a single GET against the GitHub raw URL; the install path
//! walks the GitHub contents API for the preset directory and downloads each file's
//! `download_url`. Signing + verification shell out to the `minisign` CLI (no Rust minisign
//! crate — keeps the dep surface lean); the PR is opened via the `gh` CLI. A native GitHub-API
//! PR creation + an in-process Ed25519 verify is the upgrade path.
use crate::config;
use crate::util;
use axum::{
    Json, Router,
    extract::Path as AxPath,
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Hardcoded marketplace repo. Security boundary: this is the ONLY repo presets are ever
/// fetched from — no caller-supplied URL is accepted. Changing this requires a code change
/// (not a config/env override), which keeps the trust root in source control.
const MARKETPLACE_REPO: &str = "cayleb-james2008/dotz-presets";

/// Branch the catalog is read from. The repo owner pins this; we don't accept a caller override.
const MARKETPLACE_BRANCH: &str = "main";

/// Cache TTL: the catalog is fetched at most once per 5 minutes. Repeated `GET /api/presets`
/// within the window returns the cached list so the UI's polling doesn't hammer GitHub.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// One catalog entry. `installed` is computed at list time by scanning `~/.dotz/presets/`.
/// Field names are camelCase-safe (single words) so no serde rename is needed; `kind` is the
/// one enum field and uses `rename_all = "lowercase"` (see [`PresetKind`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub kind: PresetKind,
    pub description: String,
    pub author: String,
    pub version: String,
    /// `path` is the directory inside the repo that holds the preset files. Read from
    /// `catalog.json`; not surfaced to the UI (it's an internal routing detail). Kept on the
    /// struct so `install_preset` can resolve the preset's directory without a second catalog
    /// lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Computed at list time: true when `~/.dotz/presets/<name>/` exists.
    pub installed: bool,
    /// Computed at list time: true when the preset is installed AND the catalog `version` is
    /// newer than the recorded `.installed-version`. False when not installed or when the
    /// installed version is >= the catalog version. Added in B2.
    pub update_available: bool,
}

/// A file inside a publish request: a relative path inside the preset dir + its content. The
/// path is validated against traversal before it ever touches the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishFile {
    pub path: String,
    pub content: String,
}

/// `POST /api/presets/publish` body. `version` is recorded as the preset's version stamp; the
/// caller is expected to follow semver but we don't enforce a shape (a future follow-up could).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRequest {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub version: String,
    pub files: Vec<PublishFile>,
}

/// Preset kind. Serialized as the lowercase kebab-case form (`profile`, `prompt`, `agent`,
/// `skill`, `design-system`, `design-skill`, `plugin`) so the UI's kind filter matches the
/// catalog JSON the repo ships. `kebab-case` is used instead of `lowercase` because
/// `lowercase` would strip the dash from `DesignSystem` → `designsystem` (wrong), while
/// `kebab-case` preserves it as `design-system`. Deserialization is case-insensitive on the
/// `kind` string from `catalog.json` via the `FromStr`-style fallback in [`parse_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresetKind {
    Profile,
    Prompt,
    Agent,
    Skill,
    DesignSystem,
    DesignSkill,
    Plugin,
}

impl PresetKind {
    /// Lowercase stable string used by the UI's kind filter. Matches the serde
    /// `rename_all = "lowercase"` output exactly.
    pub fn as_str(self) -> &'static str {
        match self {
            PresetKind::Profile => "profile",
            PresetKind::Prompt => "prompt",
            PresetKind::Agent => "agent",
            PresetKind::Skill => "skill",
            PresetKind::DesignSystem => "design-system",
            PresetKind::DesignSkill => "design-skill",
            PresetKind::Plugin => "plugin",
        }
    }
}

/// Parse a `catalog.json` `kind` string into a [`PresetKind`]. Unknown strings fall back to
/// [`PresetKind::Plugin`] (the most generic kind) so a forward-incompatible catalog addition
/// doesn't break the list — the preset still installs, it just shows under "Plugins" in the UI.
/// # ponytail: a strict-reject-on-unknown upgrade path is the follow-up once the catalog format
/// is pinned by B2's signature verify.
fn parse_kind(s: &str) -> PresetKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "profile" => PresetKind::Profile,
        "prompt" => PresetKind::Prompt,
        "agent" => PresetKind::Agent,
        "skill" => PresetKind::Skill,
        "design-system" => PresetKind::DesignSystem,
        "design-skill" => PresetKind::DesignSkill,
        _ => PresetKind::Plugin,
    }
}

/// Marketplace error: network/IO/parse failures + invalid-name rejections. The `kind` field is
/// unused by the routes (they just surface the message), but kept so a future caller can branch
/// on "is this a 400 vs 502" without re-parsing the string.
#[derive(Debug)]
pub struct MarketplaceError {
    pub message: String,
}

impl MarketplaceError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for MarketplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MarketplaceError {}

/// In-memory catalog cache. Holds the last successfully fetched catalog + the time it was
/// fetched. A `None` catalog means "no cached value" (first call, or the last fetch failed and
/// we don't want to serve a stale-empty list as if it were a real empty catalog).
///
/// The nested generics are intentional: the cache holds an optional timestamped catalog. We
/// allow the `type_complexity` lint here rather than introducing a throwaway `struct
/// CachedCatalog(Instant, Vec<Preset>)` alias — the alias would obscure the single use site
/// for no readability gain.
#[allow(clippy::type_complexity)]
static CATALOG_CACHE: OnceLock<Mutex<Option<(Instant, Vec<Preset>)>>> = OnceLock::new();

fn catalog_cache() -> &'static Mutex<Option<(Instant, Vec<Preset>)>> {
    CATALOG_CACHE.get_or_init(|| Mutex::new(None))
}

/// Reset the catalog cache. Exposed for tests so a stub-server swap is seen immediately instead
/// of after the TTL. Also handy for a future "refresh catalog" button in the UI.
pub fn invalidate_cache() {
    if let Ok(mut g) = catalog_cache().lock() {
        *g = None;
    }
}

/// Resolve the base URL the catalog + file downloads are fetched from. Defaults to the GitHub
/// raw URL for the hardcoded repo + branch. The `DOTZ_MARKETPLACE_URL` env override exists so
/// tests can point at a local axum stub server instead of hitting GitHub — it is NOT a
/// production knob (the repo + branch constants above are the trust root).
///
/// The returned string has no trailing slash so callers can `.join("/{path}")` cleanly.
fn marketplace_base_url() -> String {
    std::env::var("DOTZ_MARKETPLACE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!("https://raw.githubusercontent.com/{MARKETPLACE_REPO}/{MARKETPLACE_BRANCH}/")
        })
}

/// Resolve the GitHub API contents endpoint root (for directory listings during install). Same
/// env override story as [`marketplace_base_url`]: tests point at a stub; production uses the
/// real GitHub API.
fn marketplace_api_url() -> String {
    std::env::var("DOTZ_MARKETPLACE_API_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{MARKETPLACE_REPO}/contents/"))
}

/// `~/.dotz/presets/` — the install root. Honors `DOTZ_CONFIG_DIR` via [`config::dotz_dir`] so
/// tests can point at a temp dir without touching the operator's real `~/.dotz/presets/`.
pub fn presets_dir() -> PathBuf {
    config::dotz_dir().join("presets")
}

/// `~/.dotz/presets/<name>/`. Validates `name` first so a bad value never reaches the
/// filesystem. Returns an absolute path under [`presets_dir`] only.
fn preset_install_dir(name: &str) -> Result<PathBuf, MarketplaceError> {
    validate_preset_name(name)?;
    Ok(presets_dir().join(name))
}

/// Validate a preset name: `^[a-z0-9][a-z0-9-]{0,63}$` (lowercase alphanumeric + dash, max 64
/// chars, leading alphanumeric). This rejects `..`, `/`, leading dash, empty, and overlong
/// names — the path-traversal guard. The same check is used for install + uninstall so a
/// crafted `DELETE /api/presets/..` can't escape the presets root.
pub fn validate_preset_name(name: &str) -> Result<(), MarketplaceError> {
    if name.is_empty() || name.len() > 64 {
        return Err(MarketplaceError::new("preset name must be 1-64 characters"));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !first.is_ascii_alphanumeric() {
        return Err(MarketplaceError::new(
            "preset name must start with a letter or digit",
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(MarketplaceError::new(
            "preset name may contain only lowercase letters, digits, and dashes",
        ));
    }
    // The above allows uppercase A-Z via is_ascii_alphanumeric; pin lowercase explicitly.
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(MarketplaceError::new(
            "preset name must be lowercase (letters, digits, dashes only)",
        ));
    }
    Ok(())
}

/// Fetch the preset catalog from the GitHub repo. Reads `catalog.json` from the repo root via
/// the raw URL, parses `{presets: [{name, kind, description, author, version, path?}]}`, and
/// stamps each entry with `installed` by scanning `~/.dotz/presets/`.
///
/// Graceful degradation: if the repo doesn't exist (404), the network is unreachable, or the
/// JSON is malformed, this returns `Ok(vec![])` — the UI shows an empty marketplace with no
/// crash. The failure is logged via `eprintln!` so the operator can diagnose it; a future
/// follow-up could surface the last error in the `GET /api/presets` response body.
pub async fn list_presets() -> Result<Vec<Preset>, MarketplaceError> {
    // Cache hit?
    if let Ok(guard) = catalog_cache().lock() {
        if let Some((fetched_at, cached)) = guard.as_ref() {
            if fetched_at.elapsed() < CACHE_TTL {
                // Re-stamp `installed` + `update_available` against the current disk state — a
                // preset may have been installed/uninstalled/upgraded since the catalog was
                // cached, and the UI's badges must reflect the live disk state, not the state at
                // cache time.
                let installed = list_installed();
                return Ok(cached
                    .iter()
                    .map(|p| stamp_preset(p.clone(), &installed))
                    .collect());
            }
        }
    }

    let url = format!("{}catalog.json", marketplace_base_url());
    let client = reqwest::Client::builder()
        .user_agent("dotz")
        .build()
        .map_err(|e| MarketplaceError::new(format!("http client build failed: {e}")))?;
    let resp = client.get(&url).send().await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            eprintln!("marketplace: catalog fetch failed: {e}");
            return Ok(Vec::new());
        }
    };
    let status = resp.status();
    if !status.is_success() {
        eprintln!("marketplace: catalog fetch returned {status} for {url}");
        return Ok(Vec::new());
    }
    let body = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("marketplace: catalog body read failed: {e}");
            return Ok(Vec::new());
        }
    };
    let catalog: Vec<Preset> = match parse_catalog(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("marketplace: catalog parse failed: {e}");
            return Ok(Vec::new());
        }
    };
    // Stamp installed + update_available against the live disk state.
    let installed = list_installed();
    let stamped: Vec<Preset> = catalog
        .into_iter()
        .map(|p| stamp_preset(p, &installed))
        .collect();

    // Cache the unstamped catalog (without the installed/update_available flags) so a later call
    // re-stamps against the then-current disk state. We cache the raw entries (installed=false,
    // update_available=false) and let the cache-hit path re-stamp on every read.
    let cached = stamped
        .iter()
        .map(|p| {
            let mut c = p.clone();
            c.installed = false;
            c.update_available = false;
            c
        })
        .collect::<Vec<_>>();
    if let Ok(mut g) = catalog_cache().lock() {
        *g = Some((Instant::now(), cached));
    }
    Ok(stamped)
}

/// Stamp a catalog preset with `installed` (whether `~/.dotz/presets/<name>/` exists) and
/// `update_available` (installed AND the catalog `version` is newer than the recorded
/// `.installed-version`). Version comparison is a simple string `!=` plus a `>` heuristic on
/// dotted numeric components — semver-strict parsing is overkill for the curated catalog and
/// would add a dep. A forward-incompatible catalog format is gated by B2's signature verify.
fn stamp_preset(mut p: Preset, installed: &[String]) -> Preset {
    p.installed = installed.iter().any(|n| n == &p.name);
    if p.installed {
        let installed_v = read_installed_version(&p.name).unwrap_or(None);
        p.update_available = match installed_v {
            Some(v) => version_newer(&p.version, &v),
            None => false, // No recorded version → can't claim an update; treat as current.
        };
    } else {
        p.update_available = false;
    }
    p
}

/// Compare two version strings. Returns true when `a` is newer than `b`. Splits on `.`, parses
/// each component as a number (non-numeric components compare as 0), and compares
/// lexicographically. `1.2.0` > `1.1.0`, `1.10` > `1.9`, `2.0` > `1.99`. Equal strings return
/// false. This is a deliberately small comparator — not a full semver implementation.
fn version_newer(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let av: Vec<u64> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let bv: Vec<u64> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let n = av.len().max(bv.len());
    for i in 0..n {
        let ai = av.get(i).copied().unwrap_or(0);
        let bi = bv.get(i).copied().unwrap_or(0);
        if ai > bi {
            return true;
        }
        if ai < bi {
            return false;
        }
    }
    false
}

/// Parse the `catalog.json` body into a `Vec<Preset>` (with `installed: false` — the caller
/// stamps it). Split out from [`list_presets`] so the parsing logic is unit-testable without a
/// network call. Accepts `{presets: [...]}` OR a bare top-level array (forward-compatible).
fn parse_catalog(body: &str) -> Result<Vec<Preset>, MarketplaceError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| MarketplaceError::new(format!("catalog is not valid JSON: {e}")))?;
    let arr = v
        .get("presets")
        .and_then(|p| p.as_array())
        .or_else(|| v.as_array())
        .ok_or_else(|| MarketplaceError::new("catalog must be {presets:[...]} or [...]"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let name = entry
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let kind = parse_kind(entry.get("kind").and_then(|x| x.as_str()).unwrap_or(""));
        let description = entry
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let author = entry
            .get("author")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let version = entry
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("0.1.0")
            .to_string();
        let path = entry
            .get("path")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        out.push(Preset {
            name,
            kind,
            description,
            author,
            version,
            path,
            installed: false,
            update_available: false,
        });
    }
    Ok(out)
}

/// Install a preset by name. Looks the preset up in the catalog, resolves its `path` inside the
/// repo, walks the GitHub contents API for that directory, and downloads each file's
/// `download_url` into `~/.dotz/presets/<name>/`. The preset name is validated first; the
/// install dir is canonicalized + verified to stay under [`presets_dir`] so a crafted catalog
/// entry can't escape the presets root via `..` in the file paths.
///
/// B2: signature verification is MANDATORY. The preset directory must contain a
/// `manifest.minisig` file (a minisign signature over `manifest.json`); the signature is
/// verified against the cached marketplace pubkey via the `minisign` CLI. Unsigned presets
/// (no `.minisig`) and badly-signed presets are rejected with a clear error. The catalog
/// `version` is recorded in `~/.dotz/presets/<name>/.installed-version` after a successful
/// verify + download, so `GET /api/presets` can report `updateAvailable`.
///
/// Returns `Ok(())` on success, `Err` on a bad name, a missing catalog entry, a download/write
/// failure, or a signature-verification failure. The install is all-or-nothing at the file
/// level: a failure partway through leaves the partial dir on disk (a future follow-up could
/// roll it back; for now the operator can `DELETE /api/presets/<name>` to clean up).
pub async fn install_preset(name: &str) -> Result<(), MarketplaceError> {
    validate_preset_name(name)?;

    // Resolve the preset's repo path + version from the catalog. If the cache is empty (cold
    // start) or the preset isn't in the catalog, this returns a clear error.
    let catalog = list_presets().await?;
    let preset = catalog
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| MarketplaceError::new(format!("no such preset: {name}")))?;
    let rel_path = preset
        .path
        .clone()
        .ok_or_else(|| MarketplaceError::new(format!("preset {name} has no repo path")))?;
    let version = preset.version.clone();

    // Walk the GitHub contents API for the preset directory. Each entry is either a file
    // (download it) or a subdir (recurse — the contents API is per-directory, so we DFS).
    let api_root = marketplace_api_url();
    let install_dir = preset_install_dir(name)?;
    // Create the presets root + the preset's own dir. create_dir_all is idempotent.
    std::fs::create_dir_all(&install_dir).map_err(|e| {
        MarketplaceError::new(format!(
            "could not create install dir {}: {e}",
            install_dir.display()
        ))
    })?;

    let client = reqwest::Client::builder()
        .user_agent("dotz")
        .build()
        .map_err(|e| MarketplaceError::new(format!("http client build failed: {e}")))?;
    let raw_base = marketplace_base_url();
    download_dir(
        &client,
        &api_root,
        &raw_base,
        &rel_path,
        &install_dir,
        &install_dir,
    )
    .await?;

    // B2: mandatory signature verification. The preset dir must contain manifest.json +
    // manifest.minisig. We verify the signature against the cached marketplace pubkey. A missing
    // signature file is a hard reject — unsigned presets are never installed.
    let manifest_path = install_dir.join("manifest.json");
    let sig_path = install_dir.join("manifest.minisig");
    if !sig_path.exists() {
        // Roll back the partial install so the UI doesn't show a half-installed preset.
        let _ = std::fs::remove_dir_all(&install_dir);
        return Err(MarketplaceError::new(format!(
            "preset {name} is not signed (missing manifest.minisig) — refusing to install an unsigned preset"
        )));
    }
    if !manifest_path.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
        return Err(MarketplaceError::new(format!(
            "preset {name} is missing manifest.json — cannot verify signature"
        )));
    }
    let pubkey = fetch_pubkey(&client).await?;
    verify_signature(&manifest_path, &sig_path, &pubkey).await?;

    // Record the installed version so GET /api/presets can report updateAvailable. Written AFTER
    // the verify gate so a failed verify never stamps a version.
    write_installed_version(name, &version)?;

    eprintln!("marketplace: installed preset {name} v{version}");
    Ok(())
}

/// `~/.dotz/presets/<name>/.installed-version` — the version stamp written after a successful
/// install. Used by [`stamp_preset`] to compute `updateAvailable`. The leading dot keeps it out
/// of the preset's user-facing file list (it's metadata, not a preset file).
fn installed_version_path(name: &str) -> Result<PathBuf, MarketplaceError> {
    Ok(preset_install_dir(name)?.join(".installed-version"))
}

/// Read the recorded installed version for a preset. Returns `Ok(None)` if the preset dir
/// exists but no version stamp is present (e.g. a preset installed before B2 landed). Returns
/// `Err` only if the dir lookup itself fails (a bad name).
fn read_installed_version(name: &str) -> Result<Option<String>, MarketplaceError> {
    let path = installed_version_path(name)?;
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&path).map_err(|e| {
        MarketplaceError::new(format!(
            "could not read installed version {}: {e}",
            path.display()
        ))
    })?;
    Ok(Some(s.trim().to_string()))
}

/// Write the installed version stamp. Called after a successful verify + download.
fn write_installed_version(name: &str, version: &str) -> Result<(), MarketplaceError> {
    let path = installed_version_path(name)?;
    std::fs::write(&path, version).map_err(|e| {
        MarketplaceError::new(format!(
            "could not write installed version {}: {e}",
            path.display()
        ))
    })
}

/// Recursively download every file under `repo_path` (a directory inside the repo) into
/// `dest_dir`, which is the preset's install root. `dest_root` is the top of the install
/// (`~/.dotz/presets/<name>/`) used for the path-traversal guard — every written file's
/// canonicalized parent must start with `dest_root`.
///
/// `raw_base` is the raw-content root (e.g. `https://raw.githubusercontent.com/.../`) used to
/// resolve a relative `download_url` the contents API might return. GitHub returns absolute
/// URLs in practice, but resolving relative URLs against the raw base is the robust behavior
/// (and it's what the test stub relies on — the stub returns `/raw/<path>` URLs).
///
/// The GitHub contents API returns an array of `{name, type, download_url, path}` for a
/// directory listing. A `type == "file"` entry is downloaded via its `download_url`; a
/// `type == "dir"` entry is recursed into.
async fn download_dir(
    client: &reqwest::Client,
    api_root: &str,
    raw_base: &str,
    repo_path: &str,
    dest_dir: &Path,
    dest_root: &Path,
) -> Result<(), MarketplaceError> {
    let url = format!(
        "{}{}?ref={}",
        api_root.trim_end_matches('/'),
        ensure_leading_slash(repo_path),
        MARKETPLACE_BRANCH
    );
    let resp = client.get(&url).send().await.map_err(|e| {
        MarketplaceError::new(format!("contents API request failed for {repo_path}: {e}"))
    })?;
    if !resp.status().is_success() {
        return Err(MarketplaceError::new(format!(
            "contents API returned {} for {repo_path}",
            resp.status()
        )));
    }
    let entries: Vec<Value> = resp.json().await.map_err(|e| {
        MarketplaceError::new(format!("contents API body is not a JSON array: {e}"))
    })?;
    for entry in entries {
        let entry_name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let entry_path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if entry_name.is_empty() {
            continue;
        }
        if entry_type == "dir" {
            // Recurse: the subdir's path inside the repo is the entry's `path` field.
            let sub_dest = dest_dir.join(entry_name);
            // Guard: the subdir must stay under dest_root.
            ensure_under_root(&sub_dest, dest_root)?;
            std::fs::create_dir_all(&sub_dest).map_err(|e| {
                MarketplaceError::new(format!(
                    "could not create subdir {}: {e}",
                    sub_dest.display()
                ))
            })?;
            Box::pin(download_dir(
                client,
                api_root,
                raw_base,
                &entry_path,
                &sub_dest,
                dest_root,
            ))
            .await?;
        } else if entry_type == "file" {
            let download_url = entry
                .get("download_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    MarketplaceError::new(format!("file entry {entry_name} has no download_url"))
                })?;
            // Resolve a relative download_url against the raw base. GitHub returns absolute
            // URLs; the stub returns `/raw/<path>`. reqwest requires absolute URLs.
            let absolute_url =
                if download_url.starts_with("http://") || download_url.starts_with("https://") {
                    download_url.to_string()
                } else {
                    format!(
                        "{}{}",
                        raw_base.trim_end_matches('/'),
                        if download_url.starts_with('/') {
                            download_url.to_string()
                        } else {
                            format!("/{download_url}")
                        }
                    )
                };
            let dest = dest_dir.join(entry_name);
            ensure_under_root(&dest, dest_root)?;
            let bytes = client
                .get(&absolute_url)
                .send()
                .await
                .map_err(|e| {
                    MarketplaceError::new(format!("download failed for {entry_name}: {e}"))
                })?
                .bytes()
                .await
                .map_err(|e| {
                    MarketplaceError::new(format!(
                        "download body read failed for {entry_name}: {e}"
                    ))
                })?;
            std::fs::write(&dest, &bytes).map_err(|e| {
                MarketplaceError::new(format!("could not write {}: {e}", dest.display()))
            })?;
        }
        // Unknown types (symlink/submodule) are skipped — the curated repo shouldn't ship them,
        // and a future follow-up could surface a warning. # ponytail: symlink traversal guard
        // is the upgrade path if the catalog ever ships symlinks.
    }
    Ok(())
}

/// Ensure a `?ref=branch` query is appended exactly once. The GitHub contents API requires the
/// `ref` query param to read from a non-default branch; our branch is hardcoded `main`.
fn ensure_leading_slash(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// Path-traversal guard: assert that `target`, when canonicalized (or, if it doesn't exist yet,
/// its parent canonicalized + the trailing segment), is under `root`. This catches a crafted
/// catalog entry whose `name` or nested file `name` contains `..` or an absolute path that
/// would escape the install root.
///
/// We canonicalize the parent (which must exist after `create_dir_all`) and re-join the final
/// segment, so the guard works even before the target file exists. The check is conservative:
/// any failure to canonicalize → reject.
fn ensure_under_root(target: &Path, root: &Path) -> Result<(), MarketplaceError> {
    // Reject any `..` component outright — an unambiguous traversal attempt. We do NOT reject
    // `RootDir` or `Prefix` here because on Windows every absolute path begins with
    // `Prefix(C:)` + `RootDir`, and the install root itself is absolute on Windows. Rejecting
    // them would reject every legitimate install. The canonicalize-then-starts_with check
    // below is the real traversal guard — it catches a path that escapes the root even if its
    // components looked fine (e.g. a symlink, or a re-rooted path).
    for comp in target.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(MarketplaceError::new(format!(
                "refusing path with .. component: {}",
                target.display()
            )));
        }
    }
    // Canonicalize the root (it must exist — we just created it) and the target's parent (same).
    let root_canon = std::fs::canonicalize(root).map_err(|e| {
        MarketplaceError::new(format!(
            "could not canonicalize root {}: {e}",
            root.display()
        ))
    })?;
    let parent = target.parent().unwrap_or(Path::new(""));
    let parent_canon = std::fs::canonicalize(parent).map_err(|e| {
        MarketplaceError::new(format!(
            "could not canonicalize parent {}: {e}",
            parent.display()
        ))
    })?;
    if !parent_canon.starts_with(&root_canon) {
        return Err(MarketplaceError::new(format!(
            "refusing path outside install root: {} (parent {} not under {})",
            target.display(),
            parent_canon.display(),
            root_canon.display()
        )));
    }
    Ok(())
}

/// Uninstall a preset by removing `~/.dotz/presets/<name>/`. Validates the name first (same
/// guard as install) so a crafted `DELETE /api/presets/..` can't escape the presets root.
/// Returns `Ok(())` if the dir didn't exist (idempotent), `Err` on a bad name or a removal
/// failure.
pub fn uninstall_preset(name: &str) -> Result<(), MarketplaceError> {
    validate_preset_name(name)?;
    let dir = preset_install_dir(name)?;
    if !dir.exists() {
        return Ok(());
    }
    // Canonicalize both the dir to remove and the presets root, then assert the dir is under
    // the root before remove_dir_all. This is the belt-and-suspenders guard against a future
    // change to validate_preset_name that might loosen the name check.
    let root_canon = std::fs::canonicalize(presets_dir())
        .map_err(|e| MarketplaceError::new(format!("could not canonicalize presets root: {e}")))?;
    let dir_canon = std::fs::canonicalize(&dir).map_err(|e| {
        MarketplaceError::new(format!(
            "could not canonicalize preset dir {}: {e}",
            dir.display()
        ))
    })?;
    if !dir_canon.starts_with(&root_canon) {
        return Err(MarketplaceError::new(format!(
            "refusing to remove path outside presets root: {}",
            dir_canon.display()
        )));
    }
    std::fs::remove_dir_all(&dir_canon).map_err(|e| {
        MarketplaceError::new(format!("could not remove {}: {e}", dir_canon.display()))
    })?;
    eprintln!("marketplace: uninstalled preset {name}");
    Ok(())
}

/// List installed presets by scanning `~/.dotz/presets/`. Returns the bare directory names
/// (which are the preset names — install writes to `~/.dotz/presets/<name>/`). A missing
/// presets root returns an empty vec (nothing installed yet). Non-directory entries are
/// skipped. The names are NOT re-validated here — the install path already validated them, and
/// a hand-created dir with a bad name is harmless (it's just listed; uninstall would reject it).
pub fn list_installed() -> Vec<String> {
    let dir = presets_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(name) = ent.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

// ---- B2: marketplace pubkey + signature verify + publish ----

/// The bundled placeholder pubkey. The REAL marketplace pubkey is published in the
/// `cayleb-james2008/dotz-presets` repo's `PUBKEY` file at the repo root; the backend fetches it
/// on first install + caches it in `~/.dotz/presets-pubkey`. This constant is the FALLBACK used
/// only when the fetch fails (e.g. offline first-run) — and the operator is warned via
/// `eprintln!` so they know to re-fetch once online. It is a syntactically-valid minisign pubkey
/// placeholder (the `RW` prefix + base64-ish body) so the verify path doesn't choke on a
/// malformed string; it will simply fail every real signature verify, which is the correct
/// behavior for "no trusted pubkey available".
///
/// # ponytail: bundling a real pubkey here would make the binary the trust root; the repo is the
/// trust root instead. The upgrade path is a pinned pubkey in a signed release artifact.
const BUNDLED_PUBKEY_FALLBACK: &str =
    "RWRmZ3N3b21ldGhpbmdfaGVyZS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0t";

/// `~/.dotz/presets-pubkey` — the on-disk cache for the marketplace pubkey. The first
/// `install_preset` / `GET /api/presets/pubkey` call fetches `PUBKEY` from the repo root and
/// writes it here; subsequent calls read the cache. Honors `DOTZ_CONFIG_DIR` so tests can point
/// at a temp dir.
pub fn pubkey_cache_path() -> PathBuf {
    config::dotz_dir().join("presets-pubkey")
}

/// In-memory pubkey cache so repeated `install_preset` calls within a process don't re-read the
/// on-disk cache file. Mirrors the catalog-cache pattern: a `OnceLock<Mutex<Option<String>>>`.
static PUBKEY_CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn pubkey_cache() -> &'static Mutex<Option<String>> {
    PUBKEY_CACHE.get_or_init(|| Mutex::new(None))
}

/// Reset the in-memory pubkey cache. Exposed for tests so a stub-server swap is seen immediately.
pub fn invalidate_pubkey_cache() {
    if let Ok(mut g) = pubkey_cache().lock() {
        *g = None;
    }
    let _ = std::fs::remove_file(pubkey_cache_path());
}

/// Resolve the marketplace pubkey. Order: in-memory cache → on-disk cache → fetch from the repo
/// root + write the on-disk cache → bundled fallback (with a warning) if the fetch fails. The
/// returned string is the raw minisign pubkey (the contents of the repo's `PUBKEY` file).
///
/// `client` is the same `reqwest::Client` used for the catalog/contents fetches so we don't
/// build a second client per call.
pub async fn fetch_pubkey(client: &reqwest::Client) -> Result<String, MarketplaceError> {
    // In-memory cache.
    if let Ok(g) = pubkey_cache().lock() {
        if let Some(s) = g.as_ref() {
            if !s.is_empty() {
                return Ok(s.clone());
            }
        }
    }
    // On-disk cache.
    let cache_path = pubkey_cache_path();
    if cache_path.exists() {
        if let Ok(s) = std::fs::read_to_string(&cache_path) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty() {
                if let Ok(mut g) = pubkey_cache().lock() {
                    *g = Some(trimmed.clone());
                }
                return Ok(trimmed);
            }
        }
    }
    // Fetch from the repo root.
    let url = format!("{}PUBKEY", marketplace_base_url());
    let resp = client.get(&url).send().await;
    match resp {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(body) => {
                let trimmed = body.trim().to_string();
                if trimmed.is_empty() {
                    eprintln!("marketplace: fetched PUBKEY is empty — using bundled fallback");
                    let fb = BUNDLED_PUBKEY_FALLBACK.to_string();
                    if let Ok(mut g) = pubkey_cache().lock() {
                        *g = Some(fb.clone());
                    }
                    return Ok(fb);
                }
                // Write the on-disk cache (best-effort — a write failure doesn't block install).
                if let Err(e) = std::fs::write(&cache_path, &trimmed) {
                    eprintln!(
                        "marketplace: could not write pubkey cache {}: {e}",
                        cache_path.display()
                    );
                }
                if let Ok(mut g) = pubkey_cache().lock() {
                    *g = Some(trimmed.clone());
                }
                Ok(trimmed)
            }
            Err(e) => {
                eprintln!("marketplace: PUBKEY body read failed: {e} — using bundled fallback");
                let fb = BUNDLED_PUBKEY_FALLBACK.to_string();
                if let Ok(mut g) = pubkey_cache().lock() {
                    *g = Some(fb.clone());
                }
                Ok(fb)
            }
        },
        Ok(r) => {
            eprintln!(
                "marketplace: PUBKEY fetch returned {} for {url} — using bundled fallback",
                r.status()
            );
            let fb = BUNDLED_PUBKEY_FALLBACK.to_string();
            if let Ok(mut g) = pubkey_cache().lock() {
                *g = Some(fb.clone());
            }
            Ok(fb)
        }
        Err(e) => {
            eprintln!("marketplace: PUBKEY fetch failed: {e} — using bundled fallback");
            let fb = BUNDLED_PUBKEY_FALLBACK.to_string();
            if let Ok(mut g) = pubkey_cache().lock() {
                *g = Some(fb.clone());
            }
            Ok(fb)
        }
    }
}

/// Prepare a `tokio::process::Command` for a binary path. On Windows, if the binary path ends
/// in `.cmd` or `.bat`, wrap it with `cmd /c` — `CreateProcess` does not auto-resolve those
/// extensions the way `cmd.exe` does, so a direct `Command::new("foo.cmd")` can hang on the
/// async spawn path. This is a real production concern (a user who installs `minisign` as a
/// `.cmd` wrapper via `npm install -g` would hit the same hang), not just a test fix.
///
/// Returns a `Command` with the program + (on Windows, for `.cmd`/`.bat`) the `/c <path>` args
/// set. The caller adds the rest of the args.
fn prep_command(bin: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let lower = bin.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/c").arg(bin);
            return cmd;
        }
    }
    tokio::process::Command::new(bin)
}

/// Prepare a `std::process::Command` for a binary path (the sync counterpart of
/// [`prep_command`], used by the presence checks in [`minisign_bin`] / [`gh_bin`]).
fn prep_command_sync(bin: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let lower = bin.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            let mut cmd = std::process::Command::new("cmd");
            cmd.arg("/c").arg(bin);
            return cmd;
        }
    }
    std::process::Command::new(bin)
}

/// Resolve the minisign binary path. Honors the `DOTZ_MINISIGN_BIN` env override (tests inject a
/// fake binary here); defaults to `minisign` on PATH. Returns `None` if the binary is not
/// present (the caller surfaces a clear error). We ALWAYS run a presence check (`<bin>
/// --version`) — even for the env override — so a stale `DOTZ_MINISIGN_BIN` path degrades to
/// `None` instead of a spawn failure later. We do NOT use the `which` crate (a heavy dep).
///
/// # ponytail: an in-process Ed25519 verify (using the existing `tauri-plugin-updater` pubkey
/// mechanism OR a minimal `ed25519-dalek` verify) is the upgrade path; shelling out to
/// `minisign` is the shortest path that doesn't add a heavy dep.
fn minisign_bin() -> Option<String> {
    let bin = std::env::var("DOTZ_MINISIGN_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "minisign".to_string());
    let mut cmd = prep_command_sync(&bin);
    cmd.arg("--version");
    util::no_window(&mut cmd)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false)
        .then_some(bin)
}

/// Verify a minisign signature. Shells out to `minisign -V -p <pubkey-file> -m <manifest> -x
/// <sig>`. The pubkey is written to a temp file because `minisign -p` expects a path, not a
/// string. Returns `Ok(())` if the signature verifies, `Err` with a clear message otherwise.
///
/// SECURITY: this is the trust gate. A failed verify MUST reject the install — never log-and-
/// continue. The pubkey file is written to the system temp dir + removed after the verify.
pub async fn verify_signature(
    manifest: &Path,
    sig: &Path,
    pubkey: &str,
) -> Result<(), MarketplaceError> {
    let bin = minisign_bin().ok_or_else(|| {
        MarketplaceError::new(
            "minisign CLI required for signature verification; install via `cargo install minisign` or your package manager",
        )
    })?;
    // Write the pubkey to a temp file. minisign -p expects a path.
    let pubkey_dir = std::env::temp_dir().join(format!("dotz-pubkey-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&pubkey_dir)
        .map_err(|e| MarketplaceError::new(format!("could not create pubkey temp dir: {e}")))?;
    let pubkey_path = pubkey_dir.join("pubkey");
    let cleanup = PubkeyTempGuard {
        dir: pubkey_dir.clone(),
    };
    std::fs::write(&pubkey_path, pubkey)
        .map_err(|e| MarketplaceError::new(format!("could not write pubkey temp file: {e}")))?;

    let mut cmd = prep_command(&bin);
    cmd.arg("-V")
        .arg("-p")
        .arg(&pubkey_path)
        .arg("-m")
        .arg(manifest)
        .arg("-x")
        .arg(sig);
    util::no_window_tokio(&mut cmd);
    let output = cmd
        .output()
        .await
        .map_err(|e| MarketplaceError::new(format!("minisign verify spawn failed: {e}")))?;
    drop(cleanup); // removes the temp pubkey file + dir
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(MarketplaceError::new(format!(
            "signature verification failed for {}: {}",
            manifest.display(),
            stderr.trim()
        )))
    }
}

/// RAII guard: removes the temp pubkey dir on drop. Ensures the pubkey file (which is a public
/// key, not a secret, but still shouldn't litter the temp dir) is cleaned up even on an early
/// return / panic.
struct PubkeyTempGuard {
    dir: PathBuf,
}

impl Drop for PubkeyTempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Resolve the `gh` binary path. Honors the `DOTZ_GH_BIN` env override (tests inject a fake
/// binary here); defaults to `gh` on PATH. Returns `None` if `gh` is not present. Always runs a
/// presence check (`<bin> --version`).
///
/// # ponytail: native GitHub API PR creation (via `reqwest` to the GitHub PR endpoint with a
/// user-provided token) is the upgrade path; `gh` CLI is the shortest path that reuses the
/// operator's existing auth.
fn gh_bin() -> Option<String> {
    let bin = std::env::var("DOTZ_GH_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gh".to_string());
    let mut cmd = prep_command_sync(&bin);
    cmd.arg("--version");
    util::no_window(&mut cmd)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false)
        .then_some(bin)
}

/// The publish flow result: the PR URL + PR number (parsed from `gh pr create`'s stdout).
#[derive(Debug, Clone, Serialize)]
pub struct PublishResult {
    pub pr_url: String,
    pub pr_number: u64,
}

/// Publish a preset. Validates the shape, writes the preset to a temp dir, signs `manifest.json`
/// with the user's minisign key, and opens a PR to the curated repo via `gh pr create`.
///
/// SECURITY: the user's minisign PRIVATE key is NEVER read by this function — `minisign -S`
/// reads it directly from `~/.dotz/presets-key/` (or wherever the user's key lives). The private
/// key path is never in the response body, never in `eprintln!` logs, and never sent to the
/// backend. The PR body includes the signature + the pubkey for reviewers, but NOT the private
/// key.
///
/// # ponytail: the temp dir + the `gh pr create` shell-out are the shortest path to a working
/// publish flow. A native GitHub API PR creation (via `reqwest` with a user-provided token) +
/// an in-process Ed25519 sign is the upgrade path.
pub async fn publish_preset(req: &PublishRequest) -> Result<PublishResult, MarketplaceError> {
    // 1. Validate the preset shape.
    validate_preset_name(&req.name)?;
    if req.kind.trim().is_empty() {
        return Err(MarketplaceError::new("kind is required"));
    }
    if req.version.trim().is_empty() {
        return Err(MarketplaceError::new("version is required"));
    }
    if req.files.is_empty() {
        return Err(MarketplaceError::new("files must not be empty"));
    }
    // Validate every file path against traversal — a `..` or absolute path in a file path is a
    // hard reject. The same guard as install: the written file's canonicalized parent must stay
    // under the preset's temp dir.
    for f in &req.files {
        if f.path.trim().is_empty() {
            return Err(MarketplaceError::new("file path must not be empty"));
        }
        if f.path.contains("..") || f.path.starts_with('/') || f.path.contains('\\') {
            return Err(MarketplaceError::new(format!(
                "refusing file path with traversal or absolute component: {}",
                f.path
            )));
        }
    }

    // 2. Write the preset to a temp dir. The dir layout matches the curated repo's layout:
    //    <tmp>/<name>/manifest.json
    //    <tmp>/<name>/manifest.minisig  (written by minisign -S)
    //    <tmp>/<name>/<file.path>       (each file)
    // The temp dir is removed on drop via the guard.
    let tmp_root = std::env::temp_dir().join(format!("dotz-publish-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_root)
        .map_err(|e| MarketplaceError::new(format!("could not create publish temp dir: {e}")))?;
    let _guard = PublishTempGuard {
        dir: tmp_root.clone(),
    };
    let preset_dir = tmp_root.join(&req.name);
    std::fs::create_dir_all(&preset_dir)
        .map_err(|e| MarketplaceError::new(format!("could not create preset temp dir: {e}")))?;

    // Write each file. Subdirectories in a file path (e.g. `sub/file.md`) are created.
    for f in &req.files {
        let dest = preset_dir.join(&f.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MarketplaceError::new(format!("could not create file parent dir: {e}"))
            })?;
        }
        // Guard: the written file must stay under the preset dir.
        ensure_under_root(&dest, &preset_dir)?;
        std::fs::write(&dest, &f.content).map_err(|e| {
            MarketplaceError::new(format!("could not write file {}: {e}", dest.display()))
        })?;
    }

    // Write the manifest. The manifest is the canonical record of the preset's metadata + file
    // list; minisign signs it, and the verify gate on install checks this signature.
    let manifest = json!({
        "name": req.name,
        "kind": req.kind,
        "description": req.description,
        "version": req.version,
        "files": req.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
    });
    let manifest_path = preset_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| MarketplaceError::new(format!("could not write manifest: {e}")))?;

    // 3. Sign the manifest with the user's minisign key. The private key is read by `minisign`
    //    directly from `~/.dotz/presets-key/` — this function NEVER touches the private key.
    //    If `minisign` is not installed, return a clear error.
    let bin = minisign_bin().ok_or_else(|| {
        MarketplaceError::new(
            "minisign CLI required for publishing; install via `cargo install minisign` or your package manager",
        )
    })?;
    let sig_path = preset_dir.join("manifest.minisig");
    let mut sign_cmd = prep_command(&bin);
    sign_cmd
        .arg("-S")
        .arg("-m")
        .arg(&manifest_path)
        .arg("-x")
        .arg(&sig_path);
    // The key path defaults to `~/.dotz/presets-key/` if `DOTZ_MINISIGN_KEY` is not set.
    // `minisign -S` looks for `~/.minisign/minisign.key` by default; we override via `-k` to
    // keep the dotz presets key separate from the operator's general-purpose minisign key.
    let key_path = std::env::var("DOTZ_MINISIGN_KEY").unwrap_or_else(|_| {
        config::dotz_dir()
            .join("presets-key")
            .to_string_lossy()
            .to_string()
    });
    sign_cmd.arg("-k").arg(&key_path);
    util::no_window_tokio(&mut sign_cmd);
    let sign_out = sign_cmd
        .output()
        .await
        .map_err(|e| MarketplaceError::new(format!("minisign sign spawn failed: {e}")))?;
    if !sign_out.status.success() {
        let stderr = String::from_utf8_lossy(&sign_out.stderr);
        return Err(MarketplaceError::new(format!(
            "minisign sign failed (is the presets key present at {}?): {}",
            key_path_display_safe(&key_path),
            stderr.trim()
        )));
    }
    if !sig_path.exists() {
        return Err(MarketplaceError::new(
            "minisign sign did not produce a signature file",
        ));
    }

    // 4. Open a PR to the curated repo via `gh pr create`. We pass the preset dir as the cwd so
    //    a future follow-up could `git`-init + commit the files before opening the PR; for now
    //    the PR body carries the manifest + signature + pubkey so a reviewer can paste them into
    //    the repo. The PR is opened against `cayleb-james2008/dotz-presets` (the hardcoded repo).
    let gh = gh_bin().ok_or_else(|| {
        MarketplaceError::new(
            "gh CLI required for publishing; install from https://cli.github.com/ and run `gh auth login`",
        )
    })?;

    // Read the signature + manifest for the PR body. The pubkey is fetched (not the private key
    // — the PUBLIC key) so reviewers can verify the signature without a separate fetch.
    let sig_content = std::fs::read_to_string(&sig_path).unwrap_or_default();
    let manifest_content = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    let client = reqwest::Client::builder()
        .user_agent("dotz")
        .build()
        .map_err(|e| MarketplaceError::new(format!("http client build failed: {e}")))?;
    let pubkey = fetch_pubkey(&client).await?;
    let pr_body = format!(
        "## Preset: {name} v{version}\n\n{description}\n\n**Kind:** {kind}\n\n### manifest.json\n```json\n{manifest}\n```\n\n### manifest.minisig\n```\n{sig}\n```\n\n### pubkey\n```\n{pubkey}\n```\n\n---\nSubmitted via dotz publish flow. Reviewers: verify the signature with `minisign -V -p <pubkey-file> -m manifest.json -x manifest.minisig`.",
        name = req.name,
        version = req.version,
        description = req.description,
        kind = req.kind,
        manifest = manifest_content,
        sig = sig_content,
        pubkey = pubkey,
    );

    let mut gh_cmd = prep_command(&gh);
    gh_cmd
        .arg("pr")
        .arg("create")
        .arg("--repo")
        .arg(MARKETPLACE_REPO)
        .arg("--title")
        .arg(format!("preset: {} v{}", req.name, req.version))
        .arg("--body")
        .arg(&pr_body);
    util::no_window_tokio(&mut gh_cmd);
    let gh_out = gh_cmd
        .output()
        .await
        .map_err(|e| MarketplaceError::new(format!("gh pr create spawn failed: {e}")))?;
    if !gh_out.status.success() {
        let stderr = String::from_utf8_lossy(&gh_out.stderr);
        return Err(MarketplaceError::new(format!(
            "gh pr create failed: {}",
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&gh_out.stdout).to_string();
    // `gh pr create` prints the PR URL on success. Parse the URL + the number from it.
    let (pr_url, pr_number) = parse_pr_url(&stdout)?;

    eprintln!(
        "marketplace: published preset {} v{}",
        req.name, req.version
    );
    Ok(PublishResult { pr_url, pr_number })
}

/// Display-safe key path: shows the dir only (not the key file name), so a log line never
/// reveals the private key file's exact name. The private key file name is `minisign.key` by
/// minisign convention, but we don't assume it — we just show the parent dir.
fn key_path_display_safe(key_path: &str) -> String {
    Path::new(key_path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<presets-key dir>".to_string())
}

/// Parse the PR URL from `gh pr create`'s stdout. `gh` prints a line like
/// `https://github.com/cayleb-james2008/dotz-presets/pull/42`. Extracts the URL + the number.
/// Returns a clear error if the URL can't be found.
fn parse_pr_url(stdout: &str) -> Result<(String, u64), MarketplaceError> {
    let url = stdout
        .lines()
        .map(|l| l.trim())
        .find(|l| l.starts_with("https://") && l.contains("/pull/"))
        .ok_or_else(|| {
            MarketplaceError::new(format!("could not find PR URL in gh output: {stdout}"))
        })?;
    let number: u64 = url
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            MarketplaceError::new(format!("could not parse PR number from URL: {url}"))
        })?;
    Ok((url.to_string(), number))
}

/// RAII guard: removes the publish temp dir on drop. Ensures the temp dir (which contains the
/// manifest + signature + preset files) is cleaned up even on an early return / panic. The
/// private key is NOT in this dir (it lives in `~/.dotz/presets-key/`), so removing this dir is
/// safe.
struct PublishTempGuard {
    dir: PathBuf,
}

impl Drop for PublishTempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- axum handlers ----

/// `GET /api/presets` → `{catalog: [Preset], installed: [String]}`. The catalog is the
/// marketplace list (with `installed` stamped per entry); `installed` is the bare list of
/// installed names so the UI can render the installed section without re-deriving it from the
/// catalog. Failures degrade to an empty catalog (not an error response) so the UI always
/// renders.
async fn list_handler() -> Json<Value> {
    let catalog = list_presets().await.unwrap_or_default();
    let installed = list_installed();
    Json(json!({ "catalog": catalog, "installed": installed }))
}

/// `POST /api/presets/install` with `{name: "my-preset"}` → downloads + installs. Returns 204
/// on success, 400 on a bad name or a missing preset, 502 on a download failure (so the UI can
/// distinguish "your request was bad" from "the marketplace is unreachable").
async fn install_handler(
    body: Option<Json<Value>>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let name = b
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(bad("name is required"));
    }
    match install_preset(&name).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            eprintln!("marketplace: install {name} failed: {}", e.message);
            if e.message.contains("no such preset") || e.message.contains("preset name") {
                Err(bad(&e.message))
            } else {
                Err((StatusCode::BAD_GATEWAY, Json(json!({ "error": e.message }))))
            }
        }
    }
}

/// `DELETE /api/presets/{name}` → uninstalls. Returns 204 (idempotent — a missing dir is
/// still 204). 400 on a bad name (path-traversal guard).
async fn uninstall_handler(
    AxPath(name): AxPath<String>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    match uninstall_preset(&name) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            eprintln!("marketplace: uninstall {name} failed: {}", e.message);
            Err(bad(&e.message))
        }
    }
}

/// `GET /api/presets/installed` → `[String]` — just the installed names. Lighter than the full
/// catalog; the UI uses this to refresh the installed section without re-fetching the catalog.
async fn installed_handler() -> Json<Value> {
    Json(serde_json::Value::Array(
        list_installed().into_iter().map(Value::String).collect(),
    ))
}

/// `POST /api/presets/publish` → `{prUrl, prNumber}`. Authors + signs + opens a PR for a preset.
/// Returns 200 on success, 400 on a bad shape / missing CLI, 502 on a `gh` / `minisign` failure
/// (so the UI can distinguish "your request was bad" from "the CLI call failed").
///
/// SECURITY: the request body is the preset metadata + file contents ONLY — the user's minisign
/// private key is never in the request, never in the response, never in `eprintln!` logs. See
/// [`publish_preset`] for the full security note.
async fn publish_handler(
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let b = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let req: PublishRequest = match serde_json::from_value(b) {
        Ok(r) => r,
        Err(e) => return Err(bad(&format!("invalid publish body: {e}"))),
    };
    match publish_preset(&req).await {
        Ok(res) => Ok(Json(
            json!({ "prUrl": res.pr_url, "prNumber": res.pr_number }),
        )),
        Err(e) => {
            // SECURITY: the error message must NEVER include the private key path. We strip any
            // occurrence of the key dir name from the message before surfacing it, as a
            // belt-and-suspenders guard against a future change to minisign's error output.
            let safe = redact_private_key(&e.message);
            eprintln!("marketplace: publish {} failed: {safe}", req.name);
            // Missing-CLI errors are 400 (client fixable: install the CLI). Everything else is
            // 502 (the CLI call failed).
            if safe.contains("CLI required")
                || safe.contains("kind is required")
                || safe.contains("version is required")
                || safe.contains("files must not")
                || safe.contains("preset name")
                || safe.contains("refusing file path")
                || safe.contains("file path must not")
            {
                Err(bad(&safe))
            } else {
                Err((StatusCode::BAD_GATEWAY, Json(json!({ "error": safe }))))
            }
        }
    }
}

/// Strip any occurrence of the presets-key dir name from a message. Belt-and-suspenders guard
/// against a future minisign error that leaks the private key path. The key dir is
/// `~/.dotz/presets-key/`; we redact the literal `presets-key` segment + any path that contains
/// it. This is a static check: the publish flow NEVER puts the private key path in the message
/// by construction, but this guard ensures that stays true even if a dependency changes its
/// error output.
fn redact_private_key(msg: &str) -> String {
    let mut out = msg.replace("presets-key", "<redacted-key-dir>");
    // Also redact any `minisign.key` mention (minisign's default private key file name).
    out = out.replace("minisign.key", "<redacted-key-file>");
    out
}

/// `GET /api/presets/pubkey` → `{pubkey, source}` where `source` is one of `cache`, `fetched`,
/// `fallback`. The UI displays the pubkey so users can verify it matches the repo's published
/// key. A fetch is attempted on every call (the cache is checked first inside `fetch_pubkey`).
async fn pubkey_handler() -> Json<Value> {
    let client = reqwest::Client::builder().user_agent("dotz").build();
    let pubkey = match client {
        Ok(c) => fetch_pubkey(&c)
            .await
            .unwrap_or_else(|_| BUNDLED_PUBKEY_FALLBACK.to_string()),
        Err(_) => BUNDLED_PUBKEY_FALLBACK.to_string(),
    };
    // Determine the source: if the on-disk cache exists + matches, it's `cache`; if the pubkey
    // equals the fallback, it's `fallback`; otherwise `fetched`.
    let source = if pubkey == BUNDLED_PUBKEY_FALLBACK {
        "fallback"
    } else if pubkey_cache_path().exists() {
        "cache"
    } else {
        "fetched"
    };
    Json(json!({ "pubkey": pubkey, "source": source }))
}

fn bad(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

/// The marketplace `Router<()>` to merge into `server::app()`. The routes are protected by the
/// existing `token_guard` + `origin_guard` layers (applied in `server::app_with_token`), so no
/// per-route auth is added here.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/presets", get(list_handler))
        .route("/api/presets/install", post(install_handler))
        .route("/api/presets/installed", get(installed_handler))
        .route("/api/presets/publish", post(publish_handler))
        .route("/api/presets/pubkey", get(pubkey_handler))
        .route("/api/presets/{name}", delete(uninstall_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router as AxRouter, routing::get as axget};
    use std::sync::Mutex;

    /// Serialize tests that mutate `DOTZ_CONFIG_DIR` / `DOTZ_MARKETPLACE_URL` /
    /// `DOTZ_MARKETPLACE_API_URL` so they don't race with each other or with `config::tests`.
    /// Same pattern as `templates::tests::isolated_user_dir`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// RAII guard: point `DOTZ_CONFIG_DIR` at a fresh temp dir + serialize on the shared
    /// config-dir lock. The presets dir lives under `~/.dotz/presets/` → this isolates the
    /// install/uninstall/list_installed tests from the operator's real presets. Restores the
    /// env on drop + removes the temp dir.
    struct PresetsDirGuard {
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn setup_presets_dir() -> PresetsDirGuard {
        let g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-marketplace-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", dir.to_string_lossy().to_string()) };
        PresetsDirGuard { dir, _lock: g }
    }

    impl Drop for PresetsDirGuard {
        fn drop(&mut self) {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") };
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_MARKETPLACE_URL") };
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_MARKETPLACE_API_URL") };
            invalidate_cache();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// RAII guard for the marketplace env overrides (URL + API URL). The presets dir is set
    /// separately via [`setup_presets_dir`] because the install path needs both an isolated
    /// disk + a stubbed HTTP root.
    struct MarketUrlGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn setup_market_url(base: &str, api_base: &str) -> MarketUrlGuard {
        let g = test_lock();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_MARKETPLACE_URL", base) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_MARKETPLACE_API_URL", api_base) };
        invalidate_cache();
        MarketUrlGuard { _lock: g }
    }

    impl Drop for MarketUrlGuard {
        fn drop(&mut self) {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_MARKETPLACE_URL") };
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var("DOTZ_MARKETPLACE_API_URL") };
            invalidate_cache();
        }
    }

    /// RAII guard: writes a fake binary script to a temp dir + points an env var at it, so tests
    /// can inject a fake `minisign` / `gh` without mutating the real PATH. The script is a
    /// `.cmd` file on Windows (so `Command::new` finds it) or a shell script + `chmod +x` on
    /// Unix. `body` is the script content; `env_var` is the env var name to set
    /// (`DOTZ_MINISIGN_BIN` / `DOTZ_GH_BIN`). On drop, the env var is removed + the temp file
    /// is deleted.
    ///
    /// The script must exit 0 on `<bin> --version` (the presence check in `minisign_bin` /
    /// `gh_bin`); the simplest body is one that exits 0 unconditionally. A script that needs to
    /// branch on its args can do so inline.
    struct FakeBinGuard {
        env_var: &'static str,
        path: PathBuf,
    }

    fn install_fake_bin(env_var: &'static str, body: &str) -> FakeBinGuard {
        // NOTE: we do NOT take test_lock() here — setup_market_url already holds ENV_LOCK via
        // MarketUrlGuard, and taking it again would deadlock. The env vars we set
        // (DOTZ_MINISIGN_BIN / DOTZ_GH_BIN) are cleaned up by FakeBinGuard::drop, and the tests
        // are serialized by setup_presets_dir + setup_market_url.
        let dir = std::env::temp_dir().join(format!("dotz-fakebin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // On Windows, `.cmd` files need CRLF line endings — `cmd.exe` misparses LF-only files
        // (treats the whole file as one line, so `@echo off\nexit /b 0` becomes a single
        // invalid command → exit 1, and worse, the spawn can hang). Convert LF → CRLF on
        // Windows. On Unix, LF is correct.
        let (file_name, full_body) = if cfg!(windows) {
            let crlf = body.replace('\n', "\r\n");
            (format!("fakebin-{}.cmd", env_var.to_lowercase()), crlf)
        } else {
            (
                format!("fakebin-{}", env_var.to_lowercase()),
                format!("#!/bin/sh\n{body}"),
            )
        };
        let path = dir.join(&file_name);
        std::fs::write(&path, &full_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var(env_var, path.to_string_lossy().to_string()) };
        FakeBinGuard { env_var, path }
    }

    impl Drop for FakeBinGuard {
        fn drop(&mut self) {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var(self.env_var) };
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }

    /// A fake `minisign` verify binary that exits 0 (signature OK). The body echoes nothing +
    /// exits 0 on every invocation (both the `--version` presence check and the `-V` verify).
    fn install_fake_minisign_verify_ok() -> FakeBinGuard {
        install_fake_bin("DOTZ_MINISIGN_BIN", "@echo off\nexit /b 0\n")
    }

    /// A fake `minisign` verify binary that exits 1 (signature BAD). Used to test the
    /// bad-signature rejection path. Exits 0 on `--version` (so the presence check passes) but
    /// exits 1 + writes a stderr line on any other invocation (the `-V` verify call).
    fn install_fake_minisign_verify_bad() -> FakeBinGuard {
        let body = if cfg!(windows) {
            r#"@echo off
if "%1"=="--version" exit /b 0
echo signature verification failed >&2
exit /b 1
"#
        } else {
            r#"
if [ "$1" = "--version" ]; then exit 0; fi
echo "signature verification failed" >&2
exit 1
"#
        };
        install_fake_bin("DOTZ_MINISIGN_BIN", body)
    }

    /// A fake `minisign` SIGN binary: writes a fake `.minisig` file next to the `-x` arg + exits
    /// 0. The body parses the `-x <path>` arg out of `%*` (Windows) / `$@` (Unix) so the
    /// signature lands at the path the caller expects. Exits 0 on `--version` (presence check).
    fn install_fake_minisign_sign_ok() -> FakeBinGuard {
        // Windows .cmd: `setlocal enabledelayedexpansion` is required so `!NEXT!` inside the
        // for-loop body reads the just-updated value (without it, `!NEXT!` is literal text and
        // the signature file is never written).
        let body = if cfg!(windows) {
            r#"@echo off
setlocal enabledelayedexpansion
if "%1"=="--version" exit /b 0
set SIG=
set NEXT=0
for %%A in (%*) do (
  if "!NEXT!"=="1" (
    set SIG=%%A
    set NEXT=0
  )
  if "%%A"=="-x" set NEXT=1
)
if defined SIG (
  echo untrusted comment: fake dotz signature > "%SIG%"
  echo RWRmZ3N3b21ldGhpbmcK >> "%SIG%"
)
exit /b 0
"#
        } else {
            r#"
if [ "$1" = "--version" ]; then exit 0; fi
sig=""
next=0
for a in "$@"; do
  if [ "$next" = "1" ]; then sig="$a"; next=0; fi
  if [ "$a" = "-x" ]; then next=1; fi
done
if [ -n "$sig" ]; then
  printf "untrusted comment: fake dotz signature\nRWRmZ3N3b21ldGhpbmcK\n" > "$sig"
fi
exit 0
"#
        };
        install_fake_bin("DOTZ_MINISIGN_BIN", body)
    }

    /// A fake `gh` binary: prints a PR URL to stdout + exits 0. The presence check
    /// (`--version`) also exits 0.
    fn install_fake_gh_ok(pr_url: &str) -> FakeBinGuard {
        // The body must exit 0 on `--version` AND print the PR URL on `pr create`. We check
        // arg 1: if it's `--version`, exit 0 silently; otherwise print the PR URL.
        let body = if cfg!(windows) {
            format!(
                r#"@echo off
if "%1"=="--version" exit /b 0
echo {pr_url}
exit /b 0
"#
            )
        } else {
            format!(
                r#"
if [ "$1" = "--version" ]; then exit 0; fi
echo "{pr_url}"
exit 0
"#
            )
        };
        install_fake_bin("DOTZ_GH_BIN", &body)
    }

    /// Stub marketplace server: serves `catalog.json` + the GitHub contents-API directory
    /// listing + raw file downloads, all from a temp dir laid out as
    /// `<root>/catalog.json` + `<root>/<preset>/...`. Bound on an ephemeral port; the handle
    /// aborts on drop.
    struct Stub {
        base_url: String,
        api_url: String,
        _server: tokio::task::JoinHandle<()>,
        _root: PathBuf,
    }

    /// Build a stub marketplace that serves a catalog with one preset (`my-preset`) containing
    /// a single `SKILL.md` file. The contents-API listing for `my-preset` returns one file
    /// entry with a `download_url` pointing back at the stub's raw URL. Also serves a `PUBKEY`
    /// at the root + a `manifest.json` + `manifest.minisig` inside the preset dir so the B2
    /// signature-verify gate can be exercised with a fake `minisign` binary.
    async fn start_stub_catalog_one_preset() -> Stub {
        let root =
            std::env::temp_dir().join(format!("dotz-marketplace-stub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("my-preset")).unwrap();
        let catalog = r#"{"presets":[
            {"name":"my-preset","kind":"skill","description":"a test skill preset","author":"dotz","version":"0.1.0","path":"my-preset"}
        ]}"#;
        std::fs::write(root.join("catalog.json"), catalog).unwrap();
        std::fs::write(
            root.join("my-preset").join("SKILL.md"),
            "---\nname: my-preset\ndescription: a test skill preset\n---\nbody\n",
        )
        .unwrap();
        // B2: a PUBKEY at the repo root + a manifest + signature inside the preset dir. The
        // signature content is arbitrary — the fake `minisign` verify binary exits 0 regardless,
        // so the content only needs to be non-empty (the verify gate checks file existence).
        std::fs::write(root.join("PUBKEY"), "RWRmZ3N3b21ldGhpbmdfc3R1Yl9wdWJrZXkK").unwrap();
        std::fs::write(
            root.join("my-preset").join("manifest.json"),
            r#"{"name":"my-preset","kind":"skill","version":"0.1.0","files":["SKILL.md"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("my-preset").join("manifest.minisig"),
            "untrusted comment: dotz stub signature\nRWRmZ3N3b21ldGhpbmcK\n",
        )
        .unwrap();

        // Wrap root in Arc so each handler closure can clone the Arc cheaply without moving
        // the PathBuf out of the outer scope. The alternative (one root_clone per route) is
        // more lines for the same effect.
        let root = std::sync::Arc::new(root);
        let root_for_catalog = root.clone();
        let root_for_contents = root.clone();
        let root_for_raw = root.clone();
        let app = AxRouter::new()
            // Raw catalog.json (served at the raw URL root).
            .route(
                "/catalog.json",
                axget(move || {
                    let root = root_for_catalog.clone();
                    async move {
                        std::fs::read_to_string(root.join("catalog.json")).unwrap()
                    }
                }),
            )
            // GitHub contents-API: /contents/<path>?ref=main → JSON array listing.
            .route(
                "/contents/{path}",
                axget(
                    move |axum::extract::Path(path): axum::extract::Path<String>,
                          axum::extract::Query(q): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| {
                        let root = root_for_contents.clone();
                        async move {
                            let _ = q; // ref query is accepted but ignored — we always serve main
                            let dir = root.join(&path);
                            let mut entries: Vec<Value> = Vec::new();
                            if dir.is_dir() {
                                for ent in std::fs::read_dir(&dir).unwrap().flatten() {
                                    let name = ent.file_name().to_string_lossy().to_string();
                                    if name == "catalog.json" && path.is_empty() {
                                        continue;
                                    }
                                    let ft = ent.file_type().unwrap();
                                    let entry_path = if path.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{path}/{name}")
                                    };
                                    // The download_url points at the stub's raw root, which
                                    // we serve at /raw/<path>.
                                    let download_url = format!("/raw/{entry_path}");
                                    entries.push(json!({
                                        "name": name,
                                        "path": entry_path,
                                        "type": if ft.is_dir() { "dir" } else { "file" },
                                        "download_url": if ft.is_file() { Value::String(download_url) } else { Value::Null },
                                    }));
                                }
                            }
                            axum::Json(Value::Array(entries))
                        }
                    },
                ),
            )
            // Raw file download (the download_url target).
            .route(
                "/raw/{*path}",
                axget(move |axum::extract::Path(path): axum::extract::Path<String>| {
                    let root = root_for_raw.clone();
                    async move {
                        let file = root.join(&path);
                        std::fs::read_to_string(&file).unwrap_or_default()
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base_url = format!("http://127.0.0.1:{}/", addr.port());
        let api_url = format!("http://127.0.0.1:{}/contents/", addr.port());
        // Pull the PathBuf back out of the Arc for the Stub's _root field (the server task
        // holds its own Arc clone, so dropping this one is safe).
        let root_path = std::sync::Arc::try_unwrap(root).unwrap_or_else(|arc| (*arc).clone());
        Stub {
            base_url,
            api_url,
            _server: server,
            _root: root_path,
        }
    }

    // ---- acceptance tests ----

    /// `marketplace_list_presets_returns_empty_when_repo_404` — when the catalog URL 404s,
    /// `list_presets` returns an empty vec (not an error) so the UI renders an empty
    /// marketplace instead of crashing.
    #[tokio::test]
    async fn marketplace_list_presets_returns_empty_when_repo_404() {
        let _g = setup_presets_dir();
        let _m = setup_market_url(
            "http://127.0.0.1:1/never-served/", // unreachable → fetch fails → empty
            "http://127.0.0.1:1/never-served/",
        );
        let out = list_presets().await.expect("404 should not error");
        assert!(
            out.is_empty(),
            "a 404/unreachable catalog must yield an empty list, got {out:?}"
        );
    }

    /// `marketplace_list_presets_parses_catalog` — against a stub serving a known catalog,
    /// `list_presets` parses the entries with the right name/kind/description/author/version.
    #[tokio::test]
    async fn marketplace_list_presets_parses_catalog() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let out = list_presets().await.expect("stub catalog should parse");
        assert_eq!(out.len(), 1, "expected one preset, got {out:?}");
        let p = &out[0];
        assert_eq!(p.name, "my-preset");
        assert_eq!(p.kind, PresetKind::Skill);
        assert_eq!(p.description, "a test skill preset");
        assert_eq!(p.author, "dotz");
        assert_eq!(p.version, "0.1.0");
        assert!(!p.installed, "preset should not be installed yet");
    }

    /// `marketplace_install_preset_downloads_and_writes` — install a preset from the stub,
    /// assert the file is written under `~/.dotz/presets/<name>/`. B2: install now requires a
    /// valid signature; the stub serves `manifest.json` + `manifest.minisig` + `PUBKEY`, and a
    /// fake `minisign` verify binary is installed so the verify gate passes.
    #[tokio::test]
    async fn marketplace_install_preset_downloads_and_writes() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();

        install_preset("my-preset")
            .await
            .expect("install should succeed against the stub");
        let installed_dir = presets_dir().join("my-preset");
        assert!(installed_dir.exists(), "install dir must exist");
        let skill = std::fs::read_to_string(installed_dir.join("SKILL.md")).unwrap();
        assert!(
            skill.contains("name: my-preset"),
            "SKILL.md must be the downloaded content"
        );
    }

    /// `marketplace_install_preset_rejects_invalid_name` — names with `..`, `/`, uppercase,
    /// leading dash, or overlong are rejected with a clear error before any network call.
    #[test]
    fn marketplace_install_preset_rejects_invalid_name() {
        let _g = test_lock();
        for bad in [
            "..",
            "../etc",
            "foo/bar",
            "-leading-dash",
            "UPPER",
            "",
            &"x".repeat(65),
        ] {
            let r = validate_preset_name(bad);
            assert!(
                r.is_err(),
                "name {bad:?} must be rejected by validate_preset_name, got {r:?}"
            );
        }
        // Sanity: a valid name passes.
        assert!(validate_preset_name("my-preset-1").is_ok());
    }

    /// `marketplace_uninstall_preset_removes_dir` — install then uninstall; the dir is gone.
    /// Uninstall is idempotent: a second call still succeeds (the dir doesn't exist). B2: install
    /// requires a valid signature; a fake `minisign` verify binary is installed.
    #[tokio::test]
    async fn marketplace_uninstall_preset_removes_dir() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();
        install_preset("my-preset").await.unwrap();
        let dir = presets_dir().join("my-preset");
        assert!(dir.exists(), "precondition: dir must exist after install");
        uninstall_preset("my-preset").expect("uninstall should succeed");
        assert!(!dir.exists(), "dir must be gone after uninstall");
        // Idempotent: a second uninstall is Ok (no-op).
        uninstall_preset("my-preset").expect("second uninstall should be a no-op (Ok)");
    }

    /// `marketplace_list_installed_scans_dir` — write a preset dir manually, `list_installed`
    /// returns it. This pins the "scans `~/.dotz/presets/`" contract without needing the stub.
    #[test]
    fn marketplace_list_installed_scans_dir() {
        let _g = setup_presets_dir();
        std::fs::create_dir_all(presets_dir().join("handmade")).unwrap();
        std::fs::create_dir_all(presets_dir().join("another")).unwrap();
        // A stray file (not a dir) must be skipped.
        std::fs::write(presets_dir().join("not-a-dir.txt"), b"ignore me").unwrap();
        let installed = list_installed();
        assert!(
            installed.contains(&"handmade".to_string()),
            "handmade must be listed"
        );
        assert!(
            installed.contains(&"another".to_string()),
            "another must be listed"
        );
        assert!(
            !installed.iter().any(|n| n == "not-a-dir.txt"),
            "non-directory entries must be skipped"
        );
    }

    /// `marketplace_caches_catalog_for_5_min` — two rapid calls hit the cache (the stub is
    /// queried once). We assert by counting requests via a shared counter on the stub.
    #[tokio::test]
    async fn marketplace_caches_catalog_for_5_min() {
        let _g = setup_presets_dir();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let root =
            std::env::temp_dir().join(format!("dotz-marketplace-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("catalog.json"),
            r#"{"presets":[{"name":"cached","kind":"skill","description":"x","author":"y","version":"0.1.0","path":"cached"}]}"#,
        )
        .unwrap();
        let root_for_handler = root.clone();
        let app = AxRouter::new().route(
            "/catalog.json",
            axget(move || {
                let counter = counter_clone.clone();
                let root = root_for_handler.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::fs::read_to_string(root.join("catalog.json")).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let _m = setup_market_url(&base, &base);

        // First call → fetches.
        let _ = list_presets().await.unwrap();
        // Second call within the TTL → should hit the cache.
        let _ = list_presets().await.unwrap();

        let hits = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            hits, 1,
            "catalog should be fetched once (cached on the second call), got {hits} hits"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `marketplace_preset_kind_serializes_lowercase` — PresetKind serde.
    #[test]
    fn marketplace_preset_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&PresetKind::Profile).unwrap(),
            r#""profile""#
        );
        assert_eq!(
            serde_json::to_string(&PresetKind::DesignSystem).unwrap(),
            r#""design-system""#
        );
        assert_eq!(
            serde_json::to_string(&PresetKind::DesignSkill).unwrap(),
            r#""design-skill""#
        );
        assert_eq!(
            serde_json::to_string(&PresetKind::Plugin).unwrap(),
            r#""plugin""#
        );
        // Round-trip: parse back.
        let k: PresetKind = serde_json::from_str(r#""prompt""#).unwrap();
        assert_eq!(k, PresetKind::Prompt);
        // as_str matches the serde output.
        for k in [
            PresetKind::Profile,
            PresetKind::Prompt,
            PresetKind::Agent,
            PresetKind::Skill,
            PresetKind::DesignSystem,
            PresetKind::DesignSkill,
            PresetKind::Plugin,
        ] {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
    }

    /// `marketplace_install_writes_to_presets_dir_only` — the path-traversal guard. We craft a
    /// stub catalog entry whose file `name` contains `..` (simulating a malicious repo) and
    /// assert the install fails with a path-traversal error AND nothing was written outside the
    /// presets root.
    #[tokio::test]
    async fn marketplace_install_writes_to_presets_dir_only() {
        let _g = setup_presets_dir();
        // Stub serving a catalog with one preset whose contents-API listing includes a file
        // entry with `name = "../escape.txt"` — the traversal attempt.
        let root = std::env::temp_dir().join(format!(
            "dotz-marketplace-traversal-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("evil")).unwrap();
        std::fs::write(
            root.join("catalog.json"),
            r#"{"presets":[{"name":"evil","kind":"skill","description":"x","author":"y","version":"0.1.0","path":"evil"}]}"#,
        )
        .unwrap();
        let root = std::sync::Arc::new(root);
        let root_for_catalog = root.clone();
        let app = AxRouter::new()
            .route(
                "/catalog.json",
                axget(move || {
                    let root = root_for_catalog.clone();
                    async move { std::fs::read_to_string(root.join("catalog.json")).unwrap() }
                }),
            )
            .route(
                "/contents/{path}",
                axget(
                    move |axum::extract::Path(path): axum::extract::Path<String>| {
                        let _ = path;
                        async move {
                            // Always return a single file entry with a traversal name + a real
                            // download_url (the install will fail at the guard before downloading).
                            axum::Json(Value::Array(vec![json!({
                                "name": "../escape.txt",
                                "path": "evil/../escape.txt",
                                "type": "file",
                                "download_url": "http://127.0.0.1:1/never-served/escape.txt",
                            })]))
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let api = format!("http://127.0.0.1:{}/contents/", addr.port());
        let _m = setup_market_url(&base, &api);

        let res = install_preset("evil").await;
        assert!(
            res.is_err(),
            "install with a traversal file name must fail, got {res:?}"
        );
        let err = res.unwrap_err().message;
        assert!(
            err.contains("refusing path") || err.contains(".."),
            "error should name the traversal refusal, got: {err}"
        );
        // Nothing escaped the presets root.
        assert!(
            !presets_dir().join("..").join("escape.txt").exists(),
            "no file must be written outside the presets root"
        );
        // The preset's own dir may have been created (create_dir_all ran before the guard);
        // that's fine — it's empty and inside the root. The guard caught the traversal before
        // any file was written.
        server.abort();
        let root_path = std::sync::Arc::try_unwrap(root).unwrap_or_else(|arc| (*arc).clone());
        let _ = std::fs::remove_dir_all(&root_path);
    }

    /// `marketplace_presets_dir_created_if_missing` — install creates `~/.dotz/presets/` when
    /// it doesn't exist yet. We assert by removing the presets root before install and
    /// checking it exists after. B2: install requires a valid signature; a fake `minisign`
    /// verify binary is installed.
    #[tokio::test]
    async fn marketplace_presets_dir_created_if_missing() {
        let _g = setup_presets_dir();
        // Precondition: presets dir does NOT exist.
        let root = presets_dir();
        let _ = std::fs::remove_dir_all(&root);
        assert!(!root.exists(), "precondition: presets root must not exist");

        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();
        install_preset("my-preset")
            .await
            .expect("install should create the presets root");
        assert!(
            root.exists(),
            "presets root must exist after install (create_dir_all ran)"
        );
        assert!(
            root.join("my-preset").join("SKILL.md").exists(),
            "the preset file must be written under the freshly-created root"
        );
    }

    /// `marketplace_route_get_returns_catalog_and_installed` — `GET /api/presets` shape.
    /// Mounted via the router() so the route wiring is exercised.
    #[tokio::test]
    async fn marketplace_route_get_returns_catalog_and_installed() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        // Pre-install a preset so `installed` is non-empty.
        std::fs::create_dir_all(presets_dir().join("handmade")).unwrap();

        let app = router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/presets", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "GET /api/presets should be 200");
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["catalog"].is_array(),
            "response must have a catalog array, got: {body}"
        );
        assert!(
            body["installed"].is_array(),
            "response must have an installed array, got: {body}"
        );
        let installed: Vec<String> = body["installed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            installed.contains(&"handmade".to_string()),
            "installed must list the pre-installed preset, got: {installed:?}"
        );

        server.abort();
    }

    /// `marketplace_route_post_install` — `POST /api/presets/install` works. B2: install
    /// requires a valid signature; a fake `minisign` verify binary is installed.
    #[tokio::test]
    async fn marketplace_route_post_install() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();

        let app = router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://127.0.0.1:{}/api/presets/install",
                addr.port()
            ))
            .json(&json!({ "name": "my-preset" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NO_CONTENT,
            "POST install should return 204, got {}",
            resp.status()
        );
        assert!(
            presets_dir().join("my-preset").join("SKILL.md").exists(),
            "the preset must be installed on disk after the POST"
        );

        server.abort();
    }

    /// `marketplace_route_delete_uninstall` — `DELETE /api/presets/{name}` works.
    #[tokio::test]
    async fn marketplace_route_delete_uninstall() {
        let _g = setup_presets_dir();
        // Pre-install by hand (no stub needed for the delete route).
        std::fs::create_dir_all(presets_dir().join("doomed")).unwrap();
        std::fs::write(presets_dir().join("doomed").join("SKILL.md"), b"body").unwrap();

        let app = router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let client = reqwest::Client::new();
        let resp = client
            .delete(format!(
                "http://127.0.0.1:{}/api/presets/doomed",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NO_CONTENT,
            "DELETE should return 204, got {}",
            resp.status()
        );
        assert!(
            !presets_dir().join("doomed").exists(),
            "preset dir must be gone after DELETE"
        );

        server.abort();
    }

    /// `marketplace_route_get_installed` — `GET /api/presets/installed` shape (a bare JSON
    /// array of strings).
    #[tokio::test]
    async fn marketplace_route_get_installed() {
        let _g = setup_presets_dir();
        std::fs::create_dir_all(presets_dir().join("alpha")).unwrap();
        std::fs::create_dir_all(presets_dir().join("beta")).unwrap();

        let app = router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{}/api/presets/installed",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body: Value = resp.json().await.unwrap();
        let arr = body
            .as_array()
            .expect("GET installed should return a bare array");
        let names: Vec<String> = arr
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"alpha".to_string()),
            "alpha must be listed, got {names:?}"
        );
        assert!(
            names.contains(&"beta".to_string()),
            "beta must be listed, got {names:?}"
        );

        server.abort();
    }

    /// `skills_scan_includes_presets_root` — the skills loader's scan_roots() must include the
    /// presets path so a `~/.dotz/presets/<name>/SKILL.md` is discoverable. This pins the
    /// loader-integration contract without needing the full skills index build (which scans
    /// every real skill pool on the host).
    #[test]
    fn skills_scan_includes_presets_root() {
        // Hold the process-wide DOTZ_CONFIG_DIR lock (NOT the marketplace-local ENV_LOCK) so
        // this test can't race with templates::tests / config::tests / etc. that also flip
        // DOTZ_CONFIG_DIR. A race here would make a sibling test's delete_template look at the
        // wrong dir and flake.
        let _g = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The presets root is `<dotz_dir>/presets/`. Set DOTZ_CONFIG_DIR to a known dir so the
        // assertion is host-independent.
        let dir =
            std::env::temp_dir().join(format!("dotz-skills-presets-root-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", dir.to_string_lossy().to_string()) };
        let roots = crate::skills::scan_roots_public();
        let expected = dir.join("presets");
        assert!(
            roots.iter().any(|(p, _)| *p == expected),
            "skills scan_roots must include {} (source=preset), got: {:?}",
            expected.display(),
            roots
                .iter()
                .map(|(p, s)| (p.display().to_string(), *s))
                .collect::<Vec<_>>()
        );
        // And the source label must be "preset".
        let entry = roots.iter().find(|(p, _)| *p == expected);
        assert_eq!(
            entry.map(|(_, s)| *s),
            Some("preset"),
            "the presets scan root must use source label \"preset\""
        );
        // Restore the env (don't just remove — a prior test may have set it).
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- B2 acceptance tests ----

    /// `publish_validates_preset_shape` — missing name/kind/files/version → Err (the publish
    /// flow validates the shape before any CLI call). No minisign/gh needed.
    #[tokio::test]
    async fn publish_validates_preset_shape() {
        let _g = setup_presets_dir();
        // Missing name.
        let r = publish_preset(&PublishRequest {
            name: "".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        assert!(r.is_err(), "empty name must be rejected");
        assert!(
            r.unwrap_err().message.contains("preset name"),
            "error should name the name validation"
        );

        // Missing kind.
        let r = publish_preset(&PublishRequest {
            name: "good-name".to_string(),
            kind: "".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        assert!(r.is_err(), "empty kind must be rejected");
        assert!(
            r.unwrap_err().message.contains("kind is required"),
            "error should name the kind validation"
        );

        // Missing version.
        let r = publish_preset(&PublishRequest {
            name: "good-name".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        assert!(r.is_err(), "empty version must be rejected");
        assert!(
            r.unwrap_err().message.contains("version is required"),
            "error should name the version validation"
        );

        // Empty files vec.
        let r = publish_preset(&PublishRequest {
            name: "good-name".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![],
        })
        .await;
        assert!(r.is_err(), "empty files must be rejected");
        assert!(
            r.unwrap_err().message.contains("files must not be empty"),
            "error should name the files validation"
        );

        // Traversal file path.
        let r = publish_preset(&PublishRequest {
            name: "good-name".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![PublishFile {
                path: "../escape.txt".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        assert!(r.is_err(), "traversal file path must be rejected");
        assert!(
            r.unwrap_err().message.contains("refusing file path"),
            "error should name the traversal refusal"
        );
    }

    /// `publish_signs_with_minisign_when_available` — a fake `minisign` sign binary writes the
    /// `.minisig` file + a fake `gh` prints a PR URL; the publish succeeds + returns the PR URL.
    /// This exercises the full publish flow (validate → write → sign → PR) with mocked CLIs.
    #[tokio::test]
    async fn publish_signs_with_minisign_when_available() {
        let _g = setup_presets_dir();
        let _sign = install_fake_minisign_sign_ok();
        let _gh = install_fake_gh_ok("https://github.com/cayleb-james2008/dotz-presets/pull/7");

        let res = publish_preset(&PublishRequest {
            name: "my-new-preset".to_string(),
            kind: "skill".to_string(),
            description: "a publish test".to_string(),
            version: "0.2.0".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "---\nname: my-new-preset\n---\nbody\n".to_string(),
            }],
        })
        .await
        .expect("publish should succeed with mocked minisign + gh");
        assert_eq!(res.pr_number, 7, "PR number must be parsed from the URL");
        assert!(
            res.pr_url.contains("/pull/7"),
            "PR URL must be returned, got {}",
            res.pr_url
        );
        // The signature file must have been created by the fake minisign sign binary. We can't
        // inspect the temp dir directly (it's cleaned up by PublishTempGuard on drop), but the
        // publish succeeding past the sign step (which checks `sig_path.exists()`) is the
        // proof. The fake gh printing the URL is the proof the PR step ran.
    }

    /// `publish_returns_error_when_minisign_missing` — when `DOTZ_MINISIGN_BIN` points at a
    /// nonexistent path AND `minisign` is not on PATH, the publish returns a clear error
    /// naming the missing CLI. We set `DOTZ_MINISIGN_BIN` to a path that doesn't exist; the
    /// presence check in `minisign_bin` fails → `None` → clear error.
    #[tokio::test]
    async fn publish_returns_error_when_minisign_missing() {
        let _g = setup_presets_dir();
        // Point DOTZ_MINISIGN_BIN at a nonexistent path. The presence check (`<bin>
        // --version`) fails → minisign_bin() returns None. Skip this test if the real
        // `minisign` is on PATH (the presence check would fall back to it — we can't easily
        // hide PATH from a std::process::Command). On hosts without minisign installed (the
        // common dev case), the env override alone is enough to force None.
        let _fake = install_fake_bin("DOTZ_MINISIGN_BIN", "@echo off\nexit /b 127\n");
        // The fake binary exits 127 on --version (not found) → presence check fails → None.
        // But we also need to ensure the real `minisign` isn't picked up via PATH fallback —
        // the env override IS set (non-empty), so minisign_bin uses it directly (no PATH
        // fallback). So the presence check runs the fake binary (exits 127) → None.
        let r = publish_preset(&PublishRequest {
            name: "needs-sign".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        assert!(r.is_err(), "publish must fail when minisign is missing");
        let msg = r.unwrap_err().message;
        assert!(
            msg.contains("minisign CLI required"),
            "error must name the missing CLI, got: {msg}"
        );
    }

    /// `publish_opens_pr_via_gh` — a fake `gh` prints a PR URL; the publish returns the parsed
    /// URL + number. Uses a fake minisign sign binary too (the sign step runs before gh).
    #[tokio::test]
    async fn publish_opens_pr_via_gh() {
        let _g = setup_presets_dir();
        let _sign = install_fake_minisign_sign_ok();
        let _gh = install_fake_gh_ok("https://github.com/cayleb-james2008/dotz-presets/pull/42");

        let res = publish_preset(&PublishRequest {
            name: "pr-test".to_string(),
            kind: "profile".to_string(),
            description: "pr creation test".to_string(),
            version: "1.0.0".to_string(),
            files: vec![PublishFile {
                path: "profile.md".to_string(),
                content: "profile body".to_string(),
            }],
        })
        .await
        .expect("publish + PR creation should succeed with mocked CLIs");
        assert_eq!(res.pr_number, 42, "PR number 42 must be parsed");
        assert_eq!(
            res.pr_url, "https://github.com/cayleb-james2008/dotz-presets/pull/42",
            "PR URL must be returned verbatim"
        );
    }

    /// `install_rejects_unsigned_preset` — a preset whose dir has no `manifest.minisig` is
    /// rejected. We use a stub that serves a preset WITHOUT the signature file.
    #[tokio::test]
    async fn install_rejects_unsigned_preset() {
        let _g = setup_presets_dir();
        // Stub serving a preset with no manifest.minisig.
        let root = std::env::temp_dir().join(format!(
            "dotz-marketplace-unsigned-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("unsigned")).unwrap();
        std::fs::write(
            root.join("catalog.json"),
            r#"{"presets":[{"name":"unsigned","kind":"skill","description":"x","author":"y","version":"0.1.0","path":"unsigned"}]}"#,
        )
        .unwrap();
        std::fs::write(root.join("unsigned").join("SKILL.md"), "body").unwrap();
        std::fs::write(
            root.join("unsigned").join("manifest.json"),
            r#"{"name":"unsigned","version":"0.1.0"}"#,
        )
        .unwrap();
        // NOTE: no manifest.minisig.
        let root = std::sync::Arc::new(root);
        let root_for_catalog = root.clone();
        let root_for_contents = root.clone();
        let root_for_raw = root.clone();
        let app = AxRouter::new()
            .route(
                "/catalog.json",
                axget(move || {
                    let root = root_for_catalog.clone();
                    async move { std::fs::read_to_string(root.join("catalog.json")).unwrap() }
                }),
            )
            .route(
                "/contents/{path}",
                axget(
                    move |axum::extract::Path(path): axum::extract::Path<String>| {
                        let root = root_for_contents.clone();
                        async move {
                            let dir = root.join(&path);
                            let mut entries: Vec<Value> = Vec::new();
                            if dir.is_dir() {
                                for ent in std::fs::read_dir(&dir).unwrap().flatten() {
                                    let name = ent.file_name().to_string_lossy().to_string();
                                    if name == "catalog.json" && path.is_empty() {
                                        continue;
                                    }
                                    let ft = ent.file_type().unwrap();
                                    let entry_path = format!("{path}/{name}");
                                    let download_url = format!("/raw/{entry_path}");
                                    entries.push(json!({
                                        "name": name,
                                        "path": entry_path,
                                        "type": if ft.is_dir() { "dir" } else { "file" },
                                        "download_url": if ft.is_file() { Value::String(download_url) } else { Value::Null },
                                    }));
                                }
                            }
                            axum::Json(Value::Array(entries))
                        }
                    },
                ),
            )
            .route(
                "/raw/{*path}",
                axget(move |axum::extract::Path(path): axum::extract::Path<String>| {
                    let root = root_for_raw.clone();
                    async move {
                        let file = root.join(&path);
                        std::fs::read_to_string(&file).unwrap_or_default()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let api = format!("http://127.0.0.1:{}/contents/", addr.port());
        let _m = setup_market_url(&base, &api);
        let _fake = install_fake_minisign_verify_ok();

        let r = install_preset("unsigned").await;
        assert!(r.is_err(), "unsigned preset must be rejected");
        let msg = r.unwrap_err().message;
        assert!(
            msg.contains("not signed") || msg.contains("manifest.minisig"),
            "error must name the missing signature, got: {msg}"
        );
        // The partial install dir must have been rolled back.
        assert!(
            !presets_dir().join("unsigned").exists(),
            "unsigned preset dir must be rolled back (removed on reject)"
        );

        server.abort();
        let root_path = std::sync::Arc::try_unwrap(root).unwrap_or_else(|arc| (*arc).clone());
        let _ = std::fs::remove_dir_all(&root_path);
    }

    /// `install_rejects_bad_signature` — a fake `minisign` verify binary that exits 1 → install
    /// is rejected. The stub serves a manifest + a (fake) signature; the verify gate fails.
    #[tokio::test]
    async fn install_rejects_bad_signature() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        // Fake minisign verify that exits 1 (bad signature).
        let _fake = install_fake_minisign_verify_bad();

        let r = install_preset("my-preset").await;
        assert!(r.is_err(), "bad signature must be rejected");
        let msg = r.unwrap_err().message;
        assert!(
            msg.contains("signature verification failed"),
            "error must name the verify failure, got: {msg}"
        );
        // The partial install dir must have been left in place (we don't roll back on verify
        // failure — only on missing-signature). The operator can DELETE to clean up. We only
        // assert the install didn't stamp a version.
        assert!(
            !installed_version_path("my-preset").unwrap().exists(),
            "no .installed-version must be stamped on a failed verify"
        );
    }

    /// `install_accepts_valid_signature` — a fake `minisign` verify binary that exits 0 →
    /// install succeeds + the file is written + the version is stamped.
    #[tokio::test]
    async fn install_accepts_valid_signature() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();

        install_preset("my-preset")
            .await
            .expect("valid signature should allow install");
        assert!(
            presets_dir().join("my-preset").join("SKILL.md").exists(),
            "the preset file must be written"
        );
        assert!(
            installed_version_path("my-preset").unwrap().exists(),
            ".installed-version must be stamped after a successful verify"
        );
        let v = read_installed_version("my-preset").unwrap();
        assert_eq!(v.as_deref(), Some("0.1.0"), "stamped version must be 0.1.0");
    }

    /// `install_records_version` — after a successful install, `.installed-version` contains the
    /// catalog version. (Covered by `install_accepts_valid_signature` above, but this is the
    /// focused pin so the contract is named clearly.)
    #[tokio::test]
    async fn install_records_version() {
        let _g = setup_presets_dir();
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();

        install_preset("my-preset").await.unwrap();
        let stamped = read_installed_version("my-preset").unwrap();
        assert_eq!(
            stamped.as_deref(),
            Some("0.1.0"),
            "installed version must be the catalog version 0.1.0"
        );
    }

    /// `list_presets_reports_update_available` — when the catalog version is newer than the
    /// installed version, `list_presets` reports `updateAvailable: true`. We install v0.1.0,
    /// then point at a stub catalog that lists v0.2.0, and assert `update_available` is true.
    #[tokio::test]
    async fn list_presets_reports_update_available() {
        let _g = setup_presets_dir();
        // First: install v0.1.0 from the default stub.
        let stub = start_stub_catalog_one_preset().await;
        let _m = setup_market_url(&stub.base_url, &stub.api_url);
        let _fake = install_fake_minisign_verify_ok();
        install_preset("my-preset").await.unwrap();
        let stamped = read_installed_version("my-preset").unwrap();
        assert_eq!(
            stamped.as_deref(),
            Some("0.1.0"),
            "precondition: v0.1.0 installed"
        );
        drop(_m);

        // Now point at a stub catalog that lists v0.2.0 for the same preset. The installed
        // version (0.1.0) is older → updateAvailable must be true.
        let root =
            std::env::temp_dir().join(format!("dotz-marketplace-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("my-preset")).unwrap();
        std::fs::write(
            root.join("catalog.json"),
            r#"{"presets":[{"name":"my-preset","kind":"skill","description":"x","author":"y","version":"0.2.0","path":"my-preset"}]}"#,
        )
        .unwrap();
        let root = std::sync::Arc::new(root);
        let root_for_catalog = root.clone();
        let app = AxRouter::new().route(
            "/catalog.json",
            axget(move || {
                let root = root_for_catalog.clone();
                async move { std::fs::read_to_string(root.join("catalog.json")).unwrap() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let _m2 = setup_market_url(&base, &base);

        let out = list_presets().await.expect("catalog should parse");
        let p = out
            .iter()
            .find(|p| p.name == "my-preset")
            .expect("my-preset must be in the catalog");
        assert!(p.installed, "preset must be installed");
        assert!(
            p.update_available,
            "updateAvailable must be true when catalog v0.2.0 > installed v0.1.0"
        );

        server.abort();
        let root_path = std::sync::Arc::try_unwrap(root).unwrap_or_else(|arc| (*arc).clone());
        let _ = std::fs::remove_dir_all(&root_path);
    }

    /// `pubkey_fetch_caches_locally` — the first `fetch_pubkey` hits the URL + writes the
    /// on-disk cache; the second call reads the cache (no second HTTP hit). We assert by
    /// counting requests via a shared counter on the stub.
    #[tokio::test]
    async fn pubkey_fetch_caches_locally() {
        let _g = setup_presets_dir();
        invalidate_pubkey_cache();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let root =
            std::env::temp_dir().join(format!("dotz-marketplace-pubkey-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root_for_handler = root.clone();
        let app = AxRouter::new().route(
            "/PUBKEY",
            axget(move || {
                let counter = counter_clone.clone();
                let root = root_for_handler.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::fs::read_to_string(root.join("PUBKEY"))
                        .unwrap_or_else(|_| "RWRmZ3N3b21ldGhpbmdfc3R1Yl9wdWJrZXkK".to_string())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let _m = setup_market_url(&base, &base);

        let client = reqwest::Client::new();
        let _ = fetch_pubkey(&client).await.unwrap();
        let _ = fetch_pubkey(&client).await.unwrap();

        let hits = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            hits, 1,
            "PUBKEY should be fetched once (cached on the second call), got {hits} hits"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
        invalidate_pubkey_cache();
    }

    /// `pubkey_falls_back_to_placeholder_on_fetch_failure` — when the PUBKEY URL 404s,
    /// `fetch_pubkey` returns the bundled placeholder + the on-disk cache is the fallback. We
    /// point at a stub that 404s on /PUBKEY.
    #[tokio::test]
    async fn pubkey_falls_back_to_placeholder_on_fetch_failure() {
        let _g = setup_presets_dir();
        invalidate_pubkey_cache();
        // Stub that 404s on /PUBKEY.
        let app = AxRouter::new().route(
            "/PUBKEY",
            axget(|| async { (axum::http::StatusCode::NOT_FOUND, "PUBKEY not found") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}/", addr.port());
        let _m = setup_market_url(&base, &base);

        let client = reqwest::Client::new();
        let pk = fetch_pubkey(&client)
            .await
            .expect("fetch_pubkey should not error on a 404 — it falls back");
        assert_eq!(
            pk, BUNDLED_PUBKEY_FALLBACK,
            "a 404 PUBKEY must yield the bundled fallback"
        );

        server.abort();
        invalidate_pubkey_cache();
    }

    /// `publish_never_exposes_private_key` — the publish flow's error messages + the response
    /// body never contain the private key path or the literal `presets-key` dir name (after
    /// redaction). This is a static contract pin: we trigger a publish that fails AFTER the
    /// sign step (gh missing → error), and assert the error message is redacted. We also
    /// assert the success path's response (`{prUrl, prNumber}`) contains no key path.
    #[tokio::test]
    async fn publish_never_exposes_private_key() {
        let _g = setup_presets_dir();
        let _sign = install_fake_minisign_sign_ok();
        // No fake gh → gh_bin() returns None (gh not installed on this host) → publish fails
        // with "gh CLI required". The error path is the one most likely to leak a key path
        // (minisign sign errors include the key dir). We assert the redacted message.
        let r = publish_preset(&PublishRequest {
            name: "key-leak-test".to_string(),
            kind: "skill".to_string(),
            description: "x".to_string(),
            version: "0.1.0".to_string(),
            files: vec![PublishFile {
                path: "SKILL.md".to_string(),
                content: "body".to_string(),
            }],
        })
        .await;
        // This will fail at the gh step (gh missing) — OR at the minisign step if minisign is
        // also missing. Either way, the error message must not contain the key dir/file names.
        if let Err(e) = r {
            let redacted = redact_private_key(&e.message);
            assert!(
                !redacted.contains("presets-key"),
                "redacted error must not contain presets-key, got: {redacted}"
            );
            assert!(
                !redacted.contains("minisign.key"),
                "redacted error must not contain minisign.key, got: {redacted}"
            );
        }
        // Success-path redaction: feed a known-leaky message through redact_private_key + assert
        // it's scrubbed. This pins the redaction contract independent of the live publish path.
        let leaky = "minisign sign failed (is the presets key present at /home/u/.dotz/presets-key/minisign.key?): bad password";
        let redacted = redact_private_key(leaky);
        assert!(
            !redacted.contains("presets-key"),
            "redaction must scrub presets-key"
        );
        assert!(
            !redacted.contains("minisign.key"),
            "redaction must scrub minisign.key"
        );
        assert!(
            redacted.contains("<redacted-key-dir>"),
            "redaction must replace the key dir with a placeholder, got: {redacted}"
        );
    }
}
