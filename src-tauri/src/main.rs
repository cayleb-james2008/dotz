//! dotz Tauri shell — starts the dotz-core axum server in-process and opens a WebView2 window at
//! http://127.0.0.1:4317, so the unchanged web/ UI talks to the Rust backend over location.host.
//! Auto-update via tauri-plugin-updater (GitHub Releases latest.json — works with a PRIVATE source
//! repo because release assets are public). Single-instance + native folder picker.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::{json, Value};
use std::net::SocketAddr;
use tauri::Manager;

const PORT: u16 = 4317;

// Injected into the WebView2 window: recreates the `window.dotz` bridge the UI (web/app.js) expects,
// mapping to Tauri commands. app.js feature-detects each field, so anything that errors degrades gracefully.
// `version` is substituted at runtime from `app.package_info().version` (sourced from tauri.conf.json at
// build time) so the UI never shows a stale hardcoded version string.
const SHIM_TEMPLATE: &str = r#"
(function () {
  // Per-process session token (plan-015 follow-up): web/app.js sends it on every /api fetch
  // (x-dotz-token header) and appends ?token= to the /ws and browser-frame <img> URLs. Injected
  // here (initialization script) so it is never embedded in served HTML.
  window.DOTZ_TOKEN = "__DOTZ_TOKEN__";
  const invoke = (m, a) => window.__TAURI__.core.invoke('bridge', { method: m, args: a || [] });
  let statusCb = null;
  window.dotz = {
    electron: true,
    version: "__DOTZ_VERSION__",
    // Call the dialog PLUGIN command directly (permitted by dialog:default) instead of our custom
    // `bridge` app-command — app commands are NOT ACL-allowed for this window's remote http origin
    // (Tauri v2), so invoke('bridge', …) fails with "bridge not allowed. Plugin not found".
    pickDirectory: async () => {
      const sel = await window.__TAURI__.core.invoke('plugin:dialog|open', { options: { directory: true, multiple: false, title: 'Select project folder' } });
      return Array.isArray(sel) ? (sel[0] || null) : (sel || null);
    },
    update: {
      check: async () => {
        try {
          const r = await invoke('update_status');
          if (statusCb) {
            if (r && r.available) statusCb('available', { behind: 1, localSha: r.current || '', remoteSha: r.version || '', dirty: false });
            else statusCb('not-available', { localSha: (r && r.current) || '' });
          }
        } catch (e) { if (statusCb) statusCb('failed', { message: String(e) }); }
      },
      apply: async () => {
        if (statusCb) statusCb('applying');
        try {
          const r = await invoke('apply_update');
          if (!statusCb) return;
          if (r && r.ok === false) statusCb('failed', r);
          else if (r && r.ok === true && r.available === false) statusCb('not-available', { localSha: '' });
        } catch (e) { if (statusCb) statusCb('failed', { message: String(e) }); }
      },
      onStatus: (cb) => { statusCb = cb; return () => { statusCb = null; }; },
    },
    // Opt-in telemetry controls for the settings panel. Each degrades to null on error so the
    // UI (web/app.js) can feature-detect and hide the control in a plain browser / dev build.
    telemetry: {
      status: async () => { try { return await invoke('telemetry_status'); } catch (e) { return null; } },
      setEnabled: async (on) => { try { return await invoke('telemetry_set_enabled', [!!on]); } catch (e) { return null; } },
    },
  };
})();
"#;

/// Build the `window.dotz` injection script with the real package version and the per-process
/// session token substituted in. The token is pure hex (see `dotz_core::server::generate_token`),
/// so it needs no escaping inside the JS string literal.
fn shim(version: &str, token: &str) -> String {
    SHIM_TEMPLATE
        .replace("__DOTZ_VERSION__", version)
        .replace("__DOTZ_TOKEN__", token)
}

