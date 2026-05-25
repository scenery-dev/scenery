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
pub mod managed_paths;
mod oauth_listener;
mod oauth_state;
mod socket_dispatch;
pub mod sync;

use anyhow::{Context, Result};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio::{
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    sync::broadcast,
};
use tracing::{error, info, warn};
use vectorhawkd_core::{
    audit::{AuditBuffer, SqliteAuditBuffer},
    auth::{load_all_tokens, save_tokens, AuthClient},
    gateway_model::GatewayModelClient,
    model::ModelClient,
    ollama::OllamaClient,
    policy::PolicyClient,
    registry::{HttpPolicyClient, RegistryClient},
    state::AppState,
};
use vectorhawkd_mcp::sampling::HybridModelClient;
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolVisibility},
    backend::{Backend, RealBackend},
    tools::UpdateCheckCache,
};

pub use oauth_state::OAuthState;
pub use socket_dispatch::DaemonContext;

/// Bridges `OAuthState` (daemon crate) to `OAuthSubscriber` (MCP crate) so
/// that `tools::handle_login_with_oauth` can await browser callbacks without
/// introducing a direct dependency on the daemon crate from the MCP crate.
struct OAuthStateSubscriber(Arc<OAuthState>);

impl vectorhawkd_mcp::oauth::OAuthSubscriber for OAuthStateSubscriber {
    fn wait_for_code(
        &self,
        state: String,
        timeout_secs: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> {
        let oauth_state = Arc::clone(&self.0);
        Box::pin(async move {
            let rx = match oauth_state.subscribe(state).await {
                Ok(rx) => rx,
                Err(e) => {
                    warn!(error = %e, "OAuthStateSubscriber: subscribe failed");
                    return None;
                }
            };
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
                Ok(Ok((code, _state))) => Some(code),
                Ok(Err(_)) => None, // channel closed — daemon shutting down
                Err(_) => None,     // timeout elapsed
            }
        })
    }
}

/// Configuration for the daemon entry point.
///
/// The binary's `main.rs` populates these from environment variables; the CLI
/// (`vectorhawk daemon run`) populates them from CLI flags.
#[derive(Debug, Default, Clone)]
pub struct DaemonOpts {
    /// Override the registry URL. If `None`, falls back to env
    /// (`VECTORHAWK_REGISTRY_URL`) or registry-driven defaults.
    pub registry_url: Option<String>,

    /// Override the socket path. If `None`, uses `AppState::socket_path()`.
    pub socket_path_override: Option<camino::Utf8PathBuf>,

    /// Override the Ollama base URL. If `None`, falls back to env
    /// (`VECTORHAWK_OLLAMA_URL`) then managed config then the default
    /// `http://127.0.0.1:11434`.
    pub ollama_url: Option<String>,

    /// Override the Ollama model tag. If `None`, falls back to env
    /// (`VECTORHAWK_OLLAMA_MODEL`) then managed config then an empty string
    /// (resolved at call time via `resolve_model`).
    pub ollama_model: Option<String>,
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
        .or_else(|| std::env::var("VECTORHAWK_REGISTRY_URL").ok())
        .unwrap_or_else(|| "https://app.vectorhawk.ai".to_string());

    info!(registry_url, "connecting to registry");

    // Headless / CI auth: if VECTORHAWK_TOKEN is set and looks like a PAT,
    // save it to state.db so the daemon can make authenticated registry calls
    // without a browser-based OAuth flow (openclaw, CI pipelines, etc.).
    if let Ok(pat) = std::env::var("VECTORHAWK_TOKEN") {
        if pat.starts_with("vh_pat_") {
            match vectorhawkd_core::auth::save_tokens(&state, &registry_url, &pat, &pat) {
                Ok(()) => info!("VECTORHAWK_TOKEN: PAT saved for {registry_url}"),
                Err(e) => warn!(error = %e, "VECTORHAWK_TOKEN: failed to save PAT to state DB"),
            }
        } else {
            warn!("VECTORHAWK_TOKEN is set but does not start with 'vh_pat_' — ignoring");
        }
    }

    let registry = Arc::new(RegistryClient::new(&registry_url));
    let audit_buffer = Arc::new(SqliteAuditBuffer::new(Arc::clone(&registry), &state));

