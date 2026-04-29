//! VectorHawk runner — shim library.
//!
//! Exposes [`run_shim`], the single entry point for the shim binary and for
//! the `vectorhawk mcp serve` subcommand (Stream 3 — CLI).
//!
//! # Behaviour
//!
//! 1. Resolve the daemon socket path (platform-appropriate; see [`daemon_socket_path`]).
//! 2. Try to connect to the daemon socket with a 2-second timeout.
//! 3. On success: run `Server<SocketBackend>` — relay all JSON-RPC frames to the
//!    daemon over the socket.
//! 4. On failure (connect error or timeout): log a WARN to stderr, then run
//!    `Server<EmbeddedBackend>` — serve the entire session in-process. No OAuth
//!    callback, no registry sync, no audit upload.
//!
//! # Socket path
//!
//! The shim deliberately does NOT depend on `vectorhawkd-core` to avoid pulling
//! in `rusqlite` (bundled SQLite, ~1.5 MB of link weight). The socket path
//! function is duplicated here from `vectorhawkd-core::state`. The canonical
//! implementation lives there; any change must be mirrored here.
//!
//! Tracked in: TODO(M1) — extract socket path into `vectorhawkd-manifest` or a
//! zero-dep `vectorhawkd-paths` crate so neither core nor shim needs to duplicate it.

use anyhow::Result;
use std::path::PathBuf;
use tracing::warn;
use vectorhawkd_mcp::{
    backend::{EmbeddedBackend, SocketBackend},
    server::Server,
};

// ── Socket path ───────────────────────────────────────────────────────────────

/// Return the platform-appropriate path for the daemon Unix socket.
///
/// Mirrors `vectorhawkd_core::state::AppState::socket_path` exactly.
/// See that function for rationale. Do not diverge.
///
/// - macOS: `~/Library/Application Support/VectorHawk/agent.sock`
/// - Linux: `$XDG_RUNTIME_DIR/vectorhawk/agent.sock`
///   (fallback: `~/.local/share/VectorHawk/agent.sock`)
pub fn daemon_socket_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let candidate = PathBuf::from(runtime).join("vectorhawk").join("agent.sock");
            return Some(candidate);
        }
    }
    // macOS and Linux fallback: data dir
    let base = dirs::data_dir()?;
    Some(base.join("VectorHawk").join("agent.sock"))
}

// ── run_shim ──────────────────────────────────────────────────────────────────

/// Run the shim for one AI-client session.
///
/// Reads JSON-RPC from stdin, writes responses to stdout. Returns when stdin
/// closes (AI client disconnects) or on an unrecoverable error.
///
/// The shim tries the daemon socket first (2 s timeout). On failure it falls
/// back to `EmbeddedBackend` with a pre-registered stub tool set. The fallback
/// warning is written to stderr (WARN level via `tracing`); it does not appear
/// on stdout (which is the MCP wire).
pub async fn run_shim() -> Result<()> {
    let socket_path = daemon_socket_path();

    // Attempt to use the daemon via SocketBackend.
    //
    // Probe the socket with `connect()` first (which internally enforces the
    // 2-second timeout). On success the probe connection is immediately released
    // and `Server::run_stdio` — via `on_start` — opens the live session
    // connection. The extra round-trip is ~1 ms on a local socket and is
    // acceptable for M0; M1 can make `Server` accept a pre-connected backend.
    //
    // On connect failure we fall through to `EmbeddedBackend`. We do NOT fall
    // back mid-session (after stdin bytes have been consumed): if the daemon
    // dies after the session starts, the relay error propagates up and the
    // process exits — the AI client will reopen the shim. Full mid-session
    // transparent fallback requires stdin buffering and is tracked for M1.
    #[cfg(unix)]
    if let Some(ref path) = socket_path {
        let probe = SocketBackend::new(path);
        match probe.connect().await {
            Ok(()) => {
                // Probe succeeded — socket is live. Drop the probe connection
                // and start the real relay session via a fresh SocketBackend.
                drop(probe);
                let backend = SocketBackend::new(path);
                return Server::new(backend).run_stdio().await;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    socket = %path.display(),
                    "daemon socket unreachable (>2 s timeout or connect error) \
                     — falling back to in-process embedded backend"
                );
            }
        }
    }

    #[cfg(not(unix))]
    {
        // Windows: no socket support in M0. Fall through to embedded directly.
        warn!("daemon socket relay not supported on this platform (M0) — using embedded backend");
    }

    // Fallback: in-process embedded backend.
    // Pre-register a minimal stub so tools/list returns something sensible even
    // without a live daemon.
    let backend = EmbeddedBackend::with_stub_backend(
        "vectorhawk",
        &[
            ("list_skills", "List installed VectorHawk skills"),
            ("run_skill", "Run a VectorHawk skill"),
            ("install_skill", "Install a skill from the registry"),
            ("search_skills", "Search the skill registry"),
            ("get_status", "Get VectorHawk runner status"),
        ],
    );

    Server::new(backend).run_stdio().await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
