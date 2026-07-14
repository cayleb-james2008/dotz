//! Origin/Host allowlist guard for the dotz local API + WebSocket upgrade.
//!
//! The backend binds `127.0.0.1` but is otherwise unauthenticated: any local process, or a web
//! page the user merely visits, could otherwise drive the code-exec (`/api/sandbox/runs`) and VCS
//! endpoints via a plain cross-origin request or a `/ws` upgrade (CSRF). This tower middleware
//! rejects requests whose `Origin` (or `Host`) header is present-and-not-allowed with `403`,
//! while leaving the legitimate app callers working:
//!
//! * The Tauri WebView2 window loads the REMOTE http origin `http://127.0.0.1:4317`
//!   (see `src-tauri/src/main.rs` + `capabilities/default.json`), so its `Origin`/`Host` on
//!   every `fetch`/WS is a loopback http origin.
//! * The documented "browser against the `serve` bin" flow uses `http://127.0.0.1:<port>` or
//!   `http://localhost:<port>` (port defaults to 4317, overridable via `DOTZ_PORT`).
//! * The Tauri custom-protocol origins (`tauri://localhost`, and on Windows
//!   `https://tauri.localhost`) are allowed too, so a future non-remote-URL window keeps working.
//!
//! Design rationale for allowing *any* loopback port (not just 4317): a browser always stamps the
//! `Origin` header with the *visited page's own* origin, never `127.0.0.1` — so a malicious
//! `http://evil.example` page is rejected regardless of the loopback port it targets. Allowing all
//! loopback origins therefore stays safe against the browser-CSRF threat while being robust to the
//! configurable `DOTZ_PORT`. The `Host` check additionally blocks DNS-rebinding (where a rebound
//! `evil.example` would appear same-origin and omit `Origin`): legitimate requests always carry a
//! loopback `Host`.
//!
//! A request with NO `Origin` header is allowed (same-origin fetches and non-browser callers such
//! as the Tauri IPC shim / CLI tools do not send one; browsers always send it cross-origin and on
//! WS upgrades). This preserves the existing shim rather than forcing a token *for the CSRF
//! threat*. The allowlist cannot stop a hostile **local** process, though — it is free to forge
//! or omit those headers. That gap is closed by the separate, optional per-process session token
//! ([`token_guard`], the plan-015 follow-up): when a token is configured, `/ws` and every
//! `/api/*` route except `/api/health` additionally require it, via either the `x-dotz-token`
//! header or a `?token=` query parameter (WS upgrades and `<img>` loads cannot set headers).
//! The token is distributed out-of-band — the Tauri initialization script injects
//! `window.DOTZ_TOKEN`, and the headless `serve` bin reads `DOTZ_TOKEN` and prints a tokenized
//! URL to stderr — never via served HTML. A missing/wrong token gets `401` JSON, deliberately
//! distinct from this origin guard's `403`.

use axum::{
    extract::Request,
    http::{
        header::{HOST, ORIGIN},
        StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Exact-match allowlist for the Tauri custom-protocol origins. `tauri://localhost` is used on
/// Linux/macOS; `https://tauri.localhost` is the Windows WebView2 custom-protocol origin. The host
/// `tauri.localhost` is deliberately NOT treated as loopback below, so these must be matched whole.
const TAURI_ORIGINS: [&str; 2] = ["tauri://localhost", "https://tauri.localhost"];

/// Strip a trailing `:port` from a `host[:port]` (or `authority`) value, handling bracketed IPv6
/// literals like `[::1]` / `[::1]:4317`. Returns the bare hostname.
fn host_only(hostport: &str) -> &str {
    if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 literal: `[::1]` or `[::1]:4317` -> take everything up to the closing bracket.
        return rest.split(']').next().unwrap_or(rest);
    }
    // An unbracketed authority with more than one colon is an IPv6 literal without a port
    // (e.g. `::1`) — it cannot carry a port unless bracketed, so return it whole.
    if hostport.bytes().filter(|&b| b == b':').count() > 1 {
        return hostport;
    }
    match hostport.rsplit_once(':') {
        // Only treat the tail as a port if it is a non-empty run of digits, so a hostname
        // containing no port is returned unchanged.
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => hostport,
    }
}

/// Is `host` a loopback hostname (ignoring any port)? Matches `127.0.0.1`, `localhost`, and the
/// IPv6 loopback `::1`. Note: only the exact label `localhost` — NOT `*.localhost` like
/// `tauri.localhost`, which is handled by the exact Tauri-origin allowlist instead.
fn is_loopback_host(host: &str) -> bool {
    let h = host_only(host);
    h.eq_ignore_ascii_case("127.0.0.1")
        || h.eq_ignore_ascii_case("localhost")
        || h == "::1"
}

/// Should a request carrying this `Origin` header value be allowed? `true` for the Tauri
/// custom-protocol origins and any `http`/`https` loopback origin (any port); `false` otherwise
/// (including the ambiguous `null` origin from sandboxed iframes / `file://`).
pub fn origin_allowed(origin: &str) -> bool {
    if TAURI_ORIGINS.contains(&origin) {
        return true;
    }
    let (scheme, rest) = match origin.split_once("://") {
        Some(parts) => parts,
        None => return false, // includes the literal "null" origin
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // An Origin has no path, but defensively cut at the first `/` before parsing the authority.
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return false;
    }
    is_loopback_host(authority)
}

/// Should a request carrying this `Host` header value be allowed? Legitimate requests reach the
/// loopback-bound server via a loopback `Host`; a non-loopback `Host` indicates DNS rebinding or a
/// proxy and is rejected.
pub fn host_allowed(host: &str) -> bool {
    is_loopback_host(host)
}

fn forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": format!("forbidden: {reason}") })),
    )
        .into_response()
}

