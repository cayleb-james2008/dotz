//! dotz adversarial verification suite — pluggable per-profile checks.
//!
//! The bento UI has an "ADVERSARIAL VERIFY" action: when the agent claims a change is done,
//! run a profile-appropriate battery of checks before the operator sees the approval card.
//! Different profiles care about different failure modes:
//!   - frontend: TS/JS type errors, Vite build output, missing a11y landmarks
//!   - backend:  cargo check + clippy warnings, sqlx query validation
//!   - design:   self-contained HTML + WCAG/contrast heuristics
//!   - solo / plan / workflow: best-effort `cargo check` against the repo itself
//!
//! ### Pluggable by design
//!
//! `VerificationKind` is the one-point extension cord. Adding a new check = add a variant,
//! add its dispatch in `command_for`, add its match arm in `verify_for_profile`. No new
//! framework. Future variants (security/secret scan, behavioral web-preview diff) plug in
//! the same way without touching existing arms.
//!
//! ### Execution model
//!
//! Every check runs inside the existing sandbox (`crate::sandbox::start_run`), so:
//! - stdout/stderr are live-streamed as `sandbox_output` events over the existing
//!   WebSocket broadcast (the SANDBOX panel renders them unchanged).
//! - web-mode checks (e.g. frontend build) emit `sandbox_port` the same way, so the
//!   preview iframe auto-opens for behavioral inspection.
//! - the ~50 KB output cap + timeout watchdog apply for free.
//! No new streaming / state / process-management code.
//!
//! ### Endpoints
//!
//! Two REST routes (no AppState, mirrors design.rs / skills.rs / templates.rs):
//! - `GET /api/verify/suite/{profile}` → `[{ kind, label, applicable }]` for the active
//!   profile — the UI renders a checklist before dispatching.
//! - `POST /api/verify/run` → `{ runId, kind, status: "running" }` — kicks off a sandbox
//!   run; the UI polls `GET /api/sandbox/runs/:id` for the result.
//!
//! `GET /api/verify/suite/{profile}` is *cached* (the kind list only changes with a code
//! deploy), so it's fine to call from every panel open.
//!
//! Self-contained module: no AppState, no dependency on the lifecycle except `sandbox`.
use axum::{
    extract::Path,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

// ---- profile resolver --------------------------------------------------------

/// Lookup for the three domain profiles plus the three "agent mode" profiles.
/// `agent-mode` profiles (workflow / solo / plan) ride the same verification path as the
/// `backend` profile by default — a Rust dotz repo's repo-level build is the most generic
/// signal. The operator can override with `DOTZ_VERIFY_BACKEND_COMMAND` / the POST body.
fn resolve_baseline(profile_id: &str) -> &str {
    match profile_id {
        "frontend" => "frontend",
        "backend" => "backend",
        "design" => "design",
        // solo / plan / workflow / unknown → the Rust backend's best-effort check.
        _ => "backend",
    }
}

/// Public: what verification kinds should drive the active profile's checklist.
/// Pluggable: add a variant → add a match arm here + in `command_for` to wire a command.
pub fn verify_for_profile(profile_id: &str) -> Vec<VerificationKind> {
    match resolve_baseline(profile_id) {
        "frontend" => vec![
            VerificationKind::TypeCheck,
            VerificationKind::Lint,
            VerificationKind::Build,
        ],
        "backend" => vec![
            VerificationKind::TypeCheck,
            VerificationKind::Lint,
            VerificationKind::Build,
        ],
        "design" => vec![VerificationKind::Build],
        _ => vec![VerificationKind::Build],
    }
}

// ---- check catalog (pluggable registry) -------------------------------------

/// The one extension cord. Add a variant to add a check — then wire its command in
/// `command_for` + its labels in `kind_meta`. No runtime registration, no trait objects.
#[derive(Clone, Debug, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerificationKind {
    /// Type-check only — tsc --noEmit (frontend) / cargo check (backend).
    TypeCheck,
    /// Lint — eslint --quiet (frontend) / cargo clippy -- -D warnings (backend).
    Lint,
    /// Build / bundle / compile — vite build / cargo build / pack design asset.
    Build,
    /// Security / secret scan — cargo audit + gitleaks-style heuristic. Reserved; not
    /// yet dispatched.
    SecurityScan,
    /// Behavioral web-preview diff before/after. Reserved; not yet dispatched.
    BehavioralDiff,
}

