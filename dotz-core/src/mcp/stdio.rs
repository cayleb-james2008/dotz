//! C3 stdio transport: spawn a subprocess, JSON-RPC over stdin/stdout (NDJSON framing).
//!
//! Used by `mcp::client::Client` when `ServerConfig.transport == Stdio`. The transport is
//! abstracted behind a `Transport` trait so the client can also use the HTTP transport without
//! knowing which it's talking to.
//!
//! # Framing
//! Each JSON-RPC message is one line on stdin (write) / stdout (read). The reader is a
//! background task that pushes every parsed JSON value onto an mpsc channel keyed by the
//! message `id`; the client awaits its request id's response with a timeout.
//!
//! # Security
//! - The command runs with the user's privileges (same as the `bash` tool). The command being
//!   spawned is logged; env vars (which may carry secrets) are NOT logged.
//! - The subprocess is killed on `close()` so a leaked client doesn't leave orphans.
use super::ServerConfig;
use super::client::{McpError, Transport};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};

/// A JSON-RPC 2.0 message. The transport frames each message as one NDJSON line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcMessage {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_string()
}

/// stdio transport: a spawned subprocess + a background reader task that delivers parsed
/// JSON-RPC messages onto an mpsc channel. The client awaits its request id's response.
pub struct StdioTransport {
    /// The spawned child. Held so `close()` can kill it.
    child: Option<Child>,
    /// stdin writer (guarded so only one request is in-flight at a time).
    stdin: Option<tokio::process::ChildStdin>,
    /// Pending-response channels keyed by request id. The reader task fills these.
    pending: Arc<Mutex<HashMap<u64, oneshot_tx::Sender<Value>>>>,
    /// Next request id.
    next_id: Arc<AtomicU64>,
    /// Notification channel: server-originated messages without an `id` (e.g.
    /// `notifications/initialized` echoes, resource updates) flow here. The client drops
    /// notifications it doesn't care about. `Option` so `close()` can take it.
    notifications_tx: Option<mpsc::Sender<Value>>,
    /// A handle to the reader task so `close()` can abort it.
    _reader_handle: tokio::task::JoinHandle<()>,
}

/// Type alias to avoid repeating the oneshot sender type.
mod oneshot_tx {
    pub type Sender<T> = tokio::sync::oneshot::Sender<T>;
}

impl StdioTransport {
    /// Spawn the configured command, take stdin + stdout (stderr logged via `eprintln!`),
    /// and start the background reader task. Honors `util::no_window_tokio` so the packaged
    /// app doesn't flash a console window on every MCP server spawn (Windows).
    pub async fn spawn(config: &ServerConfig, server_name: &str) -> Result<Self, McpError> {
        let command = config
            .command
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| McpError::Config("stdio server requires `command`".into()))?;
        let args = config.args.clone().unwrap_or_default();

        // Log the command being spawned (NOT the env, which may carry secrets). This is the
        // trust-boundary audit log: the operator sees which command MCP launched.
        eprintln!(
            "mcp: spawning stdio server '{server_name}': {command} {}",
            args.join(" ")
        );

        let mut cmd = Command::new(command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = config.env.as_ref() {
            // Env vars are applied to the child WITHOUT being logged (they may carry secrets).
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        crate::util::no_window_tokio(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Spawn(format!("spawn {command}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Spawn("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Spawn("no stdout".into()))?;
        let stderr = child.stderr.take();

        let pending: Arc<Mutex<HashMap<u64, oneshot_tx::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (notifications_tx, _notifications_rx) = mpsc::channel::<Value>(64);

        // Background reader: parse stdout line-by-line, dispatch by id.
        let pending_for_reader = pending.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF: child closed stdout.
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg: Value = match serde_json::from_str(trimmed) {
                            Ok(v) => v,
                            Err(_) => continue, // Skip unparseable lines (e.g. server logs).
                        };
                        // Dispatch by id: a message with an `id` resolves a pending
                        // request; a message without an `id` is a notification (dropped
                        // here — the client doesn't subscribe to MCP server notifications
                        // in this ponytail scope).
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            let mut guard = pending_for_reader.lock().await;
                            if let Some(tx) = guard.remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                        // Notifications (no id) are dropped — ponytail: a notification
                        // handler is the upgrade path.
                    }
                    Err(_) => break,
                }
            }
        });

        // Stderr logger: log server stderr line-by-line (NOT the env or stdin contents).
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                eprintln!("mcp[stderr]: {trimmed}");
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            notifications_tx: Some(notifications_tx),
            _reader_handle: reader_handle,
        })
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    /// Send a JSON-RPC request and await the matching response (by id), with a timeout.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = JsonRpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: Some(method.to_string()),
            params,
            result: None,
            error: None,
        };
        let body = serde_json::to_string(&msg)
            .map_err(|e| McpError::Transport(format!("serialize: {e}")))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Transport("stdin closed".into()))?;
        // NDJSON framing: one line per message.
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("write stdin: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| McpError::Transport(format!("write newline: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("flush stdin: {e}")))?;

        // Register a pending-response channel, then await with a timeout.
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    return Err(McpError::Server(format!(
                        "{method} returned error: {err}"
                    )));
                }
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err(McpError::Transport(format!(
                "{method}: response channel closed"
            ))),
            Err(_) => {
                // Timeout: remove the pending entry to avoid a later-arriving response
                // leaking a sender.
                let mut guard = self.pending.lock().await;
                guard.remove(&id);
                Err(McpError::Timeout(format!("{method} timed out after 30s")))
            }
        }
    }

    /// Send a notification (no id, no response expected). Used for the `initialized` lifecycle
    /// notification after the `initialize` handshake.
    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let msg = JsonRpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some(method.to_string()),
            params,
            result: None,
            error: None,
        };
        let body = serde_json::to_string(&msg)
            .map_err(|e| McpError::Transport(format!("serialize notify: {e}")))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Transport("stdin closed".into()))?;
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("write notify: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| McpError::Transport(format!("write newline: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("flush notify: {e}")))?;
        Ok(())
    }

    /// Kill the child + close stdin. Idempotent.
    async fn close(&mut self) -> Result<(), McpError> {
        // Close stdin first so the child sees EOF and exits cleanly if it can.
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
        }
        // Kill the child (best-effort) so we don't leave orphaned MCP server processes.
        if let Some(mut child) = self.child.take() {
            // `start_kill` is non-blocking; `wait` reaps the child so it doesn't become a
            // zombie (Unix) or leak a handle (Windows).
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // The reader task exits on its own when the child's stdout closes (EOF). We don't
        // abort it explicitly — its JoinHandle is held in `_reader_handle` so it is not
        // detached; dropping it does not abort the task. ponytail: an explicit abort would
        // risk cutting off a final flush of buffered messages; the EOF-driven exit is
        // correct.
        // Drop the notifications channel so senders fail fast. `mpsc::Sender::closed()` is a
        // future; we don't need to await it — dropping the sender is enough to close the
        // channel from the send side. The receiver (held by nobody — we created it with
        // `channel(64)` and never stored the rx) is dropped when `self` is dropped.
        // `Option::take` so we don't move out of `&mut self`.
        drop(self.notifications_tx.take());
        Ok(())
    }
}
