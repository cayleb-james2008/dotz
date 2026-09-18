//! C3: MCP (Model Context Protocol) client for dotz.
//!
//! Implements the industry-standard tool protocol (the same one OpenCode, Claude Code, and Codex
//! speak) so dotz can connect to stdio + HTTP MCP servers, list their tools/resources/prompts,
//! and call their tools. MCP tools are surfaced to the agent via the `mcp_call` tool registered in
//! `agent::extra_tools.rs` — they do NOT emit `step_*` events (the `subagent` tool is still the
//! sole emitter of those; MCP calls dispatch through the normal tool path). MCP prompts are
//! surfaced through `skills.rs` (the single skill-discovery path) by populating a
//! `~/.dotz/mcp-prompts/` scan root with generated `SKILL.md` files.
//!
//! # Transports
//! - stdio: spawn the configured command, JSON-RPC over stdin/stdout (NDJSON framing).
//! - http: POST JSON-RPC to the configured URL; the response is a direct JSON-RPC response
//!   or an SSE stream the client reads until it sees the JSON-RPC payload.
//!
//! # OAuth
//! HTTP servers may require OAuth 2.0 with Dynamic Client Registration (RFC 7591). When a
//! server returns 401 and the config has an `oauth` block with `client_id: null`, the client
//! performs DCR against the configured `token_url`, runs the device flow, and stores the
//! resulting tokens in `~/.dotz/mcp-auth.json` (per-server, mode 0600 on Unix). Token values
//! are never logged — only the device-code/verification URI shown to the user.
//!
//! # ponytail
//! - DCR endpoint auto-discovery via the 401 `WWW-Authenticate` header +
//!   `.well-known/oauth-authorization-server` is the upgrade path; for now we accept explicit
//!   `authorization_url` + `token_url` in the config (operator-provided, works for known servers).
//! - The stdio transport runs the configured command with user privileges (same as the `bash`
//!   tool). The command being spawned is logged; env vars (which may carry secrets) are not.
//! - HTTP servers must be `https://` (or `http://localhost`/`127.0.0.1`) — SSRF guard.
pub mod client;
pub mod http;
pub mod oauth;
pub mod registry;
pub mod stdio;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The MCP protocol version dotz speaks. The spec calls this `2024-11-05`; servers that
/// negotiate a different version are accepted as long as they respond to `initialize`.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// dotz's client info, sent in the `initialize` handshake.
pub const CLIENT_NAME: &str = "dotz";
pub const CLIENT_VERSION: &str = "0.2.0";

/// Transport kind for an MCP server. `stdio` spawns a subprocess; `http` POSTs JSON-RPC to a URL.
/// `Default` is `Stdio` (the safer, no-network default for a server config that omits the
/// `transport` field — matches the most common MCP server pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    #[default]
    Stdio,
    Http,
}

/// OAuth 2.0 config for an HTTP MCP server. All fields optional — the operator provides the
/// authorization + token URLs they got from the server's docs; `client_id: null` means dotz
/// performs Dynamic Client Registration (RFC 7591) to obtain a client id/secret at first use.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OauthConfig {
    /// Authorization endpoint URL (operator-provided). None/empty disables OAuth.
    #[serde(
        rename = "authorization_url",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authorization_url: Option<String>,
    /// Token endpoint URL (operator-provided); also used as the DCR endpoint when client_id is
    /// null (RFC 7591 registration is POSTed here with a `/register` suffix heuristic).
    #[serde(rename = "token_url", default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// Device-authorization endpoint URL. Derived from `authorization_url` (replace `/authorize`
    /// with `/device_authorization`) when not provided. Operator-provided when the server's
    /// discovery doc specifies a separate device endpoint.
    #[serde(
        rename = "device_authorization_url",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub device_authorization_url: Option<String>,
    /// OAuth scopes to request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// Pre-registered client id (operator-provided). `null` → dotz runs DCR to register.
    #[serde(rename = "client_id", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Pre-registered client secret (operator-provided). `null` when DCR is in use.
    #[serde(
        rename = "client_secret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
}

/// One MCP server entry from `.dotz/mcp.json`. Stdio fields (command/args/env) are used when
/// `transport = "stdio"`; http fields (url/headers/oauth) when `transport = "http"`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ServerConfig {
    pub transport: TransportType,
    /// stdio: the command to spawn (e.g. `npx`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// stdio: command args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// stdio: extra env vars for the subprocess. May contain secrets (e.g. API tokens) — these
    /// are passed to the spawned process but NEVER logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// http: the server endpoint URL (must be `https://` or loopback `http://`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// http: extra headers to send with every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// http: OAuth config for servers requiring auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OauthConfig>,
}

/// The full `.dotz/mcp.json` config file shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
}

/// Path to the user-global MCP config: `~/.dotz/mcp.json` (honors `DOTZ_CONFIG_DIR`).
pub fn user_config_path() -> PathBuf {
    crate::config::dotz_dir().join("mcp.json")
}

