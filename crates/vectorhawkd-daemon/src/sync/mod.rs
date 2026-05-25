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
use tokio::sync::{broadcast, mpsc};
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
}

/// Spawn the SSE client and reconciler tasks.
///
/// Returns a [`ReconcilerHandle`] that the daemon's sync loop can use to query
/// reconciler status (for `doctor` output).  The two spawned tasks run
/// independently until the process exits or the SSE connection is torn down
/// via token invalidation.
pub fn run(
    config: SyncConfig,
    state: Arc<AppState>,
    list_changed_tx: broadcast::Sender<()>,
    backend_registry: Arc<BackendRegistry>,
) -> Result<ReconcilerHandle> {
    let (event_tx, event_rx) = mpsc::channel::<SyncEvent>(64);

    info!(
        registry_url = %config.registry_url,
        device_id = %config.device_id,
        "sync subsystem starting"
    );

    // Spawn SSE client — feeds events into the channel.
    let sse_config = config.clone();
    let sse_state = Arc::clone(&state);
    tokio::spawn(sse_client::run(sse_config, sse_state, event_tx));

    // Spawn reconciler — consumes events and converges local state.
    let handle = reconciler::spawn(
        event_rx,
        state,
        list_changed_tx,
        backend_registry,
        config.pusher,
    );

    Ok(handle)
}
