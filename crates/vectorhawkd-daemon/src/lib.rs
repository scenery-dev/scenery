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
use vectorhawkd_core::{
    audit::{AuditBuffer, SqliteAuditBuffer},
    registry::RegistryClient,
    state::AppState,
};
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::{Backend, RealBackend},
};

/// Configuration for the daemon entry point.
///
/// The binary's `main.rs` populates these from environment variables; the CLI
/// (`vectorhawk daemon run`) populates them from CLI flags.
#[derive(Debug, Default, Clone)]
pub struct DaemonOpts {
    /// Override the registry URL. If `None`, falls back to env
    /// (`SKILLCLUB_REGISTRY_URL`) or registry-driven defaults.
    pub registry_url: Option<String>,

    /// Override the socket path. If `None`, uses `AppState::socket_path()`.
    pub socket_path_override: Option<camino::Utf8PathBuf>,
}

/// How often (in seconds) the background sync loop ticks.
const SYNC_INTERVAL_SECS: u64 = 300;

/// Run the daemon to completion.
///
/// Bootstraps state, builds the backend registry, starts the registry sync
/// loop + audit flush loop, listens on the Unix socket, and serves shim
/// connections until SIGTERM/SIGINT.
///
/// **Tracing must be initialized by the caller** before invoking this function.
pub async fn run_daemon(opts: DaemonOpts) -> Result<()> {
    let state = AppState::bootstrap().context("failed to bootstrap application state")?;
    info!(root = %state.root_dir, "application state bootstrapped");

    let registry_url = opts
        .registry_url
        .clone()
        .or_else(|| std::env::var("SKILLCLUB_REGISTRY_URL").ok())
        .unwrap_or_else(|| "https://registry.vectorhawk.ai".to_string());

    info!(registry_url, "connecting to registry");

    let registry = Arc::new(RegistryClient::new(&registry_url));
    let audit_buffer = Arc::new(SqliteAuditBuffer::new(Arc::clone(&registry), &state));

    // Spawn the registry sync loop (300 s interval).
    // All synchronous I/O happens inside spawn_blocking so the current-thread
    // Tokio executor is never blocked by HTTP or SQLite calls.
    let sync_registry = Arc::clone(&registry);
    let sync_audit = Arc::clone(&audit_buffer);
    let sync_db_path = state.db_path.clone();
    let sync_root_dir = state.root_dir.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SYNC_INTERVAL_SECS));
        // Skip the immediate first tick — let the accept loop start first.
        interval.tick().await;

        loop {
            interval.tick().await;

            let reg = Arc::clone(&sync_registry);
            let aud = Arc::clone(&sync_audit);
            let db = sync_db_path.clone();
            let root = sync_root_dir.clone();

            let result =
                tokio::task::spawn_blocking(move || run_sync_tick(&reg, &aud, &db, &root)).await;

            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(error = %e, "registry sync tick failed; will retry on next interval");
                }
                Err(join_err) => {
                    warn!(error = %join_err, "registry sync task panicked; will retry");
                }
            }
        }
    });

    let vh_registry = Arc::new(build_stub_registry());
    let backend = Arc::new(RealBackend::new(Arc::clone(&vh_registry)));
    info!(
        "backend registry ready ({} backends)",
        vh_registry.backend_count()
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

    // Final audit flush on clean shutdown (best-effort — do not abort shutdown on error).
    let final_audit = Arc::clone(&audit_buffer);
    let shutdown_state = AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    };
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = final_audit.flush(&shutdown_state) {
            warn!(error = %e, "final audit flush on shutdown failed");
        }
    })
    .await;

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

/// One tick of the registry sync loop.
///
/// Called from `spawn_blocking` — issues synchronous SQLite and HTTP I/O.
///
/// Steps:
/// 1. Flush pending audit events to the registry.
/// 2. Refresh the approved-server list.
/// 3. Check skill lifecycle status for all installed skills.
///
/// Each step failure logs at WARN and continues; only a total inability to
/// open the database returns `Err`.
fn run_sync_tick(
    registry: &RegistryClient,
    audit: &SqliteAuditBuffer,
    db_path: &camino::Utf8PathBuf,
    root_dir: &camino::Utf8PathBuf,
) -> Result<()> {
    // ── 1. Audit flush ────────────────────────────────────────────────────────
    let state_view = AppState {
        root_dir: root_dir.clone(),
        db_path: db_path.clone(),
    };
    match audit.flush(&state_view) {
        Ok(n) if n > 0 => info!(count = n, "sync: audit flush uploaded events"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sync: audit flush failed"),
    }

    // ── 2. Approved-server list refresh ──────────────────────────────────────
    match registry.fetch_approved_servers() {
        Ok(resp) => {
            info!(
                count = resp.servers.len(),
                "sync: approved server list refreshed"
            );
        }
        Err(e) => {
            warn!(error = %e, "sync: failed to refresh approved server list");
        }
    }

    // ── 3. Skill lifecycle + version refresh ─────────────────────────────────
    let skill_ids = match state_view.list_installed_skill_ids() {
        Ok(ids) => ids,
        Err(e) => {
            warn!(error = %e, "sync: failed to read installed skills");
            return Ok(());
        }
    };

    if skill_ids.is_empty() {
        return Ok(());
    }

    match registry.check_skill_status(&skill_ids) {
        Ok(status_resp) => {
            info!(
                checked = skill_ids.len(),
                unknown_count = status_resp.unknown.len(),
                "sync: skill lifecycle status refreshed"
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                "sync: skill lifecycle check failed; skipping version updates this tick"
            );
        }
    }

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
    });
    registry
}
