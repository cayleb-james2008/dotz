//! C3 MCP client: connect to a server, list/call tools, list/read resources, list prompts.
//!
//! Owns a `Transport` (stdio or http), runs the JSON-RPC `initialize` handshake, caches the
//! tool list, and exposes the high-level methods the registry + `mcp_call` tool use.
//!
//! # Handshake
//! 1. Send `initialize` with `{protocolVersion, capabilities:{}, clientInfo:{name, version}}`.
//! 2. Receive `{protocolVersion, capabilities, serverInfo}`.
//! 3. Send `notifications/initialized` (a notification, no id, no response expected).
//! 4. Cache the server's protocol version + capabilities; mark the client `initialized`.
//!
//! # Mockability
//! The `Transport` trait is the seam: tests inject a `MockTransport` that returns canned
//! JSON-RPC responses keyed by method name, so the client's handshake + list/call paths can
//! be exercised without spawning a real subprocess or HTTP server.
use super::{
    CLIENT_NAME, CLIENT_VERSION, OauthConfig, PROTOCOL_VERSION, ServerConfig, TransportType,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP errors. The variant names what layer failed; the string is a human-readable detail
/// (token values are never included — see the `mcp_secrets_never_logged` test).
#[derive(Debug)]
pub enum McpError {
    /// Bad server config (missing command, missing URL, etc.).
    Config(String),
    /// Subprocess spawn / pipe failure (stdio transport).
    Spawn(String),
    /// Wire transport failure (stdin/stdout IO, HTTP IO, parse, timeout, mismatched id).
    Transport(String),
    /// JSON-RPC `error` object returned by the server.
    Server(String),
    /// Request timed out.
    Timeout(String),
    /// OAuth flow failure (DCR, device flow, token store).
    Oauth(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Config(s) => write!(f, "mcp config error: {s}"),
            McpError::Spawn(s) => write!(f, "mcp spawn error: {s}"),
            McpError::Transport(s) => write!(f, "mcp transport error: {s}"),
            McpError::Server(s) => write!(f, "mcp server error: {s}"),
            McpError::Timeout(s) => write!(f, "mcp timeout: {s}"),
            McpError::Oauth(s) => write!(f, "mcp oauth error: {s}"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<super::oauth::McpOauthError> for McpError {
    fn from(e: super::oauth::McpOauthError) -> Self {
        McpError::Oauth(e.to_string())
    }
}

/// The transport seam. Production impls: `StdioTransport`, `HttpTransport`. Tests inject
/// `MockTransport` (in this module's tests) so the client logic can be exercised without a
/// real subprocess or HTTP server.
#[async_trait::async_trait]
pub trait Transport: Send {
    /// Send a JSON-RPC request and await the matching response. Returns the `result` field
    /// (or `Err(McpError::Server(...))` when the server returned an `error` object).
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError>;
    /// Send a JSON-RPC notification (no id, no response expected).
    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError>;
    /// Close the transport (kill subprocess, drop HTTP client, etc.). Idempotent.
    async fn close(&mut self) -> Result<(), McpError>;
}

/// A tool definition returned by `tools/list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input. Stored as a raw `serde_json::Value` so the client
    /// doesn't need to model the full JSON-Schema dialect — the agent dispatch passes it
    /// straight through to the model as the tool's `parameters` schema.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// A resource definition returned by `resources/list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResourceDef {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

/// A prompt template definition returned by `prompts/list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArg>,
}

/// One argument of a prompt template.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptArg {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "required")]
    pub required: bool,
}

/// The MCP client. Owns the transport, runs the handshake, caches tools.
pub struct Client {
    /// Server name (for logging + registry keying).
    name: String,
    /// The transport (stdio or http). Boxed so the client is a single concrete type.
    transport: Box<dyn Transport>,
    /// Cached tool list (populated on first `list_tools` call). `None` = not yet fetched.
    tools: Option<Vec<ToolDef>>,
    /// Server's negotiated protocol version (from the `initialize` response).
    protocol_version: Option<String>,
    /// True after the `initialize` + `notifications/initialized` handshake completes.
    initialized: bool,
}

impl Client {
    /// Connect to an MCP server: build the right transport from the config, run the
    /// handshake, and return the client. For HTTP servers with an `oauth` block, the
    /// transport is created lazily (the first request triggers OAuth if needed); for stdio
    /// the subprocess is spawned eagerly here.
    pub async fn connect(config: &ServerConfig, name: &str) -> Result<Self, McpError> {
        let transport: Box<dyn Transport> = match config.transport {
            TransportType::Stdio => {
                Box::new(super::stdio::StdioTransport::spawn(config, name).await?)
            }
            TransportType::Http => Box::new(super::http::HttpTransport::new(config, name).await?),
        };
        let mut client = Self {
            name: name.to_string(),
            transport,
            tools: None,
            protocol_version: None,
            initialized: false,
        };
        client.initialize().await?;
        Ok(client)
    }

    /// Build a client from a pre-constructed transport (test seam). Skips the handshake so
    /// the test's mock can control the `initialize` response explicitly when needed.
    #[allow(dead_code)] // used by tests in this module
    pub fn from_transport(name: &str, transport: Box<dyn Transport>) -> Self {
        Self {
            name: name.to_string(),
            transport,
            tools: None,
            protocol_version: None,
            initialized: false,
        }
    }

    /// The `initialize` handshake: send `initialize`, expect a response with
    /// `protocolVersion` + `capabilities` + `serverInfo`, then send the
    /// `notifications/initialized` notification. Caches the protocol version.
    pub async fn initialize(&mut self) -> Result<(), McpError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION,
            }
        });
        let result = self.transport.request("initialize", Some(params)).await?;
        // Cache the negotiated protocol version (server may negotiate down).
        if let Some(pv) = result.get("protocolVersion").and_then(|v| v.as_str()) {
            self.protocol_version = Some(pv.to_string());
        }
        // Send the `notifications/initialized` notification (no response expected).
        self.transport
            .notify("notifications/initialized", Some(serde_json::json!({})))
            .await?;
        self.initialized = true;
        Ok(())
    }

    /// The server's negotiated protocol version (None before the handshake completes).
    #[allow(dead_code)]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// True once the `initialize` handshake has completed.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// The server name this client is connected to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// List the server's tools. Caches the result; subsequent calls return the cache without
    /// a round-trip. Format of `tools/list` response: `{ tools: [{name, description?, inputSchema}] }`.
    pub async fn list_tools(&mut self) -> Result<&[ToolDef], McpError> {
        if self.tools.is_none() {
            let result = self.transport.request("tools/list", None).await?;
            let tools_arr = result
                .get("tools")
                .cloned()
                .unwrap_or_else(|| Value::Array(vec![]));
            let tools: Vec<ToolDef> = serde_json::from_value(tools_arr)
                .map_err(|e| McpError::Transport(format!("parse tools/list: {e}")))?;
            self.tools = Some(tools);
        }
        Ok(self.tools.as_ref().unwrap())
    }

    /// Call a tool by name. Returns the `tools/call` result (typically
    /// `{ content: [{type, text}], isError }`). The caller decides how to render the
    /// content blocks; the `mcp_call` tool flattens them to a string.
    pub async fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        let params = serde_json::json!({
            "name": name,
            "arguments": args,
        });
        let result = self.transport.request("tools/call", Some(params)).await?;
        Ok(result)
    }

    /// List the server's resources.
    pub async fn list_resources(&mut self) -> Result<Vec<ResourceDef>, McpError> {
        let result = self.transport.request("resources/list", None).await?;
        let arr = result
            .get("resources")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        serde_json::from_value(arr)
            .map_err(|e| McpError::Transport(format!("parse resources/list: {e}")))
    }

    /// Read a resource by URI. Returns the `resources/read` result
    /// (`{ contents: [{uri, mimeType?, text?}] }`).
    pub async fn read_resource(&mut self, uri: &str) -> Result<Value, McpError> {
        let params = serde_json::json!({ "uri": uri });
        let result = self
            .transport
            .request("resources/read", Some(params))
            .await?;
        Ok(result)
    }

    /// List the server's prompt templates.
    pub async fn list_prompts(&mut self) -> Result<Vec<PromptDef>, McpError> {
        let result = self.transport.request("prompts/list", None).await?;
        let arr = result
            .get("prompts")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        serde_json::from_value(arr)
            .map_err(|e| McpError::Transport(format!("parse prompts/list: {e}")))
    }

    /// Close the transport (kill subprocess / drop HTTP client). Idempotent.
    pub async fn close(&mut self) -> Result<(), McpError> {
        self.transport.close().await
    }

    /// The OAuth config from the server (used by the registry to surface auth state to the
    /// UI; None for stdio servers or HTTP servers without an oauth block). Exposed so tests
    /// can assert the config is plumbed through.
    pub fn oauth_config(_server: &ServerConfig) -> Option<OauthConfig> {
        _server.oauth.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A mock transport that returns canned JSON-RPC responses keyed by method name. Tests
    /// inject this so the client's handshake + list/call paths run without a real server.
    #[allow(clippy::type_complexity)] // test-only mock; the boxed closure type is intentionally inline
    struct MockTransport {
        /// Canned (method, response) pairs. The first match wins. Stored as a closure so the
        /// mock can return a fresh error each call (McpError isn't Clone).
        responses: Vec<(
            &'static str,
            Box<dyn Fn() -> Result<Value, McpError> + Send + Sync>,
        )>,
        /// Captured requests in order.
        captured: Arc<Mutex<Vec<(String, Option<Value>)>>>,
        /// True after `close()` is called.
        closed: bool,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                responses: Vec::new(),
                captured: Arc::new(Mutex::new(Vec::new())),
                closed: false,
            }
        }
        fn with(mut self, method: &'static str, result: Value) -> Self {
            self.responses
                .push((method, Box::new(move || Ok(result.clone()))));
            self
        }
        fn with_err<F>(mut self, method: &'static str, err_fn: F) -> Self
        where
            F: Fn() -> McpError + Send + Sync + 'static,
        {
            self.responses
                .push((method, Box::new(move || Err(err_fn()))));
            self
        }
    }

    #[async_trait::async_trait]
    impl Transport for MockTransport {
        async fn request(
            &mut self,
            method: &str,
            params: Option<Value>,
        ) -> Result<Value, McpError> {
            self.captured
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            for (m, f) in &self.responses {
                if *m == method {
                    return f();
                }
            }
            Err(McpError::Transport(format!(
                "mock: no canned response for {method}"
            )))
        }
        async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
            self.captured
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            Ok(())
        }
        async fn close(&mut self) -> Result<(), McpError> {
            self.closed = true;
            Ok(())
        }
    }

    /// A canned `initialize` response (minimal: protocolVersion + capabilities + serverInfo).
    fn init_response() -> Value {
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
            "serverInfo": { "name": "mock-server", "version": "1.0" }
        })
    }

    /// The client runs `initialize` + sends `notifications/initialized` after connect.
    #[tokio::test]
    async fn mcp_client_initialize_handshake() {
        let mock = MockTransport::new().with("initialize", init_response());
        let captured = mock.captured.clone();
        let mut client = Client::from_transport("test", Box::new(mock));
        assert!(!client.is_initialized());
        client
            .initialize()
            .await
            .expect("handshake should complete");
        assert!(client.is_initialized());
        assert_eq!(
            client.protocol_version(),
            Some(PROTOCOL_VERSION),
            "protocol version cached from response"
        );
        // The client sent `initialize` then `notifications/initialized` (in that order).
        let captured = captured.lock().unwrap().clone();
        assert!(
            captured.len() >= 2,
            "handshake sends initialize + initialized"
        );
        assert_eq!(captured[0].0, "initialize");
        assert_eq!(captured[1].0, "notifications/initialized");
    }

    /// `list_tools` parses the `tools/list` response and caches it.
    #[tokio::test]
    async fn mcp_client_list_tools() {
        let tools_resp = serde_json::json!({
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } }
                },
                {
                    "name": "write_file",
                    "description": "Write a file",
                    "inputSchema": { "type": "object" }
                }
            ]
        });
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with("tools/list", tools_resp);
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let tools = client.list_tools().await.expect("list_tools should parse");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description.as_deref(), Some("Read a file"));
        assert!(tools[0].input_schema.get("properties").is_some());
    }

    /// `call_tool` sends `tools/call` with the tool name + arguments and returns the result.
    #[tokio::test]
    async fn mcp_client_call_tool() {
        let call_resp = serde_json::json!({
            "content": [
                { "type": "text", "text": "hello from tool" }
            ],
            "isError": false
        });
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with("tools/call", call_resp);
        let captured = mock.captured.clone();
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let result = client
            .call_tool("read_file", &serde_json::json!({ "path": "/tmp/x" }))
            .await
            .expect("call_tool should return result");
        assert!(result.get("content").is_some());
        // Verify the request shape.
        let captured = captured.lock().unwrap().clone();
        let call_req = captured
            .iter()
            .find(|(m, _)| m == "tools/call")
            .expect("tools/call was sent");
        assert_eq!(
            call_req
                .1
                .as_ref()
                .unwrap()
                .get("name")
                .and_then(|v| v.as_str()),
            Some("read_file")
        );
        assert_eq!(
            call_req
                .1
                .as_ref()
                .unwrap()
                .get("arguments")
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str()),
            Some("/tmp/x")
        );
    }

    /// `call_tool` surfaces a server-returned JSON-RPC error as `Err(McpError::Server(...))`.
    #[tokio::test]
    async fn mcp_client_call_tool_error() {
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with_err("tools/call", || {
                McpError::Server("tool not found".to_string())
            });
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let err = client
            .call_tool("no-such", &serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            McpError::Server(s) => assert!(s.contains("tool not found")),
            other => panic!("expected McpError::Server, got {other:?}"),
        }
    }

    /// `list_resources` parses the `resources/list` response.
    #[tokio::test]
    async fn mcp_client_list_resources() {
        let resp = serde_json::json!({
            "resources": [
                {
                    "uri": "file:///tmp/x",
                    "name": "x.txt",
                    "description": "A file",
                    "mimeType": "text/plain"
                }
            ]
        });
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with("resources/list", resp);
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let resources = client.list_resources().await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file:///tmp/x");
        assert_eq!(resources[0].name.as_deref(), Some("x.txt"));
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
    }

    /// `read_resource` sends `resources/read` with the URI and returns the result.
    #[tokio::test]
    async fn mcp_client_read_resource() {
        let resp = serde_json::json!({
            "contents": [
                { "uri": "file:///tmp/x", "mimeType": "text/plain", "text": "hello" }
            ]
        });
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with("resources/read", resp);
        let captured = mock.captured.clone();
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let result = client.read_resource("file:///tmp/x").await.unwrap();
        assert_eq!(
            result
                .get("contents")
                .and_then(|c| c.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        // Verify the URI was sent.
        let captured = captured.lock().unwrap().clone();
        let read_req = captured
            .iter()
            .find(|(m, _)| m == "resources/read")
            .expect("resources/read was sent");
        assert_eq!(
            read_req
                .1
                .as_ref()
                .unwrap()
                .get("uri")
                .and_then(|v| v.as_str()),
            Some("file:///tmp/x")
        );
    }

    /// `list_prompts` parses the `prompts/list` response.
    #[tokio::test]
    async fn mcp_client_list_prompts() {
        let resp = serde_json::json!({
            "prompts": [
                {
                    "name": "review_code",
                    "description": "Review code",
                    "arguments": [
                        { "name": "code", "description": "The code", "required": true }
                    ]
                }
            ]
        });
        let mock = MockTransport::new()
            .with("initialize", init_response())
            .with("prompts/list", resp);
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        let prompts = client.list_prompts().await.unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "review_code");
        assert_eq!(prompts[0].arguments.len(), 1);
        assert_eq!(prompts[0].arguments[0].name, "code");
        assert!(prompts[0].arguments[0].required);
    }

    /// `close` delegates to the transport (subprocess killed / HTTP client dropped).
    #[tokio::test]
    async fn mcp_client_close() {
        let mock = MockTransport::new().with("initialize", init_response());
        let mut client = Client::from_transport("test", Box::new(mock));
        client.initialize().await.unwrap();
        client.close().await.expect("close should succeed");
        // The mock is dropped here; we can't directly assert `closed` because the mock moved
        // into the client. Instead, calling close again is idempotent (no panic, no error).
        client.close().await.expect("close is idempotent");
    }
}
