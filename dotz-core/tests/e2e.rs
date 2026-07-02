//! Backend + static-frontend e2e integration test.
//!
//! Spawns the real `serve` binary (env!("CARGO_BIN_EXE_serve")) against an isolated
//! DOTZ_CONFIG_DIR and the repo's `web/` dir, then exercises the REST surface, the static UI
//! bundle, and the WebSocket handshake over real HTTP.
//!
//! - `e2e_offline` always runs and NEVER sends a WS prompt (no tokens spent).
//! - `e2e_live_prompt` only runs when DOTZ_E2E_LIVE=1 (spends real Ollama Cloud tokens).
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Absolute path to the repo's `web/` dir (the crate manifest dir is `dotz-core/`).
fn web_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("web")
        .canonicalize()
        .expect("web/ dir must exist next to dotz-core/")
}

/// A spawned `serve` process with an isolated config dir. Kills the child on Drop — Windows does
/// NOT kill children when the parent (test) exits, so the guard is mandatory.
struct ServeGuard {
    child: std::process::Child,
    port: u16,
    config_dir: PathBuf,
    work_dir: PathBuf,
    stderr_path: PathBuf,
}

impl ServeGuard {
    /// Bind 127.0.0.1:0 to reserve a free port, spawn `serve` on it with DOTZ_CONFIG_DIR /
    /// DOTZ_WEB_DIR isolation, and poll /api/health until it answers (30s budget).
    async fn start(label: &str) -> ServeGuard {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            l.local_addr().unwrap().port()
        };
        let unique = format!(
            "dotz-e2e-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let base = std::env::temp_dir().join(unique);
        let config_dir = base.join("config");
        let work_dir = base.join("work");
        std::fs::create_dir_all(&config_dir).expect("create temp config dir");
        std::fs::create_dir_all(&work_dir).expect("create temp work dir");

        let stderr_path = base.join("serve.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr capture file");

        let child = std::process::Command::new(env!("CARGO_BIN_EXE_serve"))
            .env("DOTZ_PORT", port.to_string())
            .env("DOTZ_CONFIG_DIR", &config_dir)
            .env("DOTZ_WEB_DIR", web_dir())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(stderr_file))
            .spawn()
            .expect("spawn serve binary");

        let mut guard = ServeGuard {
            child,
            port,
            config_dir,
            work_dir,
            stderr_path,
        };

        // Poll /api/health every 250ms for up to 30s.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let health = format!("http://127.0.0.1:{port}/api/health");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(resp) = client.get(&health).send().await {
                if resp.status().is_success() {
                    return guard;
                }
            }
            if std::time::Instant::now() >= deadline {
                let stderr = std::fs::read_to_string(&guard.stderr_path).unwrap_or_default();
                let _ = guard.child.kill();
                let _ = guard.child.wait();
                panic!("serve did not become healthy on port {port} within 30s.\n--- serve stderr ---\n{stderr}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn ws_url(&self, session_id: &str) -> String {
        format!("ws://127.0.0.1:{}/ws?sessionId={session_id}", self.port)
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort cleanup of the whole temp base dir (config + work + stderr log).
        if let Some(base) = self.config_dir.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// GET a path and return (status, body-bytes).
async fn get_raw(client: &reqwest::Client, base: &str, path: &str) -> (u16, Vec<u8>) {
    let resp = client
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.bytes().await.expect("read body").to_vec();
    (status, body)
}

/// GET a path, assert 200, parse JSON.
async fn get_json(client: &reqwest::Client, base: &str, path: &str) -> Value {
    let (status, body) = get_raw(client, base, path).await;
    assert_eq!(
        status,
        200,
        "GET {path} expected 200, got {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "GET {path} body is not JSON: {e}: {}",
            String::from_utf8_lossy(&body)
        )
    })
}

/// POST json to a path, return (status, json-or-null body).
async fn post_json(client: &reqwest::Client, base: &str, path: &str, body: Value) -> (u16, Value) {
    let resp = client
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {path} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, v)
}

