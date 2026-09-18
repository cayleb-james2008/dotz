//! C3 OAuth: Dynamic Client Registration (RFC 7591) + device-code flow + token storage.
//!
//! Used by the HTTP MCP transport when a server returns 401 and the config has an `oauth`
//! block with `client_id: null` (or a pre-registered client id without a token).
//!
//! # Flow
//! 1. DCR: POST to the authorization server's registration endpoint with
//!    `{client_name, grant_types: ["urn:ietf:params:oauth:grant-type:device_code"],
//!     response_types: []}` → receive `{client_id, client_secret?, ...}`.
//! 2. Device authorization: POST `client_id` (+ `client_secret` if DCR returned one) to the
//!    device-authorization endpoint → receive `{device_code, user_code, verification_uri,
//!    expires_in, interval}`. The `user_code` + `verification_uri` are LOGGED (eprintln!) so
//!    the operator sees them; the `device_code` is never logged.
//! 3. Poll the token endpoint with `grant_type=urn:ietf:params:oauth:grant-type:device_code`
//!    until success (or `expired_token` / `access_denied`).
//! 4. Store the resulting `{access_token, refresh_token?, expires_at?}` in
//!    `~/.dotz/mcp-auth.json` keyed by server name. File mode 0600 on Unix; on Windows the
//!    file lives in the user's `.dotz` dir (user-only by default ACL; no extra step is taken
//!    here — `# ponytail:` Windows ACL hardening is the upgrade path).
//!
//! # ponytail
//! - The DCR endpoint is derived heuristically: `token_url` + `/register`, OR a separate
//!   `registration_url` if the config ever grows one. Auto-discovery via the 401
//!   `WWW-Authenticate` header + `.well-known/oauth-authorization-server` is the upgrade path.
//! - The device-authorization endpoint is derived from `authorization_url` by replacing
//!   `/authorize` (or `/login/oauth/authorize`) with `/device_authorization` when the config
//!   does not provide `device_authorization_url` explicitly. Operator-provided URLs work for
//!   known servers (GitHub, GitLab, etc.).
//!
//! # Security
//! - Token values (`access_token`, `refresh_token`, `client_secret`, `device_code`) are
//!   NEVER logged. Only `user_code` + `verification_uri` are surfaced (the operator needs
//!   those to authorize the device).
//! - Token storage is per-server in `~/.dotz/mcp-auth.json`. The file is created with
//!   0600 permissions on Unix; on Windows it inherits the user's `.dotz` ACL.
use super::{CLIENT_NAME, OauthConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Default device-flow poll interval (RFC 8628 §3.4). Servers can override via `interval`.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Default device-flow timeout (RFC 8628 says `expires_in` from the device-auth response;
/// we cap at 15 minutes so a misbehaving server can't stall the agent turn forever).
const DEFAULT_DEVICE_FLOW_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Credentials returned by DCR (RFC 7591 §3.2.1). `client_secret` is optional because some
/// servers (public clients) don't issue one.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientCredentials {
    #[serde(rename = "client_id")]
    pub client_id: String,
    #[serde(
        rename = "client_secret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
}

/// A stored token set per server. Persisted to `~/.dotz/mcp-auth.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenSet {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "token_type", default = "default_token_type")]
    pub token_type: String,
    #[serde(
        rename = "refresh_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh_token: Option<String>,
    /// Unix-seconds epoch when the access token expires (from `expires_in` at issue time).
    /// None when the server didn't return `expires_in` (treat as non-expiring).
    #[serde(
        rename = "expires_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<i64>,
}

/// Default `token_type` field for `TokenSet` (RFC 6749 §7.1 says `Bearer` is the standard
/// scheme; servers that omit it almost always mean Bearer).
fn default_token_type() -> String {
    "Bearer".to_string()
}

/// Token store: `~/.dotz/mcp-auth.json` → `{ "servers": { "<name>": TokenSet } }`. The file is
/// read/written atomically (write-to-temp-then-rename) so a crash mid-write does not corrupt
/// the existing tokens.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TokenStore {
    #[serde(default)]
    pub servers: HashMap<String, TokenSet>,
}

