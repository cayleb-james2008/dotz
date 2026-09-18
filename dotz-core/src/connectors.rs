//! Optional, config-gated bridge to an external **open-connector** gateway
//! (oomol-lab/open-connector — a self-hostable "authentication gateway for AI agents").
//!
//! dotz becomes a CLIENT of a separately-run gateway; it NEVER ports providers/actions into Rust
//! and NEVER stores raw credentials. A connector entry only holds a `$VAR` token *reference*
//! (resolved via [`crate::agent::provider::resolve_api_key`] at call time) — real secrets live
//! behind the gateway. The gateway surfaces:
//!   - HTTP action invoke:   `POST {base}/v1/actions/<provider>.<action>`  body `{"input":{...}}`
//!   - OpenAPI catalog:      `GET  {base}/openapi.json`
//!   - Connection setup:     `PUT  {base}/api/connections/<provider>`      body `{authType,values}`
//!
//! OFF BY DEFAULT: when `~/.dotz/connectors.json` is absent (and no `connectors` block in
//! config.json), the registry is empty and every surface here is a no-op — behavior is
//! byte-identical to a dotz build without this module. Nothing is probed, no tool does anything,
//! and `GET /api/connections` returns exactly the three CLI providers it always did.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// A short reachability-probe timeout for `GET /api/connections` so a slow/absent gateway can
/// never stall the connections panel. Kept well under the CLI status timeout.
const PROBE_TIMEOUT: Duration = Duration::from_millis(4_000);
/// Timeout for a catalog fetch / action invoke. Actions can be slow (remote SaaS round-trip),
/// so this is more generous than the reachability probe.
const CALL_TIMEOUT: Duration = Duration::from_millis(30_000);

/// One configured gateway connector. Persisted (without secrets) in `~/.dotz/connectors.json`.
/// JSON keys are snake_case per the connector spec (`gateway_base_url`, `token_ref`); camelCase
/// aliases are accepted so an operator can use either convention.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connector {
    /// Stable id shown in `/api/connections` and passed to the connector tools.
    pub id: String,
    /// Base URL of the running gateway, e.g. `http://localhost:3000`.
    #[serde(alias = "gatewayBaseUrl")]
    pub gateway_base_url: String,
    /// A `$VAR` / `${VAR}` reference to the gateway auth token (NEVER a raw secret). Empty = no auth.
    #[serde(default, alias = "tokenRef")]
    pub token_ref: String,
    /// Off by default: a connector only participates when `enabled` is true.
    #[serde(default)]
    pub enabled: bool,
    /// Optional display label; falls back to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Connector {
    /// Gateway base with any trailing slash removed so URL joins are clean.
    fn base(&self) -> &str {
        self.gateway_base_url.trim_end_matches('/')
    }
    fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.id.clone())
    }
    /// Host portion of the gateway URL for display (no scheme/path) — never includes a secret.
    fn host(&self) -> String {
        self.base()
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or_else(|| self.base())
            .split('/')
            .next()
            .unwrap_or("")
            .to_string()
    }
}

/// Path to the connector registry file (honors `DOTZ_CONFIG_DIR` via [`crate::config::dotz_dir`]).
pub fn connectors_file() -> PathBuf {
    crate::config::dotz_dir().join("connectors.json")
}

/// Load the full connector registry. Reads `~/.dotz/connectors.json` first (a bare `[...]` array
/// OR a `{ "connectors": [...] }` object), then falls back to a `connectors` array inside
/// `config.json`. A missing file yields an empty registry (the off-by-default path); a malformed
/// file logs to stderr and also yields empty — it never panics and never disables the CLI providers.
pub fn load_connectors() -> Vec<Connector> {
    if let Ok(raw) = std::fs::read_to_string(connectors_file()) {
        return parse_registry(&raw, "connectors.json");
    }
    // Fallback: a `connectors` block inside config.json.
    if let Ok(raw) = std::fs::read_to_string(crate::config::dotz_dir().join("config.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(arr) = v.get("connectors") {
                return from_array_value(arr.clone());
            }
        }
    }
    Vec::new()
}

/// Parse the registry file body into connectors, tolerating both the array and wrapped-object shapes.
fn parse_registry(raw: &str, source: &str) -> Vec<Connector> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "connectors: {source} is not valid JSON ({e}); ignoring (no connectors loaded)"
            );
            return Vec::new();
        }
    };
    let arr = if v.is_array() {
        v
    } else {
        v.get("connectors").cloned().unwrap_or(Value::Null)
    };
    from_array_value(arr)
}

