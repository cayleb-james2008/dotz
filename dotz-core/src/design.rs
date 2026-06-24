//! Open Design systems catalog — port of the `/api/design/*` handlers in src/server.ts.
//!
//! Scans the vendored Open Design systems under `<.pi>/design-systems/<slug>/`, reading each
//! slug's `manifest.json` for `{id, name, category, description}` (the DESIGN panel catalog), and
//! serves the chosen system's reference `components.html` for the same-origin srcdoc preview.
//!
//! Self-contained: no AppState, no axum State. The `.pi` location is resolved exactly like the Node
//! code (relative to the running process, with a DOTZ_PI env override) — see `design_systems_dir`.
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

/// One catalog row, matching the Node handler's object shape and the captured fixture
/// (`design.systems.json`): `{ "id", "name", "category", "description" }` — all camelCase-safe
/// (single words) so no serde rename is needed.
#[derive(Serialize)]
struct SystemEntry {
    id: String,
    name: String,
    category: String,
    description: String,
}

/// Resolve the vendored design-systems dir the way the Node code does.
///
/// Node: `DESIGN_SYSTEMS_DIR = path.resolve(dirname(SELF), "../.pi/design-systems")` — i.e. the
/// `.pi` that ships alongside the running module (repo root in dev, app root in the packaged exe).
/// There is no env var in server.ts, but profiles.ts anchors the same `.pi`; here we honor an
/// optional `DOTZ_PI` override (pointing at the `.pi` dir) and otherwise default to `<cwd>/.pi`,
/// which is the repo/app root the process runs from. Mirrors the "`.pi` next to the app" contract.
fn design_systems_dir() -> PathBuf {
    let pi = std::env::var("DOTZ_PI")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".pi")
        });
    pi.join("design-systems")
}

/// A slug is a lowercase-ASCII project dir name: `^[a-z0-9][a-z0-9-]*$`. This skips `_schema`,
/// dotfiles, and anything non-slug — exactly the Node regex.
fn is_valid_slug(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// GET /api/design/systems — scan `.pi/design-systems`, one row per slug directory.
///
/// Mirrors server.ts: read the dir; on any error return `{ "systems": [] }`. Sort entries by name
/// (lexicographic, matching JS `localeCompare` for these ASCII slugs). Skip non-directories (the
/// LICENSE / NOTICE / README files) and non-slug names (`_schema`, dotfiles). For each slug read
/// `manifest.json` and use `name||id`, `category||""`, `description||""`; on read/parse failure
/// fall back to `{ id, name: id, category: "", description: "" }`.
async fn list_systems() -> Json<serde_json::Value> {
    let dir = design_systems_dir();
    let read = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Json(json!({ "systems": [] })),
    };

    // Collect (slug, is_dir) for valid directory slugs, then sort by name to match the Node sort.
    let mut slugs: Vec<String> = Vec::new();
    for ent in read.flatten() {
        // Skip non-directories (LICENSE / NOTICE / README.md and any stray file).
        let is_dir = match ent.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };
        if !is_dir {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if !is_valid_slug(&name) {
            continue; // skip _schema, dotfiles
        }
        slugs.push(name);
    }
    slugs.sort();

    // Read every slug's manifest.json CONCURRENTLY instead of one-at-a-time. This handler is uncached
    // and re-runs on every design-panel open, so 150+ sequential small-file reads were the cost.
    // `join_all` yields results in input order, so the output stays byte-identical to the sorted scan.
    let systems: Vec<SystemEntry> = futures_util::future::join_all(slugs.into_iter().map(|id| {
        let dir = dir.clone();
        async move {
            let manifest_path = dir.join(&id).join("manifest.json");
            match tokio::fs::read_to_string(&manifest_path)
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            {
                Some(m) => {
                    let name = m
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(id.as_str())
                        .to_string();
                    let category = m
                        .get("category")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let description = m
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    SystemEntry {
                        id: id.clone(),
                        name,
                        category,
                        description,
                    }
                }
                None => SystemEntry {
                    id: id.clone(),
                    name: id.clone(),
                    category: String::new(),
                    description: String::new(),
                },
            }
        }
    }))
    .await;

    Json(json!({ "systems": systems }))
}

/// GET /api/design/systems/:id/components — serve the slug's reference `components.html` as
/// text/html (the same-origin srcdoc preview). 400 `{error:"bad id"}` on a non-slug id; 404
/// `{error:"no components for this system"}` when the file is missing. Mirrors server.ts.
async fn system_components(Path(id): Path<String>) -> Response {
    if !is_valid_slug(&id) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "bad id" }))).into_response();
    }
    let html_path = design_systems_dir().join(&id).join("components.html");
    match std::fs::read_to_string(&html_path) {
        Ok(html) => (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no components for this system" })),
        )
            .into_response(),
    }
}

/// Stateless router for the Open Design catalog endpoints.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/design/systems", get(list_systems))
        .route(
            "/api/design/systems/{id}/components",
            get(system_components),
        )
}
