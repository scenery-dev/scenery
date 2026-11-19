//! Phase 9a runner tests: whole-tree drift, own edits as private versions,
//! catalog edits as branches, strict modes, debounce, secrets, retry, v1
//! re-baseline, authorship from the snapshot. A mock registry and a fake
//! `$HOME`; the pusher, drift scanner and marker code are the production code.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::{
    drift::{DriftScanner, DriftStatus},
    marker::marker_provenance,
    pusher::ManagedPathsPusher,
    ENV_MUTEX,
};
use mockito::{Matcher, Server, ServerGuard};
use std::fs;

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    server: ServerGuard,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

const STORED_OWN: &str = r#"{"outcome":"stored","skill_id":"adam~mine","authorship":"own","branch_state":"own","version":"0.1.0+abc","version_id":"vid-1","head_version":"0.1.0","conflict":false,"update_available":false,"scan":{"verdict":"clean"}}"#;
const STORED_BRANCH: &str = r#"{"outcome":"stored","skill_id":"cat","authorship":"catalog","branch_state":"branch","version":"1.0.0+local.x.abc","version_id":"vid-2","head_version":"1.0.0","conflict":false,"update_available":false,"scan":{"verdict":"medium"}}"#;

impl Fx {
    async fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
        // Edits made by the test are "settled" at once unless a test says otherwise.
        std::env::set_var("SCENERY_LOCAL_EDIT_SETTLE_SECS", "0");
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        let server = Server::new_async().await;
        scenery_core::auth::save_tokens(&state, &server.url(), "tok", "refresh").unwrap();
        state.set_sync_state("device_id", "dev-1").unwrap();
        Self {
            _tmp: tmp,
            home,
            state,
            server,
            prev_home,
            _lock: lock,
        }
    }

    /// Push `slug` the way the reconciler does; `own` stamps auto_managed
    /// provenance (and registry id `adam~<slug>`) through a pending takeover.
    fn push(&self, slug: &str, own: bool) -> PathBuf {
        let dir = self.home.join(".agents/skills").join(slug);
        if own {
            self.state
                .record_pending_auto_managed_takeover(
                    slug,
                    &dir.to_string_lossy(),
                    &format!("adam~{slug}"),
                    "uuid-1",
                )
                .unwrap();
        }
        ManagedPathsPusher::new(&self.state)
            .push_skill(
                slug,
                Some("inst-1"),
                b"---\nname: x\ndescription: d\n---\noriginal",
                &[("prompts/p.md".to_string(), b"my prompt".to_vec())],
            )
            .unwrap();
        dir
    }

    fn set_mode(&self, mode: &str) {
        self.state
            .set_sync_state("managed_paths_mode", mode)
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.state.db_path).unwrap()
    }

    async fn cycle(&self) -> crate::managed_paths::drift::CycleStats {
        DriftScanner::new(Arc::clone(&self.state), self.server.url())
            .unwrap()
            .run_cycle()
            .await
            .unwrap()
    }

    fn scan(&self) -> Vec<crate::managed_paths::drift::DriftOutcome> {
        DriftScanner::new(Arc::clone(&self.state), self.server.url())
            .unwrap()
            .scan_once()
            .unwrap()
    }

    async fn drift_sink(&mut self) {
        self.server
            .mock("POST", "/portal/managed-paths/drift")
            .with_status(200)
            .with_body("{}")
            .expect_at_least(0)
            .create_async()
            .await;
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        std::env::remove_var("SCENERY_LOCAL_EDIT_SETTLE_SECS");
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn tree_of(dir: &Path) -> String {
    scenery_core::skill_tree::survey(dir).unwrap().tree_hash
}

// ── whole-tree drift ─────────────────────────────────────────────────────────

#[tokio::test]
async fn tree_hash_drift_sees_a_reference_file_edit_an_added_and_a_removed_file() {
    let f = Fx::new().await;
    let dir = f.push("mine", true);
    assert!(f.scan().iter().all(|o| o.status == DriftStatus::Clean));
    let prov = marker_provenance(&f.conn(), &dir.to_string_lossy());
    assert_eq!(prov.hash_version, Some(2));
    assert_eq!(prov.tree_hash.as_deref(), Some(tree_of(&dir).as_str()));

    // a reference file (SKILL.md untouched: v1 drift would have missed this)
    fs::write(dir.join("prompts/p.md"), "changed").unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::EditedOwn);
    fs::write(dir.join("prompts/p.md"), "my prompt").unwrap();
    assert_eq!(
        f.scan()[0].status,
        DriftStatus::Clean,
        "restoring the bytes is clean again"
    );

    // an added file
    fs::write(dir.join("extra.md"), "new").unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::EditedOwn);
    fs::remove_file(dir.join("extra.md")).unwrap();
    // a removed file
    fs::remove_file(dir.join("prompts/p.md")).unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::EditedOwn);
    // noise never counts
    fs::write(dir.join("prompts/p.md"), "my prompt").unwrap();
    fs::write(dir.join(".DS_Store"), "x").unwrap();
    fs::write(dir.join("notes.md.swp"), "x").unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::Clean);
    // SKILL.md gone is DELETED
    fs::remove_file(dir.join("SKILL.md")).unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::Deleted);
}