    // Broadcast channel for `notifications/tools/list_changed`.
    // Capacity 16: if a subscriber falls behind by 16 messages it will receive
    // a `RecvError::Lagged` and coalesce (send one notification). This is
    // safe because list_changed is idempotent.
    let (list_changed_tx, _list_changed_rx_seed) = broadcast::channel::<()>(16);

    // Create the update-check cache before the sync loop so both the sync loop
    // and RealBackend share the same Arc. The sync loop populates it each tick;
    // RealBackend reads it when serving tool calls.
    let update_check_cache: UpdateCheckCache =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // Spawn the registry sync loop (300 s interval).
    // All synchronous I/O happens inside spawn_blocking so the current-thread
    // Tokio executor is never blocked by HTTP or SQLite calls.
    let sync_registry = Arc::clone(&registry);
    let sync_audit = Arc::clone(&audit_buffer);
    let sync_db_path = state.db_path.clone();
    let sync_root_dir = state.root_dir.clone();
    let sync_list_changed_tx = list_changed_tx.clone();
    let sync_update_cache = Arc::clone(&update_check_cache);
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
            let cache = Arc::clone(&sync_update_cache);

            let result =
                tokio::task::spawn_blocking(move || run_sync_tick(&reg, &aud, &db, &root, &cache))
                    .await;