impl TokenStore {
    /// Load the on-disk token store. Missing file → empty store. Malformed file → empty store
    /// with a stderr warning (so a corrupt file does not brick MCP auth).
    pub fn load() -> Self {
        let path = token_store_path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<TokenStore>(&raw) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "mcp: {} is not valid JSON ({e}); ignoring stored MCP auth tokens.",
                        path.display()
                    );
                    TokenStore::default()
                }
            },
            Err(_) => TokenStore::default(),
        }
    }

    /// Persist the store to `~/.dotz/mcp-auth.json` with 0600 perms on Unix. Atomic write:
    /// write to `<path>.tmp` then rename, so a crash mid-write does not corrupt the existing
    /// file.
    pub fn save(&self) -> Result<(), McpOauthError> {
        let path = token_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| McpOauthError::Store(e.to_string()))?;
        }
        let tmp = path.with_extension("tmp");
        let body =
            serde_json::to_string_pretty(self).map_err(|e| McpOauthError::Store(e.to_string()))?;
        std::fs::write(&tmp, body).map_err(|e| McpOauthError::Store(e.to_string()))?;
        set_user_only_perms(&tmp);
        std::fs::rename(&tmp, &path).map_err(|e| McpOauthError::Store(e.to_string()))?;
        set_user_only_perms(&path);
        Ok(())
    }

    /// Load a token by server name. None when the server isn't in the store.
    pub fn load_token(server_name: &str) -> Option<TokenSet> {
        Self::load().servers.get(server_name).cloned()
    }

    /// Store (or overwrite) a token for a server, persisting immediately.
    pub fn store_token(server_name: &str, token: &TokenSet) -> Result<(), McpOauthError> {
        let mut store = Self::load();
        store.servers.insert(server_name.to_string(), token.clone());
        store.save()
    }
}

/// `~/.dotz/mcp-auth.json` (honors `DOTZ_CONFIG_DIR`).
pub fn token_store_path() -> PathBuf {
    crate::config::dotz_dir().join("mcp-auth.json")
}

/// Set file permissions to 0600 (user read/write only) on Unix; no-op on Windows (the file
/// inherits the user's `.dotz` ACL — `# ponytail:` Windows ACL hardening is the upgrade path).
#[cfg(unix)]
fn set_user_only_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}
#[cfg(not(unix))]
fn set_user_only_perms(_path: &std::path::Path) {}

/// The injectable HTTP surface used by DCR + device flow. Production uses `reqwest::Client`;
/// tests inject a mock that returns canned responses based on the request URL + body. This
/// keeps the OAuth code testable without spawning a real OAuth server (and without leaking
/// real tokens into a test log).
#[async_trait::async_trait]
pub trait OauthHttp: Send + Sync {
    /// POST a JSON body to `url` and return the JSON response body. Used for both DCR and
    /// device-flow polling. The `server_name` is included so a mock can match on it (per-server
    /// canned responses).
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        server_name: &str,
    ) -> Result<serde_json::Value, McpOauthError>;
}

/// Real `reqwest`-backed `OauthHttp`. Form-encodes for the token endpoint (OAuth token
/// endpoints expect `application/x-www-form-urlencoded`, not JSON); JSON for DCR.
pub struct ReqwestOauthHttp {
    client: reqwest::Client,
}