/// Path to the project-local MCP config: `<cwd>/.dotz/mcp.json`.
fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".dotz").join("mcp.json")
}

/// Load the MCP config by merging project-local (`<cwd>/.dotz/mcp.json`, higher priority) with
/// user-global (`~/.dotz/mcp.json`, lower priority). A missing file at either layer is treated
/// as an empty config (no servers). A malformed file at either layer is dropped with a stderr
/// warning rather than panicking — a bad `mcp.json` must not brick the rest of dotz.
///
/// Merge semantics: project servers with the same name as user servers overwrite them (the
/// project layer is the higher-trust, more-specific layer).
pub fn load_config(cwd: &Path) -> McpConfig {
    let mut merged = McpConfig::default();

    // Lower priority: user-global.
    if let Ok(raw) = std::fs::read_to_string(user_config_path()) {
        match serde_json::from_str::<McpConfig>(&raw) {
            Ok(c) => merged.servers.extend(c.servers),
            Err(e) => eprintln!(
                "mcp: {} is not valid JSON ({e}); ignoring user MCP config. Fix or remove the file.",
                user_config_path().display()
            ),
        }
    }

    // Higher priority: project-local (overwrites same-named user servers).
    let project_path = project_config_path(cwd);
    if let Ok(raw) = std::fs::read_to_string(&project_path) {
        match serde_json::from_str::<McpConfig>(&raw) {
            Ok(c) => merged.servers.extend(c.servers),
            Err(e) => eprintln!(
                "mcp: {} is not valid JSON ({e}); ignoring project MCP config. Fix or remove the file.",
                project_path.display()
            ),
        }
    }

    merged
}

