//! Tests for the `skill install` completion wait and its verdict.
#![allow(clippy::unwrap_used)]

use super::{interpret_projection, is_noop_reinstall, poll_until};
use scenery_daemon::managed_paths::projection_outcome::ProjectionOutcome;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test]
async fn returns_once_the_condition_yields() {
    let mut calls = 0;
    let got = poll_until(
        || {
            calls += 1;
            Ok((calls >= 3).then_some("done"))
        },
        Duration::from_secs(5),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(got, Some("done"));
    assert_eq!(calls, 3);
}

#[tokio::test]
async fn times_out_with_none_never_a_false_success() {
    let got: Option<()> = poll_until(
        || Ok(None),
        Duration::from_millis(20),
        Duration::from_millis(5),
    )
    .await
    .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn propagates_a_check_error() {
    let err = poll_until::<(), _>(
        || anyhow::bail!("boom"),
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("boom"));
}

#[test]
fn ok_outcome_is_clean_success() {
    let w = interpret_projection("s", Some(ProjectionOutcome::Ok), false, "/logs").unwrap();
    assert!(w.is_empty());
}

#[test]
fn refused_link_is_a_warning_naming_the_path() {
    let path = PathBuf::from("/h/.claude/skills/s");
    let w = interpret_projection(
        "s",
        Some(ProjectionOutcome::LinkRefused { path }),
        true,
        "/logs",
    )
    .unwrap();
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("/h/.claude/skills/s"), "{w:?}");
}

#[test]
fn refused_push_is_an_error_naming_the_path() {
    let path = PathBuf::from("/h/.agents/skills/s");
    let err = interpret_projection(
        "s",
        Some(ProjectionOutcome::PushRefused { path }),
        false,
        "/logs",
    )
    .unwrap_err();
    assert!(err.to_string().contains("/h/.agents/skills/s"), "{err}");
}

#[test]
fn daemon_silence_is_an_error_pointing_at_the_daemon_log() {
    let err = interpret_projection("s", None, false, "/state/logs").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("never reported") && msg.contains("/state/logs"),
        "{msg}"
    );
}

#[test]
fn daemon_silence_with_canonical_files_on_disk_still_succeeds_with_a_warning() {
    let w = interpret_projection("s", None, true, "/state/logs").unwrap();
    assert_eq!(w.len(), 1);
}

// ── no-op reinstall fast path ────────────────────────────────────────────────

#[test]
fn unpinned_install_that_the_backend_upgrades_is_not_a_noop() {
    // `skill install foo` (no --version): the backend resolves latest 2.0.0
    // while 1.0.0 is active, so the install is an upgrade and must be awaited.
    assert!(!is_noop_reinstall(Some("1.0.0"), Some("2.0.0")));
}

#[test]
fn install_of_the_active_version_is_a_noop() {
    assert!(is_noop_reinstall(Some("2.0.0"), Some("2.0.0")));
}

#[test]
fn unknown_target_or_nothing_active_is_never_a_noop() {
    // No --version and an unreadable backend response: take the poll path.
    assert!(!is_noop_reinstall(Some("1.0.0"), None));
    assert!(!is_noop_reinstall(None, Some("1.0.0")));
}