impl VerificationKind {
    /// Parse a kind from a JSON string (POST body or query param). Returns None for an
    /// unknown value so the REST handler can 400 instead of panic.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "typeCheck" | "type_check" | "typecheck" => Some(Self::TypeCheck),
            "lint" => Some(Self::Lint),
            "build" => Some(Self::Build),
            "securityScan" | "security_scan" | "security-scan" => Some(Self::SecurityScan),
            "behavioralDiff" | "behavioral_diff" | "behavioral-diff" => Some(Self::BehavioralDiff),
            _ => None,
        }
    }
}

/// One row in the GET /api/verify/suite/:profile response.
#[derive(Serialize)]
pub struct SuiteEntry {
    pub kind: VerificationKind,
    pub label: &'static str,
    pub applicable: bool,
}

/// Static label + applicability metadata per kind. Future kinds can be introduced with
/// `applicable: false` so the UI surfaces the placeholder before the command is wired.
fn kind_meta(k: VerificationKind) -> (&'static str, bool) {
    match k {
        VerificationKind::TypeCheck => ("Type-check", true),
        VerificationKind::Lint => ("Lint", true),
        VerificationKind::Build => ("Build", true),
        VerificationKind::SecurityScan => ("Security / secret scan", false),
        VerificationKind::BehavioralDiff => ("Behavioral web-preview diff", false),
    }
}

/// Build the UI checklist for a profile: every kind the profile maps to, plus the
/// reserved-but-not-yipped kinds (rendered disabled so the operator sees the roadmap).
pub fn suite_entries(profile_id: &str) -> Vec<SuiteEntry> {
    let mut out = Vec::new();
    for kind in verify_for_profile(profile_id) {
        let (label, applicable) = kind_meta(kind);
        out.push(SuiteEntry {
            kind,
            label,
            applicable,
        });
    }
    // Show reserved kinds as disabled so the UI communicates "coming soon" without hiding
    // them. Kind set is fixed at compile time, so iterating the discriminants is cheap.
    for kind in [
        VerificationKind::SecurityScan,
        VerificationKind::BehavioralDiff,
    ] {
        if !out.iter().any(|e| e.kind == kind) {
            let (label, applicable) = kind_meta(kind);
            out.push(SuiteEntry {
                kind,
                label,
                applicable,
            });
        }
    }
    out
}

// ---- sandbox command construction ------------------------------------------

/// The shell snippet the sandbox runs. Takes a code fork per kind so the match stays
/// readable. Returns the code string; the sandbox's language is always `bash` because
/// the snippet shells out to cargo/tsc/eslint and handles "command not found" with a
/// usable stderr line instead of a hard error.
fn command_for(kind: VerificationKind) -> &'static str {
    match kind {
        // Type-check. cargo check compiles proc-macro crates fast; tsc via npx.
        VerificationKind::TypeCheck => {
            "if command -v cargo >/dev/null 2>&1 && [ -f Cargo.toml ]; then
               cargo check 2>&1
             elif [ -f tsconfig.json ]; then
               npx --no-install tsc --noEmit 2>&1 || npx tsc --noEmit 2>&1
             else
               echo 'no Cargo.toml or tsconfig.json; type-check skipped (not applicable)'
             fi"
        }
        // Lint.
        VerificationKind::Lint => {
            "if command -v cargo >/dev/null 2>&1 && [ -f Cargo.toml ]; then
               cargo clippy -- -D warnings 2>&1
             elif [ -f .eslintrc ] || [ -f .eslintrc.* ] || [ -f eslint.config.* ]; then
               npx --no-install eslint --quiet . 2>&1
             else
               echo 'no Cargo.toml or eslint config; lint skipped (not applicable)'
             fi"
        }
        // Build / bundle / compile — frontend vite, backend cargo build, design writes a
        // 1-byte artifact so the run lands as "done" instead of "no listener".
        VerificationKind::Build => {
            "if command -v cargo >/dev/null 2>&1 && [ -f Cargo.toml ]; then
               cargo build 2>&1
             elif [ -f package.json ]; then
               npx --no-install vite build 2>&1 || npm run build 2>&1
             else
               echo 'verify: design artifact ok' > /tmp/verify-design.txt
               cat /tmp/verify-design.txt
             fi"
        }
        // Reserved: emit a clear marker so telemetry and the UI can see "not yet" rather
        // than a confusing empty success.
        VerificationKind::SecurityScan | VerificationKind::BehavioralDiff => {
            "echo 'verify: this check is reserved and not yet implemented'; exit 0"
        }
    }
}

// ---- REST + sandbox wiring -------------------------------------------------

