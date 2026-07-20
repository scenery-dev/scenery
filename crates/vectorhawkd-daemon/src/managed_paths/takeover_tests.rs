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

// ── Data-loss guard: backup before delete ───────────────────────────────────

/// Adopting a skill directory must copy it byte-for-byte into the restore
/// journal's backup area BEFORE deleting it, and record a journal entry with
/// `source = adopted` pointing at that copy.
#[test]
fn perform_if_pending_backs_up_directory_before_removing_it() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(source_dir.join("prompts")).unwrap();
    fs::write(source_dir.join("SKILL.md"), b"original SKILL.md content").unwrap();
    fs::write(source_dir.join("prompts").join("p.txt"), b"a prompt").unwrap();

    seed_managed_copy(fake_home.path(), "hello-world");
    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "takeover should succeed: {result:?}");
    assert!(
        !source_dir.exists(),
        "original must be removed once backed up"
    );

    // Exactly one journal entry, describing the adopt-takeover backup.
    let journal = RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1, "expected exactly one journal entry");
    let entry = &entries[0];
    assert_eq!(entry.source, JournalSource::Adopted);
    assert_eq!(entry.op, JournalOp::FileDelete);
    assert_eq!(entry.slug.as_deref(), Some("hello-world"));
    assert_eq!(entry.target_path, source_dir.to_string_lossy());

    let backup_path = entry
        .backup_path
        .as_ref()
        .expect("adopt backups must always set a real backup_path");
    let backup_path = std::path::PathBuf::from(backup_path);

    // Byte-identical copy, including the nested file.
    assert_eq!(
        fs::read(backup_path.join("SKILL.md")).unwrap(),
        b"original SKILL.md content"
    );
    assert_eq!(
        fs::read(backup_path.join("prompts").join("p.txt")).unwrap(),
        b"a prompt"
    );

    // Layout contract: <root_dir>/restore-backups/<ts>/adopted/<slug>/.
    assert!(
        backup_path.starts_with(root.path().join("vh-root").join("restore-backups")),
        "backup must live under the journal's restore-backups area: {}",
        backup_path.display()
    );
    assert_eq!(backup_path.file_name().unwrap(), "hello-world");
    assert_eq!(
        backup_path.parent().unwrap().file_name().unwrap(),
        "adopted"
    );
}

/// The single most important guarantee: if the backup step fails for any
/// reason (here, an unwritable/unreachable backup destination), the adopt
/// takeover must ABORT rather than delete the user's original. The pending
/// record must also survive so a later retry can succeed.
#[test]
fn perform_if_pending_aborts_and_preserves_original_when_backup_fails() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let source_dir = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("SKILL.md"), b"irreplaceable original").unwrap();

    seed_managed_copy(fake_home.path(), "hello-world");
    state
        .record_pending_adopt_takeover("hello-world", &source_dir.to_string_lossy())
        .unwrap();

    // Sabotage the backup destination: put a regular FILE where the
    // `restore-backups` directory needs to be created, so every attempt to
    // create a subdirectory under it fails.
    fs::write(
        state.root_dir.join("restore-backups").as_std_path(),
        b"not a dir",
    )
    .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(
        result.is_err(),
        "backup failure must surface as an error, not be swallowed"
    );

    assert!(
        source_dir.exists(),
        "original source_path must survive a failed backup"
    );
    assert_eq!(
        fs::read(source_dir.join("SKILL.md")).unwrap(),
        b"irreplaceable original",
        "original content must be untouched"
    );
    assert_eq!(
        state.pending_adopt_takeover_source("hello-world").unwrap(),
        Some(source_dir.to_string_lossy().to_string()),
        "pending record must remain so a later call can retry"
    );

    // No journal entry should have been recorded for a backup that never
    // completed.
    let journal = RestoreJournal::for_state(&state);
    assert!(journal.read_all().unwrap().is_empty());
}

/// Symlink sources: removing the symlink must not destroy the user's data.
/// The takeover still backs up the symlink's target content (so the record
/// of what once lived at `source_path` is preserved even though the target
/// itself also survives independently), then unlinks just the symlink.
#[test]
fn perform_if_pending_backs_up_symlink_source_before_unlinking() {
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
        "the symlink target must survive independently"
    );

    let journal = RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, JournalSource::Adopted);
    assert_eq!(entries[0].target_path, source_symlink.to_string_lossy());

    let backup_path = std::path::PathBuf::from(entries[0].backup_path.as_ref().unwrap());
    assert_eq!(
        fs::read(backup_path.join("SKILL.md")).unwrap(),
        b"shared content",
        "backup must contain the symlink's target content"
    );
}

/// A dangling symlink (target already gone) has no data behind it — takeover
/// must still succeed by unlinking it directly, without requiring (or being
/// able to perform) a backup.
#[test]
fn perform_if_pending_removes_dangling_symlink_without_backup() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let missing_target = fake_home.path().join("gone").join("hello-world");
    let source_symlink = fake_home.path().join("agents-skills").join("hello-world");
    fs::create_dir_all(source_symlink.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&missing_target, &source_symlink).unwrap();
    assert!(source_symlink.is_symlink() && fs::metadata(&source_symlink).is_err());

    seed_managed_copy(fake_home.path(), "hello-world");
    state
        .record_pending_adopt_takeover("hello-world", &source_symlink.to_string_lossy())
        .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "takeover should succeed: {result:?}");
    assert!(!source_symlink.exists() && !source_symlink.is_symlink());
}

/// Single-file (non-directory) sources must also be backed up before
/// deletion, exercising the `fs::copy` (not `copy_tree_recursive`) path.
#[test]
fn perform_if_pending_backs_up_single_file_source_before_removing_it() {
    let root = tempfile::tempdir().unwrap();
    let state = make_state(&root);
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let source_file = fake_home
        .path()
        .join("agents-skills")
        .join("hello-world.md");
    fs::create_dir_all(source_file.parent().unwrap()).unwrap();
    fs::write(&source_file, b"a lone SKILL.md, no directory wrapper").unwrap();

    seed_managed_copy(fake_home.path(), "hello-world");
    state
        .record_pending_adopt_takeover("hello-world", &source_file.to_string_lossy())
        .unwrap();

    let result = perform_if_pending(&state, "hello-world");
    assert!(result.is_ok(), "takeover should succeed: {result:?}");
    assert!(!source_file.exists(), "original file must be removed");

    let journal = RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, JournalSource::Adopted);
    let backup_path = entries[0].backup_path.as_ref().unwrap();
    assert_eq!(
        fs::read(backup_path).unwrap(),
        b"a lone SKILL.md, no directory wrapper"
    );
}