            match result {
                Ok(Ok(changed)) => {
                    if changed {
                        // Sync updated the tool set — notify all connected shims.
                        // Ignore errors: no receivers means no connected shims.
                        let _ = sync_list_changed_tx.send(());
                    }
                }
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

    let managed_config = vectorhawkd_core::managed::load_managed_config(&state);

    // Resolve Ollama URL (priority: CLI opt → env var → managed config → default).
    let resolved_ollama_url = opts
        .ollama_url
        .clone()
        .or_else(|| std::env::var("VECTORHAWK_OLLAMA_URL").ok())
        .or_else(|| managed_config.as_ref().and_then(|c| c.ollama_url.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());

    // Resolve Ollama model (priority: CLI opt → env var → managed config → empty string).
    let resolved_ollama_model = opts
        .ollama_model
        .clone()
        .or_else(|| std::env::var("VECTORHAWK_OLLAMA_MODEL").ok())
        .or_else(|| managed_config.as_ref().and_then(|c| c.ollama_model.clone()))
        .unwrap_or_default();

    let ollama = OllamaClient::new(resolved_ollama_url.clone(), resolved_ollama_model.clone());

    // Report Ollama health at startup.
    let health = ollama.health_check();
    if health.reachable {
        info!(
            url = %resolved_ollama_url,
            model = %resolved_ollama_model,
            "Ollama reachable — local LLM execution enabled"
        );
        if let Ok(models) = ollama.list_models() {
            let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
            info!(available_models = ?names, "Ollama models available");
        }
    } else {
        warn!(
            url = %resolved_ollama_url,
            "Ollama not reachable — LLM steps will fail without a model client"
        );
    }

    // Build a GatewayModelClient pointing at the same registry URL so that
    // LLM steps can fall through to the cloud gateway when a local Ollama
    // model is not available or not preferred.
    let gateway = GatewayModelClient::new(
        registry_url.clone(),
        Arc::new(AppState {
            root_dir: state.root_dir.clone(),
            db_path: state.db_path.clone(),
        }),
    );

    // Wire HybridModelClient: Ollama (optional local) → GatewayModelClient.
    // The sampling fallback (McpSamplingClient) is handled at the per-shim
    // connection level in server.rs; the daemon-level client provides the
    // Ollama + gateway tier only.
    let hybrid = HybridModelClient::new(
        Some(Box::new(ollama) as Box<dyn ModelClient>),
        Box::new(gateway) as Box<dyn ModelClient>,
    );
    let model_client: Option<Arc<dyn ModelClient>> = Some(Arc::new(hybrid) as Arc<dyn ModelClient>);

    let vh_registry = Arc::new(build_stub_registry());
    load_managed_mcp_into_registry(&state, &vh_registry, list_changed_tx.clone());

    // ── F1: Managed-paths first-run migration ─────────────────────────────────
    //
    // Scans ~/.claude/skills/, ~/.claude/plugins/, and ~/.claude.json; migrates
    // anything not already tracked in `managed_path_markers`.  Run once at
    // startup.  Failure is non-fatal — the daemon continues regardless.
    //
    // Set VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER=1 to skip entirely (useful
    // in tests, CI, and operator opt-out scenarios).
    if std::env::var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_err() {
        let state_arc_f1 = Arc::new(AppState {
            root_dir: state.root_dir.clone(),
            db_path: state.db_path.clone(),
        });
        let registry_url_f1 = registry_url.clone();
        match managed_paths::ManagedPathsReconciler::new(state_arc_f1, registry_url_f1) {
            Ok(reconciler) => match reconciler.migrate_existing().await {
                Ok(report) => info!(
                    skills = report.skills_migrated,
                    plugins = report.plugins_migrated,
                    mcps = report.mcps_migrated,
                    errors = report.errors.len(),
                    "managed_paths: first-run migration complete"
                ),
                Err(e) => warn!(
                    error = %e,
                    "managed_paths: migration failed; daemon continues without ownership"
                ),
            },
            Err(e) => {
                warn!(error = %e, "managed_paths: reconciler initialisation failed; skipping migration");
            }
        }
    } else {
        info!("managed_paths: VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER set — skipping migration");
    }

    // ── F3: Drift scanner ─────────────────────────────────────────────────────
    //
    // Periodically (default 5 min) re-hashes every managed_path_markers row
    // and reports any divergence to the backend. Policy-aware: quarantines
    // when mode=quarantine, holds for approval when mode=approve_required.
    // Killswitch: same VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER env var.
    if std::env::var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_err() {
        let state_arc_f3 = Arc::new(AppState {
            root_dir: state.root_dir.clone(),
            db_path: state.db_path.clone(),
        });
        // Persist registry_url so the SSE drift-resolution handler can find it.
        if let Err(e) = state_arc_f3.set_sync_state("registry_url", &registry_url) {
            warn!(error = %e, "drift: failed to persist registry_url to sync_state");
        }
        match managed_paths::DriftScanner::new(state_arc_f3, registry_url.clone()) {
            Ok(scanner) => {
                Arc::new(scanner).spawn_loop();
                info!("drift: scanner spawned");
            }
            Err(e) => {
                warn!(error = %e, "drift: scanner init failed; running without drift detection");
            }
        }
    }

    if let Some(ref m) = managed_config {
        info!(
            org = m.org.as_deref().unwrap_or("(unspecified)"),
            "running in managed mode — MCP instructions will name the org and \
             include governance language"
        );
    }

    let registry_url_opt: Option<String> = Some(registry_url.clone());

    // Use the real registry-backed policy client. `RegistryClient::new` is
    // cheap (builds an HTTP client); no I/O happens until `fetch_policy` is
    // called. The 7-day offline grace window in `HttpPolicyClient` means
    // policy enforcement degrades gracefully when the registry is unreachable.
    let policy_client: Arc<dyn PolicyClient + Send + Sync> = Arc::new(HttpPolicyClient::new(
        RegistryClient::new(&registry_url),
        &state,
    ));

    let state_arc = Arc::new(AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    });

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

    // ── RUN2: Device registration + SSE sync subsystem ───────────────────────
    //
    // After the daemon is authenticated, register this device with the backend
    // and start the SSE-driven reconciler.  Both steps are best-effort: if the
    // backend is unreachable (offline mode) we log a warning and continue.
    let sync_state_arc = Arc::new(AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    });

    // F2: Build the managed-paths pusher (gated by the same env var as F1).
    let f2_pusher = if std::env::var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_err() {
        let pusher_state = AppState {
            root_dir: state.root_dir.clone(),
            db_path: state.db_path.clone(),
        };
        Some(Arc::new(managed_paths::ManagedPathsPusher::new(
            &pusher_state,
        )))
    } else {
        None
    };

    let sync_handle = try_start_sync(
        &registry_url,
        Arc::clone(&sync_state_arc),
        list_changed_tx.clone(),
        Arc::clone(&vh_registry),
        f2_pusher,
    )
    .await;
    if let Some(ref _handle) = sync_handle {
        info!("sync subsystem started");
    } else {
        info!("sync subsystem not started (no auth token or registration failed)");
    }

    let mut backend = RealBackend::with_full_context(
        Arc::clone(&vh_registry),
        Arc::clone(&audit_buffer) as Arc<dyn AuditBuffer>,
        managed_config,
        state_arc,
        registry_url_opt,
        policy_client,
        model_client,
        update_check_cache,
    );