#[tokio::test]
async fn a_v1_marker_is_rebaselined_once_if_skill_md_still_matches_else_it_is_an_edit() {
    let f = Fx::new().await;
    let a = f.push("untouched", false);
    let b = f.push("edited", false);
    // make both v1: no tree hash, no hash version
    f.conn()
        .execute(
            "UPDATE managed_path_markers SET tree_hash = NULL, hash_version = NULL",
            [],
        )
        .unwrap();
    // `edited` changed SKILL.md since the baseline; `untouched` changed only a reference file
    fs::write(b.join("SKILL.md"), "someone edited this").unwrap();
    fs::write(
        a.join("prompts/p.md"),
        "reference edit made before the upgrade",
    )
    .unwrap();

    let out: std::collections::HashMap<_, _> = f
        .scan()
        .into_iter()
        .map(|o| (o.slug.clone(), o.status))
        .collect();
    assert_eq!(
        out["untouched"],
        DriftStatus::Clean,
        "SKILL.md matches: re-baselined, not an edit"
    );
    assert_eq!(
        out["edited"],
        DriftStatus::EditedCatalog,
        "SKILL.md differs: an edit"
    );
    let prov = marker_provenance(&f.conn(), &a.to_string_lossy());
    assert_eq!(prov.hash_version, Some(2));
    assert_eq!(prov.tree_hash.as_deref(), Some(tree_of(&a).as_str()));
    // ...and from now on the whole tree counts
    fs::write(a.join("prompts/p.md"), "a later edit").unwrap();
    assert_eq!(
        f.scan()
            .into_iter()
            .find(|o| o.slug == "untouched")
            .unwrap()
            .status,
        DriftStatus::EditedCatalog
    );
    // the v1 marker of the edited skill was NOT silently re-baselined
    let pb = marker_provenance(&f.conn(), &b.to_string_lossy());
    assert_eq!(pb.hash_version, None);
}

// ── own edit -> private version ──────────────────────────────────────────────

