//! Unit tests for `auth_dispatch`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use vectorhawkd_mcp::protocol::INTERNAL_ERROR;

use crate::oauth_state::OAuthState;

use super::{handle_get_oauth_listener_port, handle_reload, handle_wait_for_callback};

fn id() -> Option<serde_json::Value> {
    Some(serde_json::json!(1))
}

// ── auth/get_oauth_listener_port ─────────────────────────────────────────────

#[tokio::test]
async fn get_port_returns_port_when_listener_running() {
    let resp = handle_get_oauth_listener_port(id(), Some(39127)).await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(resp.result.unwrap()["port"], 39127);
}

#[tokio::test]
async fn get_port_returns_error_when_listener_not_running() {
    let resp = handle_get_oauth_listener_port(id(), None).await;
    assert!(resp.result.is_none());
    assert_eq!(resp.error.unwrap().code, INTERNAL_ERROR);
}

// ── auth/reload ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn reload_returns_inactive_without_token_and_is_idempotent() {
    use crate::SyncController;
    use tokio::sync::broadcast;
    use vectorhawkd_core::state::AppState;
    use vectorhawkd_mcp::aggregator::BackendRegistry;

    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();

    let (tx, _rx) = broadcast::channel(16);
    let controller = Arc::new(SyncController::new(
        "https://example.invalid".to_string(),
        Arc::new(state),
        tx,
        Arc::new(BackendRegistry::new()),
        None,
    ));

    // No auth token persisted → sync must not start, and the handler must report
    // it cleanly rather than erroring.
    let resp = handle_reload(id(), Arc::clone(&controller)).await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(resp.result.unwrap()["sync_active"], false);

    // Idempotent: a second invocation is still inactive and does not panic.
    assert!(!controller.ensure_started().await);
}

// ── auth/wait_for_callback ───────────────────────────────────────────────────

#[tokio::test]
async fn wait_for_callback_receives_code() {
    let hub = Arc::new(OAuthState::new());

    // Spawn the wait handler concurrently.
    let hub_clone = Arc::clone(&hub);
    let handle = tokio::spawn(async move {
        handle_wait_for_callback(
            id(),
            serde_json::json!({"state": "s1", "timeout_secs": 5}),
            hub_clone,
        )
        .await
    });

    // Give the handler a moment to subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    hub.notify("s1".to_string(), "authorization-code".to_string())
        .await
        .unwrap();

    let resp = handle.await.unwrap();
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(resp.result.unwrap()["code"], "authorization-code");
}

#[tokio::test]
async fn wait_for_callback_times_out() {
    let hub = Arc::new(OAuthState::new());

    let resp = handle_wait_for_callback(
        id(),
        serde_json::json!({"state": "s-timeout", "timeout_secs": 1}),
        hub,
    )
    .await;

    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, INTERNAL_ERROR);
    assert!(
        err.message.contains("timed out"),
        "error message should mention timed out; got: {}",
        err.message
    );
}

#[tokio::test]
async fn wait_for_callback_timeout_below_min_rejected() {
    let hub = Arc::new(OAuthState::new());

    let resp = handle_wait_for_callback(
        id(),
        serde_json::json!({"state": "s", "timeout_secs": 0}),
        hub,
    )
    .await;

    use vectorhawkd_mcp::protocol::INVALID_PARAMS;
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}

#[tokio::test]
async fn wait_for_callback_timeout_above_max_rejected() {
    let hub = Arc::new(OAuthState::new());

    let resp = handle_wait_for_callback(
        id(),
        serde_json::json!({"state": "s", "timeout_secs": 601}),
        hub,
    )
    .await;

    use vectorhawkd_mcp::protocol::INVALID_PARAMS;
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}

#[tokio::test]
async fn wait_for_callback_daemon_shutdown_returns_error() {
    let hub = Arc::new(OAuthState::new());
    let hub_clone = Arc::clone(&hub);

    let handle = tokio::spawn(async move {
        handle_wait_for_callback(
            id(),
            serde_json::json!({"state": "s-shutdown", "timeout_secs": 30}),
            hub_clone,
        )
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    hub.cancel_all().await;

    let resp = handle.await.unwrap();
    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, INTERNAL_ERROR);
    assert!(
        err.message.contains("shutting down"),
        "error message should mention shutdown; got: {}",
        err.message
    );
}

#[tokio::test]
async fn wait_for_callback_duplicate_state_returns_invalid_params() {
    let hub = Arc::new(OAuthState::new());

    // First subscription holds the channel open.
    let _rx = hub.subscribe("dup-s".to_string()).await.unwrap();

    let resp = handle_wait_for_callback(
        id(),
        serde_json::json!({"state": "dup-s", "timeout_secs": 5}),
        hub,
    )
    .await;

    use vectorhawkd_mcp::protocol::INVALID_PARAMS;
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}

#[tokio::test]
async fn wait_for_callback_default_timeout_is_accepted() {
    let hub = Arc::new(OAuthState::new());
    let hub_clone = Arc::clone(&hub);

    // No timeout_secs — should use default (300 s) and not error.
    let handle = tokio::spawn(async move {
        handle_wait_for_callback(
            id(),
            // Omit timeout_secs entirely; serde default should kick in.
            serde_json::json!({"state": "s-default"}),
            hub_clone,
        )
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    hub.notify("s-default".to_string(), "code-default".to_string())
        .await
        .unwrap();

    let resp = handle.await.unwrap();
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap()["code"], "code-default");
}
