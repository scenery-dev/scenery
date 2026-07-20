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

/// The `.vectorhawk-managed.json` sidecar the pusher writes into every
/// canonical skill dir, which `copy_tree` necessarily carries into a copy.
const MARKER: &[u8] = br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#;

#[test]
fn link_dir_does_not_churn_backups_when_the_copy_already_matches_canonical() {
    // The unbounded-backup bug: on a copy-mode host (Windows without Developer
    // Mode) `link_dir` tore down its own byte-identical copy into a fresh
    // timestamped backup on *every* call — and `push_skill` calls it every
    // push, `migrate_skills_to_agents_dir` every daemon start. The identity
    // check makes an already-correct copy a no-op, so nothing accumulates.
    //
    // This is testable without Windows because the identity path is taken
    // before any symlink is attempted.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, canonical, link_path) = fixture();
    fs::write(canonical.join(".vectorhawk-managed.json"), MARKER).unwrap();

    // Materialise `link_path` as a byte-identical copy of canonical — exactly
    // what the copy fallback leaves behind.
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"---\nname: demo\n---\n").unwrap();
    fs::write(link_path.join(".vectorhawk-managed.json"), MARKER).unwrap();

    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let modes: Vec<_> = (0..5)
        .map(|_| link_dir(&canonical, &link_path).unwrap())
        .collect();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        modes.iter().all(|m| *m == LinkMode::Copy),
        "an identical copy must be reported as a settled Copy, not re-materialised: {modes:?}"
    );
    assert!(
        !fake_home
            .path()
            .join(".claude")
            .join(".vectorhawk-backup")
            .exists(),
        "an identical copy must never be backed up — this is the unbounded churn bug"
    );
    assert!(!link_path.is_symlink(), "the copy must be left in place");
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"---\nname: demo\n---\n"
    );
}

#[test]
fn link_dir_still_replaces_a_copy_that_has_diverged() {
    // The identity check must not suppress healing: once the copy differs from
    // canonical (any push updates canonical), the full backup-and-relink path
    // runs, so a stale copy can never persist past the next content change.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, canonical, link_path) = fixture();
    fs::write(canonical.join(".vectorhawk-managed.json"), MARKER).unwrap();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"---\nname: demo\n---\nOLD").unwrap();
    fs::write(link_path.join(".vectorhawk-managed.json"), MARKER).unwrap();

    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = link_dir(&canonical, &link_path);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(result.unwrap(), LinkMode::Symlink);
    assert!(link_is_intact(&canonical, &link_path).unwrap());
    let backed_up = find_link_backup(fake_home.path(), "demo")
        .expect("a diverged copy must still be backed up before removal");
    assert_eq!(
        fs::read(backed_up.join("SKILL.md")).unwrap(),
        b"---\nname: demo\n---\nOLD"
    );
}

#[test]
fn link_dir_treats_a_copy_with_an_extra_file_as_diverged() {
    // Same-name/same-content is not enough: a superset must not read as
    // identical, or content the copy alone carries would be silently kept.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, canonical, link_path) = fixture();
    fs::write(canonical.join(".vectorhawk-managed.json"), MARKER).unwrap();
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"---\nname: demo\n---\n").unwrap();
    fs::write(link_path.join(".vectorhawk-managed.json"), MARKER).unwrap();
    fs::write(link_path.join("extra.txt"), b"only in the copy").unwrap();

    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = link_dir(&canonical, &link_path);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(result.unwrap(), LinkMode::Symlink);
    let backed_up =
        find_link_backup(fake_home.path(), "demo").expect("a diverged copy must be backed up");
    assert_eq!(
        fs::read(backed_up.join("extra.txt")).unwrap(),
        b"only in the copy",
        "the extra file must be recoverable, not dropped"
    );
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

/// **Regression (unlink_dir / link_dir ownership disagreement).**
///
/// `link_dir` refuses to replace a real, unmarked directory at the link path
/// — that is user content. `unlink_dir` is its inverse and must agree: drift
/// can legitimately classify a user directory at `~/.claude/skills/<slug>` as
/// `link_replaced`, and a policy-driven `remove_skill` then calls
/// `unlink_dir` on it. Deleting it there would destroy user-authored content
/// that `link_dir` had just finished protecting one function over.
#[test]
fn unlink_dir_refuses_to_delete_an_unmanaged_real_dir() {
    let (_tmp, canonical, link_path) = fixture();

    // A user's own skill directory sitting where the Claude link would go —
    // no `.vectorhawk-managed.json`, so not ours.
    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"user-authored, never ours").unwrap();

    // `link_dir` already refuses this exact directory — the two must agree.
    assert!(
        link_dir(&canonical, &link_path).is_err(),
        "precondition: link_dir refuses an unmarked real dir here"
    );

    let result = unlink_dir(&link_path);

    assert!(
        result.is_err(),
        "unlink_dir must refuse an unmanaged real directory, not delete it"
    );
    assert!(
        link_path.join("SKILL.md").exists(),
        "BUG: unlink_dir deleted user-authored content at {}",
        link_path.display()
    );
    assert_eq!(
        fs::read(link_path.join("SKILL.md")).unwrap(),
        b"user-authored, never ours",
        "user content must be byte-for-byte untouched"
    );
}

/// The `LinkMode::Copy` steady state: a real directory at the link path that
/// VectorHawk *did* materialise (it carries the marker). `unlink_dir` may
/// remove it — but, like every other destructive step on this branch, only
/// after backing it up, since a marker proves we wrote it, not that its
/// current content is still reproducible.
#[test]
fn unlink_dir_backs_up_a_managed_real_dir_before_removing_it() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _canonical, link_path) = fixture();

    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &fake_home);

    fs::create_dir_all(&link_path).unwrap();
    fs::write(link_path.join("SKILL.md"), b"a copy-mode materialisation").unwrap();
    fs::write(link_path.join(".vectorhawk-managed.json"), b"{}").unwrap();

    let result = unlink_dir(&link_path);

    let backup_root = fake_home.join(".claude").join(".vectorhawk-backup");
    let recovered: Vec<_> = fs::read_dir(&backup_root)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path().join("links").join("demo").join("SKILL.md"))
                .filter(|p| p.exists())
                .collect()
        })
        .unwrap_or_default();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    result.unwrap();
    assert!(!link_path.exists(), "the managed copy must be removed");
    assert_eq!(
        recovered.len(),
        1,
        "exactly one recoverable backup must have been written under {}",
        backup_root.display()
    );
    assert_eq!(
        fs::read(&recovered[0]).unwrap(),
        b"a copy-mode materialisation",
        "the backup must hold the removed content verbatim"
    );
}
