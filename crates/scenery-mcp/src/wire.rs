//! Daemon-side wiring of the user's AI clients.
//!
//! The daemon calls [`wire_clients_in`] once at start and then on a slow
//! poll. On pkg installs ([`WirePolicy::Enforce`]) the Scenery entry is kept
//! in place. On brew/curl installs ([`WirePolicy::FirstTimeOnly`]) the entry
//! is added once per client and a user's later removal is respected.
//!
//! A pass only stats and reads small config files and writes nothing when
//! every client already has its entry.

use std::path::Path;

use crate::setup::{
    claude_code_supports_http_oauth, detect_ai_clients_in, pick_free_callback_port,
    scenery_entry_present, write_claude_code_entry, write_mcp_entry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WirePolicy {
    /// Always restore a missing entry (pkg installs).
    Enforce,
    /// Add the entry once per client, then leave it alone if removed.
    FirstTimeOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireAction {
    /// Write the entry.
    Write,
    /// Entry already there; just remember the client was wired.
    MarkOnly,
    /// The user removed it and policy says to respect that.
    Skip,
}

/// Remembers which clients have been wired before.
pub trait WireMemory {
    fn was_wired(&self, client: &str) -> bool;
    fn mark_wired(&self, client: &str);
    /// Whether the one-time upgrade baseline has run.
    fn baseline_done(&self) -> bool;
    fn set_baseline_done(&self);
}

/// Meta key recording that the upgrade baseline has run.
const BASELINE_KEY: &str = "mcp_wire_baseline";

/// Whether this pass is the one-time upgrade baseline: `FirstTimeOnly`
/// installs that predate wiring memory must not re-add entries their users
/// removed earlier, so the first pass only records what is there. `Enforce`
/// (pkg) has no such history and ignores it.
pub fn baseline_applies(policy: WirePolicy, baseline_done: bool) -> bool {
    policy == WirePolicy::FirstTimeOnly && !baseline_done
}

/// Pure decision table for one client.
pub fn decide_wire(policy: WirePolicy, entry_present: bool, was_wired: bool) -> WireAction {
    match (entry_present, was_wired, policy) {
        (true, _, _) => WireAction::MarkOnly,
        (false, false, _) => WireAction::Write,
        (false, true, WirePolicy::Enforce) => WireAction::Write,
        (false, true, WirePolicy::FirstTimeOnly) => WireAction::Skip,
    }
}

fn memory_key(client: &str) -> String {
    format!("mcp_wired:{client}")
}

#[cfg(feature = "daemon")]
impl WireMemory for scenery_core::state::AppState {
    fn was_wired(&self, client: &str) -> bool {
        matches!(self.meta_get(&memory_key(client)), Ok(Some(v)) if v == "1")
    }

    fn mark_wired(&self, client: &str) {
        if let Err(e) = self.meta_set(&memory_key(client), "1") {
            tracing::warn!(client, error = %e, "wire: could not record wired client");
        }
    }

    fn baseline_done(&self) -> bool {
        matches!(self.meta_get(BASELINE_KEY), Ok(Some(v)) if v == "1")
    }

    fn set_baseline_done(&self) {
        if let Err(e) = self.meta_set(BASELINE_KEY, "1") {
            tracing::warn!(error = %e, "wire: could not record upgrade baseline");
        }
    }
}

/// One wiring pass over every detected AI client. Returns the names of the
/// clients whose config was written.
///
/// `claude_http` is the daemon's local MCP HTTP `(port, local key)`; Claude
/// Code gets the HTTP+OAuth entry when it is `Some` and the installed Claude
/// Code supports it, otherwise the stdio entry. Per-client failures are
/// logged and skipped.
pub fn wire_clients_in(
    home: &Path,
    system_root: &Path,
    policy: WirePolicy,
    mem: &dyn WireMemory,
    claude_http: Option<(u16, String)>,
) -> Vec<String> {
    let mut written = Vec::new();
    let baseline_pass = baseline_applies(policy, mem.baseline_done());
    for client in detect_ai_clients_in(home, system_root) {
        if baseline_pass {
            if !mem.was_wired(&client.name) {
                mem.mark_wired(&client.name);
            }
            continue;
        }
        let present = scenery_entry_present(&client.config_path, &client.mcp_key, client.format);
        let was_wired = mem.was_wired(&client.name);
        match decide_wire(policy, present, was_wired) {
            WireAction::Skip => {}
            WireAction::MarkOnly => {
                if !was_wired {
                    mem.mark_wired(&client.name);
                }
            }
            WireAction::Write => {
                let result = if client.name == "Claude Code" {
                    write_claude_code_entry(
                        &client,
                        claude_http.clone(),
                        claude_code_supports_http_oauth,
                        pick_free_callback_port,
                    )
                    .map(|_| ())
                } else {
                    write_mcp_entry(&client)
                };
                match result {
                    Ok(()) => {
                        mem.mark_wired(&client.name);
                        written.push(client.name.clone());
                    }
                    Err(e) => tracing::warn!(
                        client = %client.name,
                        path = %client.config_path.display(),
                        error = %e,
                        "wire: failed to write scenery MCP entry"
                    ),
                }
            }
        }
    }
    if baseline_pass {
        mem.set_baseline_done();
    }
    written
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
