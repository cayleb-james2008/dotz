//! Local "Connections" — port of src/connections.ts. SAFETY-CRITICAL: this Rust port implements
//! READ-ONLY status only. It NEVER spawns a provider login or logout (running `gh`/`vercel` login
//! or any logout has previously logged the operator out and cleared credentials). The login/logout/
//! login-state routes return 501 (disabled — Phase 5).
//!
//! Status surfaces each provider's existing browser-CLI auth:
//!   - github: `gh auth status` (parse "logged in to" + account)
//!   - vercel: `vercel whoami` (read-only; exit 0 + non-empty output => logged in, account = last line)
//!   - neon:   READ ~/.config/neonctl/credentials.json (present & non-empty => logged in). Never calls
//!     neonctl — it has no on-PATH CLI / no logout command here, so status keys off the file.
use axum::{extract::Path, http::StatusCode, routing::get, Json, Router};
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
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    // Suppress the console-window flash on Windows: the Tauri/WebView2 desktop shell has no
    // visible console, so spawning `gh`/`vercel` without this flag would pop a transient cmd
    // window on every connection-status check. sandbox.rs and browser.rs already do this.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command.spawn();

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
    // Use to_ascii_lowercase (not to_lowercase) so the lowercased string has the SAME byte
    // length as `out`. to_lowercase can change the byte length of non-ASCII characters (e.g.
    // 'İ' U+0130 → 'i̇' U+0069+U+0307, 2 bytes → 3 bytes), which would misalign the byte
    // indices computed on `lower` when they are used to index into `out.as_bytes()` below —
    // silently missing or misreading the account name. "account" is an ASCII keyword and
    // `gh auth status` output is ASCII, so ASCII case-insensitive matching is correct here.
    let lower = out.to_ascii_lowercase();
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

/// One provider's static descriptor: (id, label, cli name, status probe fn).
type ProviderSpec = (&'static str, &'static str, &'static str, fn() -> Parsed);

/// Build all three providers' status. Each provider is independent; a panic/failure in one is
/// degraded to a blank entry (mirrors the per-provider try/catch in connections.ts `status()`).
fn all_status() -> Vec<ConnectionStatus> {
    let specs: [ProviderSpec; 3] = [
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
///
/// Status checks shell out to first-party CLIs with a 12s timeout, so we run the collection
/// off the async runtime thread. A slow or hanging `gh auth status` call must not delay other
/// REST handlers or the WebSocket event fan-out.
async fn get_connections() -> Json<Value> {
    let cli = tokio::task::spawn_blocking(all_status)
        .await
        .unwrap_or_else(|_| Vec::new());
    // The three CLI providers (real, read-only), then any configured gateway connectors. With no
    // connector configured, `gateway_statuses()` returns empty and this stays byte-identical.
    let mut connections: Vec<Value> = cli
        .into_iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    connections.extend(crate::connectors::gateway_statuses().await);
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

/// POST/GET /api/connections/{id}/login. For a configured, enabled GATEWAY connector this
/// initiates the gateway connect flow; the three CLI providers (github/vercel/neon) stay 501
/// (safety: dotz never spawns their login). Optional body { provider, authType?, values? }:
/// with authType=="api_key" + values, dotz issues PUT {gateway}/api/connections/<provider> so the
/// operator's raw credentials go to the GATEWAY (never stored in dotz); otherwise dotz returns the
/// gateway connect URL for the console/OAuth flow. `provider` defaults to the connector id.
async fn login(Path(id): Path<String>, body: Option<Json<Value>>) -> (StatusCode, Json<Value>) {
    let Some(c) = crate::connectors::enabled_by_id(&id) else {
        return disabled().await; // unknown id / CLI provider — unchanged 501
    };
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or(id.as_str())
        .to_string();
    let auth_type = body.get("authType").and_then(|v| v.as_str());
    let values = body.get("values").cloned();
    match (auth_type, values) {
        (Some("api_key"), Some(values)) => {
            let client = reqwest::Client::new();
            match crate::connectors::put_connection(&client, &c, &provider, "api_key", values).await
            {
                Ok(resp) => (
                    StatusCode::OK,
                    Json(json!({ "ok": true, "provider": provider, "connection": resp })),
                ),
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "ok": false, "error": e })),
                ),
            }
        }
        _ => {
            let connect_url = crate::connectors::connect_url(&c, &provider);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "provider": provider,
                    "connectUrl": connect_url,
                    "hint": "open the gateway console to finish connecting (OAuth / interactive); \
                             for api_key providers POST { authType: \"api_key\", values: {..} }",
                })),
            )
        }
    }
}