#[tokio::test]
async fn an_own_edit_uploads_a_private_version_and_keeps_running_in_all_four_modes() {
    for mode in ["audit_only", "warn", "quarantine", "approve_required"] {
        let mut f = Fx::new().await;
        f.set_mode(mode);
        f.drift_sink().await;
        let dir = f.push("mine", true);
        // the runner's install row: installed at 0.1.0
        f.conn()
            .execute(
                "INSERT INTO installed_skills (skill_id, active_version, installed_at, install_path) VALUES ('adam~mine', '0.1.0', 'now', 'p')",
                [],
            )
            .ok();
        let up = f
            .server
            .mock("POST", "/runner/skills/adam~mine/versions/local")
            .match_header("authorization", "Bearer tok")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(STORED_OWN)
            .expect(1)
            .create_async()
            .await;
        let patch = f
            .server
            .mock("PATCH", "/api/installations/inst-1")
            .match_body(Matcher::PartialJsonString(
                r#"{"branch_state":"own","local_version_id":"vid-1"}"#.into(),
            ))
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;

        fs::write(dir.join("prompts/p.md"), "my edit, still running").unwrap();
        let stats = f.cycle().await;
        assert_eq!(stats.drifted, 1, "{mode}");
        up.assert_async().await;
        patch.assert_async().await;
        assert_eq!(
            fs::read_to_string(dir.join("prompts/p.md")).unwrap(),
            "my edit, still running",
            "{mode}: keeps running in place"
        );
        assert!(dir.join(".scenery-managed.json").exists());
        assert!(
            !f.home.join(".claude/.scenery-quarantine").exists(),
            "{mode}: never moved aside"
        );
        // baseline moved: tree + SKILL.md hash; reported once
        let prov = marker_provenance(&f.conn(), &dir.to_string_lossy());
        assert_eq!(
            prov.tree_hash.as_deref(),
            Some(tree_of(&dir).as_str()),
            "{mode}"
        );
        let ls = load(&f.conn(), "adam~mine").unwrap();
        assert_eq!(ls.branch_state, "own");
        assert_eq!(ls.base_version.as_deref(), Some("0.1.0+abc"));
        assert_eq!(ls.local_version_id.as_deref(), Some("vid-1"));
        assert_eq!(ls.authorship.as_deref(), Some("own"));
        // the local pin followed the uploaded version (no registry re-download)
        let av: Option<String> = f
            .conn()
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id='adam~mine'",
                [],
                |r| r.get(0),
            )
            .ok();
        if let Some(v) = av {
            assert_eq!(v, "0.1.0+abc", "{mode}");
        }
        assert_eq!(
            f.cycle().await.drifted,
            0,
            "{mode}: the same edit is not uploaded twice"
        );
    }
}

#[tokio::test]
async fn a_registry_installed_own_skill_is_own_by_the_snapshot_not_by_the_marker() {
    let mut f = Fx::new().await;
    f.drift_sink().await;
    // a second device: installed from the registry, so the marker has no origin
    let dir = f.push("shared-mine", false);
    assert_eq!(f.scan()[0].status, DriftStatus::Clean);
    note_snapshot(&f.conn(), "shared-mine", Some("own"), Some("none"), None);
    fs::write(dir.join("SKILL.md"), "edited on the second device").unwrap();
    assert_eq!(f.scan()[0].status, DriftStatus::EditedOwn);
    // and the reverse: a catalog snapshot overrides an auto-managed marker
    let own_marker = f.push("odd", true);
    note_snapshot(&f.conn(), "adam~odd", Some("catalog"), None, None);
    fs::write(own_marker.join("SKILL.md"), "x").unwrap();
    let st = f
        .scan()
        .into_iter()
        .find(|o| o.slug == "odd")
        .unwrap()
        .status;
    assert_eq!(st, DriftStatus::EditedCatalog);
    // unknown authorship strings are ignored
    note_snapshot(&f.conn(), "bogus", Some("admin"), None, None);
    assert!(load(&f.conn(), "bogus").is_none());
}

// ── catalog edit -> branch / strict quarantine ───────────────────────────────

