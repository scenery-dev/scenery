//! Tests for the `skill install` completion wait.
#![allow(clippy::unwrap_used)]

use super::wait_until;
use std::time::Duration;

#[tokio::test]
async fn returns_once_the_condition_becomes_true() {
    let mut calls = 0;
    wait_until(
        || {
            calls += 1;
            Ok(calls >= 3)
        },
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(calls, 3);
}

#[tokio::test]
async fn times_out_with_an_error_instead_of_false_success() {
    let err = wait_until(
        || Ok(false),
        Duration::from_millis(20),
        Duration::from_millis(5),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
}

#[tokio::test]
async fn propagates_a_check_error() {
    let err = wait_until(
        || anyhow::bail!("boom"),
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("boom"));
}
