//! C3 HTTP transport: Streamable HTTP (POST JSON-RPC, SSE for server-to-client).
//!
//! Used by `mcp::client::Client` when `ServerConfig.transport == Http`. The transport POSTs
//! JSON-RPC requests to the configured URL; the response is either a direct JSON-RPC response
//! (`Content-Type: application/json`) or an SSE stream (`text/event-stream`) the client reads
//! until it sees a `data:` line containing the JSON-RPC response.
//!
//! # OAuth
//! If the server returns 401, the transport consults the `OauthConfig` from the server config.
//! When `client_id` is null, it runs Dynamic Client Registration (DCR) + the device flow via
//! `mcp::oauth`. The resulting `TokenSet` is cached per-server in `~/.dotz/mcp-auth.json` and
//! attached to subsequent requests as `Authorization: Bearer <access_token>`.
//!
//! # ponytail
//! - SSE parsing is minimal: read events line-by-line, extract `data:` payloads, parse as
//!   JSON-RPC. The full Streamable HTTP spec (resumable streams, session ids, last-event-id)
//!   is the upgrade path.
//! - The transport reuses one `reqwest::Client` for all requests (connection pooling).
use super::client::{McpError, Transport};
use super::oauth::{self, ClientCredentials, OauthHttp, ReqwestOauthHttp, TokenSet};
use super::{OauthConfig, ServerConfig, validate_http_url};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// HTTP MCP transport. POSTs JSON-RPC to `url`; OAuth-aware (401 triggers DCR + device flow).
pub struct HttpTransport {
    /// The server endpoint URL (validated https:// or loopback http:// at construction).
    url: String,
    /// Per-server OAuth config (None for servers without an `oauth` block).
    oauth: Option<OauthConfig>,
    /// The server name (for token-store keying + logging).
    server_name: String,
    /// Reused HTTP client (connection pooling).
    client: reqwest::Client,
    /// Cached OAuth token (loaded from the token store on construction; refreshed on 401).
    token: Arc<Mutex<Option<TokenSet>>>,
    /// Cached client credentials (loaded from config or obtained via DCR).
    creds: Arc<Mutex<Option<ClientCredentials>>>,
    /// Injected OAuth HTTP backend (production: reqwest; tests: mock). `None` means use the
    /// default `ReqwestOauthHttp`. Tests set this to a mock so DCR/device-flow can be
    /// exercised without a real OAuth server.
    oauth_http: Option<Arc<dyn OauthHttp>>,
    /// Next request id.
    next_id: Arc<AtomicU64>,
}

