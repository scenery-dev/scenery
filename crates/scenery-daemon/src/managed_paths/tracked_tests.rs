#![allow(clippy::unwrap_used)]

use super::*;
use crate::managed_paths::ENV_MUTEX;
use std::fs;

struct Home {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: AppState,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = AppState::bootstrap_in(root).unwrap();
        Self {
            _tmp: tmp,
            home,
            state,
            prev,
            _lock: lock,
        }
    }
    fn agents(&self) -> PathBuf {
        self.home.join(".agents/skills")
    }
    fn claude(&self) -> PathBuf {
        self.home.join(".claude/skills")
    }
    fn pi(&self) -> PathBuf {
        self.home.join(".pi/agent/skills")
    }
    fn track(&self, path: &Path, slug: &str, skill_id: Option<&str>) -> TrackedRow {
        upsert(
            &self.state,
            &path.to_string_lossy(),
            slug,
            "git_tracked",
            skill_id,
            Some("uuid-1"),
            Some("0.1.0"),
            "hash",
        )
        .unwrap();
        get(&self.state, &path.to_string_lossy()).unwrap().unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn write_skill(dir: &Path, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("SKILL.md"), body).unwrap();
    fs::write(dir.join("notes.md"), "keep").unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_skill_neutralize_removes_every_link_and_never_touches_the_repo() {
    let h = Home::new();
    let repo = h.home.join("code/dev");
    write_skill(&repo, "repo skill");
    fs::create_dir_all(h.agents()).unwrap();
    fs::create_dir_all(h.claude()).unwrap();
    let tracked = h.agents().join("dev");
    std::os::unix::fs::symlink(&repo, &tracked).unwrap();
    // The user's own chain: Claude's entry points at the agents entry.
    std::os::unix::fs::symlink(&tracked, h.claude().join("dev")).unwrap();
    let row = h.track(&tracked, "dev", Some("adam~dev"));

    let report = neutralize(&h.state, &row, REASON_SKILL_REVOKED).unwrap();
    assert_eq!(report.links_removed, 2, "agents link and the claude link");
    assert!(fs::symlink_metadata(&tracked).is_err());
    assert!(fs::symlink_metadata(h.claude().join("dev")).is_err());
    assert_eq!(fs::read(repo.join("SKILL.md")).unwrap(), b"repo skill");
    assert_eq!(fs::read(repo.join("notes.md")).unwrap(), b"keep");

    // Idempotent.
    assert_eq!(
        neutralize(&h.state, &row, REASON_SKILL_REVOKED)
            .unwrap()
            .changed(),
        0
    );

    // Unrevoke / uninstall: links come back exactly.
    let restored = restore_all(&h.state).unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(fs::read_link(&tracked).unwrap(), repo);
    assert_eq!(fs::read_link(h.claude().join("dev")).unwrap(), tracked);
    assert!(!get(&h.state, &row.path).unwrap().unwrap().neutralized);
}

#[cfg(unix)]
#[test]
fn real_directory_neutralize_renames_skill_md_with_an_identical_backup() {
    let h = Home::new();
    let dir = h.pi().join("gitskill");
    write_skill(&dir, "---\nname: gitskill\n---\nthe original\n");
    fs::create_dir_all(dir.join(".git")).unwrap();
    fs::create_dir_all(h.claude()).unwrap();
    std::os::unix::fs::symlink(&dir, h.claude().join("gitskill")).unwrap();
    let row = h.track(&dir, "gitskill", Some("adam~gitskill"));

    let report = neutralize(&h.state, &row, REASON_SKILL_REVOKED).unwrap();
    assert_eq!(report.skill_md_disabled, 1);
    assert_eq!(report.links_removed, 1, "the claude link resolving to it");
    assert!(!dir.join("SKILL.md").exists());
    let disabled = dir.join("SKILL.md.scenery-disabled");
    assert_eq!(
        fs::read(&disabled).unwrap(),
        b"---\nname: gitskill\n---\nthe original\n"
    );
    assert_eq!(fs::read(dir.join("notes.md")).unwrap(), b"keep");
    assert!(dir.join(".git").is_dir(), "a git repo is never touched");

    // A byte-identical copy was saved first.
    let rec_backup: String = Connection::open(&h.state.db_path)
        .unwrap()
        .query_row(
            "SELECT backup_path FROM tracked_neutralizations WHERE action = 'rename'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        fs::read(Path::new(&rec_backup).join("SKILL.md")).unwrap(),
        fs::read(&disabled).unwrap()
    );
    assert!(rec_backup.contains("restore-backups") && rec_backup.contains("adopted"));

    restore_all(&h.state).unwrap();
    assert_eq!(
        fs::read(dir.join("SKILL.md")).unwrap(),
        b"---\nname: gitskill\n---\nthe original\n"
    );
    assert!(!disabled.exists());
    assert!(fs::read_link(h.claude().join("gitskill")).is_ok());
}

#[test]
fn an_existing_disabled_file_is_never_overwritten() {
    let h = Home::new();
    let dir = h.agents().join("again");
    write_skill(&dir, "v1");
    fs::write(dir.join("SKILL.md.scenery-disabled"), "older disabled").unwrap();
    let row = h.track(&dir, "again", Some("adam~again"));
    neutralize(&h.state, &row, REASON_SKILL_REVOKED).unwrap();
    assert_eq!(
        fs::read(dir.join("SKILL.md.scenery-disabled")).unwrap(),
        b"older disabled"
    );
    assert_eq!(
        fs::read(dir.join("SKILL.md.scenery-disabled.1")).unwrap(),
        b"v1"
    );
}

#[test]
fn a_scenery_managed_dir_with_the_same_name_is_left_alone() {
    let h = Home::new();
    let tracked = h.pi().join("pdf");
    write_skill(&tracked, "mine");
    let managed = h.agents().join("pdf");
    write_skill(&managed, "catalog pdf");
    fs::write(managed.join(".scenery-managed.json"), "{}").unwrap();
    let row = h.track(&tracked, "pdf", Some("adam~pdf"));
    neutralize(&h.state, &row, REASON_SKILL_REVOKED).unwrap();
    assert!(!tracked.join("SKILL.md").exists());
    assert_eq!(fs::read(managed.join("SKILL.md")).unwrap(), b"catalog pdf");
}

#[test]
fn revocation_matches_by_registry_skill_id_never_by_bare_slug() {
    let h = Home::new();
    let dir = h.agents().join("notes");
    write_skill(&dir, "x");
    h.track(&dir, "notes", Some("adam~notes"));

    // Revoking the catalog skill `notes` (or another user's `bob~notes`) must
    // not reach adam's tracked `notes`.
    assert!(!on_skill_revoked(&h.state, "notes", None));
    assert!(!on_skill_revoked(&h.state, "bob~notes", None));
    assert!(dir.join("SKILL.md").exists());

    // A version-scoped revocation of a different version does not apply.
    assert!(!on_skill_revoked(&h.state, "adam~notes", Some("9.9.9")));
    assert!(dir.join("SKILL.md").exists());

    assert!(on_skill_revoked(&h.state, "adam~notes", None));
    assert!(!dir.join("SKILL.md").exists());
    assert!(dir.join("SKILL.md.scenery-disabled").exists());
}

#[test]
fn reconcile_follows_the_revocation_cache_and_unrevoke_restores() {
    let h = Home::new();
    let dir = h.agents().join("flow");
    write_skill(&dir, "x");
    h.track(&dir, "flow", Some("adam~flow"));

    reconcile(&h.state);
    assert!(dir.join("SKILL.md").exists(), "not revoked: untouched");

    scenery_core::revocation::upsert_revocation(&h.state, "skill", "adam~flow", None).unwrap();
    reconcile(&h.state);
    assert!(!dir.join("SKILL.md").exists());

    scenery_core::revocation::clear_revocation(&h.state, "skill", "adam~flow", None).unwrap();
    reconcile(&h.state);
    assert_eq!(fs::read(dir.join("SKILL.md")).unwrap(), b"x");
}

#[test]
fn a_recreated_revoked_skill_is_neutralized_again_with_an_audit_event() {
    let h = Home::new();
    let dir = h.agents().join("zombie");
    write_skill(&dir, "v1");
    h.track(&dir, "zombie", Some("adam~zombie"));
    scenery_core::revocation::upsert_revocation(&h.state, "skill", "adam~zombie", None).unwrap();
    reconcile(&h.state);
    assert!(!dir.join("SKILL.md").exists());

    // The other installer puts it back.
    fs::write(dir.join("SKILL.md"), "v2 from the installer").unwrap();
    reconcile(&h.state);
    assert!(!dir.join("SKILL.md").exists());
    assert_eq!(
        fs::read(dir.join("SKILL.md.scenery-disabled.1")).unwrap(),
        b"v2 from the installer"
    );
    let n: i64 = Connection::open(&h.state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'revoked_skill_reappeared'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn user_and_device_revocation_neutralize_every_tracked_skill() {
    let h = Home::new();
    let a = h.agents().join("a");
    let b = h.pi().join("b");
    write_skill(&a, "a");
    write_skill(&b, "b");
    h.track(&a, "a", Some("adam~a"));
    h.track(&b, "b", None); // never got an id: still covered by user revoke

    h.state
        .set_sync_state(scenery_core::auth::USER_REVOKED_KEY, "now")
        .unwrap();
    reconcile(&h.state);
    assert!(!a.join("SKILL.md").exists() && !b.join("SKILL.md").exists());

    // Reinstated: the restriction lifts and both come back.
    h.state
        .delete_sync_state(scenery_core::auth::USER_REVOKED_KEY)
        .unwrap();
    reconcile(&h.state);
    assert!(a.join("SKILL.md").exists() && b.join("SKILL.md").exists());

    h.state.mark_device_revoked().unwrap();
    reconcile(&h.state);
    assert!(!a.join("SKILL.md").exists());
}

#[test]
fn tracked_slug_lookup_is_case_folded() {
    let h = Home::new();
    let dir = h.agents().join("Mixed");
    write_skill(&dir, "x");
    h.track(&dir, "Mixed", None);
    assert!(is_slug_tracked(&h.state, "mixed"));
    assert!(!is_slug_tracked(&h.state, "other"));
}
