//! Local-approval flow around the launch gate: holding a server as pending,
//! the one-time OS notification, and re-evaluating every stored server when
//! `scenery approve` / `approvals revoke` sends `approvals/changed`.

use crate::launch_gate::{self, Gate};
use crate::managed_paths::ManagedPathsPusher;
use crate::sync::reconciler::{
    report_mcp_installation_status_with_hash, write_managed_mcp_json,
};
use scenery_core::approvals::StdioDefinition;
use scenery_core::state::{AppState, McpInstallRow};
use scenery_mcp::aggregator::BackendRegistry;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};
use uuid::Uuid;

/// `approval_state` values stored on `mcp_installations`.
pub const STATE_PENDING: &str = "pending";
pub const STATE_APPROVED: &str = "approved";

/// Reduce a backend-supplied name to something safe to print in a
/// notification (it is attacker-influenced text).
fn sanitize_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(40)
        .collect()
}

/// Send a one-time OS notification for `hash` ("waiting for approval").
/// Best effort: any failure is ignored. The command line is deliberately not
/// included, and text goes through argv, never through a script string.
pub fn notify_pending_once(state: &AppState, hash: &str, name: &str) {
    let key = format!("approval_notified:{hash}");
    if matches!(state.get_sync_state(&key), Ok(Some(_))) {
        return;
    }
    let _ = state.set_sync_state(&key, "1");
    let body = format!(
        "{} is waiting for approval. Run `scenery approve` in a terminal.",
        sanitize_name(name)
    );
    send_os_notification("Scenery", &body);
}

#[cfg(target_os = "macos")]
fn send_os_notification(title: &str, body: &str) {
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 1 of argv) with title (item 2 of argv)",
            "-e",
            "end run",
            body,
            title,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(target_os = "linux")]
