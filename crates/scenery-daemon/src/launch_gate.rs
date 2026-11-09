//! The launch gate. A backend-pushed stdio definition whose signed statement
//! verified is allowed to run on install, with no local approval step (the
//! device prompt was removed in 2.2.1; design 2026-11-08 "Decision change").
//! The gate still refuses definitions that set a denylisted env key
//! (`LD_*`, `DYLD_*`, `NODE_OPTIONS`, ...).
//!
//! Every path that turns a stored MCP row into something runnable goes
//! through [`check`], via `mcp_row_to_backend_entry`: daemon startup and a
//! live install.

use scenery_core::approvals::{self, StdioDefinition};
use scenery_core::state::McpInstallRow;

/// Outcome of gating one stored MCP row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Not a local command, or a verified stdio definition: may run.
    Allowed,
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
pub fn check(row: &McpInstallRow) -> Gate {
    let Some(def) = stdio_definition(row) else {
        return Gate::Allowed;
    };
    if let Some(bad) = approvals::first_denied_env_key(&def.env_keys) {
        return Gate::Refused(format!("env key {bad} not allowed"));
    }
    Gate::Allowed
}

#[cfg(test)]
#[path = "launch_gate_tests.rs"]
mod tests;
