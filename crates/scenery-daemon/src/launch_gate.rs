//! The launch gate: no backend-pushed stdio definition is registered,
//! spawned, discovered or pushed to a client until the user approved its
//! exact `(command, args, env keys)` on this machine (design 2026-11-08 §3).
//!
//! Every path that turns a stored MCP row into something runnable goes
//! through [`check`], via `mcp_row_to_backend_entry`: daemon startup, a live
//! install, and the post-approval re-registration.

use scenery_core::approvals::{self, StdioDefinition};
use scenery_core::state::{AppState, McpInstallRow};
use tracing::warn;

/// Outcome of gating one stored MCP row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Not a local command, or already approved by the user.
    Allowed,
    /// A stdio definition the user has not approved yet (approval hash).
    Pending(String),
    /// Never allowed (denylisted env key); the reason is user-facing.
    Refused(String),
}

/// The stdio definition of `row`, when its config spawns a local command.
/// Mirrors the translator's rule: `command` and `args` both present, and the
/// row is not gateway-brokered.
pub fn stdio_definition(row: &McpInstallRow) -> Option<StdioDefinition> {
    if row.gateway_url.as_deref().is_some_and(|u| !u.is_empty()) {
        return None;
    }
    let cfg: serde_json::Value = serde_json::from_str(row.server_config.as_deref()?).ok()?;
    cfg.get("args")?;
    approvals::definition_from_config(&cfg)
}

/// Gate one row.
pub fn check(state: &AppState, row: &McpInstallRow) -> Gate {
    let Some(def) = stdio_definition(row) else {
        return Gate::Allowed;
    };
    if let Some(bad) = approvals::first_denied_env_key(&def.env_keys) {
        return Gate::Refused(format!("env key {bad} not allowed"));
    }
    let hash = def.approval_hash();
    match approvals::is_approved(state, &hash) {
        Ok(true) => Gate::Allowed,
        Ok(false) => Gate::Pending(hash),
        // Fail closed: if approvals cannot be read, do not run anything.
        Err(e) => {
            warn!(error = %e, "launch gate: cannot read approvals; holding server as pending");
            Gate::Pending(hash)
        }
    }
}

#[cfg(test)]
#[path = "launch_gate_tests.rs"]
mod tests;