#[tokio::test]
async fn a_catalog_edit_in_a_non_strict_mode_is_a_branch_that_keeps_running() {
    for mode in ["audit_only", "warn"] {
        let mut f = Fx::new().await;
        f.set_mode(mode);
        f.drift_sink().await;
        let dir = f.push("cat", false);
        f.conn()
            .execute(
                "INSERT INTO installed_skills (skill_id, active_version, installed_at, install_path) VALUES ('cat', '1.0.0', 'now', 'p')",
                [],
            )
            .ok();
        note_snapshot(&f.conn(), "cat", Some("catalog"), None, None);
        let up = f
            .server
            .mock("POST", "/runner/skills/cat/versions/local")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(STORED_BRANCH)
            .expect(1)
            .create_async()
            .await;
        let _patch = f
            .server
            .mock("PATCH", "/api/installations/inst-1")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        fs::write(
            dir.join("prompts/p.md"),
            "my local tweak of a catalog skill",
        )
        .unwrap();
        assert_eq!(f.cycle().await.drifted, 1, "{mode}");
        up.assert_async().await;
        assert!(dir.join("prompts/p.md").exists(), "{mode}: still running");
        assert!(!f.home.join(".claude/.scenery-quarantine").exists());
        let ls = load(&f.conn(), "cat").unwrap();
        assert_eq!(
            (ls.branch_state.as_str(), ls.authorship.as_deref()),
            ("branch", Some("catalog")),
            "{mode}"
        );
        // a branch does not move the catalog base/pin locally
        let av: String = f
            .conn()
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id='cat'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "1.0.0".into());
        assert_eq!(av, "1.0.0");
        assert_eq!(f.cycle().await.drifted, 0, "{mode}: uploaded once");
    }
}

#[tokio::test]
async fn a_catalog_edit_in_strict_mode_is_uploaded_as_a_branch_then_quarantined_as_today() {
    for mode in ["quarantine", "approve_required"] {
        let mut f = Fx::new().await;
        f.set_mode(mode);
        f.drift_sink().await;
        let dir = f.push("cat", false);
        let up = f
            .server
            .mock("POST", "/runner/skills/cat/versions/local")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(STORED_BRANCH)
            .expect(1)
            .create_async()
            .await;
        let _patch = f
            .server
            .mock("PATCH", "/api/installations/inst-1")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        // edited just now: strict mode does not wait for the settle window
        std::env::set_var("SCENERY_LOCAL_EDIT_SETTLE_SECS", "3600");
        fs::write(dir.join("prompts/p.md"), "unapproved change").unwrap();
        f.cycle().await;
        up.assert_async().await;
        if mode == "quarantine" {
            assert!(!dir.exists(), "moved aside, as before");
            let q = f.home.join(".claude/.scenery-quarantine");
            assert!(q.exists());
        } else {
            assert!(
                dir.exists(),
                "approve_required leaves it in place awaiting approval"
            );
        }
    }
}

const STORED_BRANCH_MOCK_PATH: &str = "/runner/skills/cat/versions/local";

#[tokio::test]
async fn approve_required_stays_strict_across_cycles_and_uploads_only_once() {
    let mut f = Fx::new().await;
    f.set_mode("approve_required");
    f.drift_sink().await;
    let dir = f.push("cat", false);
    let before = marker_provenance(&f.conn(), &dir.to_string_lossy()).tree_hash;
    let up = f
        .server
        .mock("POST", STORED_BRANCH_MOCK_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_BRANCH)
        .expect(1)
        .create_async()
        .await;
    let _patch = f
        .server
        .mock("PATCH", "/api/installations/inst-1")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    fs::write(dir.join("prompts/p.md"), "unapproved change").unwrap();
    for cycle in 1..=3 {
        assert_eq!(f.cycle().await.drifted, 1, "cycle {cycle}: still drifted");
        assert_eq!(
            marker_provenance(&f.conn(), &dir.to_string_lossy()).tree_hash,
            before,
            "cycle {cycle}: the baseline is never moved in a strict mode"
        );
    }
    up.assert_async().await; // exactly one upload across three cycles
    assert!(f
        .scan()
        .iter()
        .any(|o| o.status == DriftStatus::EditedCatalog));
    assert!(dir.exists());
    // restoring the edit clears the drift
    fs::write(dir.join("prompts/p.md"), "my prompt").unwrap();
    assert_eq!(f.cycle().await.drifted, 0);
}

