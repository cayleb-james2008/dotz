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
//! WS upgrades). This preserves the existing shim rather than forcing a token.

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
}
