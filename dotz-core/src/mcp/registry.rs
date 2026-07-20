//! C3 global registry of connected MCP clients.
//!
//! `connect_all(cwd)` reads the MCP config, connects to each server in parallel, caches the
//! connected `Client`s + their tool lists. `get(server_name)` returns a clone-safe handle;
//! `call(server_name, tool, args)` routes a tool call to the right server; `list_all_tools()`
//! returns a flat list of `{server, tool}` entries for the agent dispatch (surfaced via the
//! `mcp_call` tool in `agent::extra_tools.rs`).
//!
//! The registry is a process-global `OnceLock<Mutex<HashMap<String, ClientHandle>>>`. A
//! `ClientHandle` wraps an `Arc<Mutex<Client>>` so callers can clone the handle without
//! borrowing the registry mutex while a tool call is in flight (which would serialize all
//! MCP tool calls — we want per-server concurrency, not global).
//!
//! # ponytail
//! - `connect_all` is best-effort: a server that fails to connect (spawn error, HTTP error,
//!   OAuth error) is skipped with a stderr warning; the other servers still load. A future
//!   UI will surface per-server connect state.
//! - The registry does NOT auto-connect on dotz startup. The agent (or the operator via a
//!   future REST endpoint) calls `connect_all` when MCP tools are needed. This keeps startup
//!   fast and avoids spawning subprocesses the user never asked for (the trust boundary).
use super::client::{Client, ToolDef};
use super::load_config;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

/// A connected MCP server, wrapped so the handle can be cloned cheaply. The inner `Client`
/// is behind an async mutex so per-server tool calls are serialized (the JSON-RPC protocol
/// expects one request in flight per server in this ponytail scope — pipelining is the
/// upgrade path).
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<AsyncMutex<Client>>,
}

impl ClientHandle {
    /// Wrap a connected client in a handle. Public so tests can construct a handle from a
    /// mock-backed client (the registry's `test_insert` helper accepts these).
    pub fn new(client: Client) -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(client)),
        }
    }

    /// Lock the inner client and call a tool. Acquires the per-server mutex (so calls to the
    /// same server are serialized; calls to different servers run concurrently).
    pub async fn call_tool(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, super::client::McpError> {
        let mut client = self.inner.lock().await;
        client.call_tool(tool, args).await
    }

    /// Lock the inner client and list its tools (returns a clone so the caller doesn't hold
    /// the lock while rendering).
    pub async fn list_tools(&self) -> Result<Vec<ToolDef>, super::client::McpError> {
        let mut client = self.inner.lock().await;
        Ok(client.list_tools().await?.to_vec())
    }

    /// The server name this handle is connected to.
    pub async fn name(&self) -> String {
        self.inner.lock().await.name().to_string()
    }
}

/// A flat tool entry returned by `list_all_tools`: one row per (server, tool) pair. The
/// `mcp_call` tool surfaces these to the agent (the agent dispatches by server+tool name).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolEntry {
    pub server: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The process-global registry: `OnceLock<Mutex<HashMap<String, ClientHandle>>>`. Poison
/// recovery is handled by `registry_guard` so a panicking connect/call doesn't brick the
/// registry for the rest of the process.
static REGISTRY: OnceLock<Mutex<HashMap<String, ClientHandle>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, ClientHandle>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering from a poisoned lock. A panic while holding the registry
/// (e.g. inside a connect or a tool call) must not permanently brick MCP.
fn registry_guard() -> std::sync::MutexGuard<'static, HashMap<String, ClientHandle>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read the MCP config from `<cwd>/.dotz/mcp.json` + `~/.dotz/mcp.json`, connect to every
/// configured server in parallel (best-effort: failures are logged and skipped), and cache
/// the connected clients. Returns the count of successfully connected servers.
///
/// Idempotent: re-calling replaces the registry contents (existing clients are closed first
/// so we don't leak subprocesses / HTTP connections on a reload).
pub async fn connect_all(cwd: &Path) -> usize {
    let config = load_config(cwd);
    if config.servers.is_empty() {
        // No servers configured: clear any existing registry (idempotent no-op when empty).
        clear().await;
        return 0;
    }
    // Connect in parallel (best-effort per server).
    let mut tasks: Vec<_> = Vec::new();
    for (name, server_cfg) in &config.servers {
        let name = name.clone();
        let server_cfg = server_cfg.clone();
        tasks.push(tokio::spawn(async move {
            match Client::connect(&server_cfg, &name).await {
                Ok(client) => Some((name, ClientHandle::new(client))),
                Err(e) => {
                    eprintln!("mcp: failed to connect to server '{name}': {e}");
                    None
                }
            }
        }));
    }
    let mut connected: HashMap<String, ClientHandle> = HashMap::new();
    for task in tasks {
        if let Ok(Some((name, handle))) = task.await {
            connected.insert(name, handle);
        }
    }
    let count = connected.len();
    // Close any existing clients we're about to replace (so we don't leak subprocesses on
    // a reload). Take the old map out, drop the handles (which closes the transports).
    let old = {
        let mut guard = registry_guard();
        std::mem::replace(&mut *guard, connected)
    };
    // Close the old clients outside the registry lock so a slow close doesn't block other
    // callers. Each handle's close acquires its own per-server mutex.
    drop(old);
    count
}