/// SSRF guard for HTTP MCP server URLs. Mirrors `agent::provider::validate_gateway_base_url`:
/// `https://` anywhere, or `http://localhost`/`http://127.0.0.1` for a local proxy. Anything
/// else (non-loopback `http://`, `ftp://`, empty) is rejected so a misconfigured server can't
/// redirect dotz at an arbitrary internal endpoint.
pub fn validate_http_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if u.is_empty() {
        return Err("mcp http url is required".to_string());
    }
    if let Some(rest) = u.strip_prefix("https://") {
        if rest.is_empty() {
            return Err("mcp http url 'https://' has no host".to_string());
        }
        return Ok(());
    }
    if let Some(rest) = u.strip_prefix("http://") {
        let host = rest
            .split('/')
            .next()
            .unwrap_or(rest)
            .split(':')
            .next()
            .unwrap_or(rest);
        if host == "localhost" || host == "127.0.0.1" {
            return Ok(());
        }
        return Err(format!(
            "mcp http url must be https://, or http://localhost / http://127.0.0.1 \
             (a non-loopback http:// URL is SSRF-unsafe): {u}"
        ));
    }
    Err(format!(
        "mcp http url must start with https:// or http://localhost / http://127.0.0.1: {u}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::dotz_config_dir_test_lock;

    /// Serialize tests that mutate the process-global `DOTZ_CONFIG_DIR` env var so concurrent
    /// mcp-config tests do not race on the user-global config path.
    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-mcp-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &dir) };
        let result = f(&dir);
        match prev {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop(guard);
        result
    }

    /// No `.dotz/mcp.json` at either layer → empty config (no servers).
    #[test]
    fn mcp_config_loads_empty_when_no_file() {
        with_tmp_dir(|dir| {
            let cfg = load_config(dir);
            assert!(cfg.servers.is_empty(), "no mcp.json → empty servers map");
        });
    }

    /// Both project and user configs present → project overwrites same-named user servers, but
    /// distinct user servers are preserved.
    #[test]
    fn mcp_config_loads_project_and_user() {
        with_tmp_dir(|dir| {
            // User config: two servers.
            std::fs::write(
                user_config_path(),
                r#"{
  "servers": {
    "shared": { "transport": "stdio", "command": "user-cmd" },
    "user-only": { "transport": "stdio", "command": "user-only-cmd" }
  }
}"#,
            )
            .unwrap();
            // Project config: one server that shadows `shared` + one new project-only server.
            std::fs::create_dir_all(dir.join(".dotz")).unwrap();
            std::fs::write(
                dir.join(".dotz").join("mcp.json"),
                r#"{
  "servers": {
    "shared": { "transport": "http", "url": "https://project.example.com/mcp" },
    "project-only": { "transport": "stdio", "command": "proj-cmd" }
  }
}"#,
            )
            .unwrap();

            let cfg = load_config(dir);
            assert_eq!(cfg.servers.len(), 3, "three servers after merge");
            // Project wins over user for `shared`.
            assert_eq!(
                cfg.servers.get("shared").unwrap().transport,
                TransportType::Http
            );
            assert_eq!(
                cfg.servers.get("shared").unwrap().url.as_deref(),
                Some("https://project.example.com/mcp")
            );
            // Distinct user server is preserved.
            assert_eq!(
                cfg.servers.get("user-only").unwrap().command.as_deref(),
                Some("user-only-cmd")
            );
            // Project-only server is present.
            assert_eq!(
                cfg.servers.get("project-only").unwrap().command.as_deref(),
                Some("proj-cmd")
            );
        });
    }

    /// A stdio server parses with command/args/env.
    #[test]
    fn mcp_config_parses_stdio_server() {
        with_tmp_dir(|dir| {
            std::fs::create_dir_all(dir.join(".dotz")).unwrap();
            std::fs::write(
                dir.join(".dotz").join("mcp.json"),
                r#"{
  "servers": {
    "filesystem": {
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "FOO": "bar" }
    }
  }
}"#,
            )
            .unwrap();
            let cfg = load_config(dir);
            let s = cfg.servers.get("filesystem").expect("filesystem server");
            assert_eq!(s.transport, TransportType::Stdio);
            assert_eq!(s.command.as_deref(), Some("npx"));
            let expected_args = vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
                "/tmp".to_string(),
            ];
            assert_eq!(s.args.as_deref(), Some(expected_args.as_slice()));
            assert_eq!(
                s.env
                    .as_ref()
                    .and_then(|e| e.get("FOO"))
                    .map(|s| s.as_str()),
                Some("bar")
            );
        });
    }

    /// An http server parses with url + headers.
    #[test]
    fn mcp_config_parses_http_server() {
        with_tmp_dir(|dir| {
            std::fs::create_dir_all(dir.join(".dotz")).unwrap();
            std::fs::write(
                dir.join(".dotz").join("mcp.json"),
                r#"{
  "servers": {
    "remote": {
      "transport": "http",
      "url": "https://api.example.com/mcp",
      "headers": { "X-Custom": "value" }
    }
  }
}"#,
            )
            .unwrap();
            let cfg = load_config(dir);
            let s = cfg.servers.get("remote").expect("remote server");
            assert_eq!(s.transport, TransportType::Http);
            assert_eq!(s.url.as_deref(), Some("https://api.example.com/mcp"));
            assert_eq!(
                s.headers
                    .as_ref()
                    .and_then(|h| h.get("X-Custom"))
                    .map(|s| s.as_str()),
                Some("value")
            );
            assert!(s.oauth.is_none());
        });
    }

    /// An http server with an `oauth` block parses (client_id: null → DCR).
    #[test]
    fn mcp_config_parses_oauth_config() {
        with_tmp_dir(|dir| {
            std::fs::create_dir_all(dir.join(".dotz")).unwrap();
            std::fs::write(
                dir.join(".dotz").join("mcp.json"),
                r#"{
  "servers": {
    "github": {
      "transport": "http",
      "url": "https://api.githubcopilot.com/mcp",
      "oauth": {
        "authorization_url": "https://github.com/login/oauth/authorize",
        "token_url": "https://github.com/login/oauth/access_token",
        "scopes": ["repo", "read:user"],
        "client_id": null
      }
    }
  }
}"#,
            )
            .unwrap();
            let cfg = load_config(dir);
            let s = cfg.servers.get("github").expect("github server");
            let oauth = s.oauth.as_ref().expect("oauth block");
            assert_eq!(
                oauth.authorization_url.as_deref(),
                Some("https://github.com/login/oauth/authorize")
            );
            assert_eq!(
                oauth.token_url.as_deref(),
                Some("https://github.com/login/oauth/access_token")
            );
            let expected_scopes = vec!["repo".to_string(), "read:user".to_string()];
            assert_eq!(oauth.scopes.as_deref(), Some(expected_scopes.as_slice()));
            assert!(oauth.client_id.is_none(), "client_id:null → None → DCR");
        });
    }

    /// A malformed user `mcp.json` is dropped (with a stderr warning), not panicked on. The
    /// project layer still loads if it is valid.
    #[test]
    fn mcp_config_drops_malformed_user_keeps_project() {
        with_tmp_dir(|dir| {
            std::fs::write(user_config_path(), b"{ not valid json ]").unwrap();
            std::fs::create_dir_all(dir.join(".dotz")).unwrap();
            std::fs::write(
                dir.join(".dotz").join("mcp.json"),
                r#"{ "servers": { "ok": { "transport": "stdio", "command": "x" } } }"#,
            )
            .unwrap();
            let cfg = load_config(dir);
            assert_eq!(
                cfg.servers.len(),
                1,
                "project layer survives a bad user layer"
            );
            assert!(cfg.servers.contains_key("ok"));
        });
    }

    /// SSRF guard: https anywhere, http localhost/127.0.0.1 only, anything else rejected.
    #[test]
    fn validate_http_url_accepts_and_rejects() {
        assert!(validate_http_url("https://api.example.com/mcp").is_ok());
        assert!(validate_http_url("http://localhost:3000/mcp").is_ok());
        assert!(validate_http_url("http://127.0.0.1:3000/mcp").is_ok());
        assert!(validate_http_url("http://api.example.com/mcp").is_err());
        assert!(validate_http_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_http_url("ftp://example.com").is_err());
        assert!(validate_http_url("").is_err());
        assert!(validate_http_url("https://").is_err());
    }
}