/// Deserialize an array Value into connectors, dropping malformed entries rather than failing whole.
fn from_array_value(arr: Value) -> Vec<Connector> {
    match arr {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| match serde_json::from_value::<Connector>(item) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("connectors: skipping malformed connector entry ({e})");
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Only the connectors that are enabled AND have a usable id + base URL. This is the single gate
/// that enforces off-by-default: an empty or all-disabled registry returns an empty list.
pub fn enabled_connectors() -> Vec<Connector> {
    load_connectors()
        .into_iter()
        .filter(|c| c.enabled && !c.id.trim().is_empty() && !c.gateway_base_url.trim().is_empty())
        .collect()
}

/// Find an enabled connector by id (used by the connector tools and the login/logout handlers).
pub fn enabled_by_id(id: &str) -> Option<Connector> {
    enabled_connectors().into_iter().find(|c| c.id == id)
}

/// Resolve a connector's gateway token from its `$VAR` reference. Returns:
///   - `Ok(None)`        — no token configured (bare-empty ref: a no-auth gateway),
///   - `Ok(Some(tok))`   — a resolved non-empty token,
///   - `Err(msg)`        — the ref names an env var that is unset (actionable, names only the VAR).
///
/// The resolved token is NEVER logged; error messages name the variable, never its value.
fn resolve_token(c: &Connector) -> Result<Option<String>, String> {
    let r = c.token_ref.trim();
    if r.is_empty() {
        return Ok(None);
    }
    let val = crate::agent::provider::resolve_api_key(r);
    if val.trim().is_empty() {
        if let Some(var) = r
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .or_else(|| r.strip_prefix('$'))
        {
            return Err(format!(
                "connector '{}': environment variable {var} is not set. Set {var} and retry \
                 (dotz never stores the raw token — only this $VAR reference).",
                c.id
            ));
        }
        return Ok(None);
    }
    Ok(Some(val))
}

/// Attach bearer auth to a request builder when a token resolves; propagate a missing-env error.
fn with_auth(
    mut req: reqwest::RequestBuilder,
    c: &Connector,
) -> Result<reqwest::RequestBuilder, String> {
    if let Some(tok) = resolve_token(c)? {
        req = req.bearer_auth(tok);
    }
    Ok(req)
}

/// Invoke a gateway action: `POST {base}/v1/actions/<action>` with body `{"input": <input>}`.
/// `action` is the `<provider>.<action>` identifier. Returns the parsed JSON response, or an
/// error string on transport failure or a non-2xx status. Credentials stay behind the gateway.
pub async fn invoke_action(
    client: &reqwest::Client,
    c: &Connector,
    action: &str,
    input: Value,
) -> Result<Value, String> {
    let url = format!("{}/v1/actions/{}", c.base(), action);
    let req = client
        .post(&url)
        .timeout(CALL_TIMEOUT)
        .json(&json!({ "input": input }));
    let req = with_auth(req, c)?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway request to '{}' failed: {e}", c.id))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!(
            "gateway '{}' action '{action}' returned HTTP {}: {body}",
            c.id, status
        ));
    }
    Ok(body)
}

/// Fetch the gateway's OpenAPI catalog: `GET {base}/openapi.json`. Used for action discovery.
pub async fn fetch_catalog(client: &reqwest::Client, c: &Connector) -> Result<Value, String> {
    let url = format!("{}/openapi.json", c.base());
    let req = client.get(&url).timeout(CALL_TIMEOUT);
    let req = with_auth(req, c)?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway catalog fetch for '{}' failed: {e}", c.id))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!(
            "gateway '{}' catalog returned HTTP {}",
            c.id, status
        ));
    }
    Ok(body)
}

/// Set up a gateway connection for a SaaS provider: `PUT {base}/api/connections/<provider>` with
/// `{authType, values}`. For `authType == "api_key"` the operator's `values` (raw provider
/// credentials) are handed to the GATEWAY and never stored in dotz. Returns the gateway response.
pub async fn put_connection(
    client: &reqwest::Client,
    c: &Connector,
    provider: &str,
    auth_type: &str,
    values: Value,
) -> Result<Value, String> {
    let url = format!("{}/api/connections/{}", c.base(), provider);
    let req = client
        .put(&url)
        .timeout(CALL_TIMEOUT)
        .json(&json!({ "authType": auth_type, "values": values }));
    let req = with_auth(req, c)?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway connection setup for '{}' failed: {e}", c.id))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!(
            "gateway '{}' connection PUT returned HTTP {}: {body}",
            c.id, status
        ));
    }
    Ok(body)
}

/// Delete a gateway connection for a SaaS provider: `DELETE {base}/api/connections/<provider>`.
/// The RESTful counterpart to [`put_connection`]. Credentials are the gateway's to hold and drop.
pub async fn delete_connection(
    client: &reqwest::Client,
    c: &Connector,
    provider: &str,
) -> Result<Value, String> {
    let url = format!("{}/api/connections/{}", c.base(), provider);
    let req = client.delete(&url).timeout(CALL_TIMEOUT);
    let req = with_auth(req, c)?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway connection delete for '{}' failed: {e}", c.id))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!(
            "gateway '{}' connection DELETE returned HTTP {}: {body}",
            c.id, status
        ));
    }
    Ok(body)
}

