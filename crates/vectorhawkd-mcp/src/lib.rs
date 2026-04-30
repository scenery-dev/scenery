//! VectorHawk runner — MCP protocol, aggregator, and `Backend` trait.
//!
//! # Modules
//!
//! - `protocol` — JSON-RPC 2.0 + MCP protocol types (ported from
//!   `skillrunner-mcp::protocol`)
//! - `aggregator` — `BackendRegistry`, tool namespacing (`server_id__tool_name`),
//!   tool budget enforcement (ported + simplified from `skillrunner-mcp::aggregator`)
//! - `sampling` — `McpSamplingClient`, `HybridModelClient` (ported from
//!   `skillrunner-mcp::sampling`)
//! - `setup` — AI client detection, `mcp setup` config writing (Claude Code path
//!   for M0; full matrix in M1)
//! - `backend` — the `Backend` trait + three implementations:
//!   `EmbeddedBackend` (in-process, functional in M0),
//!   `SocketBackend` (relay to daemon, scaffolded),
//!   `RealBackend` (daemon backend, scaffolded)
//! - `server` — `Server<B: Backend>` — the MCP JSON-RPC dispatch loop
//!
//! # What is NOT in M0
//!
//! - `tools.rs` / management tool handlers — deferred; M0 just proves the
//!   Backend round-trip. Management tools (`skillclub_*` → `vectorhawk_*`) land
//!   in M1 with the full skillrunner-core port.
//! - `backends_config.rs` / local YAML backend config — deferred to M1.
//! - Real HTTP dispatch in `SocketBackend::relay` — the struct exists and compiles;
//!   the daemon shim stream (Stream 3) fills in the relay loop.

pub mod aggregator;
pub mod backend;
pub mod protocol;
pub mod sampling;
pub mod server;
pub mod setup;
pub mod stdio_process;
pub mod tools;