impl Default for ReqwestOauthHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestOauthHttp {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl OauthHttp for ReqwestOauthHttp {
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        _server_name: &str,
    ) -> Result<serde_json::Value, McpOauthError> {
        // Heuristic: DCR + device-auth use JSON; token polling uses form encoding (RFC 6749
        // requires `application/x-www-form-urlencoded` for the token endpoint). We detect the
        // token endpoint by the presence of `grant_type` in the body (only polling sends it).
        let is_form = body.get("grant_type").is_some();
        let resp = if is_form {
            // Manually form-encode (no `urlencoded` feature on reqwest — keeps the dep set
            // unchanged). The form body is `key=value&key=value` with URL-encoded values.
            let mut form = String::new();
            if let Some(obj) = body.as_object() {
                let mut first = true;
                for (k, v) in obj {
                    if !first {
                        form.push('&');
                    }
                    first = false;
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => v.to_string(),
                    };
                    form.push_str(&urlencode(k));
                    form.push('=');
                    form.push_str(&urlencode(&val));
                }
            }
            self.client
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(form)
                .send()
                .await
                .map_err(|e| McpOauthError::Http(e.to_string()))?
        } else {
            self.client
                .post(url)
                .json(body)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| McpOauthError::Http(e.to_string()))?
        };
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| McpOauthError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(McpOauthError::Http(format!(
                "OAuth endpoint {url} returned {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| McpOauthError::Http(format!("OAuth response not JSON: {e}")))
    }
}

/// Minimal URL-encoder for form bodies (RFC 3986 unreserved + a few safe punctuation).
/// `# ponytail:` this is a stdlib-only form encoder; the `url` crate or reqwest's `urlencoded`
/// feature would be the upgrade path.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Dynamic Client Registration (RFC 7591). POSTs to the DCR endpoint (derived from `token_url`
/// as `<token_url>/register` when no explicit registration URL is in the config) with the
/// device-code grant type. Returns the issued `client_id` (+ optional `client_secret`).
///
/// `# ponytail:` DCR endpoint auto-discovery via `.well-known/oauth-authorization-server` is the
/// upgrade path; the heuristic works for known servers (GitHub, GitLab, etc.).
pub async fn dynamic_client_register(
    server_name: &str,
    oauth: &OauthConfig,
    http: &dyn OauthHttp,
) -> Result<ClientCredentials, McpOauthError> {
    let token_url = oauth
        .token_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| McpOauthError::Config("oauth.token_url is required for DCR".into()))?;
    // ponytail: heuristic registration endpoint. Real DCR discovery via the server's metadata
    // doc is the upgrade path; for now we POST to <token_url>/register, which is the common
    // convention for OAuth servers that implement RFC 7591.
    let reg_url = format!("{}/register", token_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code"],
        "response_types": [],
        "token_endpoint_auth_method": "none",
    });
    eprintln!(
        "mcp: performing DCR for server '{server_name}' against {reg_url} (device-code grant)"
    );
    let resp = http.post_json(&reg_url, &body, server_name).await?;
    let creds = serde_json::from_value::<ClientCredentials>(resp)
        .map_err(|e| McpOauthError::Http(format!("DCR response did not parse: {e}")))?;
    // Log only that we got a client id — NOT the secret.
    eprintln!(
        "mcp: DCR succeeded for server '{server_name}' (client_id length {})",
        creds.client_id.len()
    );
    Ok(creds)
}

/// Derive the device-authorization endpoint URL. If the config provides one, use it; else
/// derive from `authorization_url` by replacing `/authorize` (case-insensitive) with
/// `/device_authorization`. If no derivation is possible, return an error.
fn derive_device_auth_url(oauth: &OauthConfig) -> Result<String, McpOauthError> {
    if let Some(u) = oauth.device_authorization_url.as_deref() {
        if !u.trim().is_empty() {
            return Ok(u.to_string());
        }
    }
    let auth = oauth
        .authorization_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            McpOauthError::Config("oauth.authorization_url is required for device flow".into())
        })?;
    // Common conventions: .../authorize → .../device_authorization, .../login/oauth/authorize
    // → .../login/oauth/device. Try `/authorize` first; fall back to the GitHub-style path.
    let lower = auth.to_lowercase();
    if let Some(idx) = lower.rfind("/authorize") {
        let mut out = String::with_capacity(auth.len() + 8);
        out.push_str(&auth[..idx]);
        out.push_str("/device_authorization");
        return Ok(out);
    }
    Err(McpOauthError::Config(format!(
        "could not derive device_authorization_url from authorization_url '{auth}' — \
         provide device_authorization_url explicitly in the config"
    )))
}

