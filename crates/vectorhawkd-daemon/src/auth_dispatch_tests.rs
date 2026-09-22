//! Unit tests for `auth_dispatch`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use vectorhawkd_mcp::protocol::INTERNAL_ERROR;

use crate::oauth_state::OAuthState;

use super::{
    handle_get_oauth_listener_port, handle_get_portal_session, handle_reload,
    handle_wait_for_callback, NOT_AUTHENTICATED,
};

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

/// Regression test for the sales-demo bug: `vectorhawk auth pair` against a
/// daemon whose SSE sync loop is already running under *different*
/// credentials must report `sync_active: true` once the credential-change
/// restart it triggers actually connects — not `false` just because the
/// restart was still "in flight" (task freshly spawned, connection not yet
/// confirmed) at the moment `ensure_started` used to sample it. Goes through
/// `handle_reload` itself (the real `auth/reload` RPC handler CLI's
/// `daemon_auth_reload` calls), not `SyncController::ensure_started`
/// directly, so this exercises the exact code path the CLI hit.
#[tokio::test]
async fn reload_reports_active_after_credential_change_on_running_daemon() {
    use crate::SyncController;
    use tokio::sync::broadcast;
    use vectorhawkd_mcp::aggregator::BackendRegistry;

    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    // Two registrations, not one: as of 1.0.90 `register_device` posts on
    // every call instead of short-circuiting on a cached device_id, so the
    // credential-change restart re-registers. That is the point — it is what
    // refreshes the backend's agent_version for this device — and the
    // endpoint is idempotent on device_uuid.
    let register_mock = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-1"}"#)
        .expect(2)
        .create_async()
        .await;

    let sse_tok1 = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-1")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    let sse_tok2 = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-2")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    let (tx, _rx) = broadcast::channel(16);
    let controller = Arc::new(SyncController::new(
        registry_url.clone(),
        Arc::clone(&state),
        tx,
        Arc::new(BackendRegistry::new()),
        None,
    ));

    // First auth (e.g. `vectorhawk auth login`) — sync starts and connects
    // with tok-1.
    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-1", "ref-1")
        .expect("save_tokens tok-1");
    let resp = handle_reload(id(), Arc::clone(&controller)).await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(
        resp.result.unwrap()["sync_active"],
        true,
        "sync should report active once tok-1's SSE connection is up"
    );
    sse_tok1.assert_async().await;

    // The reproduction: `vectorhawk auth pair` (or `auth login`/`auth
    // token`) writes a *different* access token while the daemon's SSE loop
    // from above is still running — exactly what a re-pair mid-demo does.
    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-2", "ref-2")
        .expect("save_tokens tok-2");
    let resp = handle_reload(id(), Arc::clone(&controller)).await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(
        resp.result.unwrap()["sync_active"],
        true,
        "reload must report active once the credential-change restart's new \
         SSE connection is genuinely up — not merely because the replacement \
         task was spawned (the bug: reporting inactive while the restart \
         this same call triggered was still connecting)"
    );
    sse_tok2.assert_async().await;

    // Device registration is idempotent (cached device_id) — never repeated.
    register_mock.assert_async().await;
}

