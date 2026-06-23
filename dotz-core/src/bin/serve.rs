//! Headless backend runner — runs the axum server without the Tauri shell.
//! Used for dev + the base-URL parity e2e (DOTZ_PORT / DOTZ_WEB_DIR).
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("DOTZ_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4317);
    let web_dir = std::env::var("DOTZ_WEB_DIR").unwrap_or_else(|_| "web".into());
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Err(e) = dotz_core::server::serve(addr, web_dir.into()).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