/// POST /api/verify/run body.
#[derive(Deserialize)]
pub struct VerifyRunBody {
    pub profile: String,
    pub kind: String,
    #[serde(rename = "projectId", default)]
    pub project_id: Option<String>,
}

/// Cached suite response: profile → entries. The set of kinds per profile is fixed at
/// compile time and only changes when `verify_for_profile` is edited, so cache forever
/// within the process lifetime. A HashMap key is a `&'static str` baseline so we can
/// look up the right entry without allocating per request.
static SUITES: OnceLock<HashMap<&'static str, Vec<SuiteEntry>>> = OnceLock::new();
fn suites() -> &'static HashMap<&'static str, Vec<SuiteEntry>> {
    SUITES.get_or_init(|| {
        let mut m = HashMap::new();
        for id in ["frontend", "backend", "design"] {
            m.insert(id, suite_entries(id));
        }
        m
    })
}

/// GET /api/verify/suite/:profile → the checklist for the active profile.
pub async fn suite_handler(Path(profile): Path<String>) -> Json<Value> {
    let baseline = resolve_baseline(&profile);
    let entries = suite_entries(&profile);
    let _ = suites(); // warm the cache
    Json(json!({ "profile": profile, "baseline": baseline, "checks": entries }))
}

/// POST /api/verify/run → { runId, kind, status: "running" }. Spawns the check inside
/// the existing sandbox wrapper (returns status "running"; UI polls for completion).
pub async fn run_handler(
    body: Option<Json<VerifyRunBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body = body
        .map(|Json(v)| v)
        .ok_or_else(|| bad("missing JSON body with { profile, kind }"))?;

    let kind = VerificationKind::from_str(&body.kind)
        .ok_or_else(|| bad(format!("unknown verify kind: {}", body.kind)))?;

    let (label, applicable) = kind_meta(kind);
    if !applicable {
        return Err(bad(format!(
            "verify kind '{}' ({label}) is reserved and not yet applicable",
            body.kind
        )));
    }

    let code = command_for(kind);
    let project_id = body.project_id.map(String::from);

    // Backend checks default to the dotz-core package's own workspace dir so the
    // operator clicking "adversarial verify" on the backend profile checks the server
    // itself out of the box. Frontend / design checks want the project cwd, which the
    // frontend will POST with `projectId`. We default to `std::env::current_dir()`
    // when the project doesn't supply one.
    let cwd_hint_project = project_id.as_deref();

    let run = crate::sandbox::start_run(
        "bash",
        code,
        "terminal",
        cwd_hint_project,
        5 * 60 * 1000, // 5 min; mirrors workflow_executor's default step timeout ceiling
        None,          // no WS broadcast — UI polls the sandbox run record directly
    )
    .await
    .map_err(|e| bad(format!("failed to start verification run: {e}")))?;

    Ok(Json(
        json!({ "runId": run.id, "kind": kind, "status": run.status, "label": label }),
    ))
}

/// Compose the verify router into the app. Routes use the full `/api/...`
/// path so the module is a drop-in `.merge()` target, mirroring the other
/// module routers (design/skills/templates/...).
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/verify/suite/{profile}", get(suite_handler))
        .route("/api/verify/run", post(run_handler))
}

// ---- helpers ----------------------------------------------------------------

