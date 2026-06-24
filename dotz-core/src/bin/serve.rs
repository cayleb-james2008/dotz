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
    if let Err(e) = dotz_core::server::serve_with_shutdown_addr(
        addr,
        web_dir.into(),
        dotz_core::server::shutdown_signal(),
    )
    .await
    {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
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
}
