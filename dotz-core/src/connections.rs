//! Local "Connections" — port of src/connections.ts. SAFETY-CRITICAL: this Rust port implements
//! READ-ONLY status only. It NEVER spawns a provider login or logout (running `gh`/`vercel` login
//! or any logout has previously logged the operator out and cleared credentials). The login/logout/
//! login-state routes return 501 (disabled — Phase 5).
//!
//! Status surfaces each provider's existing browser-CLI auth:
//!   - github: `gh auth status` (parse "logged in to" + account)
//!   - vercel: `vercel whoami` (read-only; exit 0 + non-empty output => logged in, account = last line)
//!   - neon:   READ ~/.config/neonctl/credentials.json (present & non-empty => logged in). Never calls
//!             neonctl — it has no on-PATH CLI / no logout command here, so status keys off the file.
use axum::{http::StatusCode, routing::get, Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

const OUTPUT_CAP: usize = 16_384;
const STATUS_TIMEOUT: Duration = Duration::from_millis(12_000);

/// ConnectionStatus — matches the Node oracle / fixture shape. `account` and `hint` are omitted when
/// absent (mirrors the optional `account?` / `hint?` fields in connections.ts).
#[derive(Serialize)]
struct ConnectionStatus {
    id: &'static str,
    label: &'static str,
    cli: &'static str,
    installed: bool,
    #[serde(rename = "loggedIn")]
    logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

struct CmdResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

struct Parsed {
    installed: bool,
    logged_in: bool,
    account: Option<String>,
    hint: Option<String>,
}

/// Run a fixed first-party status command (no user input), capturing combined output with a short
/// timeout. `code: Some(127)` if the binary can't be spawned (treated as "not installed" downstream).
/// We mirror the Node `runCommand` which uses `shell: true`; here we invoke the program directly with
/// argv (no shell) since all status commands are fixed first-party tokens — simpler and avoids a shell
/// dependency. A spawn failure (missing CLI) => code 127, never an error.
fn run_command(program: &str, args: &[&str], timeout: Duration) -> CmdResult {
    use std::io::Read;
    let child = Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        // Missing CLI (ENOENT) or any spawn failure => not installed, never propagate an error.
        Err(err) => {
            return CmdResult {
                code: Some(127),
                stdout: String::new(),
                stderr: err.to_string(),
            }
        }
    };

    // Drain stdout/stderr on threads so a full pipe can't deadlock the wait, and so we still capture
    // partial output even if we have to kill on timeout. (Never call wait_with_output after a manual
    // wait/kill — that yields empty output on a reaped child.)
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let so_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(r) = so.as_mut() {
            let _ = r.read_to_string(&mut s);
        }
        s
    });
    let se_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(r) = se.as_mut() {
            let _ = r.read_to_string(&mut s);
        }
        s
    });

    // Bounded wait: poll for completion up to `timeout`, then kill (mirrors the Node killTree timer).
    let deadline = std::time::Instant::now() + timeout;
    let code: Option<i32> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None; // Node resolves with code: null on timeout.
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                let _ = child.kill();
                return CmdResult {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: err.to_string(),
                };
            }
        }
    };

    let stdout = cap(so_t.join().unwrap_or_default());
    let stderr = cap(se_t.join().unwrap_or_default());
    CmdResult {
        code,
        stdout,
        stderr,
    }
}

fn cap(s: String) -> String {
    if s.len() <= OUTPUT_CAP {
        return s;
    }
    // Keep the trailing OUTPUT_CAP bytes (mirrors `.slice(-OUTPUT_CAP)`), on a char boundary.
    let start = s.len() - OUTPUT_CAP;
    let mut start = start;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Mirror of connections.ts `notInstalled`: code 127, or "not recognized" / "command not found" /
/// "no such file" in the combined output (case-insensitive).
fn not_installed(r: &CmdResult) -> bool {
    let s = format!("{}\n{}", r.stderr, r.stdout).to_lowercase();
    r.code == Some(127)
        || s.contains("not recognized")
        || s.contains("command not found")
        || s.contains("no such file")
}

// ---- github: gh auth status ----
fn parse_github(r: &CmdResult) -> Parsed {
    if not_installed(r) {
        return Parsed {
            installed: false,
            logged_in: false,
            account: None,
            hint: Some("install the GitHub CLI (gh)".to_string()),
        };
    }
    let out = format!("{}\n{}", r.stdout, r.stderr);
    let lower = out.to_lowercase();
    // Trust the text, not the exit code (gh does a network token check that can fail while still
    // logged in). "not logged into any GitHub hosts" lacks the "logged in to" form and won't match.
    let logged_in = lower.contains("logged in to");
    let account = extract_account(&out);
    Parsed {
        installed: true,
        logged_in,
        account,
        hint: if logged_in {
            None
        } else {
            Some("not logged in".to_string())
        },
    }
}

/// Equivalent of /account\s+([A-Za-z0-9-]+)/i — find "account" then the next run of [A-Za-z0-9-].
fn extract_account(out: &str) -> Option<String> {
    let lower = out.to_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("account") {
        let kw_start = search_from + rel;
        let after = kw_start + "account".len();
        // Skip whitespace after "account" (the regex requires \s+).
        let bytes = out.as_bytes();
        let mut i = after;
        let mut saw_ws = false;
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r' || bytes[i] == b'\n')
        {
            saw_ws = true;
            i += 1;
        }
        if saw_ws {
            let start = i;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'-' {
                    i += 1;
                } else {
                    break;
                }
            }
            if i > start {
                return Some(out[start..i].to_string());
            }
        }
        search_from = after;
    }
    None
}