    // Wire the OAuth callback port + subscriber so vectorhawk_login can
    // complete the PKCE flow automatically after the browser redirects.
    if let Some(port) = listener_port {
        backend = backend.with_oauth(
            port,
            std::sync::Arc::new(OAuthStateSubscriber(Arc::clone(&oauth_state))),
        );
    }

    let backend = Arc::new(backend);
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
                        let ctx = DaemonContext {
                            backend: Arc::clone(&backend),
                            oauth_state: Arc::clone(&oauth_state),
                            listener_port,
                            list_changed_tx: list_changed_tx.clone(),
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

// ── RUN2: Device registration + sync startup ─────────────────────────────────

/// Register this device with the backend and start the SSE sync subsystem.
///
/// Returns `None` if no auth token is available or registration fails.
/// The daemon continues to operate without sync in that case.
async fn try_start_sync(
    registry_url: &str,
    state: Arc<AppState>,
    list_changed_tx: broadcast::Sender<()>,
    backend_registry: Arc<BackendRegistry>,
    pusher: Option<Arc<managed_paths::ManagedPathsPusher>>,
) -> Option<sync::ReconcilerHandle> {
    // Load the access token from SQLite.
    let token = match load_all_tokens(&state) {
        Ok(rows) => rows
            .into_iter()
            .find(|r| r.registry_url == registry_url)
            .map(|r| r.access_token),
        Err(e) => {
            warn!(error = %e, "sync: failed to load auth tokens");
            return None;
        }
    };

    let token = match token {
        Some(t) => t,
        None => {
            info!("sync: no auth token for {registry_url} — skipping device registration");
            return None;
        }
    };

    // Register device (or confirm existing registration).
    let device_id = match register_device(registry_url, &token, Arc::clone(&state)).await {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "sync: device registration failed — skipping SSE sync");
            return None;
        }
    };

    // Retrieve the last SSE event ID for resume.
    let last_event_id = state.get_sync_state("last_event_id").ok().flatten();

    let sync_config = sync::SyncConfig {
        registry_url: registry_url.to_string(),
        token,
        device_id,
        last_event_id,
        pusher,
    };

    match sync::run(sync_config, state, list_changed_tx, backend_registry) {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!(error = %e, "sync: failed to start sync subsystem");
            None
        }
    }
}

