//! Tests for the adopt-publish takeover flow (`perform_if_pending`).
//!
//! All tests run against temp directories and never touch the developer's
//! real `~` or `~/.claude/`.
#![allow(clippy::unwrap_used)]

use std::fs;
use tempfile::TempDir;

use super::*;
use vectorhawkd_core::state::AppState;

/// Serializes tests in this file that mutate the process-wide `HOME` env var.
/// Does not coordinate with other test files that also mutate `HOME` — this
/// mirrors the pre-existing (documented) raciness in `pusher_tests.rs` and
/// `publish.rs`'s own test module. Run with `--test-threads=1` if flakes
/// appear from concurrent HOME mutation across files.
static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Bootstrap a real `AppState` (full schema, including
/// `adopt_pending_takeovers`) backed by a temp directory.
fn make_state(root: &TempDir) -> AppState {
    let root_dir =
        camino::Utf8PathBuf::from_path_buf(root.path().join("vh-root")).expect("utf8 path");
    AppState::bootstrap_in(root_dir).expect("state bootstrap should succeed")
}

/// Point `HOME` at `fake_home` and create `~/.claude/skills/<slug>/SKILL.md`
/// so `managed_skill_present(slug)` returns `true`.
fn seed_managed_copy(fake_home: &Path, slug: &str) {
    let skill_dir = fake_home.join(".claude").join("skills").join(slug);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), b"---\nname: x\n---\n").unwrap();
}

struct HomeGuard {
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HomeGuard {
    fn set(path: &Path) -> Self {
        let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", path);
        Self { prev, _lock: lock }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        if let Some(v) = &self.prev {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }
    }
}

// ── perform_if_pending ───────────────────────────────────────────────────────

/// No pending record for the slug — no-op, no error, nothing removed.
#[test]
fn perform_if_pending_noop_when_nothing_pending() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let result = perform_if_pending(&state, "never-adopted");
    assert!(result.is_ok(), "should succeed with nothing to do");
}

/// A pending record exists, but the managed copy has not landed on disk yet
/// (e.g. `outcome == "pending_review"`, still awaiting IT approval). The
/// original source_path must be left untouched and the record must remain.
#[test]
fn perform_if_pending_defers_when_managed_copy_absent() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    // Original discovered source_path — still exists, untouched.
    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("SKILL.md"), b"original content").unwrap();

    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();

    // Intentionally do NOT seed ~/.claude/skills/hello-world/SKILL.md.
    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "deferring is not an error");

    assert!(
        source_dir.exists(),
        "original source_path must survive while the managed copy is absent"
    );
    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        Some(source_dir.to_string_lossy().to_string()),
        "pending record must remain so a later install can retry the takeover"
    );
}

/// The managed copy is confirmed present — the original directory
/// source_path is removed and the pending record is cleared.
#[test]
fn perform_if_pending_removes_source_dir_once_managed_copy_present() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("SKILL.md"), b"original content").unwrap();

    seed_managed_copy(fake_home.path(), "hello-world");

    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "takeover should succeed: {result:?}");

    assert!(
        !source_dir.exists(),
        "original discovered source_path must be removed after takeover"
    );
    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        None,
        "pending record must be cleared after takeover"
    );
}

/// A pre-VectorHawk installer left a *symlink* at the discovered
/// `source_path` (e.g. pointing into a shared marketplace checkout). Takeover
/// must unlink the symlink itself, never follow it and delete the target.
#[test]
fn perform_if_pending_unlinks_symlink_source_without_deleting_target() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let real_target = fake_home
        .path()
        .join("shared-marketplace")
        .join("hello-world");
    fs::create_dir_all(&real_target).unwrap();
    fs::write(real_target.join("SKILL.md"), b"shared content").unwrap();

    let source_symlink = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(source_symlink.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real_target, &source_symlink).unwrap();
    assert!(source_symlink.is_symlink(), "test precondition");

    seed_managed_copy(fake_home.path(), "hello-world");

    state
        .record_pending_adopt_takeover("hello-world", &source_symlink.to_string_lossy())
        .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "takeover should succeed: {result:?}");

    assert!(
        !source_symlink.exists() && !source_symlink.is_symlink(),
        "the symlink itself must be removed"
    );
    assert!(
        real_target.join("SKILL.md").exists(),
        "the symlink target must survive — only the link is owned by this takeover"
    );
}

/// Idempotent: if the original source_path was already removed by a prior
/// successful takeover, re-invoking (e.g. a redelivered SSE event, or a
/// second install completing) must not error.
#[test]
fn perform_if_pending_is_idempotent_when_source_already_removed() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    // Record a pending takeover whose source_path does not (or no longer) exist.
    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();

    seed_managed_copy(fake_home.path(), "hello-world");

    let result = perform_if_pending(&state, "hello-world");
    assert!(
        result.is_ok(),
        "removing an already-absent source_path must not error: {result:?}"
    );
    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        None,
        "pending record should still be cleared"
    );

    // Calling it again must also be a clean no-op (nothing pending anymore).
    let second = perform_if_pending(&state, "hello-world");
    assert!(second.is_ok());
}

/// The killswitch disables the whole flow: even with a pending record and a
/// present managed copy, nothing is removed and the record survives.
#[test]
fn perform_if_pending_noop_under_killswitch() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();
    seed_managed_copy(fake_home.path(), "hello-world");

    let prev = std::env::var_os(ENV_DISABLE);
    std::env::set_var(ENV_DISABLE, "1");
    let result = perform_if_pending(&state, "hello-world");
    if let Some(v) = prev {
        std::env::set_var(ENV_DISABLE, v);
    } else {
        std::env::remove_var(ENV_DISABLE);
    }

    assert!(result.is_ok());
    assert!(source_dir.exists(), "killswitch must prevent removal");
    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        Some(source_dir.to_string_lossy().to_string())
    );
}
