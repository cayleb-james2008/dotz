//! dotz-core — the Rust backend (axum server + agent/memory/... modules).
//! Replaces the Node Fastify server + pi SDK + mem0 stack, behind the identical HTTP/WS contract
//! (see docs/api-contract.md). The vanilla `web/` UI is served unchanged.
pub mod server;