/// Extract every `src="..."` / `href="..."` local asset path from an HTML document.
fn html_asset_paths(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        for chunk in html.split(attr).skip(1) {
            if let Some(end) = chunk.find('"') {
                let raw = &chunk[..end];
                if raw.is_empty()
                    || raw.starts_with("http://")
                    || raw.starts_with("https://")
                    || raw.starts_with('#')
                    || raw.starts_with("data:")
                    || raw.starts_with("mailto:")
                {
                    continue;
                }
                out.push(raw.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Extract every `url(...)` reference from a CSS document (skipping data:/external URLs).
fn css_url_paths(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in css.split("url(").skip(1) {
        if let Some(end) = chunk.find(')') {
            let raw = chunk[..end].trim().trim_matches('"').trim_matches('\'');
            if raw.is_empty()
                || raw.starts_with("http://")
                || raw.starts_with("https://")
                || raw.starts_with("data:")
            {
                continue;
            }
            out.push(raw.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Read the next WS *text* frame as JSON, with a timeout. Skips ping/pong frames.
async fn next_ws_json<S>(ws: &mut S, timeout: Duration, what: &str) -> Value
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for WS frame: {what}"));
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for WS frame: {what}"))
            .unwrap_or_else(|| panic!("WS stream closed while waiting for: {what}"))
            .unwrap_or_else(|e| panic!("WS error while waiting for {what}: {e}"));
        if let WsMessage::Text(t) = msg {
            return serde_json::from_str(&t)
                .unwrap_or_else(|e| panic!("WS frame not JSON ({what}): {e}: {t}"));
        }
        // Ping/Pong/Binary → keep waiting.
    }
}

// =====================================================================================
// Offline e2e: full REST + static UI + WS handshake. NEVER sends a prompt frame.
// =====================================================================================
#[tokio::test]
async fn e2e_offline() {
    let guard = ServeGuard::start("offline").await;
    let base = guard.base();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    // ---- 1. health ------------------------------------------------------------------
    let health = get_json(&client, &base, "/api/health").await;
    assert_eq!(
        health["ok"],
        json!(true),
        "1: /api/health ok must be true: {health}"
    );
    eprintln!("1: health embedderReady = {}", health["embedderReady"]);

    // ---- 2. static UI smoke ---------------------------------------------------------
    let (status, body) = get_raw(&client, &base, "/").await;
    assert_eq!(status, 200, "2: GET / must be 200");
    let html = String::from_utf8_lossy(&body).to_string();
    assert!(
        html.contains("app.js"),
        "2: index.html must reference app.js"
    );
    assert!(
        html.contains("data-panel=\"chat\""),
        "2: index.html must have the chat panel"
    );
    assert!(
        html.contains("data-panel=\"doctrine\""),
        "2: index.html must have the doctrine panel"
    );

    for path in html_asset_paths(&html) {
        let p = if path.starts_with('/') {
            path.clone()
        } else {
            format!("/{path}")
        };
        let (st, b) = get_raw(&client, &base, &p).await;
        assert_eq!(st, 200, "2: index.html asset {p} must be 200");
        assert!(!b.is_empty(), "2: index.html asset {p} must be non-empty");
    }
    let (st, css) = get_raw(&client, &base, "/fonts.css").await;
    assert_eq!(st, 200, "2: /fonts.css must be 200");
    let css = String::from_utf8_lossy(&css).to_string();
    let font_urls = css_url_paths(&css);
    assert!(
        !font_urls.is_empty(),
        "2: fonts.css must reference local font files"
    );
    for path in font_urls {
        let p = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        let (st, b) = get_raw(&client, &base, &p).await;
        assert_eq!(st, 200, "2: fonts.css asset {p} must be 200");
        assert!(!b.is_empty(), "2: fonts.css asset {p} must be non-empty");
    }

    // ---- 3. boot APIs all 200 JSON --------------------------------------------------
    let providers = get_json(&client, &base, "/api/providers").await;
    assert!(
        providers["providers"].is_array(),
        "3: /api/providers shape: {providers}"
    );
    let profiles = get_json(&client, &base, "/api/profiles").await;
    let profile_ids: Vec<&str> = profiles["profiles"]
        .as_array()
        .expect("3: profiles array")
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(
        profile_ids.contains(&"new-model-new-project"),
        "3: /api/profiles must include new-model-new-project: {profile_ids:?}"
    );
    for path in [
        "/api/models",
        "/api/config",
        "/api/projects",
        "/api/sessions",
        "/api/memory",
        "/api/skills",
        "/api/templates",
        "/api/workflows",
        "/api/connections",
        "/api/design/systems",
    ] {
        let v = get_json(&client, &base, path).await;
        assert!(
            v.is_object() || v.is_array(),
            "3: {path} must return JSON: {v}"
        );
    }

    // ---- 4. config isolation --------------------------------------------------------
    let (st, cfg) = post_json(
        &client,
        &base,
        "/api/config",
        json!({ "thinkingLevel": "low" }),
    )
    .await;
    assert_eq!(
        st, 200,
        "4: POST /api/config thinkingLevel=low must be 200: {cfg}"
    );
    let cfg = get_json(&client, &base, "/api/config").await;
    assert_eq!(
        cfg["config"]["thinkingLevel"],
        json!("low"),
        "4: GET /api/config must reflect the POSTed thinkingLevel: {cfg}"
    );
    assert!(
        guard.config_dir.join("config.json").exists(),
        "4: config.json must be persisted inside the isolated DOTZ_CONFIG_DIR ({})",
        guard.config_dir.display()
    );

    // ---- 5. session lifecycle -------------------------------------------------------
    let cwd = guard.work_dir.to_string_lossy().to_string();
    let (st, created) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "profileId": "new-model-new-project", "cwd": cwd }),
    )
    .await;
    assert_eq!(st, 200, "5: POST /api/sessions must succeed: {created}");
    let sid = created["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("5: create response must carry sessionId: {created}"))
        .to_string();
    assert_eq!(
        created["profileId"],
        json!("new-model-new-project"),
        "5: create response must echo profileId: {created}"
    );

    let list = get_json(&client, &base, "/api/sessions").await;
    assert!(
        list.as_array()
            .expect("5: /api/sessions is an array")
            .iter()
            .any(|s| s["sessionId"] == json!(sid.clone())),
        "5: new session must appear in GET /api/sessions: {list}"
    );
    let one = get_json(&client, &base, &format!("/api/sessions/{sid}")).await;
    assert_eq!(
        one["sessionId"],
        json!(sid.clone()),
        "5: GET /api/sessions/id: {one}"
    );

    let (st, m) = post_json(
        &client,
        &base,
        &format!("/api/sessions/{sid}/model"),
        json!({ "provider": "ollama", "modelId": "glm-5.2" }),
    )
    .await;
    assert_eq!(st, 200, "5: POST model must be 200: {m}");
    assert_eq!(
        m["model"]["provider"],
        json!("ollama"),
        "5: model summary: {m}"
    );
    assert_eq!(
        m["model"]["modelId"],
        json!("glm-5.2"),
        "5: model summary: {m}"
    );

    let (st, t) = post_json(
        &client,
        &base,
        &format!("/api/sessions/{sid}/thinking"),
        json!({ "level": "low" }),
    )
    .await;
    assert_eq!(st, 200, "5: POST thinking must be 200: {t}");
    assert_eq!(
        t["thinkingLevel"],
        json!("low"),
        "5: thinking response: {t}"
    );

    let tools = get_json(&client, &base, &format!("/api/sessions/{sid}/tools")).await;
    assert!(
        tools["active"].is_array() && tools["all"].is_array(),
        "5: tools shape: {tools}"
    );
    let active = tools["active"].clone();
    let (st, t2) = post_json(
        &client,
        &base,
        &format!("/api/sessions/{sid}/tools"),
        json!({ "tools": active }),
    )
    .await;
    assert_eq!(st, 200, "5: POST tools must be 200: {t2}");
    assert_eq!(
        t2["active"], tools["active"],
        "5: POST tools must echo the active set: {t2}"
    );

    let commands = get_json(&client, &base, &format!("/api/sessions/{sid}/commands")).await;
    assert!(
        commands["commands"].is_array(),
        "5: commands shape: {commands}"
    );

    let del = client
        .delete(format!("{base}/api/sessions/{sid}"))
        .send()
        .await
        .expect("5: DELETE session");
    assert_eq!(del.status().as_u16(), 200, "5: DELETE must be 200");
    let del_body: Value = del.json().await.expect("5: DELETE body JSON");
    assert_eq!(del_body["ok"], json!(true), "5: DELETE body: {del_body}");
    let (st, _) = get_raw(&client, &base, &format!("/api/sessions/{sid}")).await;
    assert_eq!(st, 404, "5: GET after DELETE must be 404");

    // ---- 6. error paths -------------------------------------------------------------
    let (st, e) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "profileId": "nope" }),
    )
    .await;
    assert_eq!(st, 400, "6: bad profileId must be 400: {e}");
    assert!(
        e["error"]
            .as_str()
            .unwrap_or("")
            .contains("profileId must be one of"),
        "6: bad profileId error body: {e}"
    );
    let (st, e) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "model": { "provider": "ollama" } }),
    )
    .await;
    assert_eq!(st, 400, "6: model missing modelId must be 400: {e}");
    let (st, e) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "thinkingLevel": "warp" }),
    )
    .await;
    assert_eq!(st, 400, "6: bad thinkingLevel must be 400: {e}");
    let (st, e) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "projectId": "ghost-project-does-not-exist" }),
    )
    .await;
    assert_eq!(st, 400, "6: ghost projectId must be 400: {e}");
    let (st, _) = get_raw(&client, &base, "/api/sessions/nope").await;
    assert_eq!(st, 404, "6: GET /api/sessions/nope must be 404");
    let (st, e) = post_json(
        &client,
        &base,
        "/api/sessions/nope/thinking",
        json!({ "level": "low" }),
    )
    .await;
    assert_eq!(
        st, 404,
        "6: POST thinking on ghost session must be 404: {e}"
    );
    let (st, e) = post_json(
        &client,
        &base,
        "/api/config",
        json!({ "provider": "not-a-provider" }),
    )
    .await;
    assert_eq!(st, 400, "6: bad provider on /api/config must be 400: {e}");

    // ---- 7. WS handshake, NO prompts ------------------------------------------------
    let (st, created) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({ "profileId": "new-model-new-project", "cwd": guard.work_dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(st, 200, "7: session for WS must be created: {created}");
    let sid = created["sessionId"].as_str().unwrap().to_string();

    let (mut ws, _resp) = tokio_tungstenite::connect_async(guard.ws_url(&sid))
        .await
        .expect("7: WS handshake with valid sessionId must succeed");
    let ready = next_ws_json(&mut ws, Duration::from_secs(10), "ready frame").await;
    assert_eq!(
        ready["kind"],
        json!("ready"),
        "7: first frame must be ready: {ready}"
    );
    assert_eq!(
        ready["sessionId"],
        json!(sid.clone()),
        "7: ready sessionId: {ready}"
    );

    // Garbage text must not kill the server.
    ws.send(WsMessage::Text("not json".into()))
        .await
        .expect("7: send garbage frame");
    // A no-op abort (no turn running) must also be harmless.
    ws.send(WsMessage::Text(
        json!({ "kind": "abort" }).to_string().into(),
    ))
    .await
    .expect("7: send abort frame");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let health = get_json(&client, &base, "/api/health").await;
    assert_eq!(
        health["ok"],
        json!(true),
        "7: server must survive garbage WS input"
    );
    ws.close(None).await.ok();

    // A bogus session id must be rejected BEFORE the upgrade → handshake error.
    let bogus = tokio_tungstenite::connect_async(guard.ws_url("does-not-exist")).await;
    assert!(
        bogus.is_err(),
        "7: WS handshake with a bogus sessionId must fail (session validated pre-upgrade)"
    );

    drop(guard);
}

// =====================================================================================
// Live e2e: real Ollama Cloud turn. Opt-in via DOTZ_E2E_LIVE=1 (spends tokens).
// =====================================================================================
#[tokio::test]
async fn e2e_live_prompt() {
    if std::env::var("DOTZ_E2E_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped (set DOTZ_E2E_LIVE=1)");
        return;
    }
    // One built-in retry: transient provider flakiness must not fail the suite outright.
    match live_attempt().await {
        Ok(()) => {}
        Err(first) => {
            eprintln!("live attempt 1 failed ({first}); retrying once...");
            if let Err(second) = live_attempt().await {
                panic!("live prompt failed twice.\nattempt 1: {first}\nattempt 2: {second}");
            }
        }
    }
}