/// POST /api/connections/{id}/logout. For a gateway connector this deletes the gateway-held
/// connection (DELETE {gateway}/api/connections/<provider>); CLI providers stay 501 (safety).
async fn logout(Path(id): Path<String>, body: Option<Json<Value>>) -> (StatusCode, Json<Value>) {
    let Some(c) = crate::connectors::enabled_by_id(&id) else {
        return disabled().await;
    };
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or(id.as_str())
        .to_string();
    let client = reqwest::Client::new();
    match crate::connectors::delete_connection(&client, &c, &provider).await {
        Ok(resp) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "provider": provider, "result": resp })),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        ),
    }
}

/// Register the connections routes with stateless handlers. Status is real (read-only). For the
/// three CLI providers login/logout stay 501 (they spawn nothing); a configured gateway connector's
/// login/logout is proxied to the gateway (credentials stay behind the gateway).
pub fn router() -> Router<()> {
    Router::new()
        .route("/api/connections", get(get_connections))
        .route("/api/connections/{provider}/login", get(login).post(login))
        .route(
            "/api/connections/{provider}/logout",
            axum::routing::post(logout),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_installed_detects_missing_command() {
        let r = CmdResult {
            code: Some(127),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(not_installed(&r));
    }

    #[test]
    fn not_installed_detects_command_not_found_message() {
        let r = CmdResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "gh: command not found".into(),
        };
        assert!(not_installed(&r));
    }

    #[test]
    fn extract_account_reads_gh_auth_status_output() {
        let out = "Logged in to github.com as cayleb-james2008 (account cayleb-james2008)";
        assert_eq!(extract_account(out), Some("cayleb-james2008".into()));
    }

    #[test]
    fn extract_account_skips_when_no_whitespace_after_keyword() {
        // "accountable" should not match because there's no whitespace after "account".
        let out = "Accountable behavior is required";
        assert_eq!(extract_account(out), None);
    }

    /// `extract_account` searches for the ASCII keyword "account" in a lowercased copy of
    /// the output, then uses those byte indices to read the account token from the ORIGINAL
    /// string. The old code used `to_lowercase()`, which can change the byte length of
    /// non-ASCII characters (e.g. 'İ' U+0130 → 'i̇' U+0069+U+0307, 2 bytes → 3 bytes). When
    /// such a character appears before "account" in `gh auth status` output, the byte
    /// indices computed on the lowercased string are misaligned with the original, and the
    /// account name is silently missed. `to_ascii_lowercase()` preserves byte length so the
    /// indices stay valid.
    #[test]
    fn extract_account_handles_non_ascii_before_keyword() {
        // 'İ' (U+0130) lowercases to a 3-byte sequence under to_lowercase but is unchanged
        // (2 bytes) under to_ascii_lowercase. Placing it before "account" triggers the
        // misalignment: with to_lowercase the post-keyword index points into the middle of
        // "foo-bar" (not the space), so the account is missed.
        let out = "İ account foo-bar";
        assert_eq!(
            extract_account(out),
            Some("foo-bar".into()),
            "non-ASCII before 'account' must not misalign the byte index"
        );
    }

    #[test]
    fn parse_vercel_uses_last_non_tag_line_when_success() {
        let r = CmdResult {
            code: Some(0),
            stdout: "vercel\nsomeuser".into(),
            stderr: String::new(),
        };
        let p = parse_vercel(&r);
        assert!(p.installed);
        assert!(p.logged_in);
        assert_eq!(p.account, Some("someuser".into()));
    }

    #[test]
    fn parse_vercel_not_logged_in_when_exit_nonzero() {
        let r = CmdResult {
            code: Some(1),
            stdout: "Error: not logged in".into(),
            stderr: String::new(),
        };
        let p = parse_vercel(&r);
        assert!(p.installed);
        assert!(!p.logged_in);
        assert_eq!(p.account, None);
    }

    #[test]
    fn run_command_succeeds_for_simple_builtin() {
        // After adding CREATE_NO_WINDOW on Windows, the spawn path must still work: a
        // fixed first-party command must complete with code 0 and capture stdout. This
        // guards against a regression where the flag (or its trait import) breaks the
        // spawn on either platform.
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd", vec!["/C", "echo", "dotz-ok"])
        } else {
            ("echo", vec!["dotz-ok"])
        };
        let r = run_command(program, &args, Duration::from_secs(5));
        assert_eq!(r.code, Some(0), "expected exit 0, got stderr: {}", r.stderr);
        assert!(
            r.stdout.contains("dotz-ok"),
            "stdout should contain output, got: {}",
            r.stdout
        );
    }

    #[tokio::test]
    async fn get_connections_returns_all_three_providers() {
        let resp = get_connections().await;
        let arr = resp
            .0
            .get("connections")
            .and_then(|v| v.as_array())
            .expect("connections array");
        let ids: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("id").and_then(|x| x.as_str()))
            .collect();
        assert!(ids.contains(&"github"), "github missing: {ids:?}");
        assert!(ids.contains(&"vercel"), "vercel missing: {ids:?}");
        assert!(ids.contains(&"neon"), "neon missing: {ids:?}");
    }

    // Serialize the DOTZ_CONFIG_DIR-mutating gateway tests. An async-aware mutex is used because
    // the guard is held across `.await` points (a std MutexGuard across await can stall the
    // executor and trips clippy::await_holding_lock under -D warnings).
    static GW_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// login for an UNKNOWN id (including the three CLI providers) must stay 501 — the safety
    /// contract for github/vercel/neon is unchanged when no gateway connector is configured.
    #[tokio::test]
    async fn login_unknown_provider_stays_501() {
        let _g = GW_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("dotz-conn-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);

        let (code, _body) = login(Path("github".to_string()), None).await;
        assert_eq!(
            code,
            StatusCode::NOT_IMPLEMENTED,
            "CLI providers must keep the 501 safety stub"
        );

        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// login for a configured GATEWAY connector with an api_key body must PUT the credentials to
    /// the gateway (`PUT /api/connections/<provider>`) and return 200 — credentials go to the
    /// gateway, never stored in dotz.
    #[tokio::test]
    async fn login_gateway_connector_api_key_puts_to_gateway() {
        use axum::{routing::put, Router as AxRouter};
        use std::sync::{Arc, Mutex as SMutex};

        let _g = GW_LOCK.lock().await;

        // Stub gateway capturing the PUT path + body.
        let captured: Arc<SMutex<(String, Value)>> =
            Arc::new(SMutex::new((String::new(), Value::Null)));
        let cap = captured.clone();
        let app = AxRouter::new().route(
            "/api/connections/{provider}",
            put(
                move |axum::extract::Path(provider): axum::extract::Path<String>,
                      axum::extract::Json(body): axum::extract::Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        *cap.lock().unwrap() = (provider, body);
                        axum::Json(json!({ "status": "connected" }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = std::env::temp_dir().join(format!("dotz-conn-gw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        std::fs::write(
            dir.join("connectors.json"),
            format!(
                r#"[{{ "id": "gw", "gateway_base_url": "http://127.0.0.1:{}", "enabled": true }}]"#,
                addr.port()
            ),
        )
        .unwrap();

        let body =
            json!({ "provider": "github", "authType": "api_key", "values": { "api_key": "k" } });
        let (code, out) = login(Path("gw".to_string()), Some(Json(body))).await;
        assert_eq!(
            code,
            StatusCode::OK,
            "gateway login should succeed: {:?}",
            out.0
        );
        assert_eq!(out.0["ok"], json!(true));
        assert_eq!(out.0["provider"], "github");

        let (put_provider, put_body) = captured.lock().unwrap().clone();
        assert_eq!(put_provider, "github", "PUT must target the provider path");
        assert_eq!(put_body["authType"], "api_key");
        assert_eq!(put_body["values"]["api_key"], "k");

        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        server.abort();
    }
}
