//! dotz-core — the Rust backend (axum server + agent/memory/... modules).
//! Replaces the Node Fastify server + pi SDK + mem0 stack, behind the identical HTTP/WS contract
//! (see docs/api-contract.md). The vanilla `web/` UI is served unchanged.
pub mod agent;
pub mod browser;
pub mod config;
pub mod connections;
pub mod context_bus;
pub mod design;
pub mod embed;
pub mod memory;
pub mod profiles;
pub mod projects;
pub mod sandbox;
pub mod server;
pub mod skills;
pub mod templates;
pub mod types;
pub mod verify;
pub mod workflow_executor;
pub mod workflows;