async fn live_attempt() -> Result<(), String> {
    let guard = ServeGuard::start("live").await;
    let base = guard.base();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let (st, created) = post_json(
        &client,
        &base,
        "/api/sessions",
        json!({
            "profileId": "solo",
            "model": { "provider": "ollama", "modelId": "glm-5.2" },
            "thinkingLevel": "low",
            "cwd": guard.work_dir.to_string_lossy(),
        }),
    )
    .await;
    if st != 200 {
        return Err(format!("session create failed ({st}): {created}"));
    }
    let sid = created["sessionId"]
        .as_str()
        .ok_or_else(|| format!("no sessionId in create response: {created}"))?
        .to_string();

    let (mut ws, _resp) = tokio_tungstenite::connect_async(guard.ws_url(&sid))
        .await
        .map_err(|e| format!("WS handshake failed: {e}"))?;
    let ready = next_ws_json(&mut ws, Duration::from_secs(10), "live ready frame").await;
    if ready["kind"] != json!("ready") {
        return Err(format!("first frame was not ready: {ready}"));
    }

    ws.send(WsMessage::Text(
        json!({
            "kind": "prompt",
            "text": "Reply with exactly the word PONG and nothing else. Do not use any tools."
        })
        .to_string()
        .into(),
    ))
    .await
    .map_err(|e| format!("failed to send prompt: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut saw_start = false;
    let mut saw_end = false;
    let mut saw_gate = false;
    let mut assistant_text = String::new();

    while !saw_end {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or_else(|| {
                format!(
                    "timed out after 120s (agent_start={saw_start}, text so far: {assistant_text:?})"
                )
            })?;
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| {
                format!(
                    "timed out after 120s (agent_start={saw_start}, text so far: {assistant_text:?})"
                )
            })?
            .ok_or("WS closed mid-turn")?
            .map_err(|e| format!("WS error mid-turn: {e}"))?;
        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            _ => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["kind"].as_str().unwrap_or("") {
            "gate" => saw_gate = true,
            "event" => match v["event"]["type"].as_str().unwrap_or("") {
                "agent_start" => saw_start = true,
                "agent_end" => {
                    saw_end = true;
                    if let Some(msgs) = v["event"]["messages"].as_array() {
                        for m in msgs {
                            if m["role"] != json!("assistant") {
                                continue;
                            }
                            if let Some(blocks) = m["content"].as_array() {
                                for b in blocks {
                                    if b["type"] == json!("text") {
                                        assistant_text.push_str(b["text"].as_str().unwrap_or(""));
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    ws.close(None).await.ok();

    if !saw_start {
        return Err("never saw agent_start".into());
    }
    if saw_gate {
        return Err("unexpected gate frame during a no-tools prompt".into());
    }
    if !assistant_text.to_lowercase().contains("pong") {
        return Err(format!(
            "assistant reply did not contain PONG: {assistant_text:?}"
        ));
    }
    eprintln!("live prompt OK: {assistant_text:?}");
    Ok(())
}