/// The gateway URL an operator visits (or an OAuth flow targets) to finish connecting a SaaS
/// provider through the console: `{base}/api/connections/<provider>`. Contains no secret.
pub fn connect_url(c: &Connector, provider: &str) -> String {
    format!("{}/api/connections/{}", c.base(), provider)
}

/// A non-secret summary of the configured connectors (for the `connector_list` tool). Never emits
/// the token reference's resolved value — only the `$VAR` reference itself, which is not a secret.
pub fn list_summary() -> Value {
    let all: Vec<Value> = load_connectors()
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.display_label(),
                "gatewayHost": c.host(),
                "tokenRef": c.token_ref,
                "enabled": c.enabled,
            })
        })
        .collect();
    json!({ "connectors": all })
}

/// Build the `ConnectionStatus`-shaped JSON entries for every enabled gateway connector, matching
/// the CLI providers' shape ({id,label,cli,installed,loggedIn,account?,hint?}) so the connections
/// panel renders them uniformly. Each is probed for reachability with a short timeout.
///
/// Returns an empty vec when no connector is enabled — the off-by-default path does ZERO network
/// I/O, so `GET /api/connections` is unchanged for a dotz install with no gateway configured.
pub async fn gateway_statuses() -> Vec<Value> {
    let connectors = enabled_connectors();
    if connectors.is_empty() {
        return Vec::new();
    }
    let client = reqwest::Client::new();
    let mut out = Vec::with_capacity(connectors.len());
    for c in connectors {
        out.push(probe_status(&client, &c).await);
    }
    out
}

/// Probe one connector's gateway (`GET {base}/openapi.json`) and shape a status entry. A reachable
/// gateway is `installed`; `loggedIn` additionally requires a resolvable token (or a no-auth
/// gateway). Any transport/config error degrades to a not-installed entry with an actionable hint.
async fn probe_status(client: &reqwest::Client, c: &Connector) -> Value {
    let base = json!({
        "id": c.id,
        "label": c.display_label(),
        "cli": "open-connector",
        "account": c.host(),
    });
    // A token-ref that names an unset env var is a config problem, not an unreachable gateway.
    let token = match resolve_token(c) {
        Ok(t) => t,
        Err(hint) => {
            return merge_status(base, false, false, Some(hint));
        }
    };
    let url = format!("{}/openapi.json", c.base());
    let mut req = client.get(&url).timeout(PROBE_TIMEOUT);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => merge_status(base, true, true, None),
        Ok(resp) => merge_status(
            base,
            true,
            false,
            Some(format!(
                "gateway reachable but returned HTTP {}",
                resp.status()
            )),
        ),
        Err(_) => merge_status(
            base,
            false,
            false,
            Some(format!(
                "open-connector gateway not reachable at {} — start it and retry",
                c.host()
            )),
        ),
    }
}