#[tauri::command]
async fn bridge(app: tauri::AppHandle, method: String, args: Vec<Value>) -> Result<Value, String> {
    match method.as_str() {
        "pick_directory" => Ok(pick_directory(&app).await),
        "update_status" => Ok(update_status(&app).await),
        "apply_update" => Ok(apply_update(&app).await),
        "version" => Ok(json!(app.package_info().version.to_string())),
        // Opt-in telemetry controls (see dotz_core::telemetry). Routed through the already-
        // ACL-allowed `bridge` command so no new capability entry is needed for the remote
        // http origin the WebView loads.
        "telemetry_status" => Ok(telemetry_status()),
        "telemetry_set_enabled" => {
            let enabled = args.first().and_then(Value::as_bool).unwrap_or(false);
            Ok(telemetry_set_enabled(enabled))
        }
        _ => Err(format!("unknown method: {method}")),
    }
}

/// Current opt-in telemetry state for the settings UI: `{ enabled, endpoint }`. Reads the
/// persisted config each call so the toggle reflects on-disk truth (including a hand-edited file).
fn telemetry_status() -> Value {
    let cfg = dotz_core::telemetry::load_config();
    json!({ "enabled": cfg.enabled, "endpoint": cfg.endpoint })
}

/// Flip opt-in telemetry from the settings UI and return the new status. Turning it ON with no
/// endpoint configured defaults the sink to this app's own local receiver so the daily-active +
/// command-run signals are actually collected end-to-end (set_enabled alone leaves the endpoint
/// empty, which record_event treats as a no-op — one click would otherwise collect nothing). The
/// operator can still point telemetry at the fleet ledger by editing ~/.dotz/telemetry.json.
/// Turning it OFF clears the endpoint (dotz_core::telemetry::set_enabled), a no-op here.
fn telemetry_set_enabled(enabled: bool) -> Value {
    dotz_core::telemetry::set_enabled(enabled);
    if enabled && dotz_core::telemetry::load_config().endpoint.trim().is_empty() {
        dotz_core::telemetry::set_endpoint(format!("http://127.0.0.1:{PORT}/telemetry/ingest"));
    }
    telemetry_status()
}

async fn pick_directory(app: &tauri::AppHandle) -> Value {
    use tauri_plugin_dialog::DialogExt;
    // ponytail: the old version did a blocking std::mpsc `rx.recv()` inside this async command, which
    // parks a Tokio worker thread waiting on the dialog callback — the picker never resolved (rejected
    // as "folder picker: undefined"). pick_folder is non-blocking and dispatches the native dialog on
    // the main thread itself; await the result over a oneshot instead of blocking.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p);
    });
    match rx.await {
        Ok(Some(p)) => json!(p.to_string()),
        _ => Value::Null,
    }
}

async fn update_status(app: &tauri::AppHandle) -> Value {
    use tauri_plugin_updater::UpdaterExt;
    let cur = app.package_info().version.to_string();
    let updater = match app.updater() {
        Ok(u) => u,
        Err(_) => return json!({"ok": false, "available": false, "current": cur}),
    };
    match updater.check().await {
        Ok(Some(u)) => json!({"ok": true, "available": true, "current": cur, "version": u.version}),
        Ok(None) => json!({"ok": true, "available": false, "current": cur}),
        Err(_) => json!({"ok": false, "available": false, "current": cur}),
    }
}