/// Clear the registry, closing all connected clients. Used by tests + by `connect_all` when
/// the config is empty.
pub async fn clear() {
    let old = {
        let mut guard = registry_guard();
        std::mem::take(&mut *guard)
    };
    // Close each old client.
    for (_, handle) in old {
        let _ = handle.inner.lock().await.close().await;
    }
}

/// Get a handle for a connected server by name. None when the server isn't connected.
pub fn get(server_name: &str) -> Option<ClientHandle> {
    registry_guard().get(server_name).cloned()
}

#[cfg(test)]
/// Test-only: inject a pre-built client handle into the registry under the given server name.
/// Used by the `mcp_call_tool_routes_to_registry` test to exercise the agent dispatch path
/// without spawning a real MCP subprocess.
pub fn test_insert(server_name: &str, handle: ClientHandle) {
    registry_guard().insert(server_name.to_string(), handle);
}

#[cfg(test)]
/// Test-only: remove a server from the registry (cleanup).
pub async fn test_remove(server_name: &str) {
    let handle = registry_guard().remove(server_name);
    if let Some(h) = handle {
        let _ = h.inner.lock().await.close().await;
    }
}

/// Call a tool on a connected server. Errors when the server isn't connected.
pub async fn call(
    server_name: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, super::client::McpError> {
    let handle = get(server_name).ok_or_else(|| {
        super::client::McpError::Server(format!("mcp server '{server_name}' is not connected"))
    })?;
    handle.call_tool(tool, args).await
}

/// Flat list of all tools across all connected servers. Each entry is `{server, name,
/// description?}`. The `mcp_call` tool surfaces these to the agent.
pub async fn list_all_tools() -> Vec<McpToolEntry> {
    let handles: Vec<ClientHandle> = {
        let guard = registry_guard();
        guard.values().cloned().collect()
    };
    let mut out = Vec::new();
    for handle in handles {
        let name = handle.name().await;
        match handle.list_tools().await {
            Ok(tools) => {
                for t in tools {
                    out.push(McpToolEntry {
                        server: name.clone(),
                        name: t.name,
                        description: t.description,
                    });
                }
            }
            Err(e) => {
                eprintln!("mcp: list_tools failed for server '{name}': {e}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::dotz_config_dir_test_lock;

    /// Lock `DOTZ_CONFIG_DIR` + create a temp dir for the duration of the async closure.
    /// The closure receives the dir and returns a future; the helper awaits that future,
    /// then restores the env + clears the registry. Designed for `#[tokio::test]` callers.
    ///
    /// The DOTZ_CONFIG_DIR guard is a std Mutex held across the closure's `.await`s. This
    /// is intentional (the awaited tasks never acquire `dotz_config_dir_test_lock`, so the
    /// deadlock the lint guards against cannot occur) — matches the same idiom in
    /// `memory::tests::recall_async_keeps_reactor_free_while_embedder_mutex_is_held`.
    #[allow(clippy::await_holding_lock)]
    async fn with_tmp_dir_async<T, F, Fut>(f: F) -> T
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-mcp-reg-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        let result = f(dir.clone()).await;
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Clear the registry so the next test starts clean (the env var is restored above,
        // but the global registry persists across tests).
        clear().await;
        drop(_guard);
        result
    }

    /// `connect_all` with no servers configured → 0 connected, empty registry.
    #[tokio::test]
    async fn mcp_registry_connect_all_empty() {
        with_tmp_dir_async(|dir| async move {
            // No .dotz/mcp.json → empty config.
            let n = connect_all(&dir).await;
            assert_eq!(n, 0, "no servers configured → 0 connected");
            assert!(
                get("bogus").is_none(),
                "empty registry → get returns None for any name"
            );
        })
        .await;
    }

    /// `get("bogus")` returns None when the server isn't in the registry (even when the
    /// registry is non-empty). This is the agent-dispatch error path for `mcp_call` with an
    /// unknown server.
    #[tokio::test]
    async fn mcp_registry_get_returns_none_for_unknown() {
        with_tmp_dir_async(|dir| async move {
            // Connect nothing; the registry is empty.
            let _ = connect_all(&dir).await;
            assert!(get("bogus").is_none());
            // And `call` on an unknown server returns an McpError.
            let err = call("bogus", "tool", &serde_json::json!({}))
                .await
                .unwrap_err();
            match err {
                super::super::client::McpError::Server(s) => {
                    assert!(
                        s.contains("not connected"),
                        "error should say not connected: {s}"
                    )
                }
                other => panic!("expected McpError::Server, got {other:?}"),
            }
        })
        .await;
    }

    /// `list_all_tools` on an empty registry returns an empty list.
    #[tokio::test]
    async fn mcp_registry_list_all_tools_empty() {
        with_tmp_dir_async(|dir| async move {
            let _ = connect_all(&dir).await;
            let tools = list_all_tools().await;
            assert!(tools.is_empty(), "no clients → empty tool list");
        })
        .await;
    }

    /// A panic while holding the registry mutex must not permanently brick MCP. With poison
    /// recovery, `get` + `clear` keep working after a previous lock owner panicked.
    #[tokio::test]
    async fn mcp_registry_guard_recovers_from_poisoned_mutex() {
        // Ensure the registry is initialized.
        drop(registry().lock().unwrap());
        let m = registry();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("intentional registry mutex poison");
        }));
        assert!(poisoned.is_err(), "registry mutex should be poisoned");
        // registry_guard must recover and return a usable guard.
        {
            let guard = registry_guard();
            let _ = guard.len();
        }
        // A fresh clear must also work after poison recovery.
        clear().await;
    }
}
