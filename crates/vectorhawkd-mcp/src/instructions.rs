//! Dynamic MCP `initialize` instructions builder.
//!
//! When the runner is in *managed mode* (a valid `managed.json` is present),
//! the instructions block names the org and uses stronger governance language.
//! In individual-developer mode, generic copy is returned.

use vectorhawkd_core::managed::ManagedConfig;

/// Build the MCP `initialize` `instructions` string.
///
/// `mode_label` is appended verbatim and lets the caller distinguish the
/// daemon ("daemon") vs the embedded fallback path ("in-process fallback")
/// without repeating phrasing in two places.
pub fn build_instructions(managed: Option<&ManagedConfig>, mode_label: &str) -> String {
    let mode_suffix = match mode_label {
        "" => String::new(),
        s => format!(" ({s})"),
    };

    let Some(managed) = managed else {
        return format!(
            "VectorHawk runner — governed AI platform{mode_suffix}. \
             Use vectorhawk_list to show installed skills, vectorhawk_search to \
             find more, and vectorhawk_mcp_catalog to browse approved MCP servers."
        );
    };

    let mut out = String::new();

    match managed.org.as_deref() {
        Some(org) => out.push_str(&format!(
            "VectorHawk runner — managed by {org}{mode_suffix}. "
        )),
        None => out.push_str(&format!(
            "VectorHawk runner — managed deployment{mode_suffix}. "
        )),
    }

    if managed.governance_message_enabled {
        match managed.governance_message.as_deref() {
            Some(custom) => {
                out.push_str(custom);
                out.push(' ');
            }
            None => out.push_str(
                "Tool installations require approval. Use vectorhawk_mcp_request \
                 to request a new MCP server through governance; direct install \
                 via /mcp bypasses controls. ",
            ),
        }
    }

    out.push_str(
        "Use vectorhawk_list to show installed skills, vectorhawk_search to find \
         more, vectorhawk_mcp_catalog to browse approved servers.",
    );

    out.trim().to_string()
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

    #[test]
    fn unmanaged_returns_generic_copy() {
        let s = build_instructions(None, "daemon");
        assert!(s.contains("governed AI platform"), "got: {s}");
        assert!(s.contains("(daemon)"), "got: {s}");
        assert!(!s.contains("managed by"), "got: {s}");
    }

    #[test]
    fn managed_without_org_uses_managed_deployment_phrase() {
        let m = cfg(None, None, true);
        let s = build_instructions(Some(&m), "daemon");
        assert!(s.contains("managed deployment"), "got: {s}");
        assert!(s.contains("vectorhawk_mcp_request"), "got: {s}");
    }

    #[test]
    fn managed_with_org_names_org() {
        let m = cfg(Some("Acme Corp"), None, true);
        let s = build_instructions(Some(&m), "daemon");
        assert!(s.contains("managed by Acme Corp"), "got: {s}");
    }

    #[test]
    fn custom_governance_message_replaces_default() {
        let m = cfg(
            Some("Acme Corp"),
            Some("Contact security@acme.com for tool requests."),
            true,
        );
        let s = build_instructions(Some(&m), "daemon");
        assert!(s.contains("Contact security@acme.com"), "got: {s}");
        assert!(
            !s.contains("Use vectorhawk_mcp_request to request a new MCP server"),
            "default copy should be replaced by the custom message; got: {s}"
        );
    }

    #[test]
    fn governance_message_disabled_omits_governance_block() {
        let m = cfg(Some("Acme Corp"), Some("Contact security."), false);
        let s = build_instructions(Some(&m), "daemon");
        assert!(s.contains("managed by Acme Corp"), "got: {s}");
        assert!(!s.contains("Contact security."), "got: {s}");
        assert!(
            !s.contains("Tool installations require approval"),
            "got: {s}"
        );
    }

    #[test]
    fn embedded_mode_label_renders() {
        let s = build_instructions(None, "in-process fallback");
        assert!(s.contains("(in-process fallback)"), "got: {s}");
    }

    #[test]
    fn empty_mode_label_omits_suffix() {
        let s = build_instructions(None, "");
        assert!(!s.contains('('), "got: {s}");
    }
}