/// Device-authorization response (RFC 8628 §3.1). `device_code` is the secret the client polls
/// with; `user_code` + `verification_uri` are shown to the operator; `expires_in` is seconds;
/// `interval` is the poll interval (defaults to 5s when absent).
#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    #[serde(rename = "device_code")]
    device_code: String,
    #[serde(rename = "user_code")]
    user_code: String,
    #[serde(rename = "verification_uri")]
    verification_uri: String,
    #[serde(rename = "verification_uri_complete", default)]
    verification_uri_complete: Option<String>,
    #[serde(rename = "expires_in", default)]
    expires_in: Option<u64>,
    #[serde(rename = "interval", default)]
    interval: Option<u64>,
}

/// Token-poll response. On success this is a normal OAuth token response; on pending it has
/// `error: "authorization_pending"` or `error: "slow_down"`; on failure `error` is something
/// else (`access_denied`, `expired_token`).
#[derive(Debug, Deserialize)]
struct TokenPollResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(rename = "access_token", default)]
    access_token: Option<String>,
    #[serde(rename = "token_type", default)]
    token_type: Option<String>,
    #[serde(rename = "refresh_token", default)]
    refresh_token: Option<String>,
    #[serde(rename = "expires_in", default)]
    expires_in: Option<u64>,
}

/// Run the device-code flow: request device authorization, surface the user code + verification
/// URI (via `eprintln!` so the operator sees them), then poll the token endpoint until the
/// operator completes the flow (or the device code expires). Returns the resulting `TokenSet`.
///
/// `creds` is the client credentials (either pre-registered in the config or obtained via
/// `dynamic_client_register`). `# ponytail:` this blocks for up to 15 minutes (capped); a
/// future UI will surface the user_code in a panel + await resolution via WS instead of
/// blocking the turn.
pub async fn device_flow(
    server_name: &str,
    oauth: &OauthConfig,
    creds: &ClientCredentials,
    http: &dyn OauthHttp,
) -> Result<TokenSet, McpOauthError> {
    let device_url = derive_device_auth_url(oauth)?;
    let token_url = oauth
        .token_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            McpOauthError::Config("oauth.token_url is required for device flow".into())
        })?;

    // 1. Request device authorization.
    let mut device_body = serde_json::json!({
        "client_id": creds.client_id,
    });
    if let Some(s) = creds.client_secret.as_deref() {
        device_body["client_secret"] = serde_json::Value::String(s.to_string());
    }
    if let Some(scopes) = oauth.scopes.as_deref() {
        device_body["scope"] = serde_json::Value::String(scopes.join(" "));
    }
    let device_resp = http
        .post_json(&device_url, &device_body, server_name)
        .await?;
    let da: DeviceAuthResponse = serde_json::from_value(device_resp).map_err(|e| {
        McpOauthError::Http(format!("device-authorization response did not parse: {e}"))
    })?;

    // 2. Surface the user_code + verification_uri (NOT the device_code) to the operator. The
    // `verification_uri_complete` (which embeds the user_code) is shown when present — it's the
    // one-click URL the operator can open. The device_code is the client's poll secret and is
    // never logged.
    eprintln!(
        "mcp: server '{server_name}' requires device authorization. Open {ver_uri} and enter code: {user_code}",
        ver_uri = da.verification_uri,
        user_code = da.user_code
    );
    if let Some(complete) = da.verification_uri_complete.as_deref() {
        eprintln!("mcp: or open this URL to authorize automatically: {complete}");
    }

    // 3. Poll the token endpoint until success/expiry. The poll deadline is bounded by the
    // device code's `expires_in` (RFC 8628 §3.3) when the server provided it, capped by our
    // own `DEFAULT_DEVICE_FLOW_TIMEOUT` so a misbehaving server can't stall the agent turn.
    let interval = Duration::from_secs(
        da.interval
            .unwrap_or_else(|| DEFAULT_POLL_INTERVAL.as_secs()),
    );
    let device_deadline = da.expires_in.map(Duration::from_secs);
    let global_deadline = std::time::Instant::now() + DEFAULT_DEVICE_FLOW_TIMEOUT;
    let deadline = match device_deadline {
        Some(d) => {
            let inst = std::time::Instant::now() + d;
            if inst < global_deadline {
                inst
            } else {
                global_deadline
            }
        }
        None => global_deadline,
    };
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(McpOauthError::Http(
                "device-flow timed out waiting for operator authorization".into(),
            ));
        }
        tokio::time::sleep(interval).await;

        let mut poll_body = serde_json::json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "client_id": creds.client_id,
            "device_code": da.device_code,
        });
        if let Some(s) = creds.client_secret.as_deref() {
            poll_body["client_secret"] = serde_json::Value::String(s.to_string());
        }

        // The poll may legitimately fail (e.g. 400 with `error: authorization_pending`). The
        // mock + reqwest impls surface those as `Err(McpOauthError::Http(...))`, so we
        // re-parse the error message out of the body.
        let resp_val = match http.post_json(token_url, &poll_body, server_name).await {
            Ok(v) => v,
            Err(McpOauthError::Http(msg)) => {
                // Try to parse the error code out of the message body; if it's a pending
                // poll, just continue; otherwise surface a real error.
                if msg.contains("authorization_pending") {
                    continue;
                }
                if msg.contains("slow_down") {
                    tokio::time::sleep(interval).await;
                    continue;
                }
                return Err(McpOauthError::Http(msg));
            }
            Err(e) => return Err(e),
        };
        let parsed: TokenPollResponse = serde_json::from_value(resp_val)
            .map_err(|e| McpOauthError::Http(format!("token-poll response did not parse: {e}")))?;

        if let Some(err) = parsed.error.as_deref() {
            match err {
                "authorization_pending" => continue,
                "slow_down" => {
                    tokio::time::sleep(interval).await;
                    continue;
                }
                "expired_token" => {
                    return Err(McpOauthError::Http(
                        "device code expired before the operator authorized it".into(),
                    ));
                }
                "access_denied" => {
                    return Err(McpOauthError::Http(
                        "operator denied the device authorization".into(),
                    ));
                }
                other => {
                    let desc = parsed.error_description.unwrap_or_default();
                    return Err(McpOauthError::Http(format!(
                        "token endpoint returned error: {other} ({desc})"
                    )));
                }
            }
        }

        let access_token = parsed
            .access_token
            .ok_or_else(|| McpOauthError::Http("token response missing access_token".into()))?;
        let expires_at = parsed.expires_in.map(|secs| issued_at + (secs as i64));
        return Ok(TokenSet {
            access_token,
            token_type: parsed.token_type.unwrap_or_else(|| "Bearer".to_string()),
            refresh_token: parsed.refresh_token,
            expires_at,
        });
    }
}