#[tokio::test]
async fn strict_quarantine_waits_for_the_upload_and_never_loses_an_unuploaded_edit() {
    let mut f = Fx::new().await;
    f.set_mode("quarantine");
    f.drift_sink().await;
    let dir = f.push("cat", false);
    let fail = f
        .server
        .mock("POST", STORED_BRANCH_MOCK_PATH)
        .with_status(503)
        .expect(2)
        .create_async()
        .await;
    fs::write(dir.join("prompts/p.md"), "offline strict edit").unwrap();
    f.cycle().await;
    f.cycle().await;
    fail.assert_async().await;
    assert!(dir.exists(), "offline: the tree is not moved aside");
    assert!(!f.home.join(".claude/.scenery-quarantine").exists());
    let n: i64 = f
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM managed_path_markers WHERE path = ?1",
            [dir.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the marker stays so the next scan retries");
    // back online: uploaded, then quarantined
    fail.remove_async().await;
    let ok = f
        .server
        .mock("POST", STORED_BRANCH_MOCK_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_BRANCH)
        .expect(1)
        .create_async()
        .await;
    let _patch = f
        .server
        .mock("PATCH", "/api/installations/inst-1")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    f.cycle().await;
    ok.assert_async().await;
    assert!(!dir.exists());
    assert!(f.home.join(".claude/.scenery-quarantine").exists());
}

#[tokio::test]
async fn strict_quarantine_does_not_wait_for_an_upload_that_can_never_succeed() {
    let mut f = Fx::new().await;
    f.set_mode("quarantine");
    let report = f
        .server
        .mock("POST", "/portal/managed-paths/drift")
        .match_body(Matcher::Regex("not_uploaded".into()))
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    let dir = f.push("cat", false);
    // More files than the hard cap: the tree cannot be hashed, so no upload.
    let bulk = dir.join("bulk");
    fs::create_dir_all(&bulk).unwrap();
    for i in 0..10_001 {
        fs::write(bulk.join(format!("f{i}")), "x").unwrap();
    }
    f.cycle().await;
    report.assert_async().await;
    assert!(
        !dir.exists(),
        "quarantined at once, nothing silently kept running"
    );
    assert!(f.home.join(".claude/.scenery-quarantine").exists());
}

#[tokio::test]
async fn strict_quarantine_gives_up_waiting_for_a_transient_failure_after_the_grace_window() {
    let mut f = Fx::new().await;
    f.set_mode("quarantine");
    let report = f
        .server
        .mock("POST", "/portal/managed-paths/drift")
        .match_body(Matcher::Regex("not_uploaded".into()))
        .with_status(200)
        .with_body("{}")
        .expect_at_least(2)
        .create_async()
        .await;
    let _fail = f
        .server
        .mock("POST", STORED_BRANCH_MOCK_PATH)
        .with_status(503)
        .expect_at_least(2)
        .create_async()
        .await;
    let dir = f.push("cat", false);
    fs::write(dir.join("prompts/p.md"), "never uploaded").unwrap();
    f.cycle().await;
    assert!(dir.exists(), "inside the grace window it waits");
    // The first failure was 25 hours ago.
    let key = format!("edit_upload_first_fail:{}", dir.to_string_lossy());
    let old = now_secs() - 25 * 3600;
    f.state.set_sync_state(&key, &old.to_string()).unwrap();
    f.cycle().await;
    report.assert_async().await;
    assert!(!dir.exists(), "quarantined anyway after 24h");
    let q = f.home.join(".claude/.scenery-quarantine");
    assert!(q.exists(), "the files are kept in the quarantine dir");
}

#[tokio::test]
async fn a_catalog_update_never_overwrites_an_edit_that_is_not_uploaded_yet() {
    let f = Fx::new().await;
    let dir = f.push("cat", false);
    fs::write(dir.join("prompts/p.md"), "offline edit, not uploaded").unwrap();
    // The reconciler guard: nothing is downloaded or written (the registry is
    // not even mocked, so touching it would be an error).
    crate::sync::reconciler::install_verified_version(
        uuid::Uuid::new_v4(),
        "cat",
        "1.1.0",
        &f.state,
        &f.server.url(),
        Some(&ManagedPathsPusher::new(&f.state)),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("prompts/p.md")).unwrap(),
        "offline edit, not uploaded"
    );
    // Visible, not silent: listed for `scenery doctor` and the drift report.
    assert_eq!(
        crate::managed_paths::local_edits::update_blocked_slugs(&f.state),
        vec!["cat".to_string()]
    );
    let mut f = f;
    let blocked = f
        .server
        .mock("POST", "/portal/managed-paths/drift")
        .match_body(Matcher::Regex(
            "update_blocked_local_edits_not_uploaded".into(),
        ))
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    f.cycle().await;
    blocked.assert_async().await;
    // And the pusher itself refuses as a backstop.
    let err = ManagedPathsPusher::new(&f.state)
        .push_skill("cat", None, b"---\nname: x\n---\nv2", &[])
        .unwrap_err();
    assert!(err
        .downcast_ref::<crate::managed_paths::pusher::DirtyLocalEdit>()
        .is_some());
    assert_eq!(
        fs::read_to_string(dir.join("prompts/p.md")).unwrap(),
        "offline edit, not uploaded"
    );
}

