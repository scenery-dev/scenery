//! Tests for the `discovery_adopted` auto-upload handler.
//!
//! These exercise the guards and the "record pending takeover before
//! uploading" resilience property without needing a real registry — the
//! network call itself is covered by `RegistryClient::adopt_publish`'s own
//! tests in `vectorhawkd-core`.
#![allow(clippy::unwrap_used)]

use std::{fs, sync::Arc};
use tempfile::TempDir;
use vectorhawkd_core::state::AppState;

use super::*;
use crate::managed_paths::ENV_MUTEX;

/// Bootstrap a real `AppState` (full schema) backed by a temp directory.
fn make_state(root: &TempDir) -> Arc<AppState> {
    let root_dir =
        camino::Utf8PathBuf::from_path_buf(root.path().join("vh-root")).expect("utf8 path");
    Arc::new(AppState::bootstrap_in(root_dir).expect("state bootstrap should succeed"))
}

/// Non-`"skill"` kinds are out of scope (plugin/mcp adopt-publish is
/// deferred) — must no-op without touching state or attempting any upload.
#[tokio::test]
async fn handle_discovery_adopted_noop_for_non_skill_kind() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);

    let result = handle_discovery_adopted(
        Arc::clone(&state),
        "https://app.vectorhawk.ai".to_string(),
        "some-mcp".to_string(),
        "mcp".to_string(),
        "/does/not/matter".to_string(),
    )
    .await;

    assert!(result.is_ok(), "non-skill kinds must no-op, not error");
    assert!(
        state
            .pending_adopt_takeover_source("some-mcp")
            .unwrap()
            .is_none(),
        "no pending-takeover record should be created for a non-skill kind"
    );
}

/// If `source_path` is already gone, a previous takeover already completed —
/// this must be a clean idempotent no-op (SSE redelivery after success).
#[tokio::test]
async fn handle_discovery_adopted_noop_when_source_already_removed() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);

    let missing = root
        .path()
        .join(format!("missing-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();

    let result = handle_discovery_adopted(
        Arc::clone(&state),
        "https://app.vectorhawk.ai".to_string(),
        "hello-world".to_string(),
        "skill".to_string(),
        missing,
    )
    .await;

    assert!(
        result.is_ok(),
        "already-removed source_path must be a no-op, not an error"
    );
    assert!(
        state
            .pending_adopt_takeover_source("hello-world")
            .unwrap()
            .is_none(),
        "no pending-takeover record should be created when there's nothing to take over"
    );
}

/// The pending-takeover record must be written *before* the upload attempt,
/// so a crash (or, here, a missing-token failure) mid-flight still leaves
/// enough state for the deferred-approval convergence path to finish the
/// takeover later.
///
/// `#[tokio::test]` defaults to a current-thread runtime and this test has no
/// other concurrent task, so holding a std Mutex across the single `.await`
/// below cannot block a worker pool or deadlock — safe, unlike the general
/// case clippy's lint guards against.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handle_discovery_adopted_records_pending_takeover_before_failing_without_token() {
    // This test never sets the env vars itself, but `handle_discovery_adopted`
    // reads `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` — take the shared
    // guard so a concurrent test toggling that var can't flip the killswitch
    // mid-run and turn this into a spurious early `Ok(())`.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);

    let source_dir = root.path().join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        source_dir.join("SKILL.md"),
        b"---\nname: hello-world\n---\n",
    )
    .unwrap();

    let result = handle_discovery_adopted(
        Arc::clone(&state),
        "https://app.vectorhawk.ai".to_string(),
        "hello-world".to_string(),
        "skill".to_string(),
        source_dir.to_string_lossy().to_string(),
    )
    .await;

    assert!(
        result.is_err(),
        "no stored auth token should fail the upload"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("token") || msg.contains("auth"),
        "error should mention the missing token; got: {msg}"
    );

    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        Some(source_dir.to_string_lossy().to_string()),
        "pending-takeover record must survive an upload failure"
    );
    assert!(
        source_dir.exists(),
        "the original source_path must never be removed on a failed upload"
    );
}

/// The killswitch disables the whole handler, including the pending-takeover
/// record write.
///
/// See the `await_holding_lock` note above — single `.await` on a
/// current-thread runtime, no other concurrent task, so this is safe.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handle_discovery_adopted_noop_under_killswitch() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);

    let source_dir = root.path().join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("SKILL.md"), b"content").unwrap();

    let prev = std::env::var_os(ENV_DISABLE);
    std::env::set_var(ENV_DISABLE, "1");
    let result = handle_discovery_adopted(
        Arc::clone(&state),
        "https://app.vectorhawk.ai".to_string(),
        "hello-world".to_string(),
        "skill".to_string(),
        source_dir.to_string_lossy().to_string(),
    )
    .await;
    if let Some(v) = prev {
        std::env::set_var(ENV_DISABLE, v);
    } else {
        std::env::remove_var(ENV_DISABLE);
    }

    assert!(result.is_ok(), "killswitch must short-circuit cleanly");
    assert!(
        state
            .pending_adopt_takeover_source("hello-world")
            .unwrap()
            .is_none(),
        "killswitch must prevent even recording a pending takeover"
    );
    assert!(source_dir.exists());
}
