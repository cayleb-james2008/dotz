//! Headless backend runner — runs the axum server without the Tauri shell.
//! Used for dev + the base-URL parity e2e (DOTZ_PORT / DOTZ_WEB_DIR).
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("DOTZ_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4317);
    if port == 0 {
        eprintln!("error: DOTZ_PORT must be a non-zero u16");
        std::process::exit(1);
    }
    let web_dir = web_dir();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let token = session_token();
    match &token {
        Some(t) => eprintln!(
            "dotz-core session token ENFORCED on /api + /ws (from DOTZ_TOKEN) — open http://127.0.0.1:{port}/?token={t}"
        ),
        None => eprintln!(
            "dotz-core session token disabled (set DOTZ_TOKEN to require a bearer token on /api + /ws)"
        ),
    }
    if let Err(e) = dotz_core::server::serve_with_shutdown_addr_token(
        addr,
        web_dir.into(),
        dotz_core::server::shutdown_signal(),
        token,
    )
    .await
    {
        eprintln!("{}", format_startup_error(&e, &addr));
        std::process::exit(1);
    }
}

/// Format a server startup `io::Error` into a clear, actionable message. A bare OS error like
/// `"Address already in use (os error 98)"` doesn't tell the operator *which* port collided or
/// what to do; this adds the address and a remediation hint for the common port-in-use case so
/// the headless `serve` bin is as helpful as the Tauri shell's `bind_listener`.
fn format_startup_error(e: &std::io::Error, addr: &SocketAddr) -> String {
    let kind = e.kind();
    let hint = match kind {
        std::io::ErrorKind::AddrInUse => {
            format!(
                "dotz-core could not bind to {addr}: port already in use. \
                 Set DOTZ_PORT to a free port or stop the process holding {addr}."
            )
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("dotz-core could not bind to {addr}: permission denied ({e}).")
        }
        std::io::ErrorKind::AddrNotAvailable => {
            format!("dotz-core could not bind to {addr}: address not available ({e}).")
        }
        _ => format!("dotz-core server error on {addr}: {e}"),
    };
    hint
}

/// Resolve the opt-in session token from `DOTZ_TOKEN` (plan-015 follow-up). Whitespace is
/// trimmed; an unset or empty value disables enforcement, keeping the documented
/// browser-against-serve dev flow fully backward compatible.
fn session_token() -> Option<String> {
    std::env::var("DOTZ_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the static UI directory from `DOTZ_WEB_DIR`. An empty-but-set env var is treated as
/// unset so callers don't accidentally serve the current working directory.
fn web_dir() -> String {
    std::env::var("DOTZ_WEB_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "web".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn web_dir_honors_nonempty_env_and_falls_back_on_empty() {
        let guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("DOTZ_WEB_DIR").ok();

        std::env::set_var("DOTZ_WEB_DIR", "custom-web");
        assert_eq!(web_dir(), "custom-web");

        std::env::set_var("DOTZ_WEB_DIR", "");
        assert_eq!(web_dir(), "web");

        std::env::remove_var("DOTZ_WEB_DIR");
        assert_eq!(web_dir(), "web");

        match prev {
            Some(p) => std::env::set_var("DOTZ_WEB_DIR", p),
            None => std::env::remove_var("DOTZ_WEB_DIR"),
        }
        drop(guard);
    }

    /// `DOTZ_TOKEN` is trimmed, and an empty/whitespace/unset value disables enforcement — the
    /// backward-compatible default for the browser-against-serve dev flow.
    #[test]
    fn session_token_trims_and_treats_empty_as_disabled() {
        let guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("DOTZ_TOKEN").ok();

        std::env::set_var("DOTZ_TOKEN", "  abc123  ");
        assert_eq!(session_token().as_deref(), Some("abc123"));
        std::env::set_var("DOTZ_TOKEN", "   ");
        assert_eq!(session_token(), None);
        std::env::set_var("DOTZ_TOKEN", "");
        assert_eq!(session_token(), None);
        std::env::remove_var("DOTZ_TOKEN");
        assert_eq!(session_token(), None);

        match prev {
            Some(p) => std::env::set_var("DOTZ_TOKEN", p),
            None => std::env::remove_var("DOTZ_TOKEN"),
        }
        drop(guard);
    }

    /// A port-in-use error must produce a message that names the address and gives a
    /// remediation hint, so the operator knows *which* port collided and what to do — not a
    /// bare `"server error: Address already in use (os error 98)"`.
    #[test]
    fn format_startup_error_port_in_use_is_actionable() {
        let addr: SocketAddr = "127.0.0.1:4317".parse().unwrap();
        let err = std::io::Error::new(std::io::ErrorKind::AddrInUse, " Address already in use");
        let msg = format_startup_error(&err, &addr);
        assert!(
            msg.contains("127.0.0.1:4317"),
            "port-in-use message must name the address: {msg}"
        );
        assert!(
            msg.contains("port already in use"),
            "port-in-use message must explain the cause: {msg}"
        );
        assert!(
            msg.contains("DOTZ_PORT"),
            "port-in-use message must hint at the remediation: {msg}"
        );
    }

    /// A non-bind error (e.g. a generic I/O failure) must still include the address so the
    /// operator can correlate, but should not claim the port is in use.
    #[test]
    fn format_startup_error_generic_io_includes_address() {
        let addr: SocketAddr = "127.0.0.1:4317".parse().unwrap();
        let err = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "boom");
        let msg = format_startup_error(&err, &addr);
        assert!(
            msg.contains("127.0.0.1:4317"),
            "generic error message must still name the address: {msg}"
        );
        assert!(
            !msg.contains("port already in use"),
            "generic error must not falsely claim port-in-use: {msg}"
        );
        assert!(
            msg.contains("boom"),
            "generic error must preserve the underlying message: {msg}"
        );
    }
}