async fn apply_update(app: &tauri::AppHandle) -> Value {
    use tauri_plugin_updater::UpdaterExt;
    let cur = app.package_info().version.to_string();
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "message": e.to_string()}),
    };
    match updater.check().await {
        Ok(Some(u)) => match u.download_and_install(|_, _| {}, || {}).await {
            Ok(()) => {
                app.restart();
            }
            Err(e) => json!({"ok": false, "message": e.to_string()}),
        },
        Ok(None) => json!({"ok": true, "available": false, "current": cur}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dotz fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    // Bind the server socket before spawning the server task. If port 4317 is already in use
    // (e.g. a previous dotz instance or another service) we fail fast here instead of opening a
    // window that connects to the wrong server while our own server task errors in the background.
    let listener = bind_listener(addr)?;
    // Per-process session token (plan-015 follow-up): generated fresh on every launch, enforced
    // by the embedded server on /api + /ws, and handed to the WebView exclusively through the
    // initialization script below — out-of-band, never via served HTML.
    let token = dotz_core::server::generate_token();

    let builder = tauri::Builder::default()
        // single-instance first (Tauri 2 requirement): relaunching focuses the running window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![bridge])
        .setup(move |app| {
            // Resolve bundled resources (web/, .pi/, assets/) so the embedded server can serve them.
            // Dev override: DOTZ_WEB_DIR / DOTZ_PI / DOTZ_ASSETS point at the worktree.
            let res = app
                .path()
                .resource_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let web_dir = std::env::var("DOTZ_WEB_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| res.join("web"));
            if std::env::var("DOTZ_PI").is_err() {
                std::env::set_var("DOTZ_PI", res.join(".pi"));
            }
            if std::env::var("DOTZ_ASSETS").is_err() {
                std::env::set_var("DOTZ_ASSETS", res.join("assets"));
            }
            // In-app browser binary (bundled as a resource).
            if std::env::var("DOTZ_BROWSER_BIN").is_err() {
                std::env::set_var(
                    "DOTZ_BROWSER_BIN",
                    res.join("agent-browser")
                        .join("agent-browser-win32-x64.exe"),
                );
            }

            // Graceful shutdown: notify the server task when the Tauri event loop exits so axum
            // can drain open connections instead of dropping them on process exit.
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
            let shutdown = async move {
                let _ = shutdown_rx.changed().await;
            };

            let server_token = token.clone();
            let server_task = tauri::async_runtime::spawn(async move {
                if let Err(e) = dotz_core::server::serve_with_shutdown_token(
                    listener,
                    web_dir,
                    shutdown,
                    Some(server_token),
                )
                .await
                {
                    eprintln!("dotz-core server error: {e}");
                }
            });
            app.manage(shutdown_tx);

            // Fire-and-forget the app-launch telemetry signal. Opt-in by default —
            // record_app_launch is a silent no-op until the operator enables telemetry
            // in settings, so this never blocks or sends anything without consent.
            // Spawned on the Tauri async runtime (already running here) so it can't
            // delay window creation.
            tauri::async_runtime::spawn(async move {
                dotz_core::telemetry::record_app_launch().await;
                // Daily-active heartbeat: the other half of the weekly-active metric. Gated to at
                // most once per UTC calendar day inside dotz-core, and (like record_app_launch) a
                // silent no-op until the operator opts in — so it never sends without consent.
                dotz_core::telemetry::record_daily_active_if_new_day().await;
            });
            // Keep the server task's JoinHandle so the Exit handler can actually await the
            // drain — signalling shutdown without awaiting it lets the process die mid-drain,
            // truncating in-flight requests and WS close frames. Mutex<Option<..>> because the
            // RunEvent closure only gets shared state access and the handle must be taken once.
            app.manage(std::sync::Mutex::new(Some(server_task)));

            // Wait until the server accepts connections, then open the window on it.
            let mut ready = false;
            for _ in 0..100 {
                if std::net::TcpStream::connect(addr).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !ready {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "dotz-core server failed to start in time",
                )) as Box<dyn std::error::Error>);
            }
            let url = format!("http://127.0.0.1:{PORT}");
            let parsed = url.parse().map_err(|e| {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid URL {url}: {e}"),
                )) as Box<dyn std::error::Error>
            })?;
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(parsed))
                .title("dotz · ultra code")
                .inner_size(1480.0, 920.0)
                .min_inner_size(1000.0, 700.0)
                .initialization_script(shim(&app.package_info().version.to_string(), &token))
                .build()?;
            Ok(())
        });

    let app = builder.build(tauri::generate_context!())?;
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Dispose all browser sessions and reap any lingering agent-browser processes so the
            // app does not leave headless Chrome instances running after exit. Bound the cleanup
            // so a hung `agent-browser close` command cannot block shutdown indefinitely.
            // 2 s, not 10: graceful close is best-effort (reap_stray_browsers force-kills the
            // image right after), and the WHOLE exit path must stay well under ~9 s of WM_CLOSE —
            // measured 2026-07-13: three hung session closes at 3 s each pushed exit to 10.1 s,
            // which reads as a hung app to `taskkill` verification. Budget now ≈ 2 s here +
            // reap + 3 s drain ≈ 5.5 s worst case.
            tauri::async_runtime::block_on(async {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    dotz_core::browser::dispose_all(),
                )
                .await;
            });
            dotz_core::browser::reap_stray_browsers();
            if let Some(tx) = app_handle.try_state::<tokio::sync::watch::Sender<()>>() {
                let _ = tx.send(());
            }
            // Await the server's graceful drain with a bounded timeout. The watch signal above
            // only STARTS the drain (axum stops accepting and waits for connection tasks); if
            // the process exits immediately the drain is truncated and in-flight work is
            // dropped on the floor. 3 s is generous — WS loops break promptly on the shutdown
            // watch — while the bound guarantees a hung connection can't wedge app exit.
            let server_task = app_handle
                .try_state::<std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>()
                .and_then(|s| s.lock().ok().and_then(|mut g| g.take()));
            if let Some(task) = server_task {
                tauri::async_runtime::block_on(async {
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(3), task).await;
                });
            }
        }
    });
    Ok(())
}

