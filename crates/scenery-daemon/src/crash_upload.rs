//! Upload queued crash records (see `scenery_core::crash`).
//!
//! Called once when the sync loop starts (not after the first 5-minute tick,
//! so a daemon that crash-loops still gets its reports out) and again on every
//! tick while records remain queued. Blocking I/O: run via `spawn_blocking`.

use std::sync::{atomic::AtomicBool, Arc};

use camino::Utf8PathBuf;
use scenery_core::{crash, registry::RegistryClient, state::AppState};
use tracing::{debug, info, warn};

pub(crate) fn upload_queued_crash_reports(
    registry: &RegistryClient,
    db_path: &Utf8PathBuf,
    root_dir: &Utf8PathBuf,
    block_third_party: &Arc<AtomicBool>,
) {
    if crash::list_pending(root_dir).is_empty() {
        return;
    }
    let state_view = AppState {
        root_dir: root_dir.clone(),
        db_path: db_path.clone(),
        block_third_party_inference: Arc::clone(block_third_party),
    };
    // Same guard as the sync tick: never send the device token to a registry
    // other than the one this device was paired against.
    let origin = scenery_core::registry_url::validate_origin(&registry.base_url)
        .unwrap_or_else(|_| registry.base_url.trim_end_matches('/').to_string());
    if !crate::registry_origin_matches(&state_view, &origin) {
        return;
    }
    crate::install_registry_credentials(&state_view, registry);
    match crash::upload_pending(root_dir, registry) {
        Ok(0) => {}
        Ok(n) => info!(count = n, "uploaded crash reports"),
        Err(e) => debug!(error = %e, "crash report upload deferred; will retry"),
    }
}

/// Log, at warn level, that the previous run ended without a clean shutdown.
pub(crate) fn note_previous_run(record: Option<&crash::CrashRecord>) {
    if let Some(rec) = record {
        warn!(
            report_id = %rec.report_id,
            previous_version = %rec.runner_version,
            "previous daemon run did not shut down cleanly; queued an unclean-shutdown report"
        );
    }
}

/// The accept loop has ended for a graceful reason (SIGTERM, SIGINT, binary
/// replaced): this run no longer counts as crash-prone.
pub(crate) fn mark_graceful_shutdown(root_dir: &Utf8PathBuf) {
    crash::clear_run_marker(root_dir);
}

#[cfg(test)]
#[path = "crash_lifecycle_tests.rs"]
mod tests;
