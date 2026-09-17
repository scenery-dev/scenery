//! Background sync subsystem: SSE client + reconciler.
//!
//! The public entry point is [`run`], which spawns two cooperating async tasks:
//!
//! 1. [`sse_client`] — opens a persistent SSE connection to the backend and
//!    pushes [`SyncEvent`]s onto an `mpsc` channel.
//! 2. [`reconciler`] — consumes events and converges local skill state.
//!
//! The subsystem is optional: if `registry_url` is absent or the daemon is
//! operating in offline mode, `run` immediately returns `Ok(())`.

pub mod reconciler;
pub mod sse_client;

pub use reconciler::ReconcilerHandle;
pub use sse_client::SyncEvent;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::info;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::aggregator::BackendRegistry;

use crate::managed_paths::ManagedPathsPusher;

/// Configuration for the sync subsystem.
#[derive(Clone)]
pub struct SyncConfig {
    /// Registry base URL (e.g. `https://app.vectorhawk.ai`).
    pub registry_url: String,
    /// Bearer token for the SSE connection.  Refreshed on 401.
    pub token: String,
    /// Stable device UUID persisted in SQLite `sync_state`.
    pub device_id: String,
    /// Last SSE event ID received (for resume on reconnect).
    pub last_event_id: Option<String>,
    /// F2: pusher for writing installs into Claude Code's native directories.
    /// `None` when `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` is set.
    pub pusher: Option<Arc<ManagedPathsPusher>>,
    /// The SSE client's *current* access token, live-updated.
    ///
    /// Seeded with `token` at spawn time. `sse_client::run`'s internal
    /// 401-triggered `try_refresh_token` writes the newly-rotated access
    /// token here (in addition to persisting it via `save_tokens`) so that
    /// anything holding a clone of this handle — namely
    /// [`crate::SyncController::ensure_started`]'s stored [`crate::RunningSync`]
    /// — can read the token the connection is ACTUALLY using right now,
    /// rather than the value frozen at spawn. Without this, a silent internal
    /// refresh leaves the on-disk token ahead of what `ensure_started`
    /// believes the connection is using, and the next legitimate
    /// `auth login`/`auth pair` call sees a spurious mismatch and restarts an
    /// already-healthy connection for no reason.
    pub live_token: Arc<tokio::sync::RwLock<String>>,
}

/// Spawn the SSE client and reconciler tasks.
///
/// Returns a [`ReconcilerHandle`] that the daemon's sync loop can use to query
/// reconciler status (for `doctor` output), a clone of the event channel
/// sender, an [`tokio::task::AbortHandle`] for the spawned SSE-client task,
/// and a `watch::Receiver<bool>` that reports whether the SSE client is
/// *currently* connected. The sender lets the periodic sync tick
/// (`run_sync_tick`) feed a polled `GET /api/sync/snapshot` result into the
/// *same* reconciler that consumes live SSE events — a safety net for a delta
/// dropped while the SSE connection stays healthy. The abort handle lets
/// [`crate::SyncController::ensure_started`] cancel a stale SSE connection
/// (started with credentials that have since been superseded by a fresh
/// `auth login`/`auth pair`) rather than leaving it running forever alongside
/// a freshly-started replacement. The connected-receiver lets
/// `ensure_started` distinguish "the SSE task was spawned" from "the SSE
/// connection is actually up" — spawning a task always succeeds, so
/// `is_some()` on this function's `Ok` result alone cannot tell a caller
/// whether sync is genuinely running (see `ensure_started`'s doc comment).
/// The two spawned tasks otherwise run independently until the process exits,
/// the SSE connection is torn down via token invalidation, or
/// `SyncController` aborts them for a credential-aware restart.
pub fn run(
    config: SyncConfig,
    state: Arc<AppState>,
    list_changed_tx: broadcast::Sender<()>,
    backend_registry: Arc<BackendRegistry>,
) -> Result<(
    ReconcilerHandle,
    mpsc::Sender<SyncEvent>,
    tokio::task::AbortHandle,
    watch::Receiver<bool>,
)> {
    let (event_tx, event_rx) = mpsc::channel::<SyncEvent>(64);
    let (connected_tx, connected_rx) = watch::channel(false);

    info!(
        registry_url = %config.registry_url,
        device_id = %config.device_id,
        "sync subsystem starting"
    );

    // Spawn SSE client — feeds events into the channel.
    let sse_config = config.clone();
    let sse_state = Arc::clone(&state);
    let sse_tx = event_tx.clone();
    let sse_join = tokio::spawn(sse_client::run(sse_config, sse_state, sse_tx, connected_tx));
    let sse_abort = sse_join.abort_handle();

    // Spawn reconciler — consumes events and converges local state.
    let handle = reconciler::spawn(
        event_rx,
        state,
        list_changed_tx,
        backend_registry,
        config.pusher,
    );

    Ok((handle, event_tx, sse_abort, connected_rx))
}