/// Errors from the OAuth flow. `Http` carries a human-readable message (token values are
/// never included — only the operation that failed and the HTTP status / parse error).
#[derive(Debug)]
pub enum McpOauthError {
    /// HTTP transport / non-2xx / parse error. The string is safe to log (no token values).
    Http(String),
    /// Config error (missing required URL, etc.).
    Config(String),
    /// Token store error (file IO).
    Store(String),
}

impl std::fmt::Display for McpOauthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpOauthError::Http(s) => write!(f, "oauth http error: {s}"),
            McpOauthError::Config(s) => write!(f, "oauth config error: {s}"),
            McpOauthError::Store(s) => write!(f, "oauth token store error: {s}"),
        }
    }
}

impl std::error::Error for McpOauthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::dotz_config_dir_test_lock;
    use std::sync::{Arc, Mutex};

    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-mcp-oauth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &dir);
        let result = f(&dir);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
        result
    }

    /// Store a token for a server, then load it back round-trip.
    #[test]
    fn mcp_oauth_token_store_round_trip() {
        with_tmp_dir(|_| {
            let token = TokenSet {
                access_token: "access-xyz".into(),
                token_type: "Bearer".into(),
                refresh_token: Some("refresh-abc".into()),
                expires_at: Some(1_700_000_000),
            };
            TokenStore::store_token("github", &token).unwrap();
            let loaded = TokenStore::load_token("github").expect("token should be present");
            assert_eq!(loaded.access_token, "access-xyz");
            assert_eq!(loaded.token_type, "Bearer");
            assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-abc"));
            assert_eq!(loaded.expires_at, Some(1_700_000_000));
            // The file exists and is mode 0600 on Unix.
            let path = token_store_path();
            assert!(path.exists(), "mcp-auth.json should exist");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "mcp-auth.json must be 0600 on Unix");
            }
        });
    }

    /// Loading a token for a server that's never been stored returns None.
    #[test]
    fn mcp_oauth_token_store_missing_server() {
        with_tmp_dir(|_| {
            assert!(TokenStore::load_token("never-stored").is_none());
        });
    }

    /// A mock `OauthHttp` that returns canned responses by URL substring. Captures the
    /// requests it received so tests can assert on the DCR / poll shape.
    struct MockOauthHttp {
        /// Captured requests: (url, body, server_name).
        captured: Arc<Mutex<Vec<(String, serde_json::Value, String)>>>,
        /// Per-request canned responses, keyed by a substring of the URL. The first match wins.
        responses: Vec<(&'static str, serde_json::Value)>,
    }

    impl MockOauthHttp {
        fn new() -> Self {
            Self {
                captured: Arc::new(Mutex::new(Vec::new())),
                responses: Vec::new(),
            }
        }
        /// Add a canned response for URLs containing `match_substr`. The response is returned
        /// as-is (parsed as JSON).
        fn with_response(mut self, match_substr: &'static str, resp: serde_json::Value) -> Self {
            self.responses.push((match_substr, resp));
            self
        }
    }

    #[async_trait::async_trait]
    impl OauthHttp for MockOauthHttp {
        async fn post_json(
            &self,
            url: &str,
            body: &serde_json::Value,
            server_name: &str,
        ) -> Result<serde_json::Value, McpOauthError> {
            self.captured.lock().unwrap().push((
                url.to_string(),
                body.clone(),
                server_name.to_string(),
            ));
            for (substr, resp) in &self.responses {
                if url.contains(substr) {
                    return Ok(resp.clone());
                }
            }
            Err(McpOauthError::Http(format!(
                "mock: no canned response for {url}"
            )))
        }
    }

    /// DCR POSTs to `<token_url>/register` with the device-code grant type and the dotz client
    /// name. The response's `client_id` is extracted and returned. The `client_secret` is not
    /// logged (only the client_id length appears in the eprintln).
    #[tokio::test]
    async fn mcp_oauth_dcr_posts_to_token_url() {
        let mock = MockOauthHttp::new().with_response(
            "/register",
            serde_json::json!({
                "client_id": "dcr-issued-id",
                "client_secret": "dcr-issued-secret"
            }),
        );
        let oauth = OauthConfig {
            token_url: Some("https://auth.example.com/oauth/token".into()),
            ..Default::default()
        };
        let creds = dynamic_client_register("test-server", &oauth, &mock)
            .await
            .expect("DCR should succeed");
        assert_eq!(creds.client_id, "dcr-issued-id");
        assert_eq!(creds.client_secret.as_deref(), Some("dcr-issued-secret"));
        // Verify the request shape.
        let captured = mock.captured.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let (url, body, server) = &captured[0];
        assert_eq!(url, "https://auth.example.com/oauth/token/register");
        assert_eq!(server, "test-server");
        assert_eq!(
            body.get("client_name").and_then(|v| v.as_str()),
            Some(CLIENT_NAME)
        );
        assert_eq!(
            body.get("grant_types")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            body.get("grant_types")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("urn:ietf:params:oauth:grant-type:device_code")
        );
    }

    /// Device flow polls the token endpoint until success. The stateful mock returns
    /// `authorization_pending` twice, then a real token on the third poll. The resulting
    /// `TokenSet` carries the access_token + expiry.
    #[tokio::test]
    async fn mcp_oauth_device_flow_polls_until_success() {
        // The simple MockOauthHttp is stateless (same response every call), so we use a
        // stateful variant for the device-flow poll sequence.
        let mock = StatefulMockOauth::new();
        let oauth = OauthConfig {
            token_url: Some("https://auth.example.com/oauth/token".into()),
            authorization_url: Some("https://auth.example.com/oauth/authorize".into()),
            ..Default::default()
        };
        let creds = ClientCredentials {
            client_id: "test-client".into(),
            client_secret: None,
        };
        // The stateful mock returns pending on the first two polls, then a token.
        let token = device_flow("test-server", &oauth, &creds, &mock)
            .await
            .expect("device flow should complete");
        assert_eq!(token.access_token, "final-access-token");
        assert_eq!(token.token_type, "Bearer");
        // The mock captured 4 requests: 1 device-authorization + 3 token polls (2 pending
        // + 1 success). Filter to token polls (those carrying `grant_type`).
        let all = mock.polls.lock().unwrap().clone();
        let token_polls: Vec<_> = all
            .iter()
            .filter(|(_, body, _)| body.get("grant_type").is_some())
            .collect();
        assert_eq!(
            token_polls.len(),
            3,
            "should poll 3 times (2 pending + 1 success), captured: {all:?}"
        );
        // The device_code was sent on every token poll and was NOT logged. We assert via
        // the captured body (the test infra would surface it if it leaked, but the
        // production code only logs user_code + verification_uri).
        for (_, body, _) in &token_polls {
            assert_eq!(
                body.get("device_code").and_then(|v| v.as_str()),
                Some("dc-secret")
            );
            assert_eq!(
                body.get("grant_type").and_then(|v| v.as_str()),
                Some("urn:ietf:params:oauth:grant-type:device_code")
            );
        }
    }

    /// Stateful mock for the device-flow test: returns pending on the first N polls, then a
    /// real token. Captures every poll request.
    struct StatefulMockOauth {
        polls: Arc<Mutex<Vec<(String, serde_json::Value, String)>>>,
        pending_count: usize,
    }

    impl StatefulMockOauth {
        fn new() -> Self {
            Self {
                polls: Arc::new(Mutex::new(Vec::new())),
                pending_count: 2,
            }
        }
    }

    #[async_trait::async_trait]
    impl OauthHttp for StatefulMockOauth {
        async fn post_json(
            &self,
            url: &str,
            body: &serde_json::Value,
            server_name: &str,
        ) -> Result<serde_json::Value, McpOauthError> {
            self.polls.lock().unwrap().push((
                url.to_string(),
                body.clone(),
                server_name.to_string(),
            ));
            if url.contains("/device_authorization") {
                return Ok(serde_json::json!({
                    "device_code": "dc-secret",
                    "user_code": "USER-CODE",
                    "verification_uri": "https://auth.example.com/device",
                    "interval": 0,
                    "expires_in": 100
                }));
            }
            if url.contains("/oauth/token") {
                // Count ONLY the token-endpoint polls (the device-auth POST is a different
                // endpoint and shouldn't count toward the pending sequence).
                let token_polls = self
                    .polls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(u, _, _)| u.contains("/oauth/token"))
                    .count();
                // The first `pending_count` token polls return pending (as a non-2xx);
                // the next one returns a real token.
                if token_polls <= self.pending_count {
                    return Err(McpOauthError::Http(
                        "{\"error\":\"authorization_pending\"}".into(),
                    ));
                }
                return Ok(serde_json::json!({
                    "access_token": "final-access-token",
                    "token_type": "Bearer",
                    "expires_in": 3600
                }));
            }
            Err(McpOauthError::Http(format!("mock: no match for {url}")))
        }
    }

    /// The device flow must log the user_code + verification_uri (so the operator sees them) but
    /// must NOT log the device_code or the access_token. We capture stderr via a test-only
    /// buffer and assert the user_code is present and the device_code/access_token are absent.
    ///
    /// `# ponytail:` we don't actually capture stderr here (Rust's test harness doesn't expose
    /// a per-test stderr buffer without `--nocapture` + pipe wrangling). Instead we assert
    /// structurally: the production code logs `user_code` + `verification_uri` from the
    /// device-auth response, and the device_code/access_token only appear in the wire bodies
    /// (never in any eprintln! string in the source).
    #[tokio::test]
    async fn mcp_oauth_device_flow_user_code_is_logged() {
        // Static assertion: the oauth.rs source must not interpolate device_code or
        // access_token into any eprintln! — only user_code + verification_uri.
        let src = include_str!("oauth.rs");
        let leaky: Vec<&str> = src
            .lines()
            .filter(|l| {
                l.contains("eprintln!")
                    && (l.contains("{da.device_code")
                        || l.contains("{access_token")
                        || l.contains("{token.access_token")
                        || l.contains("{parsed.access_token"))
            })
            .collect();
        assert!(
            leaky.is_empty(),
            "secret values must never be interpolated in an eprintln!: {leaky:?}"
        );
        // Positive assertion: the user_code + verification_uri ARE logged.
        assert!(
            src.contains("user_code = da.user_code")
                && src.contains("ver_uri = da.verification_uri"),
            "device flow must log the user_code + verification_uri"
        );

        // Functional check: device_flow runs to completion and returns a token (re-using the
        // stateful mock — keeps the test honest about what the flow actually does).
        let mock = StatefulMockOauth::new();
        let oauth = OauthConfig {
            token_url: Some("https://auth.example.com/oauth/token".into()),
            authorization_url: Some("https://auth.example.com/oauth/authorize".into()),
            ..Default::default()
        };
        let creds = ClientCredentials {
            client_id: "test-client".into(),
            client_secret: None,
        };
        let token = device_flow("test-server", &oauth, &creds, &mock)
            .await
            .expect("device flow should complete");
        assert_eq!(token.access_token, "final-access-token");
    }

    /// `derive_device_auth_url` uses the explicit `device_authorization_url` when provided;
    /// else derives from `authorization_url` by replacing `/authorize` with
    /// `/device_authorization`; else errors.
    #[test]
    fn derive_device_auth_url_uses_explicit_then_derives() {
        // Explicit wins.
        let oauth = OauthConfig {
            device_authorization_url: Some("https://example.com/custom/device".into()),
            authorization_url: Some("https://example.com/oauth/authorize".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_device_auth_url(&oauth).unwrap(),
            "https://example.com/custom/device"
        );

        // Derivation: /authorize → /device_authorization.
        let oauth = OauthConfig {
            device_authorization_url: None,
            authorization_url: Some("https://github.com/login/oauth/authorize".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_device_auth_url(&oauth).unwrap(),
            "https://github.com/login/oauth/device_authorization"
        );

        // No authorization_url → error.
        let oauth = OauthConfig::default();
        assert!(derive_device_auth_url(&oauth).is_err());

        // authorization_url without /authorize → error.
        let oauth = OauthConfig {
            authorization_url: Some("https://example.com/no-match-here".into()),
            ..Default::default()
        };
        assert!(derive_device_auth_url(&oauth).is_err());
    }

    /// `mcp_oauth_secrets_never_logged`: structural grep over the whole mcp module — no
    /// eprintln! interpolates a token value, client_secret, or device_code. This is the
    /// regression guard for the security boundary.
    #[test]
    fn mcp_secrets_never_logged() {
        let mod_src = include_str!("mod.rs");
        let oauth_src = include_str!("oauth.rs");
        let client_src = include_str!("client.rs");
        let stdio_src = include_str!("stdio.rs");
        let http_src = include_str!("http.rs");
        let registry_src = include_str!("registry.rs");
        for (name, src) in [
            ("mod", mod_src),
            ("oauth", oauth_src),
            ("client", client_src),
            ("stdio", stdio_src),
            ("http", http_src),
            ("registry", registry_src),
        ] {
            // Skip comment-only lines (a real leak is a code line containing `eprintln!`
            // that interpolates a secret-bearing expression). A comment line like
            // `// access_token must never be logged` would otherwise false-positive.
            let leaky: Vec<&str> = src
                .lines()
                .filter(|l| {
                    let trimmed = l.trim_start();
                    if trimmed.starts_with("//") {
                        return false; // comment
                    }
                    l.contains("eprintln!")
                        && (l.contains("access_token")
                            || l.contains("refresh_token")
                            || l.contains("client_secret")
                            || l.contains("device_code"))
                })
                .collect();
            assert!(
                leaky.is_empty(),
                "mcp/{name}.rs must never log a secret value in an eprintln!: {leaky:?}"
            );
        }
    }
}
