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
    let resp = dispatch_line(line, &mut mode)
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
    let resp = dispatch_line(line, &mut mode)
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
    let resp = dispatch_line(line, &mut mode).await;
    assert!(resp.is_none(), "notifications must not produce a response");
}

/// Malformed JSON gets a parse-error response regardless of mode.
#[tokio::test]
async fn malformed_json_gets_parse_error() {
    let mut mode = SessionMode::DaemonRequired {
        reason: "test".to_string(),
    };
    let resp = dispatch_line("this is not json", &mut mode)
        .await
        .expect("must produce a response");
    let err = resp.error.expect("must error");
    assert_eq!(
        err.code,
        vectorhawkd_mcp::protocol::PARSE_ERROR,
        "must surface the parse error code, not the daemon-unreachable code"
    );
}
