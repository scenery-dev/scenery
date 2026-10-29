//! Unit tests for `OAuthState`.

#![allow(clippy::unwrap_used)]

use super::{OAuthState, OAuthStateError};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn subscribe_and_notify_roundtrip() {
    let state = Arc::new(OAuthState::new());

    let rx = state.subscribe("state-abc".to_string()).await.unwrap();
    state
        .notify("state-abc".to_string(), "code-xyz".to_string())
        .await
        .unwrap();

    let (code, returned_state) = timeout(Duration::from_millis(100), rx)
        .await
        .expect("should not time out")
        .expect("channel should not be closed");

    assert_eq!(code, "code-xyz");
    assert_eq!(returned_state, "state-abc");
}

#[tokio::test]
async fn two_concurrent_waiters_are_independent() {
    let hub = Arc::new(OAuthState::new());

    let rx1 = hub.subscribe("state-1".to_string()).await.unwrap();
    let rx2 = hub.subscribe("state-2".to_string()).await.unwrap();

    hub.notify("state-2".to_string(), "code-B".to_string())
        .await
        .unwrap();
    hub.notify("state-1".to_string(), "code-A".to_string())
        .await
        .unwrap();

    let (c1, _) = timeout(Duration::from_millis(100), rx1)
        .await
        .expect("rx1 should not time out")
        .unwrap();
    let (c2, _) = timeout(Duration::from_millis(100), rx2)
        .await
        .expect("rx2 should not time out")
        .unwrap();

    assert_eq!(c1, "code-A");
    assert_eq!(c2, "code-B");
}

#[tokio::test]
async fn double_subscribe_same_state_returns_error() {
    let hub = OAuthState::new();

    let _rx = hub.subscribe("state-dup".to_string()).await.unwrap();
    let result = hub.subscribe("state-dup".to_string()).await;

    assert!(
        matches!(result, Err(OAuthStateError::DuplicateSubscriber(ref s)) if s == "state-dup"),
        "expected DuplicateSubscriber, got {result:?}"
    );
}

#[tokio::test]
async fn notify_without_subscriber_returns_error() {
    let hub = OAuthState::new();
    let result = hub
        .notify("orphan-state".to_string(), "code".to_string())
        .await;

    assert!(
        matches!(result, Err(OAuthStateError::NoSubscriber(ref s)) if s == "orphan-state"),
        "expected NoSubscriber, got {result:?}"
    );
}

#[tokio::test]
async fn cancel_all_closes_receivers() {
    let hub = Arc::new(OAuthState::new());

    let rx1 = hub.subscribe("s1".to_string()).await.unwrap();
    let rx2 = hub.subscribe("s2".to_string()).await.unwrap();

    hub.cancel_all().await;

    // Both receivers should observe a closed channel (RecvError).
    assert!(rx1.await.is_err(), "rx1 should be closed after cancel_all");
    assert!(rx2.await.is_err(), "rx2 should be closed after cancel_all");
}

#[tokio::test]
async fn cancel_all_on_empty_hub_is_no_op() {
    let hub = OAuthState::new();
    // Must not panic or error.
    hub.cancel_all().await;
}

#[tokio::test]
async fn subscribe_allowed_after_notify_consumed_entry() {
    let hub = OAuthState::new();

    let rx = hub.subscribe("reusable-state".to_string()).await.unwrap();
    hub.notify("reusable-state".to_string(), "code1".to_string())
        .await
        .unwrap();
    let _ = rx.await.unwrap();

    // Entry was removed by notify; a new subscribe for the same state should succeed.
    let rx2 = hub.subscribe("reusable-state".to_string()).await;
    assert!(
        rx2.is_ok(),
        "subscribe should succeed after the previous entry was consumed"
    );
}
