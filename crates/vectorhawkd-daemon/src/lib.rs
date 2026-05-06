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
//!
//! # spawn_blocking discipline (M1.6 audit)
//!
//! The daemon uses `tokio::runtime::Builder::new_current_thread()`. Any
//! blocking call on the executor thread stalls ALL concurrent connections.
//! The following invariants are enforced and must be preserved by future
//! contributors:
//!
//! ## Hot path (per-shim-connection tasks spawned by the accept loop)
//!
//! - `socket_dispatch::serve_connection` → `RealBackend::call_tool` → SAFE:
//!   - `BackendTransport::Stub`: pure in-memory, no I/O.
//!   - `BackendTransport::Http`: async `reqwest` (tokio-native), non-blocking.
//!   - `BackendTransport::Stdio`: wrapped in `tokio::task::spawn_blocking` in
//!     `aggregator.rs::dispatch`.
//! - `audit.record()` from `RealBackend` — NOT YET WIRED (M1.4 pending). When
//!   it is wired, the call site MUST go through `spawn_blocking` because
//!   `SqliteAuditBuffer::record` opens a `rusqlite::Connection` synchronously.
//!   See the TODO comment in `socket_dispatch.rs`.
//!
//! ## Background tasks
//!
//! - Registry sync loop: wraps `run_sync_tick` in `spawn_blocking`. This
//!   function issues sync HTTP (`reqwest::blocking`) and sync SQLite calls.
//!   Adding any new sync I/O to `run_sync_tick` is safe.
//! - Final audit flush on shutdown: wrapped in `spawn_blocking`.
//!
//! ## Startup (before accept loop)
//!
//! - `AppState::bootstrap()`, `std::fs::create_dir_all`, `std::fs::remove_file`,
//!   `std::fs::set_permissions`: called once before the Tokio accept loop is hot.
//!   These are acceptable at startup time and must NOT be moved into the accept
//!   loop or per-connection handlers without adding `spawn_blocking`.

mod auth_dispatch;
mod oauth_listener;
mod oauth_state;
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
    auth::{load_all_tokens, save_tokens, AuthClient},
    registry::RegistryClient,
    state::AppState,
};
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::{Backend, RealBackend},
};

pub use oauth_state::OAuthState;
pub use socket_dispatch::DaemonContext;

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

/// How often (in seconds) the token refresh loop checks for near-expiry tokens.
const REFRESH_INTERVAL_SECS: u64 = 60;

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

    // Spawn the token refresh loop (60 s interval).
    // Checks every stored access token and refreshes any that are within 5 min
    // of expiry.  All sync I/O (SQLite + HTTP) happens inside spawn_blocking
    // per the spawn_blocking discipline documented at the top of this file.
    let refresh_db_path = state.db_path.clone();
    let refresh_root_dir = state.root_dir.clone();
    let refresh_registry_url = registry_url.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(REFRESH_INTERVAL_SECS));
        // Skip the immediate first tick — let the accept loop start first.
        interval.tick().await;

        loop {
            interval.tick().await;

            let db = refresh_db_path.clone();
            let root = refresh_root_dir.clone();
            let reg_url = refresh_registry_url.clone();

            let result = tokio::task::spawn_blocking(move || {
                let state_view = AppState {
                    root_dir: root,
                    db_path: db,
                };
                refresh_one_tick(&state_view, &reg_url)
            })
            .await;

            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(error = %e, "token refresh tick failed; will retry on next interval");
                }
                Err(join_err) => {
                    warn!(error = %join_err, "token refresh task panicked; will retry");
                }
            }
        }
    });

    let vh_registry = Arc::new(build_stub_registry());
    let managed_config = vectorhawkd_core::managed::load_managed_config(&state);
    if let Some(ref m) = managed_config {
        info!(
            org = m.org.as_deref().unwrap_or("(unspecified)"),
            "running in managed mode — MCP instructions will name the org and \
             include governance language"
        );
    }
    let backend = Arc::new(RealBackend::with_audit_and_managed(
        Arc::clone(&vh_registry),
        Arc::clone(&audit_buffer) as Arc<dyn AuditBuffer>,
        managed_config,
    ));
    info!(
        "backend registry ready ({} backends)",
        vh_registry.backend_count()
    );

    // Build the OAuth notification hub.
    let oauth_state = Arc::new(OAuthState::new());

    // Start the OAuth callback HTTP listener (non-fatal if all ports in use).
    let (listener_port, listener_handle) = match oauth_listener::start_listener(Arc::clone(
        &oauth_state,
    ))
    .await
    {
        Ok(Some((addr, handle))) => {
            info!(port = addr.port(), "OAuth callback listener started");
            (Some(addr.port()), Some(handle))
        }
        Ok(None) => {
            warn!("OAuth callback listener could not bind — auth login will be unavailable");
            (None, None)
        }
        Err(e) => {
            warn!(error = %e, "OAuth callback listener failed to start — auth login will be unavailable");
            (None, None)
        }
    };

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
                        let ctx = DaemonContext {
                            backend: Arc::clone(&backend),
                            oauth_state: Arc::clone(&oauth_state),
                            listener_port,
                        };
                        tokio::spawn(socket_dispatch::serve_connection(stream, ctx));
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

    // Cancel all pending OAuth waiters so CLI subscribers get a clean error.
    oauth_state.cancel_all().await;

    // Stop the OAuth HTTP listener.
    if let Some(handle) = listener_handle {
        handle.abort();
    }

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

/// One tick of the token refresh loop.
///
/// Called from `spawn_blocking` — issues synchronous SQLite and HTTP I/O.
///
/// Steps:
/// 1. Load all rows from `auth_tokens`.
/// 2. For each row whose `access_token` expires within 5 minutes, call
///    `AuthClient::refresh` using the stored `refresh_token`.
/// 3. On success, overwrite the row with the new tokens and log INFO.
/// 4. On failure, log WARN and continue to the next row (do not panic).
///
/// This function is exposed as `pub` so tests can drive a single tick
/// without running the full 60-second loop.
pub fn refresh_one_tick(state: &AppState, registry_url: &str) -> Result<()> {
    let rows = load_all_tokens(state).context("refresh_one_tick: failed to load auth_tokens")?;

    for row in rows {
        if !AuthClient::needs_refresh(&row.access_token) {
            continue;
        }

        tracing::debug!(
            registry_url = %row.registry_url,
            "token near expiry — attempting refresh"
        );

        // Use the registry_url stored in the row (each row may target a
        // different registry) rather than the daemon's primary registry_url.
        let client = AuthClient::new(&row.registry_url);
        match client.refresh(&row.refresh_token) {
            Ok(new_tokens) => {
                match save_tokens(
                    state,
                    &row.registry_url,
                    &new_tokens.access_token,
                    &new_tokens.refresh_token,
                ) {
                    Ok(()) => {
                        info!(
                            registry_url = %row.registry_url,
                            "refresh_one_tick: token rotated successfully"
                        );
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            registry_url = %row.registry_url,
                            "refresh_one_tick: failed to save rotated token"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    registry_url = %row.registry_url,
                    "refresh_one_tick: token refresh HTTP call failed"
                );
            }
        }
    }

    let _ = registry_url; // primary registry_url param kept for future rate-limit context
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
        consecutive_errors: 0,
        unhealthy: false,
    });
    registry
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "refresh_loop_tests.rs"]
mod refresh_loop_tests;