/// Tower middleware: reject a request whose `Host` or `Origin` header is present-and-not-allowed
/// with `403`, otherwise pass it through unchanged. Applied to the whole router in
/// [`crate::server::app`], so it also guards the `/ws` upgrade (browsers always send `Origin` on a
/// WS handshake). A missing header is treated as allowed — see the module docs.
pub async fn origin_guard(req: Request, next: Next) -> Response {
    let headers = req.headers();

    if let Some(host) = headers.get(HOST).and_then(|v| v.to_str().ok()) {
        if !host_allowed(host) {
            return forbidden("host not allowed");
        }
    }
    if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_allowed(origin) {
            return forbidden("cross-origin request rejected");
        }
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------------------------
// Per-process session token (plan-015 follow-up)
// ---------------------------------------------------------------------------------------------

/// The request header carrying the session token for callers that can set headers (the central
/// `api()` helper in `web/app.js`). Headerless callers (WS handshake, `<img>` loads) use the
/// `?token=` query parameter instead.
pub const TOKEN_HEADER: &str = "x-dotz-token";

/// Generate a fresh per-process bearer token: two v4 UUIDs in `simple` (32 lowercase hex chars)
/// form, concatenated — 64 URL-safe hex chars / 244 bits of randomness. Hex-only so it never
/// needs percent-encoding in a query string or escaping in an injected JS string literal.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Does this request path require the session token (when one is configured)? `true` for the
/// `/ws` upgrade and everything under `/api/` EXCEPT the `/api/health` probe. Static files are
/// exempt so the SPA shell, scripts, styles, and fonts load without the token — the token guards
/// the *actions* (API + WS), not the inert UI bytes.
pub fn path_requires_token(path: &str) -> bool {
    if path == "/ws" {
        return true;
    }
    path.starts_with("/api/") && path != "/api/health"
}

/// Extract the `token` parameter from a raw query string (`a=1&token=abc`). No percent-decoding:
/// generated tokens are pure hex and never need encoding. Only the exact `token` key matches.
pub fn query_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

/// Constant-time byte-string equality: the comparison time must not depend on *where* the first
/// mismatching byte sits, or a local attacker could grind the token byte-by-byte via timing. The
/// length check short-circuits, but the token length is public (always 64), so that leaks nothing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Is the provided token acceptable for the required one? `required = None` means the token
/// feature is disabled — everything passes. With a required token, only a constant-time exact
/// match passes; a missing token fails.
pub fn token_ok(required: Option<&str>, provided: Option<&str>) -> bool {
    match required {
        None => true,
        Some(req) => match provided {
            Some(p) => constant_time_eq(req.as_bytes(), p.as_bytes()),
            None => false,
        },
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized: missing or invalid session token" })),
    )
        .into_response()
}

/// Tower middleware: when `required` is `Some`, demand the session token on every
/// [`path_requires_token`] path — accepted from the [`TOKEN_HEADER`] header OR the `?token=`
/// query parameter (WS handshakes and `<img>` loads cannot set headers). Missing/wrong token
/// gets `401` JSON, deliberately distinct from the origin guard's `403` so the two rejection
/// layers are distinguishable in logs and tests. With `required = None` this is a no-op
/// pass-through, keeping the token-less `app()`/serve paths byte-identical in behavior.
pub async fn token_guard(required: Option<String>, req: Request, next: Next) -> Response {
    if let Some(required) = required.as_deref() {
        if path_requires_token(req.uri().path()) {
            let header = req
                .headers()
                .get(TOKEN_HEADER)
                .and_then(|v| v.to_str().ok());
            let query = req.uri().query().and_then(query_token);
            // Accept EITHER carrier: a stale/wrong header must not veto a correct ?token=.
            let ok = token_ok(Some(required), header) || token_ok(Some(required), query);
            if !ok {
                return unauthorized();
            }
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_tauri_custom_protocol_origins() {
        assert!(origin_allowed("tauri://localhost"));
        assert!(origin_allowed("https://tauri.localhost"));
    }

    #[test]
    fn allows_loopback_http_origins_any_port() {
        // The Tauri WebView2 remote origin and the documented serve-bin browser origins.
        assert!(origin_allowed("http://127.0.0.1:4317"));
        assert!(origin_allowed("http://localhost:4317"));
        assert!(origin_allowed("http://127.0.0.1:5173"));
        assert!(origin_allowed("http://localhost:3000"));
        // No explicit port is still loopback.
        assert!(origin_allowed("http://127.0.0.1"));
        assert!(origin_allowed("http://localhost"));
        // IPv6 loopback.
        assert!(origin_allowed("http://[::1]:4317"));
        // https loopback is fine too (still loopback).
        assert!(origin_allowed("https://127.0.0.1:4317"));
    }

    #[test]
    fn rejects_foreign_origins() {
        assert!(!origin_allowed("http://evil.example"));
        assert!(!origin_allowed("https://evil.example"));
        assert!(!origin_allowed("http://evil.example:4317"));
        // A subdomain of localhost is NOT loopback (only the Tauri exact match is allowed).
        assert!(!origin_allowed("http://tauri.localhost"));
        assert!(!origin_allowed("http://localhost.evil.example"));
        assert!(!origin_allowed("http://127.0.0.1.evil.example"));
        // Non-web schemes and the ambiguous null origin are rejected.
        assert!(!origin_allowed("null"));
        assert!(!origin_allowed("file://"));
        assert!(!origin_allowed(""));
    }

    #[test]
    fn host_allow_matches_loopback_only() {
        assert!(host_allowed("127.0.0.1:4317"));
        assert!(host_allowed("localhost:4317"));
        assert!(host_allowed("127.0.0.1"));
        assert!(host_allowed("localhost"));
        assert!(host_allowed("[::1]:4317"));
        assert!(host_allowed("::1"));
        assert!(!host_allowed("evil.example"));
        assert!(!host_allowed("evil.example:4317"));
        assert!(!host_allowed("192.168.1.10:4317"));
    }

    #[test]
    fn host_only_strips_ports_and_brackets() {
        assert_eq!(host_only("127.0.0.1:4317"), "127.0.0.1");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[::1]:4317"), "::1");
        assert_eq!(host_only("[::1]"), "::1");
        // A trailing non-numeric segment is not a port, so it is preserved.
        assert_eq!(host_only("host:notaport"), "host:notaport");
    }

    // ---- session token ------------------------------------------------------------------

    #[test]
    fn generate_token_is_64_url_safe_hex_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64, "token must be 64 chars: {a}");
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "token must be lowercase hex (URL-safe, no encoding needed): {a}"
        );
        assert_eq!(b.len(), 64);
        assert_ne!(a, b, "two generated tokens must differ");
    }

    #[test]
    fn path_requires_token_covers_ws_and_api_except_health() {
        // The WS upgrade and every /api/ route require the token.
        assert!(path_requires_token("/ws"));
        assert!(path_requires_token("/api/models"));
        assert!(path_requires_token("/api/config"));
        assert!(path_requires_token("/api/sandbox/runs"));
        assert!(path_requires_token("/api/browser/frame"));
        // Only the EXACT health path is exempt — a sub-path is a different route.
        assert!(path_requires_token("/api/health/x"));
        assert!(!path_requires_token("/api/health"));
        // Static files are exempt: the SPA shell must load without a token in the URL.
        assert!(!path_requires_token("/"));
        assert!(!path_requires_token("/index.html"));
        assert!(!path_requires_token("/app.js"));
        assert!(!path_requires_token("/fonts.css"));
        // `/api` without the trailing slash is not an API route (falls through to static),
        // and a prefix-lookalike is not /api/ either.
        assert!(!path_requires_token("/api"));
        assert!(!path_requires_token("/apix/foo"));
        assert!(!path_requires_token("/ws2"));
    }

    #[test]
    fn token_ok_matrix() {
        // No token configured: the feature is off, everything passes.
        assert!(token_ok(None, None));
        assert!(token_ok(None, Some("anything")));
        // Token configured: exact match only; missing/empty/prefix/case-variant all fail.
        assert!(token_ok(Some("secret"), Some("secret")));
        assert!(!token_ok(Some("secret"), None));
        assert!(!token_ok(Some("secret"), Some("")));
        assert!(!token_ok(Some("secret"), Some("secre")));
        assert!(!token_ok(Some("secret"), Some("secrets")));
        assert!(!token_ok(Some("secret"), Some("SECRET")));
        // Real-shaped token round-trips.
        let t = generate_token();
        assert!(token_ok(Some(&t), Some(&t)));
        assert!(!token_ok(Some(&t), Some(&generate_token())));
    }

    #[test]
    fn query_token_extracts_only_the_exact_token_param() {
        assert_eq!(query_token("token=abc"), Some("abc"));
        assert_eq!(query_token("sessionId=1&token=abc"), Some("abc"));
        assert_eq!(query_token("token=abc&sessionId=1"), Some("abc"));
        assert_eq!(query_token("token="), Some(""));
        assert_eq!(query_token("sessionId=1"), None);
        assert_eq!(query_token(""), None);
        // A key that merely ENDS in "token" must not match.
        assert_eq!(query_token("xtoken=abc"), None);
        assert_eq!(query_token("afterSeq=-1&t=3"), None);
    }
}
