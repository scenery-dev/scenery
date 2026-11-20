//! Kill switch vs. backup: a revoke removes the skill even when the safety
//! backup of an edited folder fails; a user-initiated removal fails closed.
#![allow(clippy::unwrap_used)]

use std::fs;

use rusqlite::Connection;
use scenery_core::state::AppState;

use super::*;
use crate::managed_paths::marker::{insert_db_marker, set_marker_provenance};
use crate::managed_paths::ENV_MUTEX;

/// An edited, Scenery-owned skill whose backup cannot be written (a file
/// squats where `restore-backups/` must be created).
fn edited_skill_with_broken_backups() -> (
    ManagedPathsPusher,
    tempfile::TempDir,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap_in(
        camino::Utf8PathBuf::from_path_buf(data.path().to_path_buf()).unwrap(),
    )
    .unwrap();
    let pusher = ManagedPathsPusher::new(&state);
    let dir = home.path().join(".agents/skills/pdf");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), b"original").unwrap();
    let conn = Connection::open(&pusher.db_path).unwrap();
    insert_db_marker(
        &conn,
        &ManagedPathMarker {
            path: dir.to_string_lossy().to_string(),
            kind: "skill".into(),
            slug: "pdf".into(),
            installation_id: Some("inst-1".into()),
            source_sha256: "x".into(),
            migrated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let prov = Provenance {
        origin: Some("auto_managed".into()),
        tree_hash: Some(scenery_core::skill_tree::survey(&dir).unwrap().tree_hash),
        hash_version: Some(crate::managed_paths::marker::HASH_VERSION_TREE),
        ..Default::default()
    };
    set_marker_provenance(&conn, &dir.to_string_lossy(), &prov).unwrap();
    fs::write(dir.join("SKILL.md"), b"user edits").unwrap();
    fs::write(data.path().join("restore-backups"), "blocker").unwrap();
    (pusher, data, home, dir)
}

fn with_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", home);
    let out = f();
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    out
}

#[test]
fn a_failing_backup_during_revoke_still_removes_the_skill() {
    let (pusher, _data, home, dir) = edited_skill_with_broken_backups();
    let out = with_home(home.path(), || pusher.remove_skill_for_revoke("pdf")).unwrap();
    assert!(out.removed);
    assert!(!dir.exists(), "the kill switch wins over the backup");
    let note = out.backup_failure.expect("the failure is reported");
    assert!(note.contains("back up"), "{note}");
}

#[test]
fn a_failing_backup_during_user_removal_keeps_the_skill() {
    let (pusher, _data, home, dir) = edited_skill_with_broken_backups();
    let r = with_home(home.path(), || pusher.remove_skill("pdf"));
    assert!(r.is_err());
    assert!(
        dir.join("SKILL.md").exists(),
        "user edits are not destroyed"
    );
}