/// Register this device with the backend.
///
/// Calls `POST /api/devices/register` with a stable device UUID and system
/// info.  The backend returns a `device_id` which we persist in `sync_state`.
///
/// On success, returns the `device_id` to use for SSE connections.
async fn register_device(registry_url: &str, token: &str, state: Arc<AppState>) -> Result<String> {
    // Retrieve or generate a stable device UUID.
    let device_uuid = match state.get_sync_state("device_uuid")? {
        Some(u) => u,
        None => {
            let new_uuid = uuid::Uuid::new_v4().to_string();
            state.set_sync_state("device_uuid", &new_uuid)?;
            new_uuid
        }
    };

    // Check if we already have a device_id from a previous registration.
    if let Some(existing_id) = state.get_sync_state("device_id")? {
        return Ok(existing_id);
    }

    let url = format!(
        "{}/api/devices/register",
        registry_url.trim_end_matches('/')
    );

    // Prefer HOSTNAME env var (set by most shells); fall back to gethostname
    // via the `libc` crate already in the workspace.
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| {
        // Try gethostname via libc on Unix.
        #[cfg(unix)]
        {
            let mut buf = vec![0u8; 256];
            if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
        }
        "unknown".to_string()
    });

    let payload = serde_json::json!({
        "device_uuid": device_uuid,
        "hostname": hostname,
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "agent_version": env!("CARGO_PKG_VERSION"),
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client for device registration")?;

    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("device registration HTTP call failed to {url}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("device registration returned 401 — token expired");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("device registration failed (HTTP {status}): {body}");
    }

    #[derive(serde::Deserialize)]
    struct RegisterResponse {
        device_id: String,
    }

    let reg: RegisterResponse = resp
        .json()
        .await
        .context("failed to deserialize device registration response")?;

    state
        .set_sync_state("device_id", &reg.device_id)
        .context("failed to persist device_id")?;

    info!(device_id = %reg.device_id, "device registered with backend");
    Ok(reg.device_id)
}

/// One tick of the registry sync loop.
///
/// Called from `spawn_blocking` — issues synchronous SQLite and HTTP I/O.
///
/// Steps:
/// 1. Flush pending audit events to the registry.
/// 2. Refresh the approved-server list.
/// 3. Check skill lifecycle status + run version updates for all installed skills,
///    populating `update_check_cache` with skills that have a newer version available.
/// 4. Flush unsynced skill ratings to the registry.
/// 5. Flush execution stats to the registry.
/// 6. Scan AI client configs for unmanaged MCP servers; buffer audit events.
///
/// Each step failure logs at WARN and continues; only a total inability to
/// open the database returns `Err`.
///
/// Returns `true` if the sync detected changes to the approved-server list or
/// skill set that might alter the tool list visible to connected shims.  The
/// caller broadcasts `list_changed` when this returns `true`.
pub fn run_sync_tick(
    registry: &RegistryClient,
    audit: &SqliteAuditBuffer,
    db_path: &camino::Utf8PathBuf,
    root_dir: &camino::Utf8PathBuf,
    update_cache: &UpdateCheckCache,
) -> Result<bool> {
    use vectorhawkd_core::policy::MockPolicyClient;

    let mut changed = false;

    let state_view = AppState {
        root_dir: root_dir.clone(),
        db_path: db_path.clone(),
    };

    // ── 1. Audit flush ────────────────────────────────────────────────────────
    match audit.flush(&state_view) {
        Ok(n) if n > 0 => info!(count = n, "sync: audit flush uploaded events"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sync: audit flush failed"),
    }

    // ── 2. Approved-server list refresh ──────────────────────────────────────
    // Any successful server-list refresh may have added or removed backends,
    // so treat a successful non-empty response as a potential tool-set change.
    match registry.fetch_approved_servers() {
        Ok(resp) => {
            info!(
                count = resp.servers.len(),
                "sync: approved server list refreshed"
            );
            if !resp.servers.is_empty() {
                changed = true;
            }
        }
        Err(e) => {
            warn!(error = %e, "sync: failed to refresh approved server list");
        }
    }

    // ── 3. Skill lifecycle + version updates + update-check cache population ──
    let skill_ids = match state_view.list_installed_skill_ids() {
        Ok(ids) => ids,
        Err(e) => {
            warn!(error = %e, "sync: failed to read installed skills");
            // Continue to remaining steps even if skills can't be read.
            flush_ratings_and_scan(registry, &state_view, update_cache)?;
            return Ok(changed);
        }
    };

    if !skill_ids.is_empty() {
        // check_skill_updates performs lifecycle enforcement (uninstall unknown,
        // deactivate unpublished) AND voluntary version updates. It also calls
        // check_skill_status internally, so there is no separate lifecycle-only call.
        let policy_client = MockPolicyClient::new();
        match vectorhawkd_core::updater::check_skill_updates(&state_view, registry, &policy_client)
        {
            Ok(changes) => {
                if changes > 0 {
                    info!(changes, "sync: skill updates applied");
                    changed = true;
                }
            }
            Err(e) => {
                warn!(error = %e, "sync: skill update check failed");
            }
        }

        // Populate the update-check cache so MCP tool handlers can surface
        // "update available" hints without re-querying the registry per call.
        populate_update_check_cache(registry, &state_view, update_cache);
    }

    flush_ratings_and_scan(registry, &state_view, update_cache)?;

    // Record successful sync time so `vectorhawk doctor` can show it.
    let _ = state_view.record_sync_time();

    Ok(changed)
}

/// Populate the `UpdateCheckCache` by comparing each installed skill's active
/// version against the registry's latest version. Runs after `check_skill_updates`
/// so any just-applied updates are reflected.
///
/// Failures per skill are logged at DEBUG and skipped — a stale cache entry is
/// better than aborting the whole tick.
fn populate_update_check_cache(
    registry: &RegistryClient,
    state: &AppState,
    cache: &UpdateCheckCache,
) {
    use rusqlite::Connection;
    use semver::Version;
    use vectorhawkd_mcp::tools::UpdateCheckEntry;

    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "sync: cannot open DB for update-cache population");
            return;
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT skill_id, active_version FROM installed_skills WHERE current_status = 'active'",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "sync: failed to prepare installed-skills query for cache");
            return;
        }
    };

    let rows: Vec<(String, String)> = match stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .and_then(|iter| iter.collect::<rusqlite::Result<Vec<_>>>())
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "sync: failed to read active skills for cache");
            return;
        }
    };
    drop(stmt);
    drop(conn);

    let now = std::time::Instant::now();

    for (skill_id, active_version) in rows {
        let installed = match Version::parse(&active_version) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let latest_version = match registry.fetch_skill_detail(&skill_id) {
            Ok(detail) => detail.latest_version.and_then(|v| Version::parse(&v).ok()),
            Err(e) => {
                tracing::debug!(skill_id, error = %e, "sync: cache population: fetch detail failed");
                continue;
            }
        };

        let newer = latest_version.filter(|latest| latest > &installed);

        if let Ok(mut guard) = cache.lock() {
            guard.insert(
                skill_id,
                UpdateCheckEntry {
                    checked_at: now,
                    latest_version: newer,
                },
            );
        }
    }
}

