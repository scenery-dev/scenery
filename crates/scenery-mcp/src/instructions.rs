//! Dynamic MCP `initialize` instructions builder.
//!
//! When the runner is in *managed mode* (a valid `managed.json` is present),
//! the instructions block names the org and uses stronger governance language.
//! In individual-developer mode, generic copy is returned.
//!
//! # Must match `tools/list`
//!
//! The management tools (`scenery_list`, `scenery_search`,
//! `scenery_mcp_catalog`, `scenery_mcp_request`) are hidden from
//! `tools/list` by default — see `tools::build_tool_list`'s doc comment —
//! and only surface when `SCENERY_EXPOSE_BUILTIN_TOOLS=1` and/or the user
//! is logged in. Naming one of them here when it isn't actually exposed
//! sends the AI client looking for a tool that doesn't exist. Every mention
//! below is therefore gated on `exposed_tool_names`, which callers must
//! build from the exact same tool list `tools/list` returns for this
//! session (see `backend.rs`'s `initialize` impl).

use scenery_core::managed::ManagedConfig;

/// Build the MCP `initialize` `instructions` string.
///
/// `mode_label` is appended verbatim and lets the caller distinguish the
/// daemon ("daemon") vs the embedded fallback path ("in-process fallback")
/// without repeating phrasing in two places.
///
/// `logged_in` should be `true` when the user has valid auth tokens for the
/// configured registry. When `false` and a registry URL is present, a nudge
/// to call `scenery_login` is prepended so Claude surfaces it immediately.
///
/// `exposed_tool_names` is the exact set of tool names `tools/list` returns
/// for this session (e.g. from `tools::build_tool_list`, mapped to just the
/// names). Any built-in tool this function would otherwise mention is
/// skipped unless its name is present here — see this module's doc comment.
pub fn build_instructions(
    managed: Option<&ManagedConfig>,
    mode_label: &str,
    logged_in: bool,
    exposed_tool_names: &[String],
) -> String {
    let mode_suffix = match mode_label {
        "" => String::new(),
        s => format!(" ({s})"),
    };

    let has_tool = |name: &str| exposed_tool_names.iter().any(|n| n == name);

    // Only nudge toward `scenery_login` when it's actually exposed —
    // `tools::build_tool_list` omits it whenever no registry URL is
    // configured at all, in which case there is nothing to log into and
    // naming the tool here would be exactly the bug this module exists to
    // prevent.
    let login_nudge = if !logged_in && has_tool("scenery_login") {
        "⚠️ You are not logged in to the Scenery registry. \
         Call `scenery_login` to authenticate — this unlocks skill search, \
         install, publish, and governance features. "
    } else {
        ""
    };

    let discovery_hint = build_discovery_hint(exposed_tool_names);

    let Some(managed) = managed else {
        return format!(
            "{login_nudge}Scenery runner — governed AI platform{mode_suffix}.{discovery_hint}"
        )
        .trim()
        .to_string();
    };

    let mut out = login_nudge.to_string();

    match managed.org.as_deref() {
        Some(org) => out.push_str(&format!("Scenery runner — managed by {org}{mode_suffix}. ")),
        None => out.push_str(&format!(
            "Scenery runner — managed deployment{mode_suffix}. "
        )),
    }

    if managed.governance_message_enabled {
        match managed.governance_message.as_deref() {
            Some(custom) => {
                out.push_str(custom);
                out.push(' ');
            }
            None if has_tool("scenery_mcp_request") => out.push_str(
                "Tool installations require approval. Use scenery_mcp_request \
                 to request a new MCP server through governance; direct install \
                 via /mcp bypasses controls. ",
            ),
            None => out.push_str(
                "Tool installations require approval; direct install via /mcp \
                 bypasses controls. ",
            ),
        }
    }

    out.push_str(&discovery_hint);
    out.trim().to_string()
}