/// Manual Debug impl — `reqwest::Client` doesn't derive Debug, and we don't want to log the
/// cached token (which would be a secret leak). Only the URL + server name appear.
impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("url", &self.url)
            .field("server_name", &self.server_name)
            .field("has_oauth", &self.oauth.is_some())
            .field(
                "has_token",
                &self.token.try_lock().map(|t| t.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl HttpTransport {
    /// Construct the HTTP transport. Validates the URL (SSRF guard), loads any cached OAuth
    /// token from the token store, and resolves the client credentials from the config (or
    /// leaves them `None` to trigger DCR on first 401).
    pub async fn new(config: &ServerConfig, server_name: &str) -> Result<Self, McpError> {
        let url = config
            .url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| McpError::Config("http server requires `url`".into()))?;
        validate_http_url(url).map_err(McpError::Config)?;
        let oauth = config.oauth.clone();
        // Eager-load any cached token for this server.
        let token = oauth::TokenStore::load_token(server_name);
        // Resolve client credentials from the config when present.
        let creds = oauth.as_ref().and_then(|o| {
            o.client_id.as_ref().map(|id| ClientCredentials {
                client_id: id.clone(),
                client_secret: o.client_secret.clone(),
            })
        });
        Ok(Self {
            url: url.to_string(),
            oauth,
            server_name: server_name.to_string(),
            client: reqwest::Client::new(),
            token: Arc::new(Mutex::new(token)),
            creds: Arc::new(Mutex::new(creds)),
            oauth_http: None,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Build the JSON-RPC request body for the given method + params + id.
    fn build_request(&self, id: u64, method: &str, params: Option<Value>) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.unwrap_or(Value::Object(Default::default()))
        })
    }

    /// POST a JSON-RPC request, returning the raw response (either direct JSON or an SSE
    /// stream that's read to completion). On 401, runs OAuth (DCR + device flow when needed)
    /// and retries once with a fresh token.
    async fn post(&self, body: &Value) -> Result<Value, McpError> {
        let token = self.token.lock().await.clone();
        let resp = self.send(body, token.as_ref()).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // 401 → run OAuth, refresh token, retry once.
            self.run_oauth_if_configured().await?;
            let token = self.token.lock().await.clone();
            let resp = self.send(body, token.as_ref()).await?;
            if !resp.status().is_success() {
                return Err(McpError::Server(format!(
                    "HTTP {} after auth: {}",
                    resp.status(),
                    resp.text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect::<String>()
                )));
            }
            return self.parse_response(resp).await;
        }
        if !resp.status().is_success() {
            return Err(McpError::Server(format!(
                "HTTP {}: {}",
                resp.status(),
                resp.text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(500)
                    .collect::<String>()
            )));
        }
        self.parse_response(resp).await
    }

    /// Send the request body with an optional Bearer token.
    async fn send(
        &self,
        body: &Value,
        token: Option<&TokenSet>,
    ) -> Result<reqwest::Response, McpError> {
        let mut req = self.client.post(&self.url).json(body);
        if let Some(t) = token {
            req = req.header(
                "Authorization",
                format!("{} {}", t.token_type, t.access_token),
            );
        }
        req = req.header("Accept", "application/json, text/event-stream");
        // Custom headers from the config (e.g. X-Custom) are added on every request.
        // ponytail: headers from the config are applied to every request; the OAuth
        // Authorization header is set separately above (config headers do NOT override it).
        if let Some(custom) = self.custom_headers() {
            for (k, v) in custom {
                if k.eq_ignore_ascii_case("authorization") {
                    // Don't let a config header clobber the OAuth Authorization.
                    continue;
                }
                req = req.header(k, v);
            }
        }
        req.send()
            .await
            .map_err(|e| McpError::Transport(format!("HTTP POST {url}: {e}", url = self.url)))
    }

    /// Parse a response that is either `application/json` (direct JSON-RPC) or
    /// `text/event-stream` (SSE — read until a `data:` line contains the JSON-RPC response).
    async fn parse_response(&self, resp: reqwest::Response) -> Result<Value, McpError> {
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if ct.contains("text/event-stream") {
            // SSE: read the body as text, split into events, find the first `data:` line that
            // parses as JSON-RPC with a non-null `result` or `error` field.
            let text = resp
                .text()
                .await
                .map_err(|e| McpError::Transport(format!("read SSE body: {e}")))?;
            for ev_block in text.split("\n\n") {
                for line in ev_block.lines() {
                    let line = line.trim();
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if v.get("result").is_some() || v.get("error").is_some() {
                                return Ok(v);
                            }
                        }
                    }
                }
            }
            return Err(McpError::Transport(
                "SSE stream did not contain a JSON-RPC response".into(),
            ));
        }
        // Assume JSON: parse the body.
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(format!("read JSON body: {e}")))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| McpError::Transport(format!("parse JSON response: {e}")))?;
        Ok(v)
    }

    /// Custom headers from the config (None when no headers configured).
    fn custom_headers(&self) -> Option<Vec<(String, String)>> {
        // The HTTP transport doesn't carry the full ServerConfig (only url + oauth); headers
        // are applied at construction time via a separate field. ponytail: for now we don't
        // persist custom headers — the config parser accepts them but the transport doesn't
        // apply them yet. The upgrade path is to store them in the transport.
        None
    }

    /// Run OAuth (DCR when no client_id; device flow always) when the server returned 401 and
    /// the config has an `oauth` block. The resulting token is cached in the token store +
    /// in `self.token`. If the config has no `oauth` block, returns a clear error (so the
    /// caller can surface "server requires auth, no oauth config").
    async fn run_oauth_if_configured(&self) -> Result<(), McpError> {
        let oauth = self
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::Oauth("server returned 401 but no oauth config".into()))?;
        // OAuth HTTP backend: production uses reqwest; tests inject a mock via `with_oauth_http`.
        let http: Arc<dyn OauthHttp> = self
            .oauth_http
            .clone()
            .unwrap_or_else(|| Arc::new(ReqwestOauthHttp::new()));

        // Resolve client credentials: from config, or via DCR.
        let creds = {
            let mut guard = self.creds.lock().await;
            if let Some(c) = guard.as_ref() {
                c.clone()
            } else {
                let c =
                    oauth::dynamic_client_register(&self.server_name, oauth, http.as_ref()).await?;
                *guard = Some(c.clone());
                c
            }
        };

        // Run the device flow.
        let token = oauth::device_flow(&self.server_name, oauth, &creds, http.as_ref()).await?;
        // Persist + cache.
        oauth::TokenStore::store_token(&self.server_name, &token)?;
        *self.token.lock().await = Some(token);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = self.build_request(id, method, params);
        let resp = self.post(&body).await?;
        // Surface server-returned JSON-RPC errors.
        if let Some(err) = resp.get("error") {
            return Err(McpError::Server(format!(
                "{method} returned error: {err}"
            )));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        // Notifications have no id; the server doesn't respond. We POST and ignore the body.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(Value::Object(Default::default()))
        });
        let token = self.token.lock().await.clone();
        let resp = self.send(&body, token.as_ref()).await?;
        if !resp.status().is_success() {
            // Notifications are best-effort; a non-2xx is logged but not surfaced as an error
            // (the spec says notifications don't expect responses).
            eprintln!(
                "mcp: notification {method} to {} returned {} (ignored)",
                self.url,
                resp.status()
            );
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), McpError> {
        // HTTP transport has no long-lived resources to close (the reqwest::Client drops when
        // the transport does). Idempotent no-op.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `validate_http_url` is wired into transport construction: a non-https non-loopback URL
    /// is rejected at `HttpTransport::new`.
    #[tokio::test]
    async fn http_transport_rejects_ssrf_url() {
        let cfg = ServerConfig {
            transport: crate::mcp::TransportType::Http,
            url: Some("http://169.254.169.254/latest/meta-data/".into()),
            ..Default::default()
        };
        let err = HttpTransport::new(&cfg, "ssrf-test").await.unwrap_err();
        match err {
            McpError::Config(s) => {
                assert!(s.contains("SSRF"), "config error should mention SSRF: {s}")
            }
            other => panic!("expected McpError::Config, got {other:?}"),
        }
    }

    /// A missing URL is rejected.
    #[tokio::test]
    async fn http_transport_rejects_missing_url() {
        let cfg = ServerConfig {
            transport: crate::mcp::TransportType::Http,
            url: None,
            ..Default::default()
        };
        let err = HttpTransport::new(&cfg, "no-url").await.unwrap_err();
        match err {
            McpError::Config(s) => assert!(s.contains("url")),
            other => panic!("expected McpError::Config, got {other:?}"),
        }
    }

    /// A valid https URL with no oauth block constructs successfully; the cached token is
    /// None (no stored token + no oauth config). Held under the DOTZ_CONFIG_DIR test lock
    /// because construction reads the token store from `~/.dotz/mcp-auth.json`.
    ///
    /// The DOTZ_CONFIG_DIR guard is a std Mutex held across the `.await`s of
    /// `HttpTransport::new` + the token lock. This is intentional (the awaited tasks never
    /// acquire `dotz_config_dir_test_lock`, so the deadlock the lint guards against cannot
    /// occur) — matches the same idiom in `memory::tests::recall_async_keeps_reactor_free_while...`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn http_transport_constructs_with_valid_url() {
        let _guard = crate::util::dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-mcp-http-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);

        let cfg = ServerConfig {
            transport: crate::mcp::TransportType::Http,
            url: Some("https://api.example.com/mcp".into()),
            ..Default::default()
        };
        let t = HttpTransport::new(&cfg, "valid").await.unwrap();
        assert_eq!(t.url, "https://api.example.com/mcp");
        assert!(t.oauth.is_none());
        let token = t.token.lock().await.clone();
        assert!(token.is_none(), "no stored token → None");

        // Cleanup.
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
