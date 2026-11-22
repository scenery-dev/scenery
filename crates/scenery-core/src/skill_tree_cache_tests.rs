#![allow(clippy::unwrap_used)]

use super::*;
use std::time::Duration;

fn skill(dir: &Path) {
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::write(dir.join("SKILL.md"), "---\nname: demo\n---\nbody\n").unwrap();
    fs::write(dir.join("references/a.md"), "alpha").unwrap();
}

/// Each edit must land on a strictly newer timestamp than the cached stamp
/// even on a filesystem with coarse mtimes.
fn tick() {
    std::thread::sleep(Duration::from_millis(20));
}

#[test]
fn unchanged_tree_is_read_once() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::ZERO);
    let a = cache.survey(tmp.path()).unwrap();
    let s1 = cache.scan_secrets(tmp.path()).unwrap();
    assert_eq!(cache.reads(), 2);
    for _ in 0..5 {
        let b = cache.survey(tmp.path()).unwrap();
        assert_eq!(b.tree_hash, a.tree_hash);
        assert_eq!(cache.scan_secrets(tmp.path()).unwrap(), s1);
    }
    assert_eq!(cache.reads(), 2, "an unchanged tree must not be re-read");
}

#[test]
fn same_size_content_edit_is_seen() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::ZERO);
    let before = cache.survey(tmp.path()).unwrap().tree_hash;
    tick();
    fs::write(tmp.path().join("references/a.md"), "omega").unwrap();
    let after = cache.survey(tmp.path()).unwrap().tree_hash;
    assert_ne!(before, after);
    assert_eq!(after, skill_tree::survey(tmp.path()).unwrap().tree_hash);
}

#[test]
fn edit_with_restored_mtime_is_still_seen() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let f = tmp.path().join("references/a.md");
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();
    let cache = TreeCache::new(Duration::ZERO);
    let before = cache.survey(tmp.path()).unwrap().tree_hash;
    tick();
    fs::write(&f, "omega").unwrap();
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let after = cache.survey(tmp.path()).unwrap().tree_hash;
    if cfg!(unix) {
        assert_ne!(before, after, "ctime must catch a restored mtime");
    }
}

#[test]
fn added_file_new_secret_and_exec_bit_are_seen() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::ZERO);
    let h0 = cache.survey(tmp.path()).unwrap().tree_hash;
    assert!(cache.scan_secrets(tmp.path()).unwrap().is_empty());
    tick();
    fs::write(
        tmp.path().join("references/b.md"),
        "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
    )
    .unwrap();
    let h1 = cache.survey(tmp.path()).unwrap().tree_hash;
    assert_ne!(h0, h1);
    assert_eq!(
        cache.scan_secrets(tmp.path()).unwrap(),
        vec!["references/b.md".to_string()]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tick();
        let p = tmp.path().join("references/a.md");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(cache.survey(tmp.path()).unwrap().tree_hash, h1);
    }
}

#[test]
fn deleted_file_is_seen() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::ZERO);
    let h0 = cache.survey(tmp.path()).unwrap().tree_hash;
    fs::remove_file(tmp.path().join("references/a.md")).unwrap();
    assert_ne!(cache.survey(tmp.path()).unwrap().tree_hash, h0);
}

#[test]
fn recently_touched_tree_is_never_memoised() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::from_secs(3600));
    cache.survey(tmp.path()).unwrap();
    cache.survey(tmp.path()).unwrap();
    assert_eq!(
        cache.reads(),
        2,
        "inside the racy window every call re-reads"
    );
}

#[test]
fn missing_dir_is_an_error_not_a_hit() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = TreeCache::new(Duration::ZERO);
    assert!(cache.survey(&tmp.path().join("nope")).is_err());
}

#[test]
fn one_shared_stamp_serves_survey_and_secret_scan_and_sees_edits() {
    let tmp = tempfile::tempdir().unwrap();
    skill(tmp.path());
    let cache = TreeCache::new(Duration::ZERO);
    let st = cache.settled_stamp(tmp.path());
    assert!(st.is_some());
    let a = cache.survey_with(tmp.path(), st).unwrap();
    let s = cache.scan_secrets_with(tmp.path(), st).unwrap();
    assert_eq!(cache.reads(), 2);
    let st2 = cache.settled_stamp(tmp.path());
    assert_eq!(
        cache.survey_with(tmp.path(), st2).unwrap().tree_hash,
        a.tree_hash
    );
    assert_eq!(cache.scan_secrets_with(tmp.path(), st2).unwrap(), s);
    assert_eq!(cache.reads(), 2, "a shared stamp still hits the memo");

    tick();
    fs::write(tmp.path().join("references/a.md"), "BETA!").unwrap();
    let st3 = cache.settled_stamp(tmp.path());
    assert_ne!(
        cache.survey_with(tmp.path(), st3).unwrap().tree_hash,
        a.tree_hash
    );

    // The global wrappers agree with the plain cached calls.
    let g = settle(tmp.path());
    assert_eq!(
        survey_at(tmp.path(), &g).unwrap().tree_hash,
        survey_cached(tmp.path()).unwrap().tree_hash
    );
}