/// The honest-failure counterpart: when the SSE connection genuinely cannot
/// be established (backend rejects every attempt), `auth/reload` must still
/// report `sync_active: false` — the fix must not soften a real failure into
/// a false "success" alongside fixing the false "failure".
#[tokio::test]
async fn reload_reports_inactive_when_sse_connection_genuinely_fails() {
    use crate::SyncController;
    use tokio::sync::broadcast;
    use vectorhawkd_mcp::aggregator::BackendRegistry;

    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    // Device registration succeeds — the failure is specifically in getting
    // the SSE stream up, e.g. the registry rejecting every connection
    // attempt (a real "registry unreachable" would look the same from
    // `ensure_started`'s point of view: the task spawns fine, the
    // connection itself never comes up).
    let register_mock = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-1"}"#)
        .expect(1)
        .create_async()
        .await;

    let sse_fail = server
        .mock("GET", "/api/sync/events")
        .with_status(500)
        .with_body("internal error")
        .expect_at_least(1)
        .create_async()
        .await;

    let (tx, _rx) = broadcast::channel(16);
    let controller = Arc::new(SyncController::new(
        registry_url.clone(),
        Arc::clone(&state),
        tx,
        Arc::new(BackendRegistry::new()),
        None,
    ));

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-1", "ref-1")
        .expect("save_tokens");

    let resp = handle_reload(id(), Arc::clone(&controller)).await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(
        resp.result.unwrap()["sync_active"],
        false,
        "reload must report inactive — a real diagnostic, not a softened \
         success — when the SSE connection never comes up"
    );

    register_mock.assert_async().await;
    sse_fail.assert_async().await;
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

// ── auth/get_portal_session ──────────────────────────────────────────────────

/// Force the SQLite fallback so these tests don't pollute the real macOS
/// keychain. Holds a global mutex so concurrent tests can't race each
/// other's env-var set/clear (cargo test runs in parallel by default).
/// Mirrors the identical helper in `refresh_loop_tests.rs` / `vectorhawkd-core`'s
/// own `auth.rs` test suite — kept local rather than shared since it's a tiny,
/// test-only utility and none of those modules expose it publicly.
struct KeychainOff {
    _g: std::sync::MutexGuard<'static, ()>,
}
static KEYCHAIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl KeychainOff {
    fn enable() -> Self {
        let _g = KEYCHAIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("VECTORHAWK_DISABLE_KEYCHAIN", "1");
        KeychainOff { _g }
    }
}
impl Drop for KeychainOff {
    fn drop(&mut self) {
        std::env::remove_var("VECTORHAWK_DISABLE_KEYCHAIN");
    }
}

/// Bootstrap a fresh, isolated `AppState` (real SQLite schema, no rows).
/// Returns the `TempDir` guard alongside it — the caller must keep it alive
/// for as long as the `AppState` is used, same as the pattern already used by
/// `reload_returns_inactive_without_token_and_is_idempotent` above.
fn bootstrap_state() -> (vectorhawkd_core::state::AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = vectorhawkd_core::state::AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

#[tokio::test]
async fn get_portal_session_returns_not_authenticated_when_no_tokens_stored() {
    let (state, _tmp) = bootstrap_state(); // fresh bootstrapped AppState, no auth_tokens rows

    let resp = handle_get_portal_session(id(), &state, "https://example.invalid").await;

    assert!(
        resp.error.is_some(),
        "expected an error when no tokens are stored"
    );
    assert_eq!(resp.error.as_ref().unwrap().code, NOT_AUTHENTICATED);
    assert_eq!(resp.error.as_ref().unwrap().message, "not authenticated");
}

#[tokio::test]
async fn get_portal_session_returns_full_session_on_success() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let mock = server
        .mock("GET", "/portal/auth/me")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"u1","email":"portal@example.com","display_name":"Portal User"}"#)
        .create_async()
        .await;

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "acc-tok", "ref-tok")
        .expect("save_tokens");

    let resp = handle_get_portal_session(id(), &state, &registry_url).await;

    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_eq!(result["access_token"], "acc-tok");
    assert_eq!(result["refresh_token"], "ref-tok");
    assert_eq!(result["auth_scope"], "portal");
    assert_eq!(result["user"]["id"], "u1");
    assert_eq!(result["user"]["email"], "portal@example.com");
    assert_eq!(result["user"]["display_name"], "Portal User");
    mock.assert_async().await;
}

#[tokio::test]
async fn get_portal_session_returns_internal_error_when_me_fails() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let mock = server
        .mock("GET", "/portal/auth/me")
        .with_status(401)
        .with_body("unauthorized")
        .create_async()
        .await;

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "acc-tok", "ref-tok")
        .expect("save_tokens");

    let resp = handle_get_portal_session(id(), &state, &registry_url).await;

    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, INTERNAL_ERROR);
    assert!(
        err.message.contains("failed to fetch user info"),
        "error message should mention the failed user-info fetch; got: {}",
        err.message
    );
    mock.assert_async().await;
}
