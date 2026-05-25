//! Tests for the shim library.
//!
//! Kept in a separate file so `#[allow(clippy::unwrap_used)]` is scoped here
//! and does not pollute production code.
#![allow(clippy::unwrap_used)]

use crate::{daemon_required_error, dispatch_line, SessionMode};

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

/// `daemon_required_error` returns an error with code -32001 and a message
/// containing the install hint.
#[test]
fn daemon_required_error_contains_install_hint() {
    let resp = daemon_required_error(
        Some(serde_json::json!(1)),
        "daemon socket unreachable: connection refused",
    );
    let err = resp
        .error
        .expect("daemon-required response must be an error");
    assert_eq!(err.code, -32001i64, "must use the DAEMON_UNREACHABLE code");
    assert!(
        err.message.contains("vectorhawk daemon install"),
        "message must include `vectorhawk daemon install` install hint; got: {}",
        err.message
    );
    assert!(
        err.message.contains("connection refused"),
        "message must include the underlying reason; got: {}",
        err.message
    );
}

/// In `DaemonRequired` mode, every recognized request gets the standard
/// daemon-unreachable error.
#[tokio::test]
async fn daemon_required_mode_returns_error_for_initialize() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "test: socket missing".to_string(),
    };
    let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let resp = dispatch_line(line, &mut mode, None)
        .await
        .expect("initialize must produce a response");
    let err = resp.error.expect("response must be an error");
    assert_eq!(err.code, -32001);
    assert!(err.message.contains("daemon"));
}

/// In `DaemonRequired` mode, `tools/list` also gets the standard error
/// (every method, not just initialize).
#[tokio::test]
async fn daemon_required_mode_returns_error_for_tools_list() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "test".to_string(),
    };
    let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    let resp = dispatch_line(line, &mut mode, None)
        .await
        .expect("tools/list must produce a response");
    assert_eq!(resp.error.expect("must error").code, -32001);
}

/// Notifications (no id) get no response even in `DaemonRequired` mode.
#[tokio::test]
async fn daemon_required_mode_drops_notifications() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "test".to_string(),
    };
    let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    let resp = dispatch_line(line, &mut mode, None).await;
    assert!(resp.is_none(), "notifications must not produce a response");
}

/// Malformed JSON gets a parse-error response regardless of mode.
#[tokio::test]
async fn malformed_json_gets_parse_error() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "test".to_string(),
    };
    let resp = dispatch_line("this is not json", &mut mode, None)
        .await
        .expect("must produce a response");
    let err = resp.error.expect("must error");
    assert_eq!(
        err.code,
        vectorhawkd_mcp::protocol::PARSE_ERROR,
        "must surface the parse error code, not the daemon-unreachable code"
    );
}

// ── F2: --server prefix filter tests ─────────────────────────────────────────

/// `filter_tools_for_server` keeps only tools whose names start with the slug
/// prefix, and strips the prefix from the returned names.
#[cfg(unix)]
#[test]
fn filter_tools_for_server_strips_prefix_and_filters() {
    use vectorhawkd_mcp::protocol::{ToolDefinition, ToolsListResult};

    let result = ToolsListResult {
        tools: vec![
            ToolDefinition {
                name: "filesystem__read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "filesystem__write_file".to_string(),
                description: "Write a file".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "github__create_issue".to_string(),
                description: "Create an issue".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "vectorhawk_list_skills".to_string(),
                description: "VH management".to_string(),
                input_schema: serde_json::json!({}),
            },
        ],
    };

    let filtered = crate::filter_tools_for_server(result, "filesystem");

    assert_eq!(
        filtered.tools.len(),
        2,
        "only filesystem tools must survive"
    );
    assert_eq!(
        filtered.tools[0].name, "read_file",
        "prefix must be stripped"
    );
    assert_eq!(
        filtered.tools[1].name, "write_file",
        "prefix must be stripped"
    );
}

/// `filter_tools_for_aggregator` drops `<slug>__*` tools whose slug is in the
/// exclusion set but keeps everything else (other backends + unprefixed
/// management tools).
#[cfg(unix)]
#[test]
fn filter_tools_for_aggregator_drops_excluded_slugs() {
    use vectorhawkd_mcp::protocol::{ToolDefinition, ToolsListResult};

    let result = ToolsListResult {
        tools: vec![
            ToolDefinition {
                name: "everything__echo".to_string(),
                description: "".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "everything__get_sum".to_string(),
                description: "".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "filesystem__read_file".to_string(),
                description: "".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "vectorhawk_list".to_string(),
                description: "".to_string(),
                input_schema: serde_json::json!({}),
            },
        ],
    };

    let excluded: std::collections::HashSet<String> =
        std::iter::once("everything".to_string()).collect();
    let filtered = crate::filter_tools_for_aggregator(result, &excluded);

    let names: Vec<&str> = filtered.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["filesystem__read_file", "vectorhawk_list"]);
}

/// Empty exclusion set is a no-op.
#[cfg(unix)]
#[test]
fn filter_tools_for_aggregator_empty_exclusions_keeps_all() {
    use vectorhawkd_mcp::protocol::{ToolDefinition, ToolsListResult};

    let result = ToolsListResult {
        tools: vec![ToolDefinition {
            name: "everything__echo".to_string(),
            description: "".to_string(),
            input_schema: serde_json::json!({}),
        }],
    };
    let filtered = crate::filter_tools_for_aggregator(result, &std::collections::HashSet::new());
    assert_eq!(filtered.tools.len(), 1);
}

/// When no tools match the slug, the result is an empty list (not an error).
#[cfg(unix)]
#[test]
fn filter_tools_for_server_returns_empty_when_no_match() {
    use vectorhawkd_mcp::protocol::{ToolDefinition, ToolsListResult};

    let result = ToolsListResult {
        tools: vec![ToolDefinition {
            name: "github__create_pr".to_string(),
            description: "Create PR".to_string(),
            input_schema: serde_json::json!({}),
        }],
    };

    let filtered = crate::filter_tools_for_server(result, "filesystem");
    assert!(
        filtered.tools.is_empty(),
        "no matching tools must yield empty list"
    );
}

/// Without --server, the dispatch passes the full tool list through.
/// This exercises `dispatch_line` in DaemonRequired mode; the filter is
/// applied in `relay_via_socket` which requires a live socket.  Here we
/// verify that without a server_slug the mode is entered correctly.
#[tokio::test]
async fn without_server_slug_daemon_required_returns_error() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "no socket".to_string(),
    };
    let line = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#;
    // No server slug — full passthrough (in DaemonRequired mode, always errors).
    let resp = dispatch_line(line, &mut mode, None).await.unwrap();
    assert_eq!(resp.error.unwrap().code, -32001);
}

/// With server slug in DaemonRequired mode — still returns the daemon error,
/// not a filter-related error.  The filter only applies once a live socket is
/// established.
#[tokio::test]
async fn with_server_slug_daemon_required_still_returns_error() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "no socket".to_string(),
    };
    let line = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#;
    let resp = dispatch_line(line, &mut mode, Some("filesystem"))
        .await
        .unwrap();
    assert_eq!(resp.error.unwrap().code, -32001);
}
