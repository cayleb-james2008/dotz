//! Open Design systems catalog — port of the `/api/design/*` handlers in src/server.ts.
//!
//! Scans the vendored Open Design systems under `<.pi>/design-systems/<slug>/`, reading each
//! slug's `manifest.json` for `{id, name, category, description}` (the DESIGN panel catalog), and
//! serves the chosen system's reference `components.html` for the same-origin srcdoc preview.
//!
//! Self-contained: no AppState, no axum State. The `.pi` location is resolved exactly like the Node
//! code (relative to the running process, with a DOTZ_PI env override) — see `design_systems_dir`.
use axum::{
    Json, Router,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
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
    Json(list_systems_value().await)
}

/// The catalog value (`{ "systems": [...] }`) the REST handler wraps in `Json`. Exposed so the
/// `design_list` agent tool reuses the exact scan/sort/manifest logic instead of duplicating it.
pub async fn list_systems_value() -> serde_json::Value {
    let dir = design_systems_dir();
    let read = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return json!({ "systems": [] }),
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

    json!({ "systems": systems })
}

/// GET /api/design/systems/:id/components — serve the slug's reference `components.html` as
/// text/html (the same-origin srcdoc preview). 400 `{error:"bad id"}` on a non-slug id; 404
/// `{error:"no components for this system"}` when the file is missing. Mirrors server.ts.
async fn system_components(Path(id): Path<String>) -> Response {
    match system_components_html(&id).await {
        Ok(html) => (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response(),
        Err(e) if e == "bad id" => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
    }
}

/// Read a system's reference `components.html`. `Err("bad id")` for a non-slug id (→ 400),
/// `Err("no components for this system")` when the file is missing (→ 404). Exposed so the
/// `design_use` agent tool reuses the same guard + read as the REST handler.
pub async fn system_components_html(id: &str) -> Result<String, String> {
    if !is_valid_slug(id) {
        return Err("bad id".to_string());
    }
    let html_path = design_systems_dir().join(id).join("components.html");
    tokio::fs::read_to_string(&html_path)
        .await
        .map_err(|_| "no components for this system".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct PiDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        pi: std::path::PathBuf,
        prev: Option<String>,
    }

    impl Drop for PiDirGuard {
        fn drop(&mut self) {
            match &self.prev {
                // TODO: Audit that the environment access only happens in single-threaded code.
                Some(p) => unsafe { std::env::set_var("DOTZ_PI", p) },
                // TODO: Audit that the environment access only happens in single-threaded code.
                None => unsafe { std::env::remove_var("DOTZ_PI") },
            }
            let _ = std::fs::remove_dir_all(&self.pi);
        }
    }

    /// Create an isolated `.pi/design-systems/<slug>/components.html` tree and point DOTZ_PI at it.
    fn with_tmp_design_systems() -> (PiDirGuard, std::path::PathBuf) {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pi = std::env::temp_dir().join(format!("dotz-design-test-{}", uuid::Uuid::new_v4()));
        let prev = std::env::var("DOTZ_PI").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_PI", &pi) };
        let guard = PiDirGuard {
            _lock: guard,
            pi: pi.clone(),
            prev,
        };
        (guard, pi)
    }

    fn make_slug_dir(pi: &std::path::Path, slug: &str) -> std::path::PathBuf {
        let dir = pi.join("design-systems").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn system_components_serves_existing_html_async() {
        let (_guard, pi) = with_tmp_design_systems();
        let slug_dir = make_slug_dir(&pi, "stripe");
        let html = "<html><body>Open Design components</body></html>";
        std::fs::write(slug_dir.join("components.html"), html).unwrap();

        let resp = system_components(Path("stripe".to_string())).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let headers = resp.headers();
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html")
        );

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert_eq!(body, html);
    }

    #[tokio::test]
    async fn system_components_returns_404_for_missing_file() {
        let (_guard, pi) = with_tmp_design_systems();
        make_slug_dir(&pi, "missing");

        let resp = system_components(Path("missing".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "no components for this system");
    }

    #[tokio::test]
    async fn system_components_returns_400_for_bad_slug() {
        let (_guard, _pi) = with_tmp_design_systems();

        let resp = system_components(Path("_schema".to_string())).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "bad id");
    }

    /// `system_components_html` (used by the `design_use` agent tool) returns the raw HTML for a
    /// valid slug, `Err("bad id")` for a non-slug, and `Err("no components...")` when missing.
    #[tokio::test]
    async fn system_components_html_returns_html_or_typed_errors() {
        let (_guard, pi) = with_tmp_design_systems();
        let slug_dir = make_slug_dir(&pi, "stripe");
        std::fs::write(slug_dir.join("components.html"), "<h1>ok</h1>").unwrap();

        assert_eq!(
            system_components_html("stripe").await.unwrap(),
            "<h1>ok</h1>"
        );
        assert_eq!(
            system_components_html("_schema").await.unwrap_err(),
            "bad id"
        );
        assert_eq!(
            system_components_html("missing").await.unwrap_err(),
            "no components for this system"
        );
    }

    /// GET /api/design/systems must scan slugs in lexicographic order, read manifest.json fields,
    /// and fall back to the slug as the name when the manifest is missing.
    #[tokio::test]
    async fn list_systems_sorts_by_slug_and_reads_manifest() {
        let (_guard, pi) = with_tmp_design_systems();

        let beta_dir = make_slug_dir(&pi, "beta");
        std::fs::write(
            beta_dir.join("manifest.json"),
            r#"{"name":"Beta System","category":"visual","description":"Beta desc"}"#,
        )
        .unwrap();

        let alpha_dir = make_slug_dir(&pi, "alpha");
        std::fs::write(
            alpha_dir.join("manifest.json"),
            r#"{"name":"Alpha System"}"#,
        )
        .unwrap();

        // No manifest → fallback to slug name with empty category/description.
        let _gamma_dir = make_slug_dir(&pi, "gamma");

        let resp = list_systems().await;
        let systems = resp.0["systems"].as_array().expect("systems array");
        assert_eq!(systems.len(), 3);

        assert_eq!(systems[0]["id"], "alpha");
        assert_eq!(systems[0]["name"], "Alpha System");
        assert_eq!(systems[0]["category"], "");

        assert_eq!(systems[1]["id"], "beta");
        assert_eq!(systems[1]["name"], "Beta System");
        assert_eq!(systems[1]["category"], "visual");
        assert_eq!(systems[1]["description"], "Beta desc");

        assert_eq!(systems[2]["id"], "gamma");
        assert_eq!(systems[2]["name"], "gamma");
    }
}