/// Steps 4–6 of `run_sync_tick`: ratings flush, execution-stats flush, and
/// unmanaged server scan. Split out so the early-return path when skill_ids is
/// empty still runs these steps.
fn flush_ratings_and_scan(
    registry: &RegistryClient,
    state: &AppState,
    _update_cache: &UpdateCheckCache,
) -> Result<()> {
    use vectorhawkd_core::{
        audit::AuditEvent,
        ratings::{get_execution_stats, get_unsynced_ratings, mark_ratings_synced},
    };
    use vectorhawkd_mcp::setup::detect_unmanaged_servers;

    // ── 4. Ratings flush ──────────────────────────────────────────────────────
    match rusqlite::Connection::open(&state.db_path) {
        Ok(conn) => {
            match get_unsynced_ratings(&conn) {
                Ok(ratings) if !ratings.is_empty() => {
                    match registry.upload_skill_ratings(&ratings) {
                        Ok(()) => {
                            let ids: Vec<i64> = ratings.iter().map(|r| r.id).collect();
                            match mark_ratings_synced(&conn, &ids) {
                                Ok(()) => info!(count = ids.len(), "sync: skill ratings flushed"),
                                Err(e) => {
                                    warn!(error = %e, "sync: failed to mark ratings synced after upload")
                                }
                            }
                        }
                        Err(e) => warn!(error = %e, "sync: ratings upload failed"),
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "sync: failed to read unsynced ratings"),
            }

            // ── 5. Execution stats flush ──────────────────────────────────────
            match get_execution_stats(&conn) {
                Ok(stats) if !stats.is_empty() => match registry.upload_execution_stats(&stats) {
                    Ok(()) => info!(count = stats.len(), "sync: execution stats flushed"),
                    Err(e) => warn!(error = %e, "sync: execution stats upload failed"),
                },
                Ok(_) => {}
                Err(e) => warn!(error = %e, "sync: failed to read execution stats"),
            }
        }
        Err(e) => {
            warn!(error = %e, "sync: failed to open DB for ratings/stats flush");
        }
    }

    // ── 6. Unmanaged MCP server scan ──────────────────────────────────────────
    // Scan AI client config files for non-vectorhawk MCP servers and buffer an
    // audit event for each one. IT admins can use this stream to detect shadow
    // MCP installations that bypass governance.
    let unmanaged = detect_unmanaged_servers();
    for server in &unmanaged {
        let event = AuditEvent {
            event_type: "unmanaged_server_detected".to_string(),
            payload: serde_json::json!({
                "server_name": server.server_name,
                "config_path": server.config_path,
                "client_name": server.client_name,
            }),
        };
        // Write directly to SQLite (we're already in spawn_blocking).
        let conn = match rusqlite::Connection::open(&state.db_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, server = %server.server_name, "sync: cannot open DB for unmanaged audit event");
                continue;
            }
        };
        let payload_json = serde_json::to_string(&event.payload).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = conn.execute(
            "INSERT INTO audit_events (event_type, payload, created_at, uploaded) VALUES (?1, ?2, ?3, 0)",
            rusqlite::params![event.event_type, payload_json, now],
        ) {
            warn!(error = %e, server = %server.server_name, "sync: failed to buffer unmanaged audit event");
        }
    }
    if !unmanaged.is_empty() {
        info!(
            count = unmanaged.len(),
            "sync: unmanaged MCP servers detected — audit events buffered"
        );
    }

    Ok(())
}