/// Build the trailing "use scenery_list / _search / _mcp_catalog"
/// sentence, including only the clauses for tools actually present in
/// `exposed_tool_names`. Returns an empty string (not even a leading space)
/// when none of the three are exposed, so callers can append it unconditionally.
fn build_discovery_hint(exposed_tool_names: &[String]) -> String {
    let has_tool = |name: &str| exposed_tool_names.iter().any(|n| n == name);

    let mut clauses: Vec<&str> = Vec::new();
    if has_tool("scenery_list") {
        clauses.push("scenery_list to show installed skills");
    }
    if has_tool("scenery_search") {
        clauses.push("scenery_search to find more");
    }
    if has_tool("scenery_mcp_catalog") {
        clauses.push("scenery_mcp_catalog to browse approved MCP servers");
    }

    if clauses.is_empty() {
        return String::new();
    }
    format!(" Use {}.", clauses.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(org: Option<&str>, custom_msg: Option<&str>, msg_enabled: bool) -> ManagedConfig {
        ManagedConfig {
            managed: true,
            org: org.map(str::to_string),
            registry_url: None,
            api_key: None,
            allow_user_installs: true,
            governance_message: custom_msg.map(str::to_string),
            governance_message_enabled: msg_enabled,
            ollama_url: None,
            ollama_model: None,
        }
    }

    fn names(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Every built-in tool this module ever mentions, exposed — the
    /// `SCENERY_EXPOSE_BUILTIN_TOOLS=1` + logged-in surface. Tests about
    /// other concerns (org naming, mode label, login nudge) use this so
    /// they aren't incidentally testing the gating behaviour too.
    fn all_tools() -> Vec<String> {
        names(&[
            "scenery_list",
            "scenery_search",
            "scenery_mcp_catalog",
            "scenery_mcp_request",
            "scenery_login",
        ])
    }

    /// The real-world default surface: builtins hidden, nothing exposed
    /// beyond installed skills (irrelevant to instructions text) and
    /// possibly `scenery_login`.
    fn no_builtins() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn unmanaged_returns_generic_copy() {
        let s = build_instructions(None, "daemon", true, &all_tools());
        assert!(s.contains("governed AI platform"), "got: {s}");
        assert!(s.contains("(daemon)"), "got: {s}");
        assert!(!s.contains("managed by"), "got: {s}");
    }

    #[test]
    fn managed_without_org_uses_managed_deployment_phrase() {
        let m = cfg(None, None, true);
        let s = build_instructions(Some(&m), "daemon", true, &all_tools());
        assert!(s.contains("managed deployment"), "got: {s}");
        assert!(s.contains("scenery_mcp_request"), "got: {s}");
    }

    #[test]
    fn managed_with_org_names_org() {
        let m = cfg(Some("Acme Corp"), None, true);
        let s = build_instructions(Some(&m), "daemon", true, &all_tools());
        assert!(s.contains("managed by Acme Corp"), "got: {s}");
    }

    #[test]
    fn custom_governance_message_replaces_default() {
        let m = cfg(
            Some("Acme Corp"),
            Some("Contact security@acme.com for tool requests."),
            true,
        );
        let s = build_instructions(Some(&m), "daemon", true, &all_tools());
        assert!(s.contains("Contact security@acme.com"), "got: {s}");
        assert!(
            !s.contains("Use scenery_mcp_request to request a new MCP server"),
            "default copy should be replaced by the custom message; got: {s}"
        );
    }

    #[test]
    fn governance_message_disabled_omits_governance_block() {
        let m = cfg(Some("Acme Corp"), Some("Contact security."), false);
        let s = build_instructions(Some(&m), "daemon", true, &all_tools());
        assert!(s.contains("managed by Acme Corp"), "got: {s}");
        assert!(!s.contains("Contact security."), "got: {s}");
        assert!(
            !s.contains("Tool installations require approval"),
            "got: {s}"
        );
    }

    #[test]
    fn embedded_mode_label_renders() {
        let s = build_instructions(None, "in-process fallback", true, &all_tools());
        assert!(s.contains("(in-process fallback)"), "got: {s}");
    }

    #[test]
    fn empty_mode_label_omits_suffix() {
        let s = build_instructions(None, "", true, &all_tools());
        assert!(!s.contains('('), "got: {s}");
    }

    #[test]
    fn not_logged_in_prepends_nudge() {
        let s = build_instructions(None, "daemon", false, &all_tools());
        assert!(s.contains("scenery_login"), "got: {s}");
        assert!(s.contains("not logged in"), "got: {s}");
    }

    #[test]
    fn logged_in_omits_nudge() {
        let s = build_instructions(None, "daemon", true, &all_tools());
        assert!(!s.contains("not logged in"), "got: {s}");
        assert!(!s.contains("scenery_login"), "got: {s}");
    }

    // ── Bug: instructions must never name a tool tools/list doesn't expose ──

    /// The real default surface: builtins hidden (`SCENERY_EXPOSE_BUILTIN_TOOLS`
    /// unset). Unmanaged mode's generic copy previously always named
    /// scenery_list/_search/_mcp_catalog regardless of what was actually
    /// exposed.
    #[test]
    fn unmanaged_default_surface_names_no_hidden_builtin_tools() {
        let s = build_instructions(None, "daemon", true, &no_builtins());
        for tool in ["scenery_list", "scenery_search", "scenery_mcp_catalog"] {
            assert!(
                !s.contains(tool),
                "instructions must not name '{tool}' when it isn't exposed; got: {s}"
            );
        }
    }

    /// Managed mode's governance block previously always named
    /// scenery_mcp_request even when builtins were hidden.
    #[test]
    fn managed_default_surface_names_no_hidden_builtin_tools() {
        let m = cfg(Some("Acme Corp"), None, true);
        let s = build_instructions(Some(&m), "daemon", true, &no_builtins());
        for tool in [
            "scenery_list",
            "scenery_search",
            "scenery_mcp_catalog",
            "scenery_mcp_request",
        ] {
            assert!(
                !s.contains(tool),
                "instructions must not name '{tool}' when it isn't exposed; got: {s}"
            );
        }
        // The governance block itself still renders, just without naming
        // the hidden tool.
        assert!(
            s.contains("Tool installations require approval"),
            "got: {s}"
        );
    }

    /// Only the exposed subset is named — not an all-or-nothing switch.
    #[test]
    fn discovery_hint_names_only_the_exposed_subset() {
        let s = build_instructions(None, "daemon", true, &names(&["scenery_search"]));
        assert!(s.contains("scenery_search"), "got: {s}");
        assert!(!s.contains("scenery_list"), "got: {s}");
        assert!(!s.contains("scenery_mcp_catalog"), "got: {s}");
    }

    /// Surface state: no registry URL configured at all. `logged_in` is
    /// always `false` in that case (nothing to be logged into), and
    /// `tools::build_tool_list` never adds `scenery_login` without a
    /// registry URL either — so the nudge must not mention it.
    #[test]
    fn no_registry_configured_omits_login_nudge_entirely() {
        let s = build_instructions(None, "daemon", false, &no_builtins());
        assert!(
            !s.contains("scenery_login"),
            "must not nudge toward a login tool that isn't exposed; got: {s}"
        );
        assert!(
            !s.contains("not logged in"),
            "the whole nudge sentence must be omitted, not just the tool name; got: {s}"
        );
    }

    #[test]
    fn fully_exposed_surface_still_names_every_builtin() {
        let s = build_instructions(None, "daemon", true, &all_tools());
        assert!(s.contains("scenery_list"), "got: {s}");
        assert!(s.contains("scenery_search"), "got: {s}");
        assert!(s.contains("scenery_mcp_catalog"), "got: {s}");
    }
}
