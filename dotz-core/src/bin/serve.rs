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
    let web_dir = std::env::var("DOTZ_WEB_DIR").unwrap_or_else(|_| "web".into());
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) =
        dotz_core::server::serve_with_shutdown(listener, web_dir.into(), shutdown_signal()).await
    {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

/// Wait for SIGINT (all platforms) or SIGTERM (Unix) so the axum server can drain open
/// connections instead of leaving them hanging on a hard kill.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
