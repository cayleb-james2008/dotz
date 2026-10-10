use axum::{Router, response::Html, routing::get};
use serde_json::json;
use std::net::SocketAddr;

async fn page() -> Html<&'static str> {
    Html(
        "<!doctype html><html><head><title>Dotz pinned browser smoke</title></head><body><button style='position:absolute;left:20px;top:20px;width:180px;height:80px' onclick=\"this.textContent='Pressed'\">Press me</button></body></html>",
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let app = Router::new().route("/", get(page));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let url = format!("http://{address}/");

    let before =
        dotz_core::browser::start("smoke-project", &url, None, Some((800, 600)), None, None)
            .await?;
    assert_eq!(before.page.title, "Dotz pinned browser smoke");
    assert!(
        before.snapshot.contains("Press me"),
        "initial snapshot: {}",
        before.snapshot
    );
    let independent = dotz_core::browser::start(
        "smoke-project-independent",
        &url,
        None,
        Some((800, 600)),
        None,
        None,
    )
    .await?;
    assert_eq!(independent.page.title, "Dotz pinned browser smoke");
    let after = dotz_core::browser::act(&json!({
        "sessionId": before.session_id.clone(),
        "action": "clickAt",
        "x": 60,
        "y": 60,
        "expectedSeq": before.seq,
    }))
    .await?;
    assert!(
        after.snapshot.contains("Pressed"),
        "post-click snapshot: {}",
        after.snapshot
    );
    let stopped = dotz_core::browser::stop(&before.session_id).await?;
    assert_eq!(stopped.status, "stopped");
    let independent_after = dotz_core::browser::act(&json!({
        "sessionId": independent.session_id.clone(),
        "action": "clickAt",
        "x": 60,
        "y": 60,
        "expectedSeq": independent.seq,
    }))
    .await?;
    assert!(independent_after.snapshot.contains("Pressed"));
    let independent_stopped = dotz_core::browser::stop(&independent.session_id).await?;
    assert_eq!(independent_stopped.status, "stopped");
    server.abort();
    println!("PASS: two pinned sessions, observe+click, isolated stop, cleanup");
    Ok(())
}