// ── debounce, secrets, retry ─────────────────────────────────────────────────

#[tokio::test]
async fn an_edit_still_being_typed_waits_and_offline_edits_collapse_to_the_newest() {
    let mut f = Fx::new().await;
    f.drift_sink().await;
    let dir = f.push("mine", true);
    let up = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_OWN)
        .expect(1)
        .create_async()
        .await;
    std::env::set_var("SCENERY_LOCAL_EDIT_SETTLE_SECS", "3600");
    fs::write(dir.join("prompts/p.md"), "edit 1").unwrap();
    f.cycle().await;
    fs::write(dir.join("prompts/p.md"), "edit 2").unwrap();
    f.cycle().await;
    assert!(
        load(&f.conn(), "adam~mine").map_or(true, |l| l.uploaded_tree.is_none()),
        "nothing uploaded while it is being edited"
    );
    // the user stops: only the newest tree is uploaded, once
    std::env::set_var("SCENERY_LOCAL_EDIT_SETTLE_SECS", "0");
    f.cycle().await;
    up.assert_async().await;
    assert_eq!(
        load(&f.conn(), "adam~mine")
            .unwrap()
            .uploaded_tree
            .as_deref(),
        Some(tree_of(&dir).as_str())
    );
}

#[tokio::test]
async fn an_edit_holding_a_secret_is_never_uploaded_and_not_retried() {
    let mut f = Fx::new().await;
    f.drift_sink().await;
    let dir = f.push("mine", true);
    let up = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .expect(0)
        .create_async()
        .await;
    fs::write(
        dir.join("prompts/p.md"),
        "token = sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n-----BEGIN RSA PRIVATE KEY-----\n",
    )
    .unwrap();
    f.cycle().await;
    f.cycle().await;
    up.assert_async().await;
    let ls = load(&f.conn(), "adam~mine").unwrap();
    assert_eq!(ls.held_tree.as_deref(), Some(tree_of(&dir).as_str()));
    assert!(
        dir.join("prompts/p.md").exists(),
        "the skill itself keeps running"
    );
    // a changed tree is looked at again
    fs::write(dir.join("prompts/p.md"), "clean now").unwrap();
    let up2 = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_OWN)
        .expect(1)
        .create_async()
        .await;
    f.cycle().await;
    up2.assert_async().await;
}

#[tokio::test]
async fn a_failed_upload_changes_nothing_and_is_retried_on_the_next_scan() {
    let mut f = Fx::new().await;
    f.drift_sink().await;
    let dir = f.push("mine", true);
    let before = marker_provenance(&f.conn(), &dir.to_string_lossy()).tree_hash;
    let fail = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .with_status(503)
        .expect(2)
        .create_async()
        .await;
    fs::write(dir.join("prompts/p.md"), "offline edit").unwrap();
    assert_eq!(f.cycle().await.drifted, 1);
    assert_eq!(f.cycle().await.drifted, 1, "still an edit: retried");
    fail.assert_async().await;
    assert_eq!(
        marker_provenance(&f.conn(), &dir.to_string_lossy()).tree_hash,
        before,
        "baseline untouched until the registry has it"
    );
    assert_eq!(
        fs::read_to_string(dir.join("prompts/p.md")).unwrap(),
        "offline edit"
    );
    // back online
    fail.remove_async().await;
    let ok = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_OWN)
        .expect(1)
        .create_async()
        .await;
    f.cycle().await;
    ok.assert_async().await;
    assert_eq!(f.cycle().await.drifted, 0);
}

