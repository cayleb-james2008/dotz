//! Standalone telemetry tooling — makes the "5 external weekly-active users" metric collectable.
//!
//! `telemetry receive [--bind ADDR] [--token TOKEN]`
//!   Hosts POST `/telemetry/ingest` (the same JSONL sink the in-app receiver appends to) on a
//!   configurable address, so EXTERNAL opted-in installs can reach an operator-run collector —
//!   the in-app receiver is loopback-only by design and can never see them. A non-loopback bind
//!   REQUIRES a shared token (`--token` / `DOTZ_TELEMETRY_TOKEN`): fail-closed, the open internet
//!   never gets an unauthenticated append-to-disk endpoint. Clients send the token as the
//!   `x-dotz-telemetry-token` header (set `"token"` in their `~/.dotz/telemetry.json`).
//!   Actually exposing the port (VPS, tunnel, port-forward) stays an operator decision —
//!   see `docs/telemetry.md`.
//!
//! `telemetry weekly [--sink PATH]`
//!   Aggregates the sink into the metric: one row per UTC ISO week with the DISTINCT install
//!   count (`sessionId`, the stable per-install telemetry id) and the raw event count.
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("receive") => receive(&args[1..]).await,
        Some("weekly") => weekly(&args[1..]),
        _ => {
            eprintln!(
                "usage: telemetry receive [--bind ADDR] [--token TOKEN]\n       telemetry weekly [--sink PATH]"
            );
            std::process::exit(2);
        }
    }
}

/// Value of `--<name> <value>` in `args`, if present. Hand-rolled on purpose (ponytail: two
/// flags do not justify a CLI-parser dependency; the backend stays axum + tokio only).
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Resolve the shared token from `--token` / `DOTZ_TELEMETRY_TOKEN`; trimmed, empty = none —
/// mirroring the `DOTZ_TOKEN` handling in the `serve` bin.
fn resolve_token(args: &[String]) -> Option<String> {
    flag(args, "--token")
        .or_else(|| std::env::var("DOTZ_TELEMETRY_TOKEN").ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

async fn receive(args: &[String]) {
    let bind = flag(args, "--bind")
        .or_else(|| std::env::var("DOTZ_TELEMETRY_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:4318".into());
    let addr: SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: invalid --bind {bind:?}: {e} (want e.g. 0.0.0.0:4318)");
            std::process::exit(2);
        }
    };
    let token = resolve_token(args);
    // Fail-closed: an internet-facing unauthenticated append-to-disk endpoint is an easy
    // disk-filler and a metric-poisoning vector. Loopback binds may stay tokenless (that is the
    // in-app receiver's trust model); anything wider demands the shared secret.
    if !addr.ip().is_loopback() && token.is_none() {
        eprintln!(
            "error: refusing non-loopback bind {addr} without a shared token — pass --token or set DOTZ_TELEMETRY_TOKEN."
        );
        std::process::exit(2);
    }
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: could not bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "dotz telemetry receiver on http://{addr}/telemetry/ingest — {}; sink: {}",
        if token.is_some() {
            "shared token REQUIRED (x-dotz-telemetry-token header)"
        } else {
            "no token (loopback)"
        },
        dotz_core::telemetry::sink_file().display()
    );
    if let Err(e) = axum::serve(listener, dotz_core::telemetry::router_with_token(token)).await {
        eprintln!("telemetry receiver error: {e}");
        std::process::exit(1);
    }
}

fn weekly(args: &[String]) {
    let sink = flag(args, "--sink")
        .map(PathBuf::from)
        .unwrap_or_else(dotz_core::telemetry::sink_file);
    let raw = match std::fs::read_to_string(&sink) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot read sink {}: {e}", sink.display());
            std::process::exit(1);
        }
    };
    let rows = dotz_core::telemetry::weekly_active(&raw);
    if rows.is_empty() {
        println!("no countable events in {}", sink.display());
        return;
    }
    println!(
        "{:<10}  {:>17}  {:>7}",
        "iso-week", "distinct-installs", "events"
    );
    for (week, installs, events) in rows {
        println!("{week:<10}  {installs:>17}  {events:>7}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_extracts_value_or_none() {
        let args: Vec<String> = ["--bind", "0.0.0.0:4318", "--token", "s"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag(&args, "--bind").as_deref(), Some("0.0.0.0:4318"));
        assert_eq!(flag(&args, "--token").as_deref(), Some("s"));
        assert_eq!(flag(&args, "--sink"), None);
        // Trailing flag with no value -> None, not a panic.
        let tail: Vec<String> = vec!["--token".into()];
        assert_eq!(flag(&tail, "--token"), None);
    }

    #[test]
    fn resolve_token_trims_and_treats_empty_as_none() {
        // Only exercises the --token arg path; the env fallback shares serve.rs's tested pattern.
        let some: Vec<String> = vec!["--token".into(), "  abc  ".into()];
        assert_eq!(resolve_token(&some).as_deref(), Some("abc"));
        let blank: Vec<String> = vec!["--token".into(), "   ".into()];
        assert_eq!(resolve_token(&blank), None);
    }
}