/// Load any rows from `mcp_installations` (via SQLite) and register each as a
/// live backend in the aggregator at daemon startup.
///
/// This is the startup path that makes previously-installed MCP servers visible
/// to Claude Code immediately on daemon launch, without waiting for the next SSE
/// event. Failures are logged at WARN per entry and never abort startup — a
/// missing or malformed row is skipped rather than crashing the daemon.
///
/// Called once from `run_daemon` after `build_stub_registry()`.
fn load_managed_mcp_into_registry(
    state: &AppState,
    registry: &Arc<BackendRegistry>,
    list_changed_tx: tokio::sync::broadcast::Sender<()>,
) {
    let rows = match state.list_mcp_installs() {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "startup: failed to read mcp_installations — no managed MCP backends loaded");
            return;
        }
    };

    let mut loaded = 0usize;
    for row in &rows {
        match mcp_row_to_backend_entry(row) {
            Some(entry) => {
                let server_id = entry.server_id.clone();
                registry.register_backend(entry.clone());
                info!(server_id = %server_id, "startup: registered managed MCP backend");
                loaded += 1;
                spawn_tool_discovery(Arc::clone(registry), entry, list_changed_tx.clone());
            }
            None => {
                warn!(
                    mcp_server_id = %row.mcp_server_id,
                    mcp_server_name = %row.mcp_server_name,
                    "startup: skipping managed MCP backend — server_config missing command/url"
                );
            }
        }
    }

    if loaded > 0 {
        info!(
            count = loaded,
            "startup: managed MCP backends registered in aggregator (tool discovery running in background)"
        );
    }
}

/// Fetch a backend's tool list off-thread and re-register the entry with the
/// populated `tools` Vec. This keeps daemon startup fast (no blocking npx-pull
/// per backend) while making the tools visible to `tools/list` once each
/// backend has responded.
///
/// Re-registration via `register_backend` overwrites the existing entry by
/// server_id, so the empty-tools placeholder is replaced atomically.
pub fn spawn_tool_discovery(
    registry: Arc<BackendRegistry>,
    entry: BackendEntry,
    list_changed_tx: tokio::sync::broadcast::Sender<()>,
) {
    // Skip when no Tokio runtime is in scope (synchronous tests call
    // load_managed_mcp_into_registry directly without a runtime).
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let server_id = entry.server_id.clone();
        let name = entry.name.clone();
        let result = match &entry.transport {
            BackendTransport::Stdio {
                command, args, env, ..
            } => BackendRegistry::fetch_tools_stdio(command, args, env).await,
            BackendTransport::Http { url, auth_token } => {
                registry.fetch_tools_http(url, auth_token.as_deref()).await
            }
            BackendTransport::Stub => return,
        };

        match result {
            Ok(tools) if !tools.is_empty() => {
                let mut new_entry = entry.clone();
                new_entry.tools = tools;
                let count = new_entry.tools.len();
                registry.register_backend(new_entry);
                info!(
                    server_id = %server_id,
                    name = %name,
                    tools = count,
                    "tool discovery completed — backend re-registered with tools"
                );
                // Fire a second tools/list_changed so AI clients refresh now
                // that the backend actually exposes tools. The first
                // notification fired immediately on install (when the entry
                // was still tools-empty), so without this clients see a
                // stale empty list and never re-fetch.
                let _ = list_changed_tx.send(());
            }
            Ok(_) => {
                warn!(server_id = %server_id, name = %name, "tool discovery returned zero tools");
            }
            Err(e) => {
                warn!(
                    server_id = %server_id,
                    name = %name,
                    error = %e,
                    "tool discovery failed — backend will remain visible but exposing no tools"
                );
            }
        }
    });
}