// ── uninstall: synced edits need no backup ───────────────────────────────────

#[tokio::test]
async fn an_uploaded_edit_is_not_an_unuploaded_edit_at_uninstall() {
    let mut f = Fx::new().await;
    f.drift_sink().await;
    let dir = f.push("mine", true);
    let _up = f
        .server
        .mock("POST", "/runner/skills/adam~mine/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(STORED_OWN)
        .create_async()
        .await;
    fs::write(dir.join("prompts/p.md"), "synced edit").unwrap();
    let prov = marker_provenance(&f.conn(), &dir.to_string_lossy());
    assert!(crate::managed_paths::pusher::own_tree_has_unuploaded_edits(
        &dir, &prov
    ));
    f.cycle().await;
    let prov = marker_provenance(&f.conn(), &dir.to_string_lossy());
    assert!(
        !crate::managed_paths::pusher::own_tree_has_unuploaded_edits(&dir, &prov),
        "after the upload the tree matches the baseline: uninstall takes no removed-own backup"
    );
    // a later, not yet uploaded edit is still protected
    fs::write(dir.join("prompts/p.md"), "newer edit").unwrap();
    assert!(crate::managed_paths::pusher::own_tree_has_unuploaded_edits(
        &dir, &prov
    ));
}

#[tokio::test]
async fn restore_canonical_of_a_branch_forgets_the_branch_and_an_own_skill_is_refused() {
    let f = Fx::new().await;
    let cat = f.push("cat", false);
    let own = f.push("own", true);
    note_snapshot(&f.conn(), "cat", Some("catalog"), None, None);
    f.conn()
        .execute(
            "UPDATE skill_local_state SET branch_state = 'branch', local_version_id = 'v1', uploaded_tree = 'x' WHERE skill_id = 'cat'",
            [],
        )
        .unwrap();
    crate::managed_paths::drift::handle_drift_resolution(
        Arc::clone(&f.state),
        f.server.url(),
        "d1".into(),
        "cat".into(),
        "skill".into(),
        "restore_canonical".into(),
    )
    .await
    .unwrap();
    assert!(!cat.exists());
    let ls = load(&f.conn(), "cat").unwrap();
    assert_eq!(
        (ls.branch_state.as_str(), ls.local_version_id),
        ("none", None)
    );
    // own, even without an auto-managed marker once the snapshot says own
    note_snapshot(&f.conn(), "own", Some("own"), None, None);
    let res = crate::managed_paths::drift::handle_drift_resolution(
        Arc::clone(&f.state),
        f.server.url(),
        "d2".into(),
        "own".into(),
        "skill".into(),
        "restore_canonical".into(),
    )
    .await;
    assert!(res.is_err());
    assert!(own.join("SKILL.md").exists());
}

#[test]
fn the_snapshot_row_carries_authorship_and_branch_fields_and_old_backends_omit_them() {
    use crate::sync::sse_client::InstallationRecord;
    let r: InstallationRecord = serde_json::from_str(
        r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"adam~x","version":"0.1.0","state":"installed","authorship":"own","branch_state":"own","base_version":"0.1.0+abc"}"#,
    )
    .unwrap();
    assert_eq!(r.authorship.as_deref(), Some("own"));
    assert_eq!(r.branch_state.as_deref(), Some("own"));
    assert_eq!(r.base_version.as_deref(), Some("0.1.0+abc"));
    let old: InstallationRecord = serde_json::from_str(
        r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"x","version":"1","state":"installed"}"#,
    )
    .unwrap();
    assert!(old.authorship.is_none() && old.branch_state.is_none());
}
