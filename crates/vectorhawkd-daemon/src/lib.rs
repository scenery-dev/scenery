//! `vectorhawkd` — VectorHawk runner daemon library.
//!
//! Long-running per-user agent. The binary `vectorhawkd` is a thin wrapper
//! around [`run_daemon`]; the user CLI's `vectorhawk daemon run --foreground`
//! also calls [`run_daemon`] so support has a single code path to debug.
//!
//! # Process model
//!
//! - Tokio current-thread runtime (the caller chooses; defaults to
//!   `current_thread` in the binary).
//! - One task per accepted shim connection, routed through `socket_dispatch`.
//! - SIGTERM / SIGINT cause the accept loop to stop; in-flight tasks drain for
//!   up to 2 s, then the socket file is removed and the function returns.
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

/// Configuration for the daemon entry point.
///
/// The binary's `main.rs` populates these from environment variables; the CLI
/// (`vectorhawk daemon run`) populates them from CLI flags. M1.4 will extend
/// this with registry policy / sync overrides as those land.
#[derive(Debug, Default, Clone)]
pub struct DaemonOpts {
    /// Override the registry URL. If `None`, falls back to env
    /// (`SKILLCLUB_REGISTRY_URL`) or registry-driven defaults.
    /// Honored by M1.4 once the registry is wired into the daemon's sync loop.
    pub registry_url: Option<String>,

    /// Override the socket path. If `None`, uses `AppState::socket_path()`.
    pub socket_path_override: Option<camino::Utf8PathBuf>,
}

/// Run the daemon to completion.
///
/// Bootstraps state, builds the (M0 stub) backend registry, listens on the
/// Unix socket, and serves shim connections until SIGTERM/SIGINT.
///
/// **Tracing must be initialized by the caller** before invoking this function.
/// The binary and the CLI both wire up `tracing_subscriber` themselves so this
/// function does not double-init.
pub async fn run_daemon(opts: DaemonOpts) -> Result<()> {
    let state = AppState::bootstrap().context("failed to bootstrap application state")?;
    info!(root = %state.root_dir, "application state bootstrapped");

    if let Some(url) = &opts.registry_url {
        info!(
            registry_url = %url,
            "registry URL override (not yet honored — M1.4)"
        );
    }

    let registry = Arc::new(build_stub_registry());
    let backend = Arc::new(RealBackend::new(Arc::clone(&registry)));
    info!(
        "backend registry ready ({} backends)",
        registry.backend_count()
    );

    let socket_path = opts
        .socket_path_override
        .unwrap_or_else(|| state.socket_path());

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket parent dir: {parent}"))?;
    }

    if socket_path.exists() {
        warn!(path = %socket_path, "removing stale socket file from previous run");
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket: {socket_path}"))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {socket_path}"))?;

    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&socket_path, perms)
        .with_context(|| format!("failed to set socket permissions: {socket_path}"))?;

    info!(path = %socket_path, "listening on Unix socket");

    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("failed to register SIGINT handler")?;

    let socket_path_for_cleanup = socket_path.clone();

    loop {
        tokio::select! {
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
                        error!(error = %e, "accept failed");
                    }
                }
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM — shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT — shutting down");
                break;
            }
        }
    }

    info!("draining in-flight connections (up to 2 s)");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    backend.on_shutdown().await;

    if socket_path_for_cleanup.exists() {
        if let Err(e) = std::fs::remove_file(&socket_path_for_cleanup) {
            warn!(
                error = %e,
                path = %socket_path_for_cleanup,
                "failed to remove socket on shutdown"
            );
        } else {
            info!(path = %socket_path_for_cleanup, "socket removed");
        }
    }

    info!("vectorhawkd shut down cleanly");
    Ok(())
}

/// Construct the M0 stub `BackendRegistry`.
///
/// Registers a single in-memory backend (`stub`) with two tools (`echo`,
/// `ping`) so that `tools/list` returns at least one result. Real HTTP
/// backends land in M1.3.
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
        consecutive_errors: 0,
        unhealthy: false,
    });
    registry
}
