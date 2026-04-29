//! VectorHawk runner — stdio↔socket relay shim.
//!
//! Spawned per AI-client session. Reads JSON-RPC from stdin, relays to the
//! daemon over Unix socket, writes responses/notifications back to stdout.
//! Falls back to running an in-process embedded server if the daemon socket
//! is unreachable for >2 seconds.
//!
//! Typically invoked as `vectorhawk mcp serve`; this binary exists so the
//! shim can also be tested in isolation.

fn main() {
    eprintln!("vectorhawkd-shim: not yet implemented (M0 placeholder)");
    std::process::exit(2);
}