/// Merge the installed/loggedIn/hint fields into a base status object.
fn merge_status(mut base: Value, installed: bool, logged_in: bool, hint: Option<String>) -> Value {
    if let Some(obj) = base.as_object_mut() {
        obj.insert("installed".into(), json!(installed));
        obj.insert("loggedIn".into(), json!(logged_in));
        if let Some(h) = hint {
            obj.insert("hint".into(), json!(h));
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that mutate DOTZ_CONFIG_DIR / connector env vars.
    static LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir =
            std::env::temp_dir().join(format!("dotz-connectors-test-{}", uuid::Uuid::new_v4()));
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

    /// OFF BY DEFAULT: with no connectors.json present, the registry is empty, no connector is
    /// enabled, and gateway_statuses does zero work — proving a gateway-free install is unchanged.
    #[test]
    fn registry_is_empty_and_off_by_default_when_no_file() {
        with_tmp_dir(|_| {
            assert!(load_connectors().is_empty(), "no file => empty registry");
            assert!(
                enabled_connectors().is_empty(),
                "no file => nothing enabled"
            );
            assert!(enabled_by_id("anything").is_none());
            // gateway_statuses must return empty WITHOUT touching the network.
            let rt = tokio::runtime::Runtime::new().unwrap();
            let statuses = rt.block_on(gateway_statuses());
            assert!(statuses.is_empty(), "no connectors => no status entries");
        });
    }

    /// The registry parses both the bare-array and wrapped-object shapes, and `enabled_connectors`
    /// filters out disabled entries.
    #[test]
    fn parses_array_and_object_shapes_and_filters_disabled() {
        with_tmp_dir(|dir| {
            std::fs::write(
                dir.join("connectors.json"),
                r#"[
                  { "id": "gw", "gateway_base_url": "http://localhost:3000", "token_ref": "$OC_TOKEN", "enabled": true },
                  { "id": "off", "gateway_base_url": "http://localhost:3001", "enabled": false }
                ]"#,
            )
            .unwrap();
            let all = load_connectors();
            assert_eq!(all.len(), 2, "both entries parse");
            let enabled = enabled_connectors();
            assert_eq!(enabled.len(), 1, "the disabled entry is filtered out");
            assert_eq!(enabled[0].id, "gw");
            assert_eq!(enabled[0].token_ref, "$OC_TOKEN");

            // Wrapped-object shape with camelCase alias.
            std::fs::write(
                dir.join("connectors.json"),
                r#"{ "connectors": [ { "id": "gw2", "gatewayBaseUrl": "http://localhost:3002", "tokenRef": "$OC2", "enabled": true } ] }"#,
            )
            .unwrap();
            let all = load_connectors();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].id, "gw2");
            assert_eq!(all[0].gateway_base_url, "http://localhost:3002");
            assert_eq!(all[0].token_ref, "$OC2");
        });
    }

    /// A malformed connectors.json must degrade to an empty registry (never panic, never disable
    /// the CLI providers).
    #[test]
    fn malformed_registry_degrades_to_empty() {
        with_tmp_dir(|dir| {
            std::fs::write(dir.join("connectors.json"), b"{ not valid json ]").unwrap();
            assert!(load_connectors().is_empty());
        });
    }

    /// Token-ref resolution: a `$VAR` reference resolves from the environment; an unset var yields
    /// an actionable error that names the variable but never a value; a bare-empty ref is no-auth.
    #[test]
    fn token_ref_resolution_covers_set_unset_and_empty() {
        with_tmp_dir(|_| {
            let c = Connector {
                id: "gw".into(),
                gateway_base_url: "http://localhost:3000".into(),
                token_ref: "$DOTZ_TEST_OC_TOKEN".into(),
                enabled: true,
                label: None,
            };
            std::env::set_var("DOTZ_TEST_OC_TOKEN", "secret-value-123");
            assert_eq!(
                resolve_token(&c).unwrap(),
                Some("secret-value-123".to_string())
            );

            std::env::remove_var("DOTZ_TEST_OC_TOKEN");
            let err = resolve_token(&c).unwrap_err();
            assert!(
                err.contains("DOTZ_TEST_OC_TOKEN"),
                "error must name the var: {err}"
            );
            assert!(
                !err.contains("secret-value-123"),
                "error must never contain a token value"
            );

            let no_auth = Connector {
                token_ref: "".into(),
                ..c
            };
            assert_eq!(
                resolve_token(&no_auth).unwrap(),
                None,
                "empty ref => no auth"
            );
        });
    }

    /// Action-proxy request shaping against a stub gateway: `invoke_action` must POST to
    /// `/v1/actions/<provider>.<action>`, wrap the payload as `{"input":{...}}`, and send the
    /// resolved token as `Authorization: Bearer <token>`.
    #[tokio::test]
    async fn invoke_action_shapes_request_and_sends_bearer_token() {
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex as AMutex};

        // Captured request facts from the stub gateway: (dotted-action-path, auth-header, body).
        let captured: Arc<AMutex<(String, String, Value)>> =
            Arc::new(AMutex::new((String::new(), String::new(), Value::Null)));
        let cap = captured.clone();

        // open-connector uses `<provider>.<action>` as ONE path segment; capture it verbatim.
        let app = Router::new().route(
            "/v1/actions/{action}",
            post(
                move |axum::extract::Path(action): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      axum::extract::Json(body): axum::extract::Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        *cap.lock().unwrap() = (action, auth, body);
                        axum::Json(json!({ "ok": true, "result": "done" }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        std::env::set_var("DOTZ_TEST_INVOKE_TOKEN", "tkn-abc");
        let c = Connector {
            id: "gw".into(),
            gateway_base_url: format!("http://127.0.0.1:{}", addr.port()),
            token_ref: "$DOTZ_TEST_INVOKE_TOKEN".into(),
            enabled: true,
            label: None,
        };
        let client = reqwest::Client::new();
        let out = invoke_action(&client, &c, "github.create_issue", json!({ "title": "hi" }))
            .await
            .expect("invoke should succeed against the stub");
        assert_eq!(out["ok"], json!(true));

        let (path, auth, body) = captured.lock().unwrap().clone();
        assert_eq!(
            path, "github.create_issue",
            "path must be the dotted action"
        );
        assert_eq!(auth, "Bearer tkn-abc", "bearer token must be sent");
        assert_eq!(
            body,
            json!({ "input": { "title": "hi" } }),
            "payload must be wrapped as {{input}}"
        );

        std::env::remove_var("DOTZ_TEST_INVOKE_TOKEN");
        server.abort();
    }
}
