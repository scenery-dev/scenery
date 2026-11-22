//! Change-only reporting of shadow MCP servers (`unmanaged_server_detected`).
//!
//! The registry sync tick scans every AI client config for MCP servers that
//! Scenery does not manage. It used to buffer one audit event per server on
//! EVERY tick (every 5 minutes), so an unchanged machine re-sent the same
//! events forever: an upload per tick and a flooded admin Activity Log. The
//! backend only displays these events (no consumer expects a periodic
//! re-report; the discovery upload already refreshes each MCP item's
//! `last_seen_at`), so an event is now emitted when a server first appears,
//! and again only if it disappeared and came back.
//!
//! The set already reported is persisted in `sync_state` so a daemon restart
//! does not re-emit everything.

use scenery_core::state::AppState;
use scenery_mcp::setup::UnmanagedServer;
use std::collections::BTreeSet;
use tracing::warn;

/// `sync_state` key: JSON array of the identity keys last seen.
pub(crate) const REPORTED_KEY: &str = "unmanaged_mcp_reported";

fn identity(s: &UnmanagedServer) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        s.client_name, s.config_path, s.server_name
    )
}

fn load_reported(state: &AppState) -> BTreeSet<String> {
    state
        .get_sync_state(REPORTED_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<BTreeSet<String>>(&j).ok())
        .unwrap_or_default()
}

/// The servers in `current` that were not in the set reported last time.
/// Records `current` as the new reported set (only when it changed, so an
/// unchanged tick writes nothing).
pub(crate) fn newly_seen<'a>(
    state: &AppState,
    current: &'a [UnmanagedServer],
) -> Vec<&'a UnmanagedServer> {
    let before = load_reported(state);
    let now: BTreeSet<String> = current.iter().map(identity).collect();
    let fresh: Vec<&UnmanagedServer> = current
        .iter()
        .filter(|s| !before.contains(&identity(s)))
        .collect();
    if now != before {
        let json = serde_json::to_string(&now).unwrap_or_else(|_| "[]".into());
        if let Err(e) = state.set_sync_state(REPORTED_KEY, &json) {
            warn!(error = %e, "sync: could not record reported unmanaged MCP servers");
        }
    }
    fresh
}

#[cfg(test)]
#[path = "unmanaged_audit_tests.rs"]
mod tests;
