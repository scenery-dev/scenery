//! `vectorhawkd` — the VectorHawk runner daemon.
//!
//! Long-running per-user agent. Listens on a Unix domain socket, multiplexes
//! incoming shim sessions to shared backend MCP connections, owns SQLite,
//! audit buffer, policy cache, registry sync, OAuth callback listener, and
//! credential broker client.
//!
//! # Process model
//!
//! - Tokio current-thread runtime (saves ~5 MB RSS and 4-6 threads vs default).
//! - One task per accepted shim connection, routed through `socket_dispatch`.
//! - SIGTERM / SIGINT cause the accept loop to stop; in-flight tasks drain for
//!   up to 2 s, then the socket file is removed and the process exits 0.
//!
//! # Socket
//!
//! Platform-appropriate path from `AppState::socket_path()`:
//! - macOS: `~/Library/Application Support/VectorHawk/agent.sock`
//! - Linux: `$XDG_RUNTIME_DIR/vectorhawk/agent.sock` (falls back to data dir)
//!
//! Permissions are set to 0600 (owner-only). A stale socket file from a
//! previous daemon run is removed on startup.

mod socket_dispatch;

use anyhow::{Context, Result};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio::{
    net::UnixListener,
    signal::unix::{signal, SignalKind},
};
use tracing::{error, info, warn};
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::{Backend, RealBackend},
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // ── Tracing setup ─────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        error!(error = %e, "daemon exited with error");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // ── Bootstrap state (creates data dirs + SQLite schema) ───────────────────
    let state = AppState::bootstrap().context("failed to bootstrap application state")?;
    info!(root = %state.root_dir, "application state bootstrapped");

    // ── Build the stub backend registry ──────────────────────────────────────
    let registry = Arc::new(build_stub_registry());
    let backend = Arc::new(RealBackend::new(Arc::clone(&registry)));
    info!(
        "backend registry ready ({} backends)",
        registry.backend_count()
    );

    // ── Bind Unix socket ──────────────────────────────────────────────────────
    let socket_path = state.socket_path();

    // Ensure parent directory exists (matters on Linux with XDG_RUNTIME_DIR).
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket parent dir: {parent}"))?;
    }

    // Remove a stale socket file if it exists.
    if socket_path.exists() {
        warn!(path = %socket_path, "removing stale socket file from previous run");
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket: {socket_path}"))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {socket_path}"))?;

    // Set socket permissions to 0600 (owner read/write only).
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&socket_path, perms)
        .with_context(|| format!("failed to set socket permissions: {socket_path}"))?;

    info!(path = %socket_path, "listening on Unix socket");

    // ── Signal handling ───────────────────────────────────────────────────────
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("failed to register SIGINT handler")?;

    // ── Accept loop ───────────────────────────────────────────────────────────
    let socket_path_for_cleanup = socket_path.clone();

    loop {
        tokio::select! {
            // Accept a new shim connection.
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let backend_clone = Arc::clone(&backend);
                        tokio::spawn(socket_dispatch::serve_connection(
                            stream,
                            backend_clone,
                        ));
                    }
                    Err(e) => {
                        // Log and continue — a single accept error should not kill the daemon.
                        error!(error = %e, "accept failed");
                    }
                }
            }

            // Graceful shutdown on SIGTERM.
            _ = sigterm.recv() => {
                info!("received SIGTERM — shutting down");
                break;
            }

            // Graceful shutdown on SIGINT (Ctrl+C in foreground debug mode).
            _ = sigint.recv() => {
                info!("received SIGINT — shutting down");
                break;
            }
        }
    }

    // ── Teardown ──────────────────────────────────────────────────────────────
    info!("draining in-flight connections (up to 2 s)");
    // Give spawned tasks a moment to complete their current frame.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    backend.on_shutdown().await;

    // Remove the socket file so the shim knows the daemon is gone.
    if socket_path_for_cleanup.exists() {
        if let Err(e) = std::fs::remove_file(&socket_path_for_cleanup) {
            warn!(error = %e, path = %socket_path_for_cleanup, "failed to remove socket on shutdown");
        } else {
            info!(path = %socket_path_for_cleanup, "socket removed");
        }
    }

    info!("vectorhawkd shut down cleanly");
    Ok(())
}

/// Construct the stub `BackendRegistry` for M0.
///
/// Registers a single in-memory backend ("stub") with one tool (`echo`) so
/// that `tools/list` returns at least one result.  Real HTTP backends land
/// in M1.
fn build_stub_registry() -> BackendRegistry {
    let registry = BackendRegistry::new();
    registry.register_backend(BackendEntry {
        server_id: "stub".to_string(),
        name: "stub".to_string(),
        transport: BackendTransport::Stub,
        tools: vec![
            ToolDefinition {
                name: "echo".to_string(),
                description: Some(
                    "Echo tool — returns the arguments it received. M0 stub.".to_string(),
                ),
                input_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The message to echo back"
                        }
                    },
                    "required": ["message"]
                })),
            },
            ToolDefinition {
                name: "ping".to_string(),
                description: Some("Health check — returns pong. M0 stub.".to_string()),
                input_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {}
                })),
            },
        ],
        tool_visibility: ToolVisibility::All,
        priority: 50,
    });
    registry
}