fn send_os_notification(title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--", title, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn send_os_notification(_title: &str, _body: &str) {}

/// PATCH `pending_approval` for a held server: `error_message` carries the
/// display command (so the portal device page can show it) and the hash.
pub async fn report_pending(
    state: &Arc<AppState>,
    registry_url: &str,
    installation_id: Uuid,
    def: &StdioDefinition,
    hash: &str,
) {
    report_mcp_installation_status_with_hash(
        installation_id,
        "pending_approval",
        Some(&def.display()),
        Some(hash),
        registry_url,
        state,
    )
    .await;
}

/// Re-evaluate every stored MCP server against the current approvals.
/// Newly approved servers are registered, discovered, pushed and reported
/// `installed`; servers whose approval was revoked are torn down and put back
/// to pending. Returns how many servers changed.
pub async fn apply_approvals_changed(
    state: &Arc<AppState>,
    registry_url: &str,
    backend_registry: &Arc<BackendRegistry>,
    pusher: Option<&Arc<ManagedPathsPusher>>,
    list_changed_tx: &broadcast::Sender<()>,
) -> usize {
    let st = Arc::clone(state);
    let rows = tokio::task::spawn_blocking(move || {
        let rows = st.list_mcp_installs()?;
        let approval = st.mcp_approval_rows()?;
        anyhow::Ok((rows, approval))
    })
    .await;
    let (rows, approval) = match rows {
        Ok(Ok(v)) => v,
        _ => {
            warn!("approvals: cannot read mcp_installations");
            return 0;
        }
    };

    let mut changed = 0usize;
    for row in &rows {
        let prior = approval
            .get(&row.mcp_server_id)
            .and_then(|(_, s, _)| s.clone());
        let was_pending = prior.as_deref() == Some(STATE_PENDING);
        match launch_gate::check(state, row) {
            Gate::Allowed if was_pending => {
                activate(state, registry_url, backend_registry, pusher, list_changed_tx, row).await;
                changed += 1;
            }
            Gate::Pending(hash) if !was_pending => {
                hold_running(state, registry_url, backend_registry, pusher, row, &hash).await;
                changed += 1;
            }
            Gate::Allowed | Gate::Pending(_) | Gate::Refused(_) => {}
        }
    }
    if changed > 0 {
        let _ = list_changed_tx.send(());
    }
    changed
}

async fn activate(
    state: &Arc<AppState>,
    registry_url: &str,
    backend_registry: &Arc<BackendRegistry>,
    pusher: Option<&Arc<ManagedPathsPusher>>,
    list_changed_tx: &broadcast::Sender<()>,
    row: &McpInstallRow,
) {
    let hash = launch_gate::stdio_definition(row).map(|d| d.approval_hash());
    let st = Arc::clone(state);
    let id = row.mcp_server_id.clone();
    let h = hash.clone();
    let wrote = tokio::task::spawn_blocking(move || {
        st.set_mcp_approval(&id, h.as_deref(), Some(STATE_APPROVED))?;
        write_managed_mcp_json(&st)
    })
    .await;
    if !matches!(wrote, Ok(Ok(()))) {
        warn!(mcp_server_id = %row.mcp_server_id, "approvals: failed to record approval");
        return;
    }
    let slug = crate::mcp_server_slug(&row.mcp_server_name);
    if let Some(p) = pusher {
        if let Err(e) = p.push_mcp(&slug, Some(&row.installation_id), row.gateway_url.is_some()) {
            warn!(slug = %slug, error = %e, "approvals: push_mcp failed (non-fatal)");
        }
    }
    match crate::mcp_row_to_backend_entry(state, row) {
        crate::Launch::Ready(entry) => {
            backend_registry.register_backend(entry.clone());
            crate::spawn_tool_discovery(Arc::clone(backend_registry), entry, list_changed_tx.clone());
            info!(slug = %slug, "approvals: approved server registered");
        }
        other => warn!(slug = %slug, launch = ?other, "approvals: approved server has no runnable transport"),
    }
    if let Ok(iid) = Uuid::parse_str(&row.installation_id) {
        report_mcp_installation_status_with_hash(iid, "installed", None, None, registry_url, state)
            .await;
    }
}

async fn hold_running(
    state: &Arc<AppState>,
    registry_url: &str,
    backend_registry: &Arc<BackendRegistry>,
    pusher: Option<&Arc<ManagedPathsPusher>>,
    row: &McpInstallRow,
    hash: &str,
) {
    let slug = crate::mcp_server_slug(&row.mcp_server_name);
    // Stop anything already running for this server, and take it out of
    // the client's config until it is approved again.
    backend_registry.remove_backend(&slug);
    if let Some(p) = pusher {
        if let Err(e) = p.remove_mcp(&slug) {
            warn!(slug = %slug, error = %e, "approvals: remove_mcp failed");
        }
    }
    let st = Arc::clone(state);
    let id = row.mcp_server_id.clone();
    let h = hash.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        st.set_mcp_approval(&id, Some(&h), Some(STATE_PENDING))?;
        write_managed_mcp_json(&st)
    })
    .await;
    if let (Some(def), Ok(iid)) = (
        launch_gate::stdio_definition(row),
        Uuid::parse_str(&row.installation_id),
    ) {
        report_pending(state, registry_url, iid, &def, hash).await;
        notify_pending_once(state, hash, &row.mcp_server_name);
    }
}

/// Startup pass over stored rows: anything gated as pending is marked,
/// reported and notified; nothing is registered for it.
pub fn mark_pending_at_startup(state: &Arc<AppState>, registry_url: &str, row: &McpInstallRow, hash: &str) {
    if let Err(e) = state.set_mcp_approval(&row.mcp_server_id, Some(hash), Some(STATE_PENDING)) {
        warn!(error = %e, "startup: failed to mark server pending");
    }
    let (Some(def), Ok(iid)) = (
        launch_gate::stdio_definition(row),
        Uuid::parse_str(&row.installation_id),
    ) else {
        return;
    };
    notify_pending_once(state, hash, &row.mcp_server_name);
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let st = Arc::clone(state);
    let url = registry_url.to_string();
    let hash = hash.to_string();
    tokio::spawn(async move {
        report_pending(&st, &url, iid, &def, &hash).await;
    });
}

#[cfg(test)]
#[path = "approval_flow_tests.rs"]
mod tests;
