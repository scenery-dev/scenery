//! `vectorhawkd` — the VectorHawk runner daemon.
//!
//! Long-running per-user agent. Listens on a Unix domain socket, multiplexes
//! incoming shim sessions to shared backend MCP connections, owns SQLite,
//! audit buffer, policy cache, registry sync, OAuth callback listener, and
//! credential broker client.

fn main() {
    eprintln!("vectorhawkd: not yet implemented (M0 placeholder)");
    std::process::exit(2);
}
