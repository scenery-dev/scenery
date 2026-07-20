//! Tests for directory link management.
//!
//! All tests run against temp directories — they never touch the developer's
//! real `~`.
#![allow(clippy::unwrap_used)]

use super::links::*;
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
    // its own prior work and replace it rather than bailing out.
    let (_tmp, canonical, link_path) = fixture();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"stale copy").unwrap();
    fs::write(
        link_path.join(".vectorhawk-managed.json"),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let mode = link_dir(&canonical, &link_path).unwrap();

    assert_eq!(mode, LinkMode::Symlink);
    assert!(link_is_intact(&canonical, &link_path).unwrap());
    // Content now reflects the canonical directory, not the stale copy.
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"---\nname: demo\n---\n"
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
