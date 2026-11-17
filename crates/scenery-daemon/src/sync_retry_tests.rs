//! Tests for the background sync start-retry (`sync_retry.rs`).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use scenery_core::auth::USER_REVOKED_KEY;
use scenery_core::state::AppState;
use scenery_mcp::aggregator::BackendRegistry;
use tokio::sync::{broadcast, Notify};

use super::{RetryConfig, RetryOutcome};
use crate::SyncController;

const FAST: RetryConfig = RetryConfig {
    initial: Duration::from_millis(20),
    max: Duration::from_millis(80),
};

fn disable_keychain() {
    // Same process-wide switch the other daemon tests rely on, so these never
    // touch the real macOS keychain.
    std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
}

fn bootstrap_state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    (Arc::new(AppState::bootstrap_in(root).unwrap()), tmp)
}

fn controller(registry_url: &str, state: &Arc<AppState>) -> Arc<SyncController> {
    let (tx, _rx) = broadcast::channel(16);
    Arc::new(SyncController::new(
        registry_url.to_string(),
        Arc::clone(state),
        tx,
        Arc::new(BackendRegistry::new()),
        None,
    ))
}

/// Wait until the controller has really spawned sync (non-blocking
/// `is_started` also reports `true` while a start is merely in progress).
async fn wait_started(what: &str, sc: &SyncController) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // Started AND connected, so the SSE mock has really been hit.
    while !matches!(
        sc.handle.lock().await.as_ref(),
        Some(r) if *r.connected_rx.borrow()
    ) {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Boot with no token: sync does not start. A token then becomes available
/// (keychain unblocked / login elsewhere) and the background retry starts sync
/// with no restart and no explicit `ensure_started` call.
#[tokio::test]
async fn retry_starts_sync_when_token_becomes_available() {
    disable_keychain();
    let (state, _tmp) = bootstrap_state();
    let mut server = mockito::Server::new_async().await;
    let url = server.url();
    let register = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-r1"}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    let sse = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer acc-1")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    let sc = controller(&url, &state);
    assert!(!sc.ensure_started().await, "no token yet: boot start fails");
    assert!(sc.event_sender().await.is_none());

    let kick = Arc::new(Notify::new());
    tokio::spawn(Arc::clone(&sc).run_start_retry(FAST, kick));

    // Still no token: retries run but nothing starts.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sc.event_sender().await.is_none());

    scenery_core::auth::save_tokens(&state, &url, "acc-1", "ref-1").unwrap();
    wait_started("sync to start without a restart", &sc).await;

    register.assert_async().await;
    sse.assert_async().await;
}

/// The retry prefers the token the daemon holds in memory over re-reading the
/// token store: with nothing readable on disk, a noted token plus a kick
/// starts sync.
#[tokio::test]
async fn retry_uses_in_memory_token_and_kick() {
    disable_keychain();
    let (state, _tmp) = bootstrap_state();
    let mut server = mockito::Server::new_async().await;
    let url = server.url();
    let _register = server
        .mock("POST", "/api/devices/register")
        .match_header("authorization", "Bearer mem-tok")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-r2"}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    let sse = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer mem-tok")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    let sc = controller(&url, &state);
    // Long delays: only the kick can start it within the test's lifetime.
    let slow = RetryConfig {
        initial: Duration::from_secs(60),
        max: Duration::from_secs(300),
    };
    let kick = Arc::new(Notify::new());
    tokio::spawn(Arc::clone(&sc).run_start_retry(slow, Arc::clone(&kick)));

    sc.note_token(Some("mem-tok".to_string()));
    kick.notify_one();
    wait_started("kicked start", &sc).await;
    sse.assert_async().await;
}

/// A revoked user must never trigger a start attempt, even with a readable
/// token in the store and a token in memory.
#[tokio::test]
async fn revoked_user_gets_no_retries() {
    disable_keychain();
    let (state, _tmp) = bootstrap_state();
    let mut server = mockito::Server::new_async().await;
    let url = server.url();
    let register = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_body(r#"{"device_id":"nope"}"#)
        .expect(0)
        .create_async()
        .await;

    scenery_core::auth::save_tokens(&state, &url, "acc-x", "ref-x").unwrap();
    // After save_tokens, which clears the marker.
    state
        .set_sync_state(USER_REVOKED_KEY, "2026-11-16T00:00:00Z")
        .unwrap();

    let sc = controller(&url, &state);
    sc.note_token(Some("acc-x".to_string()));
    assert_eq!(sc.retry_start_once().await, RetryOutcome::Revoked);

    let kick = Arc::new(Notify::new());
    tokio::spawn(Arc::clone(&sc).run_start_retry(FAST, Arc::clone(&kick)));
    kick.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(sc.event_sender().await.is_none());
    register.assert_async().await;
}
