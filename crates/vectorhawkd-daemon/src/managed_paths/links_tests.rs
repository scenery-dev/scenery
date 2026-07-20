//! Tests for directory link management.
//!
//! All tests run against temp directories — they never touch the developer's
//! real `~`.
#![allow(clippy::unwrap_used)]

use super::links::*;
use crate::managed_paths::ENV_MUTEX;
use std::fs;
use tempfile::TempDir;

/// Build a canonical skill dir containing a SKILL.md, plus an empty dir to
/// link into. Returns (tmp, canonical, link_path).
fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().join("agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"---\nname: demo\n---\n").unwrap();
    let link_root = tmp.path().join("claude/skills");
    fs::create_dir_all(&link_root).unwrap();
    (tmp, canonical, link_root.join("demo"))
}

#[test]
fn link_dir_creates_symlink_that_resolves_to_canonical() {
    let (_tmp, canonical, link_path) = fixture();

    let mode = link_dir(&canonical, &link_path).unwrap();

    assert_eq!(mode, LinkMode::Symlink);
    assert!(link_path.is_symlink());
    assert!(link_path.join("SKILL.md").exists());
    assert!(link_is_intact(&canonical, &link_path).unwrap());
}

#[test]
fn link_dir_is_idempotent() {
    let (_tmp, canonical, link_path) = fixture();

    link_dir(&canonical, &link_path).unwrap();
    let mode = link_dir(&canonical, &link_path).unwrap();

    assert_eq!(mode, LinkMode::Symlink);
    assert!(link_is_intact(&canonical, &link_path).unwrap());
}

#[test]
fn link_dir_replaces_a_stale_link_pointing_elsewhere() {
    let (tmp, canonical, link_path) = fixture();
    let wrong = tmp.path().join("agents/skills/other");
    fs::create_dir_all(&wrong).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&wrong, &link_path).unwrap();

    let mode = link_dir(&canonical, &link_path).unwrap();

    assert_eq!(mode, LinkMode::Symlink);
    assert!(link_is_intact(&canonical, &link_path).unwrap());
}

#[test]
fn link_dir_refuses_to_clobber_a_real_directory() {
    // This also covers the "real directory without our marker" case for
    // idempotency: it proves the reject path leaves user content byte-for-
    // byte untouched, so a marker-less directory is never mistaken for our
    // own prior Copy-mode materialisation.
    let (_tmp, canonical, link_path) = fixture();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"user content").unwrap();

    let err = link_dir(&canonical, &link_path).unwrap_err();

    assert!(err.to_string().contains("real directory"));
    // The user's content survives untouched.
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"user content"
    );
}

#[test]
fn link_dir_replaces_its_own_prior_copy_materialisation() {
    // Simulates the Windows-without-Developer-Mode fallback: `link_path` is
    // a real directory (not a symlink) carrying the
    // `.vectorhawk-managed.json` marker that `copy_tree` would have copied
    // in from the canonical directory. `link_dir` must recognise this as
    // its own prior work and replace it rather than bailing out — and must
    // back the directory up first, since the marker proves only that
    // VectorHawk wrote it, not that its content is still reproducible.
    //
    // `$HOME` is redirected because the backup root is derived from it; the
    // shared mutex keeps that from racing other env-mutating tests.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, canonical, link_path) = fixture();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"stale copy").unwrap();
    fs::write(
        link_path.join(".vectorhawk-managed.json"),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = link_dir(&canonical, &link_path);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    let mode = result.unwrap();
    assert_eq!(mode, LinkMode::Symlink);
    assert!(link_is_intact(&canonical, &link_path).unwrap());
    // Content now reflects the canonical directory, not the stale copy.
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"---\nname: demo\n---\n"
    );

    // The replaced directory is recoverable from the backup root, under the
    // same `.vectorhawk-backup/<ts>/` convention the F1 migrator uses.
    let backed_up = find_link_backup(fake_home.path(), "demo")
        .expect("link_dir must back up the directory it removes");
    assert_eq!(fs::read(backed_up.join("SKILL.md")).unwrap(), b"stale copy");

    drop(tmp);
}

/// Locate `~/.claude/.vectorhawk-backup/<ts>/links/<name>` without hardcoding
/// the run timestamp.
fn find_link_backup(home: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let base = home.join(".claude").join(".vectorhawk-backup");
    for run in fs::read_dir(base).ok()? {
        let candidate = run.ok()?.path().join("links").join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn link_dir_refuses_to_remove_when_the_backup_cannot_be_made() {
    // If the directory cannot be preserved, the removal must not happen:
    // a failed relink (Claude Code still sees the old content) is strictly
    // better than an unrecoverable delete. `$HOME` is pointed at a path that
    // cannot be created as a directory, so the backup root creation fails.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, canonical, link_path) = fixture();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"stale copy").unwrap();
    fs::write(
        link_path.join(".vectorhawk-managed.json"),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let blocker = tempfile::tempdir().unwrap();
    let not_a_dir = blocker.path().join("home-is-a-file");
    fs::write(&not_a_dir, b"x").unwrap();

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &not_a_dir);

    let result = link_dir(&canonical, &link_path);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(result.is_err(), "backup failure must abort the removal");
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"stale copy",
        "the directory must survive a failed backup"
    );
}

#[test]
fn link_is_intact_is_false_when_link_is_missing() {
    let (_tmp, canonical, link_path) = fixture();
    assert!(!link_is_intact(&canonical, &link_path).unwrap());
}

#[test]
fn link_is_intact_is_false_when_replaced_by_a_real_dir() {
    let (_tmp, canonical, link_path) = fixture();
    fs::create_dir_all(&link_path).unwrap();
    assert!(!link_is_intact(&canonical, &link_path).unwrap());
}

#[test]
fn unlink_dir_removes_the_link_but_not_the_canonical_dir() {
    let (_tmp, canonical, link_path) = fixture();
    link_dir(&canonical, &link_path).unwrap();

    unlink_dir(&link_path).unwrap();

    assert!(!link_path.exists());
    assert!(!link_path.is_symlink());
    assert!(canonical.join("SKILL.md").exists());
}

#[test]
fn unlink_dir_is_idempotent_when_absent() {
    let (_tmp, _canonical, link_path) = fixture();
    unlink_dir(&link_path).unwrap();
    unlink_dir(&link_path).unwrap();
}
