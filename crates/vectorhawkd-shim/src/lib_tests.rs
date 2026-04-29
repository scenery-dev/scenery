//! Tests for the shim library.
//!
//! Kept in a separate file so `#[allow(clippy::unwrap_used)]` is scoped here
//! and does not pollute production code.
#![allow(clippy::unwrap_used)]

use vectorhawkd_mcp::{
    backend::{Backend, EmbeddedBackend},
    protocol::ToolCallParams,
};

/// Round-trip `initialize` through `EmbeddedBackend` — the in-process fallback.
///
/// This is the canonical shim fallback test: verifies that the embedded
/// backend (which is what `run_shim` uses when the daemon socket is
/// unreachable) correctly handles an MCP initialize request.
#[tokio::test]
async fn embedded_initialize_round_trip() {
    let backend = EmbeddedBackend::with_stub_backend(
        "vectorhawk",
        &[
            ("list_skills", "List installed VectorHawk skills"),
            ("run_skill", "Run a VectorHawk skill"),
        ],
    );

    let result = backend.initialize(serde_json::json!({})).await.unwrap();

    assert_eq!(
        result.protocol_version, "2024-11-05",
        "protocol version must match MCP spec"
    );
    assert_eq!(
        result.server_info.name, "vectorhawkd",
        "server name must be vectorhawkd"
    );
    assert!(
        result.capabilities.tools.is_some(),
        "tools capability must be present"
    );
    // Fallback instructions are set — this is the distinguishing marker that
    // lets support identify embedded mode in logs.
    let instructions = result.instructions.unwrap_or_default();
    assert!(
        instructions.contains("fallback"),
        "embedded mode instructions must mention fallback; got: {instructions:?}"
    );
}

/// Round-trip `tools/list` through `EmbeddedBackend`.
///
/// Verifies that stub tools appear with the expected namespace prefix
/// (`vectorhawk__<tool_name>`).
#[tokio::test]
async fn embedded_tools_list_round_trip() {
    let backend = EmbeddedBackend::with_stub_backend(
        "vectorhawk",
        &[
            ("list_skills", "List installed VectorHawk skills"),
            ("run_skill", "Run a VectorHawk skill"),
            ("install_skill", "Install a skill from the registry"),
            ("search_skills", "Search the skill registry"),
            ("get_status", "Get VectorHawk runner status"),
        ],
    );

    let result = backend.list_tools(serde_json::json!({})).await.unwrap();

    assert_eq!(result.tools.len(), 5, "all 5 stub tools must appear");

    let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"vectorhawk__list_skills"),
        "expected vectorhawk__list_skills in {names:?}"
    );
    assert!(
        names.contains(&"vectorhawk__get_status"),
        "expected vectorhawk__get_status in {names:?}"
    );
}

/// Five `tools/call` round-trips through `EmbeddedBackend`.
///
/// Satisfies M0 acceptance criterion: the in-process fallback can service
/// >=5 tool calls without JSON-RPC error.
#[tokio::test]
async fn embedded_five_tool_calls_round_trip() {
    let backend = EmbeddedBackend::with_stub_backend(
        "vectorhawk",
        &[
            ("list_skills", "List installed VectorHawk skills"),
            ("run_skill", "Run a VectorHawk skill"),
            ("install_skill", "Install a skill from the registry"),
            ("search_skills", "Search the skill registry"),
            ("get_status", "Get VectorHawk runner status"),
        ],
    );

    let tool_names = [
        "vectorhawk__list_skills",
        "vectorhawk__run_skill",
        "vectorhawk__install_skill",
        "vectorhawk__search_skills",
        "vectorhawk__get_status",
    ];

    for tool_name in tool_names {
        let result = backend
            .call_tool(ToolCallParams {
                name: tool_name.to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();

        assert!(
            result.is_error.is_none() || result.is_error == Some(false),
            "stub call should not be an error for {tool_name}"
        );
        assert!(
            !result.content.is_empty(),
            "content must not be empty for {tool_name}"
        );
    }
}

/// `daemon_socket_path` returns a path ending in `agent.sock`.
///
/// The exact path depends on HOME/XDG env, but the filename must always be
/// `agent.sock` — that's the contract agreed by daemon and shim.
#[test]
fn daemon_socket_path_ends_with_agent_sock() {
    let path = crate::daemon_socket_path();
    let path = path.expect("daemon_socket_path must return Some on macOS/Linux");
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("agent.sock"),
        "socket path must end with agent.sock; got: {}",
        path.display()
    );
}

/// `daemon_socket_path` contains `VectorHawk` on macOS.
///
/// Guards against accidental renames drifting from the daemon's path.
#[test]
#[cfg(target_os = "macos")]
fn daemon_socket_path_contains_vectorhawk_on_macos() {
    let path = crate::daemon_socket_path().unwrap();
    assert!(
        path.to_string_lossy().contains("VectorHawk"),
        "macOS socket path must be under VectorHawk data dir; got: {}",
        path.display()
    );
}
