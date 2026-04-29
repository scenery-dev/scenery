//! VectorHawk runner — MCP protocol, aggregator, and `Backend` trait.
//!
//! Houses:
//! - JSON-RPC / MCP protocol types (ported from `skillrunner-mcp::protocol`)
//! - Aggregator and `BackendRegistry` (ported from `skillrunner-mcp::aggregator`)
//! - Tool dispatch and skill-as-tool mapping (ported from `skillrunner-mcp::tools`)
//! - Sampling delegation (ported from `skillrunner-mcp::sampling`)
//! - AI client setup (ported from `skillrunner-mcp::setup`)
//! - The new `Server<B: Backend>` generic that the daemon and shim
//!   instantiate with different `Backend` implementations
//!   (`SocketBackend` for the shim's normal path, `EmbeddedBackend` for
//!   the shim's fallback path, the real backend for the daemon).

#[doc(hidden)]
pub fn _placeholder() {}
