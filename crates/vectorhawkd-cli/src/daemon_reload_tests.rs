//! Regression tests for `daemon_auth_reload` driven over a REAL Unix socket
//! against a fake daemon — not `handle_reload`/`SyncController` in isolation.
//!
//! ## Why these tests exist (PAIRMSG2)
//!
//! `vectorhawkd-daemon`'s own unit tests (`auth_dispatch_tests.rs`,
//! `sync_controller_hook_tests.rs`) call `handle_reload` directly and all
//! passed — the daemon-side `sync_active` computation was correct. But the
//! real `vectorhawk auth pair` against a running daemon still printed "the
//! daemon could not start syncing yet" in the live demo sandbox, immediately
//! after `vectorhawk daemon install` (i.e. within
//! `socket_dispatch::ALERT_WINDOW` of boot, while an adoption alert from the
//! daemon's F1 first-run migration was still fresh).
//!
//! Root cause, proven by driving `auth/reload` directly over the daemon's
//! socket in the sandbox (bypassing the CLI): the daemon's response was
//! always `{"result":{"sync_active":true}}`, sent promptly. But
//! `socket_dispatch::run_loop` unconditionally writes any fresh pending
//! adoption alert to a NEW connection, as a `notifications/message` frame
//! (no `id`), BEFORE it reads or dispatches the connection's first request.
//! `daemon_auth_reload` read exactly one frame and assumed it was the
//! response to its `auth/reload` request — so on a connection racing a fresh
//! alert, it parsed the *notification* instead, found no
//! `result.sync_active`, and silently fell into the `_ => SyncInactive` arm.
//!
//! No unit test that calls `handle_reload` (or even `dispatch`) directly can
//! see this: the alert-frame write lives in `run_loop`/`serve_connection`,
//! which only runs for a real accepted connection. These tests spin up a
//! fake daemon that speaks the exact wire protocol
//! (`vectorhawkd_mcp::backend::{read_framed, write_framed}`, matching
//! `socket_dispatch`'s framing byte-for-byte) over a real `UnixListener`, and
//! call the actual (unmodified, private) `daemon_auth_reload` — so a
//! regression in either the framing contract or the CLI's parsing of it
//! would fail these tests, not just a mocked handler call.

use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use vectorhawkd_mcp::backend::{read_framed, write_framed};

use super::{daemon_auth_reload, DaemonReload};

/// Bind a fresh Unix socket in a temp dir and return `(listener, path)`.
fn bind_socket() -> (UnixListener, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("agent.sock");
    let listener = UnixListener::bind(&path).expect("bind unix socket");
    (listener, dir, path.to_string_lossy().into_owned())
}

/// Read one framed JSON-RPC request off `stream` and return it parsed.
async fn read_request(stream: &mut UnixStream) -> serde_json::Value {
    let bytes = read_framed(stream)
        .await
        .expect("read_framed failed")
        .expect("peer closed before sending a request");
    serde_json::from_slice(&bytes).expect("request was not valid JSON")
}

/// The real bug reproduction: a connection that races a fresh adoption alert
/// receives that alert — an unsolicited `notifications/message` frame with
/// no `id`, exactly what `socket_dispatch::send_alert_frame` writes — BEFORE
/// the daemon ever reads the `auth/reload` request. The daemon's actual
/// answer (`sync_active: true`) only follows after that. A CLI that reads
/// exactly one frame and treats it as the response gets this wrong; one that
/// skips notification frames (no `id`) gets it right.
#[tokio::test]
async fn daemon_auth_reload_skips_a_leading_notification_frame() {
    let (listener, _dir, socket_path) = bind_socket();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");

        // Mirrors `socket_dispatch::run_loop`: the pending-alert notification
        // is written to the connection unconditionally, before anything is
        // read from it.
        let alert = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "info",
                "logger": "vectorhawk",
                "data": "VectorHawk auto-adopted 1 tool for governance",
            },
        });
        write_framed(&mut stream, &serde_json::to_vec(&alert).unwrap())
            .await
            .expect("write alert frame");

        // Now read the CLI's actual `auth/reload` request and answer it
        // honestly, echoing the request's `id` the way `JsonRpcResponse`
        // does.
        let request = read_request(&mut stream).await;
        assert_eq!(request["method"], "auth/reload");
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "sync_active": true },
        });
        write_framed(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .expect("write response frame");
    });

    let result = tokio::time::timeout(Duration::from_secs(5), daemon_auth_reload(&socket_path))
        .await
        .expect("daemon_auth_reload timed out");

    server.await.expect("fake daemon task panicked");

    assert_eq!(
        result,
        DaemonReload::SyncActive,
        "a leading notification frame must not be mistaken for the auth/reload \
         response — the CLI should skip it and keep reading"
    );
}

/// Counterpart with no alert in flight (the common case, and what the
/// existing daemon-side unit tests already cover): the response frame really
/// is the first frame, and must still be parsed correctly.
#[tokio::test]
async fn daemon_auth_reload_reads_response_with_no_leading_notification() {
    let (listener, _dir, socket_path) = bind_socket();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut stream).await;
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "sync_active": true },
        });
        write_framed(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .expect("write response frame");
    });

    let result = tokio::time::timeout(Duration::from_secs(5), daemon_auth_reload(&socket_path))
        .await
        .expect("daemon_auth_reload timed out");

    server.await.expect("fake daemon task panicked");

    assert_eq!(result, DaemonReload::SyncActive);
}

/// A genuine failure (`sync_active: false`) must still be reported honestly
/// after skipping a leading notification — the fix must not turn a real
/// failure into a false success either.
#[tokio::test]
async fn daemon_auth_reload_reports_inactive_after_notification_when_sync_genuinely_failed() {
    let (listener, _dir, socket_path) = bind_socket();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");

        let alert = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": { "level": "info", "logger": "vectorhawk", "data": "adopted" },
        });
        write_framed(&mut stream, &serde_json::to_vec(&alert).unwrap())
            .await
            .expect("write alert frame");

        let request = read_request(&mut stream).await;
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "sync_active": false },
        });
        write_framed(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .expect("write response frame");
    });

    let result = tokio::time::timeout(Duration::from_secs(5), daemon_auth_reload(&socket_path))
        .await
        .expect("daemon_auth_reload timed out");

    server.await.expect("fake daemon task panicked");

    assert_eq!(result, DaemonReload::SyncInactive);
}
