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
const SHIM: &str = r#"
(function () {
  const invoke = (m, a) => window.__TAURI__.core.invoke('bridge', { method: m, args: a || [] });
  let statusCb = null;
  window.dotz = {
    electron: true,
    version: "0.2.0",
    pickDirectory: () => invoke('pick_directory'),
    update: {
      check: async () => {
        try {
          const r = await invoke('update_status');
          if (statusCb) {
            if (r && r.available) statusCb('available', { behind: 1, localSha: r.current || '', remoteSha: r.version || '', dirty: false });
            else statusCb('not-available', { localSha: (r && r.current) || '' });
          }
        } catch (e) { if (statusCb) statusCb('failed', { error: String(e) }); }
      },
      apply: async () => {
        if (statusCb) statusCb('applying');
        try { const r = await invoke('apply_update'); if (r && r.ok === false && statusCb) statusCb('failed', r); }
        catch (e) { if (statusCb) statusCb('failed', { error: String(e) }); }
      },
      onStatus: (cb) => { statusCb = cb; return () => { statusCb = null; }; },
    },
  };
})();
"#;

#[tauri::command]
async fn bridge(app: tauri::AppHandle, method: String, _args: Vec<Value>) -> Result<Value, String> {
    match method.as_str() {
        "pick_directory" => Ok(pick_directory(&app)),
        "update_status" => Ok(update_status(&app).await),
        "apply_update" => Ok(apply_update(&app).await),
        "version" => Ok(json!(app.package_info().version.to_string())),
        _ => Err(format!("unknown method: {method}")),
    }
}

fn pick_directory(app: &tauri::AppHandle) -> Value {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p);
    });
    match rx.recv() {
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
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    match updater.check().await {
        Ok(Some(u)) => match u.download_and_install(|_, _| {}, || {}).await {
            Ok(()) => {
                app.restart();
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        Ok(None) => json!({"ok": true, "available": false}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
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

            tauri::async_runtime::spawn(async move {
                if let Err(e) =
                    dotz_core::server::serve_with_shutdown_addr(addr, web_dir, shutdown).await
                {
                    eprintln!("dotz-core server error: {e}");
                }
            });
            app.manage(shutdown_tx);

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
                .initialization_script(SHIM)
                .build()?;
            Ok(())
        });

    let app = builder.build(tauri::generate_context!())?;
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Dispose all browser sessions and reap any lingering agent-browser processes so the
            // app does not leave headless Chrome instances running after exit. Bound the cleanup
            // so a hung `agent-browser close` command cannot block shutdown indefinitely.
            tauri::async_runtime::block_on(async {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    dotz_core::browser::dispose_all(),
                )
                .await;
            });
            dotz_core::browser::reap_stray_browsers();
            if let Some(tx) = app_handle.try_state::<tokio::sync::watch::Sender<()>>() {
                let _ = tx.send(());
            }
        }
    });
    Ok(())
}