/// Translate a `McpInstallRow` from SQLite into a `BackendEntry` for the aggregator.
///
/// Translation rules for `server_config` (a JSON blob):
/// - `{"command": "...", "args": [...], "env": {...}}` → `BackendTransport::Stdio`
/// - `{"url": "..."}` → `BackendTransport::Http` (no auth_token in this pass)
/// - `null` or neither key present → returns `None` (row is skipped with a warning)
///
/// The `server_id` in the aggregator is the `mcp_server_id` UUID string so that
/// live install/deactivate events (which carry that UUID) can find the entry with
/// a direct key lookup.
///
/// Tools list starts empty. The AI client must call `tools/list` against the
/// vectorhawk MCP server to trigger discovery — that call fans out to all
/// registered backends and caches their tool definitions. Subsequent
/// `tools/call` invocations dispatch against the cached list; calling
/// `tools/call` BEFORE any `tools/list` returns an error from
/// `aggregator.rs::dispatch` (tool name not in cache). This matches the MCP
/// protocol contract — clients must list before they call.
/// Convert an MCP server display name into a kebab-case slug suitable for the
/// aggregator's `server_id` key. Tool names surfaced to AI clients are
/// namespaced as `<server_id>__<tool>`, so a readable slug gives the user
/// `everything__echo` rather than `e9d7cc5a-a676-4682-9c5d-68a4e389f368__echo`.
///
/// Lowercases, replaces every non-alphanumeric run with a single `-`, trims
/// leading/trailing `-`. Empty input collapses to `"server"` to keep
/// `register_backend` from being called with an empty key.
pub fn mcp_server_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "server".to_string()
    } else {
        trimmed
    }
}

pub fn mcp_row_to_backend_entry(
    row: &vectorhawkd_core::state::McpInstallRow,
) -> Option<BackendEntry> {
    let config: serde_json::Value = match &row.server_config {
        Some(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    mcp_server_id = %row.mcp_server_id,
                    error = %e,
                    "mcp_row_to_backend_entry: invalid server_config JSON — skipping"
                );
                return None;
            }
        },
        None => {
            return None;
        }
    };

    let transport = if let (Some(command), Some(args_val)) = (
        config.get("command").and_then(|v| v.as_str()),
        config.get("args"),
    ) {
        let args: Vec<String> = args_val
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let env: std::collections::HashMap<String, String> = config
            .get("env")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        BackendTransport::Stdio {
            command: command.to_string(),
            args,
            env,
            process: Arc::new(std::sync::Mutex::new(None)),
        }
    } else if let Some(url) = config.get("url").and_then(|v| v.as_str()) {
        BackendTransport::Http {
            url: url.to_string(),
            auth_token: None,
        }
    } else {
        return None;
    };

    Some(BackendEntry {
        server_id: mcp_server_slug(&row.mcp_server_name),
        name: row.mcp_server_name.clone(),
        transport,
        tools: vec![],
        tool_visibility: ToolVisibility::All,
        priority: 50,
        consecutive_errors: 0,
        unhealthy: false,
    })
}

/// Construct the M0 stub `BackendRegistry`.
///
/// Construct an empty `BackendRegistry`. Live backends are loaded from
/// `managed-mcp.json` via `load_managed_mcp_into_registry`. The M0 stub
/// backend (echo/ping) was removed once real backend registration shipped.
pub fn build_stub_registry() -> BackendRegistry {
    BackendRegistry::new()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "refresh_loop_tests.rs"]
mod refresh_loop_tests;

#[cfg(test)]
#[path = "sync_tick_tests.rs"]
mod sync_tick_tests;

#[cfg(test)]
mod slug_tests {
    use super::mcp_server_slug;

    #[test]
    fn slug_lowercases_and_hyphenates() {
        assert_eq!(mcp_server_slug("Filesystem"), "filesystem");
        assert_eq!(mcp_server_slug("GitHub MCP"), "github-mcp");
        assert_eq!(
            mcp_server_slug("GW2c-Runner Default"),
            "gw2c-runner-default"
        );
    }

    #[test]
    fn slug_collapses_runs_and_trims() {
        assert_eq!(mcp_server_slug("  Foo   Bar  "), "foo-bar");
        assert_eq!(mcp_server_slug("---weird---"), "weird");
        assert_eq!(mcp_server_slug("a/b/c"), "a-b-c");
    }

    #[test]
    fn slug_strips_unicode_to_alphanumeric() {
        assert_eq!(mcp_server_slug("Atlassian™"), "atlassian");
        assert_eq!(mcp_server_slug("notion (alpha)"), "notion-alpha");
    }

    #[test]
    fn slug_empty_collapses_to_server() {
        assert_eq!(mcp_server_slug(""), "server");
        assert_eq!(mcp_server_slug("---"), "server");
    }
}