// ---- vercel: vercel whoami ----
fn parse_vercel(r: &CmdResult) -> Parsed {
    if not_installed(r) {
        return Parsed {
            installed: false,
            logged_in: false,
            account: None,
            hint: Some("install the Vercel CLI".to_string()),
        };
    }
    let combined = format!("{}\n{}", r.stdout, r.stderr);
    let lines: Vec<String> = combined
        .split('\n')
        .map(|l| l.trim_end_matches('\r').trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('<'))
        .collect();
    let logged_in = r.code == Some(0) && !lines.is_empty();
    Parsed {
        installed: true,
        logged_in,
        account: if logged_in {
            lines.last().cloned()
        } else {
            None
        },
        hint: if logged_in {
            None
        } else {
            Some("not logged in".to_string())
        },
    }
}

// ---- neon: READ ~/.config/neonctl/credentials.json (never call neonctl) ----
fn neon_credentials_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config")
        .join("neonctl")
        .join("credentials.json")
}

fn status_neon() -> Parsed {
    // present & non-empty => logged in. Read-only; never spawns neonctl.
    let logged_in = std::fs::metadata(neon_credentials_path())
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    Parsed {
        installed: true,
        logged_in,
        account: None,
        hint: if logged_in {
            None
        } else {
            Some(
                "click Log in — opens Neon's browser auth via npx neonctl (no global install needed)"
                    .to_string(),
            )
        },
    }
}

fn status_github() -> Parsed {
    parse_github(&run_command("gh", &["auth", "status"], STATUS_TIMEOUT))
}

fn status_vercel() -> Parsed {
    parse_vercel(&run_command("vercel", &["whoami"], STATUS_TIMEOUT))
}

/// Build all three providers' status. Each provider is independent; a panic/failure in one is
/// degraded to a blank entry (mirrors the per-provider try/catch in connections.ts `status()`).
fn all_status() -> Vec<ConnectionStatus> {
    let specs: [(&'static str, &'static str, &'static str, fn() -> Parsed); 3] = [
        ("github", "GitHub", "gh", status_github),
        ("vercel", "Vercel", "vercel", status_vercel),
        ("neon", "Neon", "neonctl", status_neon),
    ];
    specs
        .iter()
        .map(|&(id, label, cli, f)| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(p) => ConnectionStatus {
                    id,
                    label,
                    cli,
                    installed: p.installed,
                    logged_in: p.logged_in,
                    account: p.account,
                    hint: p.hint,
                },
                Err(_) => ConnectionStatus {
                    id,
                    label,
                    cli,
                    installed: false,
                    logged_in: false,
                    account: None,
                    hint: Some("status check failed".to_string()),
                },
            }
        })
        .collect()
}

// ---- handlers ----

/// GET /api/connections -> { connections: [ConnectionStatus...] }
async fn get_connections() -> Json<Value> {
    let connections = all_status();
    Json(json!({ "connections": connections }))
}

/// Login / logout / login-state are DISABLED in the Rust port for safety (Phase 5). They never
/// spawn anything — running a provider login/logout has logged the operator out before.
async fn disabled() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(
            json!({ "error": "connection login/logout disabled in Rust port (safety) — Phase 5" }),
        ),
    )
}

/// Register the connections routes with stateless handlers. Status is real (read-only); login/logout
/// return 501 and spawn nothing.
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/connections", get(get_connections))
        .route(
            "/api/connections/{provider}/login",
            get(disabled).post(disabled),
        )
        .route(
            "/api/connections/{provider}/logout",
            axum::routing::post(disabled),
        )
}