/// Bind a tokio TcpListener for the dotz-core server, failing fast with a clear message when the
/// configured port is already in use.
fn bind_listener(
    addr: SocketAddr,
) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error + Send + Sync>> {
    let std_listener = std::net::TcpListener::bind(addr).map_err(|e| {
        Box::new(std::io::Error::new(
            e.kind(),
            format!("dotz-core server could not bind to {addr}: {e}"),
        )) as Box<dyn std::error::Error + Send + Sync>
    })?;
    std_listener.set_nonblocking(true).map_err(|e| {
        Box::new(std::io::Error::new(
            e.kind(),
            format!("failed to set non-blocking mode for server socket: {e}"),
        )) as Box<dyn std::error::Error + Send + Sync>
    })?;
    // ponytail: TcpListener::from_std must run inside a Tokio reactor, but run()/main() is sync.
    // Convert on the same global tauri async runtime the server task later runs on, so the listener
    // is registered to the reactor that will poll it. (Fixes "there is no reactor running" panic.)
    let listener =
        tauri::async_runtime::block_on(
            async move { tokio::net::TcpListener::from_std(std_listener) },
        )?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim must substitute BOTH placeholders — a leftover `__DOTZ_TOKEN__` would inject a
    /// useless literal token and brick every authenticated call from the WebView.
    #[test]
    fn shim_substitutes_version_and_token() {
        let s = shim("9.9.9", "aabbccdd");
        assert!(s.contains("version: \"9.9.9\""), "version substituted: {s}");
        assert!(
            s.contains("window.DOTZ_TOKEN = \"aabbccdd\";"),
            "token substituted: {s}"
        );
        assert!(!s.contains("__DOTZ_VERSION__"));
        assert!(!s.contains("__DOTZ_TOKEN__"));
    }

    /// Binding to an already-occupied port must fail fast with a clear error message instead of
    /// letting the Tauri shell open a window against the wrong server.
    #[tokio::test]
    async fn bind_listener_fails_when_port_in_use() {
        let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupant.local_addr().unwrap();

        let result = bind_listener(addr);
        assert!(
            result.is_err(),
            "bind_listener must fail when the port is already in use"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("could not bind"),
            "error should explain the bind failure: {msg}"
        );
    }

    /// Binding to an ephemeral/free port must succeed and return a usable tokio listener.
    /// Plain #[test]: bind_listener block_on's the global tauri runtime internally, which
    /// panics ("cannot start a runtime from within a runtime") under #[tokio::test] — exactly
    /// like production, where run() calls it from the sync main thread.
    #[test]
    fn bind_listener_succeeds_on_free_port() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = bind_listener(addr).expect("should bind to a free ephemeral port");
        let bound_addr = listener
            .local_addr()
            .expect("listener should have a local address");
        assert!(
            bound_addr.port() > 0,
            "ephemeral bind should return a non-zero port"
        );
    }
}