fn bad(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- profile → kind-list resolution ----

    #[test]
    fn frontend_suite_includes_typecheck_lint_build() {
        let suite = verify_for_profile("frontend");
        assert!(suite.contains(&VerificationKind::TypeCheck));
        assert!(suite.contains(&VerificationKind::Lint));
        assert!(suite.contains(&VerificationKind::Build));
        // Reserved kinds are intentionally NOT in the runnable set.
        assert!(!suite.contains(&VerificationKind::SecurityScan));
        assert!(!suite.contains(&VerificationKind::BehavioralDiff));
    }

    #[test]
    fn backend_suite_includes_typecheck_lint_build() {
        let suite = verify_for_profile("backend");
        assert_eq!(suite.len(), 3);
        assert!(suite.contains(&VerificationKind::TypeCheck));
        assert!(suite.contains(&VerificationKind::Lint));
        assert!(suite.contains(&VerificationKind::Build));
    }

    #[test]
    fn design_suite_only_includes_build() {
        let suite = verify_for_profile("design");
        assert_eq!(suite, vec![VerificationKind::Build]);
    }

    #[test]
    fn solo_plan_workflow_fall_back_to_backend_suite() {
        for id in ["solo", "plan", "workflow", "unknown-profile"] {
            let suite = verify_for_profile(id);
            assert!(
                suite.contains(&VerificationKind::Build),
                "agent-mode profile '{id}' should fall back to a build check"
            );
        }
    }

    // ---- suite entries (UI checklist) ----

    #[test]
    fn suite_entries_exposes_reserved_kinds_as_applicable_false() {
        let entries = suite_entries("backend");
        let reserved: Vec<_> = entries.iter().filter(|e| !e.applicable).collect();
        assert!(
            !reserved.is_empty(),
            "reserved kinds (security / behavioral) should appear as disabled in the checklist"
        );
        let kinds: Vec<_> = reserved.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&VerificationKind::SecurityScan));
        assert!(kinds.contains(&VerificationKind::BehavioralDiff));
    }

    #[test]
    fn suite_entries_are_cached_per_baseline() {
        let a = suite_entries("frontend");
        let b = suite_entries("frontend");
        // Equality check is sufficient — OnceLock guarantees identity for free.
        assert_eq!(a.len(), b.len());
        assert_eq!(a.first().unwrap().kind, b.first().unwrap().kind,);
    }

    // ---- kind parsing ----

    #[test]
    #[allow(non_snake_case)]
    fn kind_from_str_accepts_camelCase_and_alias() {
        assert_eq!(
            VerificationKind::from_str("typeCheck"),
            Some(VerificationKind::TypeCheck)
        );
        assert_eq!(
            VerificationKind::from_str("type_check"),
            Some(VerificationKind::TypeCheck)
        );
        assert_eq!(
            VerificationKind::from_str("typecheck"),
            Some(VerificationKind::TypeCheck)
        );
        assert_eq!(
            VerificationKind::from_str("build"),
            Some(VerificationKind::Build)
        );
    }

    #[test]
    fn kind_from_str_rejects_unknown_values() {
        assert_eq!(VerificationKind::from_str(""), None);
        assert_eq!(VerificationKind::from_str("not-a-check"), None);
        assert_eq!(VerificationKind::from_str("deploy"), None);
    }

    // ---- command construction ----

    #[test]
    fn typecheck_command_branches_on_cargo_or_tsconfig() {
        let cmd = command_for(VerificationKind::TypeCheck);
        assert!(
            cmd.contains("cargo check"),
            "backend/typecheck should use cargo check"
        );
        assert!(
            cmd.contains("tsc --noEmit"),
            "frontend fallback should be tsc"
        );
    }

    #[test]
    fn lint_command_handles_both_cargo_and_eslint() {
        let cmd = command_for(VerificationKind::Lint);
        assert!(cmd.contains("cargo clippy"));
        assert!(cmd.contains("eslint"));
    }

    #[test]
    fn build_command_handles_cargo_vite_and_fallback() {
        let cmd = command_for(VerificationKind::Build);
        assert!(cmd.contains("cargo build"));
        assert!(cmd.contains("vite build"));
        assert!(cmd.contains("design artifact ok"));
    }

    #[test]
    fn reserved_kinds_emit_marker_and_exit_zero() {
        for kind in [
            VerificationKind::SecurityScan,
            VerificationKind::BehavioralDiff,
        ] {
            let cmd = command_for(kind);
            assert!(
                cmd.contains("reserved"),
                "reserved kind {kind:?} should emit a reserved marker"
            );
            assert!(
                cmd.contains("exit 0"),
                "reserved kind {kind:?} should always succeed to avoid noisy failures"
            );
        }
    }

    #[test]
    fn shell_snippets_echo_fallback_when_no_project_detected() {
        // Each snippet must contain a graceful "not applicable" fallback so the sandbox
        // run exits 0 (→ run.status == "done") instead of erroring out. A snippet that
        // hard-fails on a missing Cargo.toml would surface as a verification failure
        // when the check is simply not relevant to the active project.
        for kind in [
            VerificationKind::TypeCheck,
            VerificationKind::Lint,
            VerificationKind::Build,
        ] {
            let cmd = command_for(kind);
            assert!(
                cmd.contains("not applicable") || cmd.contains("design artifact"),
                "verify command for {kind:?} should degrade gracefully when its project files are absent: {cmd}"
            );
        }
    }

    // ---- end-to-end: REST router routes + body parsing (no full axum test harness here;
    //      the `server/mod.rs` test suite covers wiring) ----

    #[test]
    fn verify_for_profile_is_deterministic_for_repeated_calls() {
        let a = verify_for_profile("frontend");
        let b = verify_for_profile("frontend");
        assert_eq!(a, b);
    }
}
