# dotz telemetry — collecting the weekly-active metric

The fleet goal metric is **5 external weekly-active users**: distinct opted-in installs seen per
UTC ISO week. This doc covers how events flow, how the operator collects events from EXTERNAL
installs (the part that was uncollectable by design before 2026-07-17 — the only receiver was
loopback-only inside each install, writing to that user's own disk), and how to compute the metric.

Everything stays **opt-in and PII-free**: events are `{eventType, sessionId, ts, (command)}` only
(`test_no_pii_in_event_payload` pins the schema), and nothing is sent until telemetry is enabled
in Settings.

## Event flow

1. Emitter (`dotz-core/src/telemetry.rs`): on app launch / slash-command run / first launch of a
   UTC day, POSTs one JSON event to the configured `endpoint` in `~/.dotz/telemetry.json`.
2. Receiver: POST `/telemetry/ingest` appends the event verbatim to
   `~/.dotz/telemetry_sink.jsonl` (on the RECEIVER's machine).
3. Aggregator: `telemetry weekly` reduces the sink to distinct installs per ISO week.

### Enabled implies a working endpoint

Toggling telemetry ON (Settings) runs `enable_with_working_endpoint`: an **empty or unreachable**
endpoint is replaced with the app's own local receiver (`http://127.0.0.1:4317/telemetry/ingest`),
so opt-in always collects end-to-end instead of silently dropping events. A **reachable** custom
endpoint is kept. The Settings panel shows a live reachability verdict for the configured sink
(`UNREACHABLE` in red when the probe fails); the probe is a GET (any HTTP answer counts, even 405)
so it never pollutes the sink.

## Standalone receiver (external installs → operator)

The in-app receiver is loopback-only and inherits the app's origin/Host guard — external users can
never reach it. To collect THEIR events, the operator hosts the standalone receiver:

```bash
# on the collector host (VPS / tunnel target / port-forwarded box)
cargo run -p dotz-core --bin telemetry -- receive --bind 0.0.0.0:4318 --token <shared-secret>
# or: DOTZ_TELEMETRY_BIND=0.0.0.0:4318 DOTZ_TELEMETRY_TOKEN=<shared-secret> telemetry receive
```

Rules:

- A **non-loopback bind refuses to start without a token** (fail-closed: no unauthenticated
  append-to-disk endpoint on the open internet). Loopback binds (default `127.0.0.1:4318`) may
  stay tokenless.
- Requests must carry the token as the `x-dotz-telemetry-token` header; missing/wrong → `401`,
  nothing written. The compare is constant-time.
- Events land in the receiver host's `~/.dotz/telemetry_sink.jsonl` (`DOTZ_CONFIG_DIR` honored).
- **Actually exposing the port/hostname (VPS, tunnel, DNS, firewall) is an operator decision and
  stays operator-gated** — nothing in the repo or the app starts a public listener on its own.

Each external beta user then opts in by editing `~/.dotz/telemetry.json` on their install (or the
operator ships it in onboarding notes):

```json
{ "enabled": true, "endpoint": "http://<collector-host>:4318/telemetry/ingest", "token": "<shared-secret>" }
```

Turning telemetry OFF in Settings wipes `endpoint` + `token` (re-enable must re-confirm the sink;
a plain re-toggle ON falls back to the local receiver).

## Computing the metric

```bash
cargo run -p dotz-core --bin telemetry -- weekly            # default sink
cargo run -p dotz-core --bin telemetry -- weekly --sink path/to/telemetry_sink.jsonl
```

Output — one row per UTC ISO week (`iso_week` is pinned against Python `isocalendar()` anchors,
including 53-week years and year-boundary Mondays):

```
iso-week    distinct-installs   events
2026-W29                    3       41
2026-W30                    5       77
```

`distinct-installs` counts distinct `sessionId` values — the stable, anonymous per-install id
minted into `~/.dotz/telemetry_id.json` on first send. **The goal gate is
`distinct-installs >= 5`** on the operator's collector sink (the operator's own install, if it
posts there too, contributes 1 — subtract it when reporting *external* users).

Garbage/truncated sink lines are skipped, never fatal.
