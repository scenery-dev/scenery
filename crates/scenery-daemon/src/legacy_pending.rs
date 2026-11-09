//! Rows a 2.2.0 runner left in `approval_state='pending'`.
//!
//! 2.2.0 held verified stdio servers until `scenery approve`. 2.2.1 removed
//! that prompt (design 2026-11-08 "Decision change"), so on startup every such
//! row is activated: the stored state is cleared and the backend is told
//! `installed`, which replaces `pending_approval`. Nothing stays pending.

use crate::sync::reconciler::report_mcp_installation_status;
use scenery_core::state::{AppState, McpInstallRow};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;
use uuid::Uuid;

/// `approval_state` value a 2.2.0 runner wrote for a held server.
pub const STATE_PENDING: &str = "pending";

type ApprovalRows = HashMap<String, (Option<String>, Option<String>, Option<String>)>;

/// Whether the stored approval bookkeeping marks `server_id` as pending.
pub fn is_legacy_pending(rows: &ApprovalRows, server_id: &str) -> bool {
    rows.get(server_id).and_then(|(_, s, _)| s.as_deref()) == Some(STATE_PENDING)
}

/// Clear the pending state of `row` (it now runs) and report `installed`.
pub fn activate_at_startup(state: &Arc<AppState>, registry_url: &str, row: &McpInstallRow) {
    clear_state(state, row);
    spawn_report(state, registry_url, row, "installed", None);
}

/// A pending row that the gate now refuses (denylisted env key): clear the
/// pending state and report the refusal, so it is not stuck pending.
pub fn refuse_at_startup(
    state: &Arc<AppState>,
    registry_url: &str,
    row: &McpInstallRow,
    reason: &str,
) {
    clear_state(state, row);
    spawn_report(state, registry_url, row, "error", Some(reason.to_string()));
}

fn clear_state(state: &Arc<AppState>, row: &McpInstallRow) {
    let hash = crate::launch_gate::stdio_definition(row).map(|d| d.approval_hash());
    if let Err(e) = state.set_mcp_approval(&row.mcp_server_id, hash.as_deref(), None) {
        warn!(error = %e, "startup: failed to clear legacy pending state");
    }
}

fn spawn_report(
    state: &Arc<AppState>,
    registry_url: &str,
    row: &McpInstallRow,
    status: &'static str,
    message: Option<String>,
) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Ok(iid) = Uuid::parse_str(&row.installation_id) else {
        return;
    };
    let st = Arc::clone(state);
    let url = registry_url.to_string();
    tokio::spawn(async move {
        report_mcp_installation_status(iid, status, message.as_deref(), &url, &st).await;
    });
}
