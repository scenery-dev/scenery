//! Auto-manage orchestration against a mock backend and a fake `$HOME`.
//!
//! Installs go through the real `install_verified_version`; each test seeds the
//! registry version on disk first (`skills/<slug>/versions/<v>`), which is the
//! reconciler's "already local" path, so no signed artifact download is needed
//! while the conversion, marker, links and takeover are the production code.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::{discoveries::collect_discoveries, ENV_MUTEX};
use mockito::{Matcher, Server, ServerGuard};
use scenery_core::state::AppState;
use std::{fs, path::Path, time::Duration};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    server: ServerGuard,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    async fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        state.set_sync_state("device_id", "dev-1").unwrap();
        let server = Server::new_async().await;
        Self {
            _tmp: tmp,
            home,
            state,
            server,
            prev_home,
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

    /// A user skill: `SKILL.md` (name == slug) plus `references/x.md`.
    fn user_skill(&self, root: &Path, slug: &str) -> PathBuf {
        let d = root.join(slug);
        fs::create_dir_all(d.join("references")).unwrap();
        fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {slug}\ndescription: mine\n---\nORIGINAL body\n"),
        )
        .unwrap();
        fs::write(d.join("references/x.md"), "my reference").unwrap();
        d
    }

    /// The version the registry would install (compiled: policy injected).
    fn seed_registry_version(&self, slug: &str, version: &str) {
        let v = self
            .state
            .root_dir
            .join("skills")
            .join(slug)
            .join("versions")
            .join(version);
        fs::create_dir_all(v.join("references").as_std_path()).unwrap();
        fs::write(
            v.join("SKILL.md").as_std_path(),
            format!("---\nname: {slug}\ndescription: mine\nmetadata:\n  scenery:\n    version: {version}\n---\nCOMPILED body\n"),
        )
        .unwrap();
        fs::write(v.join("references/x.md").as_std_path(), "my reference").unwrap();
    }

    fn ctx(&self, future_clock: bool) -> TickCtx {
        TickCtx {
            state: Arc::clone(&self.state),
            registry_url: self.server.url(),
            token: "tok".into(),
            device_id: "dev-1".into(),
            classify: ClassifyContext {
                canonical_root: Some(self.agents()),
                home: Some(self.home.clone()),
                now: future_clock
                    .then(|| std::time::SystemTime::now() + Duration::from_secs(86_400)),
            },
        }
    }

    fn scan(&self) -> Vec<DiscoveryItem> {
        collect_discoveries(
            &crate::managed_paths::discoveries::extra_roots(),
            &self.state,
        )
    }

    fn managed_response(
        &mut self,
        skill_id: &str,
        version: &str,
        mode: &str,
        expect: usize,
    ) -> mockito::Mock {
        let body = serde_json::json!({
            "outcome": "managed", "mode": mode, "reason": null,
            "skill_id": skill_id, "skill_uuid": "11111111-2222-3333-4444-555555555555",
            "owner_namespace": "adam", "version": version,
            "installation_id": if mode == "manage" { serde_json::json!("8b3f6b8a-6e1e-4f39-9b8a-000000000001") } else { serde_json::Value::Null },
            "installation_state": if mode == "manage" { "desired" } else { "none" },
            "idempotent": false,
            "scan": {"verdict": "ALLOW", "decision": "accept", "findings": []},
            "errors": [], "files": []
        });
        self.server
            .mock("POST", "/runner/skills/auto-manage")
            .match_header("authorization", "Bearer tok")
            .match_body(Matcher::Regex(format!("name=\"mode\"\r\n\r\n{mode}")))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .expect(expect)
            .create()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn directive(path: &Path, action: &str) -> Directive {
    Directive {
        discovery_id: "disc-1".into(),
        source_path: path.to_string_lossy().to_string(),
        action: action.into(),
        reason: None,
        tracked_reason: None,
    }
}

fn on() -> Option<AutoManagePolicy> {
    Some(AutoManagePolicy {
        auto_manage_skills: true,
        auto_submit_mcp: true,
    })
}

fn response(directives: Vec<Directive>) -> DiscoveryResponse {
    DiscoveryResponse {
        directives: Some(directives),
        policy: on(),
    }
}

fn dir_names(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn marker_file(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(dir.join(".scenery-managed.json")).unwrap()).unwrap()
}

// ── happy paths ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn converts_a_canonical_skill_in_place_with_provenance_backup_and_links() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "pdf");
    env.seed_registry_version("pdf", "0.1.0");
    let m = env.managed_response("adam~pdf", "0.1.0", "manage", 1);

    let items = env.scan();
    assert_eq!(items.len(), 1);
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.managed, 1, "{s:?}");

    // The directory is now the registry copy, marked auto_managed.
    let body = fs::read_to_string(dir.join("SKILL.md")).unwrap();
    assert!(
        body.contains("COMPILED body"),
        "installs the registry bytes: {body}"
    );
    let mk = marker_file(&dir);
    assert_eq!(mk["origin"], "auto_managed");
    assert_eq!(mk["skill_id"], "adam~pdf");
    assert_eq!(mk["skill_uuid"], "11111111-2222-3333-4444-555555555555");
    assert_eq!(mk["hash_version"], 2);
    assert!(mk["tree_hash"].as_str().unwrap().len() == 64);

    // DB row carries the same provenance (the trusted copy).
    let conn = Connection::open(&env.state.db_path).unwrap();
    let prov = super::super::marker::marker_provenance(&conn, &dir.to_string_lossy());
    assert!(prov.is_auto_managed());
    assert_eq!(prov.skill_id.as_deref(), Some("adam~pdf"));
    assert_eq!(prov.tree_hash.as_deref(), mk["tree_hash"].as_str());

    // The original is backed up byte-for-byte and journaled.
    let entry = scenery_core::restore_journal::RestoreJournal::for_state(&env.state)
        .read_all()
        .unwrap()
        .into_iter()
        .find(|e| e.source == scenery_core::restore_journal::JournalSource::Adopted)
        .expect("adopted journal entry");
    let backup = PathBuf::from(entry.backup_path.unwrap());
    assert_eq!(
        fs::read_to_string(backup.join("SKILL.md")).unwrap(),
        "---\nname: pdf\ndescription: mine\n---\nORIGINAL body\n"
    );
    assert_eq!(
        fs::read(backup.join("references/x.md")).unwrap(),
        b"my reference"
    );

    // The journal's push entry records the origin uninstall keys its own-skill
    // rule on, plus the fingerprint of what Scenery wrote.
    let push = scenery_core::restore_journal::RestoreJournal::for_state(&env.state)
        .read_all()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.op == scenery_core::restore_journal::JournalOp::ArtifactPush)
        .unwrap();
    assert_eq!(push.detail["origin"], "auto_managed");
    assert_eq!(push.detail["skill_id"], "adam~pdf");
    assert!(push.detail["tree_sha256"].as_str().is_some());

    // Claude Code link, local-name mapping, namespace, attempt, audit, no leftovers.
    assert!(fs::read_link(env.claude().join("pdf")).is_ok());
    assert_eq!(env.state.skill_local_name("adam~pdf"), "pdf");
    assert_eq!(
        env.state.registry_name_for_local("pdf").as_deref(),
        Some("adam~pdf")
    );
    assert_eq!(
        env.state
            .get_sync_state(OWNER_NAMESPACE_KEY)
            .unwrap()
            .as_deref(),
        Some("adam")
    );
    assert_eq!(
        get_attempt(&env.state, "pdf", &dir.to_string_lossy())
            .unwrap()
            .status,
        "managed"
    );
    assert!(env
        .state
        .pending_adopt_takeover_source("pdf")
        .unwrap()
        .is_none());
    assert_eq!(
        dir_names(&env.agents()),
        vec!["pdf"],
        "no staging/old siblings"
    );
    let audits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'skill_auto_managed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn a_noncanonical_root_is_taken_over_after_backup() {
    let mut env = Env::new().await;
    let src = env.user_skill(&env.pi(), "pi-only");
    env.seed_registry_version("pi-only", "0.1.0");
    let m = env.managed_response("adam~pi-only", "0.1.0", "manage", 1);

    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&src, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.managed, 1, "{s:?}");

    assert!(
        !src.exists(),
        "the original is removed from the pi root after backup"
    );
    let canonical = env.agents().join("pi-only");
    assert_eq!(marker_file(&canonical)["origin"], "auto_managed");
    let backups = env.state.root_dir.join("restore-backups");
    assert!(backups.as_std_path().exists());
}

#[tokio::test]
async fn the_local_folder_name_and_frontmatter_name_never_change() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "keepname");
    env.seed_registry_version("keepname", "0.1.0");
    let _m = env.managed_response("adam~keepname", "0.1.0", "manage", 1);
    let items = env.scan();
    process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(dir_names(&env.agents()), vec!["keepname"]);
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("name: keepname"));
}

// ── gating ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_directives_or_policy_off_do_nothing() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "quiet");
    let m = env.managed_response("adam~quiet", "0.1.0", "manage", 0);
    let items = env.scan();

    // Older backend: no directives.
    let s = process_response(&env.ctx(true), DiscoveryResponse::default(), &items).await;
    assert_eq!(s, TickSummary::default());

    // Policy off in the response.
    let off = DiscoveryResponse {
        directives: Some(vec![directive(&dir, "manage")]),
        policy: Some(AutoManagePolicy {
            auto_manage_skills: false,
            auto_submit_mcp: true,
        }),
    };
    assert_eq!(
        process_response(&env.ctx(true), off, &items).await,
        TickSummary::default()
    );

    // The off policy is cached: a later response WITHOUT a policy stays off.
    let later = DiscoveryResponse {
        directives: Some(vec![directive(&dir, "manage")]),
        policy: None,
    };
    assert_eq!(
        process_response(&env.ctx(true), later, &items).await,
        TickSummary::default()
    );
    assert_eq!(
        env.state.cached_auto_manage_policy(),
        Some(AutoManagePolicy {
            auto_manage_skills: false,
            auto_submit_mcp: true
        })
    );

    // Never told a policy at all: nothing either.
    let env2 = Env::new_drop_policy(&env);
    assert!(!skills_enabled(env2));
    m.assert();
    assert!(!dir.join(".scenery-managed.json").exists());
}

impl Env {
    fn new_drop_policy(env: &Env) -> Option<AutoManagePolicy> {
        env.state
            .delete_sync_state(scenery_core::state::AUTO_MANAGE_POLICY_KEY)
            .unwrap();
        env.state.cached_auto_manage_policy()
    }
}

#[tokio::test]
async fn a_directive_for_a_path_outside_this_scan_is_ignored() {
    let mut env = Env::new().await;
    env.user_skill(&env.agents(), "real");
    let m = env.managed_response("adam~real", "0.1.0", "manage", 0);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(Path::new("/etc"), "manage")]),
        &items,
    )
    .await;
    assert_eq!(s, TickSummary::default());
    m.assert();
}

// ── local safety wins over the directive ────────────────────────────────────

#[tokio::test]
async fn a_changed_tree_aborts_and_a_recent_edit_defers_with_no_upload() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "busy");
    let m = env.managed_response("adam~busy", "0.1.0", "manage", 0);
    let items = env.scan();

    // Reported, then edited before the directive was acted on.
    fs::write(dir.join("references/x.md"), "edited just now").unwrap();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.deferred, 1, "{s:?}");

    // Unchanged since the report, but touched within 10 minutes (real clock).
    let items = env.scan();
    let s = process_response(
        &env.ctx(false),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.deferred, 1, "{s:?}");
    m.assert();
    assert!(!dir.join(".scenery-managed.json").exists());
    assert!(get_attempt(&env.state, "busy", &dir.to_string_lossy()).is_none());
}

#[tokio::test]
async fn a_held_skill_is_never_uploaded_and_is_listed_for_the_user() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "leaky");
    fs::write(dir.join(".env"), "TOKEN=1").unwrap();
    let m = env.managed_response("adam~leaky", "0.1.0", "manage", 0);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.held, 1, "{s:?}");
    assert!(dir.join(".env").exists() && !dir.join(".scenery-managed.json").exists());

    let attention = attention_list(&env.state);
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].slug, "leaky");
    assert_eq!(attention[0].status, "held");
    assert_eq!(attention[0].reason.as_deref(), Some("secrets_detected"));

    // Told once per tree, not every tick.
    process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    let n: i64 = Connection::open(&env.state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'skill_held'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn any_link_in_an_upload_holds_the_skill_instead_of_uploading() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "linky");
    std::os::unix::fs::symlink("references/x.md", dir.join("alias.md")).unwrap();
    // Classified `track` (symlink in tree); the track-mode pack still refuses links.
    let m = env.managed_response("adam~linky", "0.1.0", "track", 0);
    let items = env.scan();
    assert_eq!(
        items[0].detail.as_ref().unwrap()["tracked_reason"],
        "symlink_in_tree"
    );
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "track")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.held, 1, "{s:?}");
    assert_eq!(
        attention_list(&env.state)[0].reason.as_deref(),
        Some("contains_symlink")
    );
}

#[tokio::test]
async fn the_directive_cannot_convert_what_the_local_classifier_tracks() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "devrepo");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // Backend says manage; the machine says git_tracked: the upload is track-mode
    // and nothing is converted or marked.
    let m = env.managed_response("adam~devrepo", "0.3.0", "track", 1);
    let items = env.scan();
    assert_eq!(items[0].detail.as_ref().unwrap()["treatment"], "track");
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.tracked, 1, "{s:?}");

    assert!(
        !dir.join(".scenery-managed.json").exists(),
        "no marker in a tracked skill"
    );
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("ORIGINAL body"));
    assert!(
        env.claude().join("devrepo").symlink_metadata().is_err(),
        "no client link"
    );
    let row = tracked::get(&env.state, &dir.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(row.skill_id.as_deref(), Some("adam~devrepo"));
    assert_eq!(row.version.as_deref(), Some("0.3.0"));
    assert_eq!(row.reason, "git_tracked");
    assert!(env
        .state
        .pending_adopt_takeover_source("devrepo")
        .unwrap()
        .is_none());
    assert_eq!(
        get_attempt(&env.state, "devrepo", &dir.to_string_lossy())
            .unwrap()
            .status,
        "tracked"
    );

    // Dev skills are not re-uploaded on edits.
    fs::write(dir.join("references/x.md"), "edited").unwrap();
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "track")]),
        &items,
    )
    .await;
    assert_eq!(s.tracked, 0);
    m.assert();
}

// ── ~/.claude/skills is a scan root (real dirs only) ────────────────────────

#[tokio::test]
async fn a_fresh_claude_skill_is_converted_once_and_tracked_or_managed_ones_survive_restarts() {
    let mut env = Env::new().await;
    // Scenery's own link into the claude root must never be reported.
    let own = env.user_skill(&env.agents(), "linked");
    env.seed_registry_version("linked", "0.1.0");
    let _m0 = env.managed_response("adam~linked", "0.1.0", "manage", 1);
    let items = env.scan();
    process_response(
        &env.ctx(true),
        response(vec![directive(&own, "manage")]),
        &items,
    )
    .await;
    assert!(fs::read_link(env.claude().join("linked")).is_ok());

    // A dev skill in the claude root is tracked in place; a fresh one is new.
    let dev = env.user_skill(&env.claude(), "devrepo");
    fs::create_dir_all(dev.join(".git")).unwrap();
    let fresh = env.user_skill(&env.claude(), "fresh");
    env.seed_registry_version("fresh", "0.1.0");
    let m_manage = env.managed_response("adam~fresh", "0.1.0", "manage", 1);
    let m_track = env.managed_response("adam~devrepo", "0.3.0", "track", 1);

    let items = env.scan();
    let mut slugs: Vec<_> = items.iter().map(|i| i.slug.as_str()).collect();
    slugs.sort();
    assert_eq!(slugs, vec!["devrepo", "fresh"], "own links are skipped");
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&fresh, "manage"), directive(&dev, "track")]),
        &items,
    )
    .await;
    assert_eq!((s.managed, s.tracked), (1, 1), "{s:?}");
    m_manage.assert();
    m_track.assert();

    // Fresh: converted in place into the canonical root, then linked back.
    let canonical = env.agents().join("fresh");
    assert_eq!(marker_file(&canonical)["origin"], "auto_managed");
    assert_eq!(
        fs::read_link(env.claude().join("fresh")).unwrap(),
        canonical,
        "the claude path is now a link to the managed copy"
    );
    // Tracked: untouched, no marker.
    assert!(!dev.join(".scenery-managed.json").exists());
    assert!(fs::read_to_string(dev.join("SKILL.md"))
        .unwrap()
        .contains("ORIGINAL body"));

    // Restarts: nothing is converted or uploaded again, bytes unchanged.
    let before = (snapshot(&canonical), snapshot(&dev));
    for _ in 0..2 {
        let items = env.scan();
        assert!(
            items.iter().all(|i| i.slug == "devrepo"),
            "only the tracked dev skill is re-reported"
        );
        let s = process_response(
            &env.ctx(true),
            response(vec![directive(&dev, "track")]),
            &items,
        )
        .await;
        assert_eq!((s.managed, s.tracked), (0, 0), "{s:?}");
    }
    assert_eq!(before, (snapshot(&canonical), snapshot(&dev)));
    m_manage.assert();
    m_track.assert();
}

// ── failure and crash safety ────────────────────────────────────────────────

#[tokio::test]
async fn an_upload_failure_leaves_the_disk_untouched_and_backs_off() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "offline");
    let m = env
        .server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(503)
        .expect(1)
        .create();
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.failed, 1, "{s:?}");
    assert!(!dir.join(".scenery-managed.json").exists());
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("ORIGINAL body"));
    assert_eq!(dir_names(&env.agents()), vec!["offline"]);
    assert!(env
        .state
        .pending_adopt_takeover_source("offline")
        .unwrap()
        .is_none());

    // Backing off: the next tick does not call the server again.
    let a = get_attempt(&env.state, "offline", &dir.to_string_lossy()).unwrap();
    assert_eq!(a.status, "failed");
    assert!(a.next_try_at > now_unix());
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.skipped, 1, "{s:?}");
    m.assert();
}

/// Disk-full shape: the backup cannot be written. The conversion refuses to
/// replace an un-backed-up directory, rolls back, and the original survives
/// byte-for-byte with no staging leftovers. Once the disk is fine again the
/// next attempt converts it.
#[tokio::test]
async fn a_failed_backup_mid_conversion_rolls_back_and_a_later_tick_converts() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "fragile");
    env.seed_registry_version("fragile", "0.1.0");
    let m = env.managed_response("adam~fragile", "0.1.0", "manage", 2);
    let items = env.scan();

    // restore-backups is a FILE, so no backup can be created.
    let blocker = env.state.root_dir.join("restore-backups");
    fs::write(blocker.as_std_path(), "not a dir").unwrap();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.failed, 1, "{s:?}");
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).unwrap(),
        "---\nname: fragile\ndescription: mine\n---\nORIGINAL body\n"
    );
    assert_eq!(
        fs::read(dir.join("references/x.md")).unwrap(),
        b"my reference"
    );
    assert!(!dir.join(".scenery-managed.json").exists());
    assert_eq!(
        dir_names(&env.agents()),
        vec!["fragile"],
        "no staging/old leftovers"
    );
    assert_eq!(
        get_attempt(&env.state, "fragile", &dir.to_string_lossy())
            .unwrap()
            .reason
            .as_deref(),
        Some("conversion_incomplete")
    );
    // A stale pending takeover is cleared (the next attempt re-records it).
    assert!(env
        .state
        .pending_adopt_takeover_source("fragile")
        .unwrap()
        .is_none());

    // Disk is healthy again; backoff elapsed.
    fs::remove_file(blocker.as_std_path()).unwrap();
    let conn = Connection::open(&env.state.db_path).unwrap();
    conn.execute("UPDATE auto_manage_attempts SET next_try_at = 0", [])
        .unwrap();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.managed, 1, "{s:?}");
    m.assert();
    assert_eq!(marker_file(&dir)["origin"], "auto_managed");
}

/// A crash after the swap (process killed before the marker is written) is
/// rolled back at the next daemon start: the user's original is whole again.
#[tokio::test]
async fn a_crash_mid_conversion_is_recovered_at_startup_and_nothing_is_rediscovered() {
    let env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "crashy");
    let mut c = super::super::conversion::Conversion::begin(
        env.state.root_dir.as_std_path(),
        "crashy",
        &dir,
    )
    .unwrap();
    fs::write(c.staging().join("SKILL.md"), "half").unwrap();
    c.swap(&scenery_core::restore_journal::RestoreJournal::for_state(
        &env.state,
    ))
    .unwrap();
    drop(c); // killed here: new tree live, original aside, no marker

    // While it is in that state, discovery must not see the half-written
    // sibling dirs as user skills.
    let names: Vec<String> = env.scan().into_iter().map(|i| i.slug).collect();
    assert_eq!(names, vec!["crashy"]);

    let report = crate::managed_paths::recover_interrupted_conversions(&env.state);
    assert_eq!(report.rolled_back, vec!["crashy"]);
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("ORIGINAL body"));
    assert_eq!(dir_names(&env.agents()), vec!["crashy"]);
}

#[tokio::test]
async fn a_scan_reject_blocks_and_is_not_retried_for_the_same_tree() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "rejected");
    let m = env
        .server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({"outcome": "blocked", "mode": "manage", "reason": "scan_rejected",
                "errors": [], "files": []})
            .to_string(),
        )
        .expect(1)
        .create();
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.blocked, 1, "{s:?}");
    assert!(
        !dir.join(".scenery-managed.json").exists(),
        "original untouched"
    );
    let att = attention_list(&env.state);
    assert_eq!(att[0].status, "blocked");
    assert_eq!(att[0].reason.as_deref(), Some("scan_rejected"));

    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.skipped, 1);
    m.assert();

    // A changed tree is retried.
    fs::write(dir.join("references/x.md"), "fixed").unwrap();
    assert!(should_attempt(
        &env.state,
        "rejected",
        &dir.to_string_lossy(),
        &env.scan()[0].local.as_ref().unwrap().tree_hash
    ));
}

#[tokio::test]
async fn protocol_refusals_are_handled_without_touching_disk() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "refused");
    let items = env.scan();

    // 401: stop the tick.
    let m = env
        .server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(401)
        .with_body(r#"{"detail":"nope"}"#)
        .expect(1)
        .create();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert!(s.unauthorized);
    m.assert();
    drop(m);

    // 403 auto_manage_disabled: the policy cache flips off.
    let m = env
        .server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(403)
        .with_body(r#"{"detail":{"code":"auto_manage_disabled","message":"off"}}"#)
        .expect(1)
        .create();
    process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert!(
        !env.state
            .cached_auto_manage_policy()
            .unwrap()
            .auto_manage_skills
    );
    assert!(!dir.join(".scenery-managed.json").exists());
}

#[tokio::test]
async fn at_most_five_conversions_per_tick() {
    let mut env = Env::new().await;
    let mut ds = Vec::new();
    for i in 0..7 {
        let slug = format!("bulk{i}");
        let d = env.user_skill(&env.agents(), &slug);
        env.seed_registry_version(&slug, "0.1.0");
        ds.push(directive(&d, "manage"));
    }
    let _m = env.managed_response("adam~bulk", "0.1.0", "manage", 5);
    // The mock answers every call with the same skill_id; that is fine here,
    // we only count conversions attempted.
    let items = env.scan();
    let s = process_response(&env.ctx(true), response(ds), &items).await;
    assert_eq!(s.managed + s.failed, MAX_CONVERSIONS_PER_TICK, "{s:?}");
}

// ── attempts bookkeeping ────────────────────────────────────────────────────

#[tokio::test]
async fn attempt_rules_terminal_failed_and_changed_hash() {
    let env = Env::new().await;
    let st = &env.state;
    assert!(should_attempt(st, "a", "/p", "h1"), "never tried");

    record_attempt(
        st,
        "a",
        "/p",
        "h1",
        AttemptStatus::Skipped,
        Some("already_managed"),
    );
    assert!(
        !should_attempt(st, "a", "/p", "h1"),
        "terminal for this tree"
    );
    assert!(
        should_attempt(st, "a", "/p", "h2"),
        "changed tree is retried"
    );

    record_attempt(
        st,
        "a",
        "/p",
        "h1",
        AttemptStatus::Failed,
        Some("upload_failed"),
    );
    assert!(!should_attempt(st, "a", "/p", "h1"), "backing off");
    let first = get_attempt(st, "a", "/p").unwrap();
    record_attempt(
        st,
        "a",
        "/p",
        "h1",
        AttemptStatus::Failed,
        Some("upload_failed"),
    );
    let second = get_attempt(st, "a", "/p").unwrap();
    assert_eq!(second.fail_count, first.fail_count + 1);
    assert!(
        second.next_try_at > first.next_try_at,
        "exponential backoff"
    );

    record_attempt(st, "a", "/p", "h1", AttemptStatus::Managed, None);
    assert_eq!(get_attempt(st, "a", "/p").unwrap().fail_count, 0);

    // Stale terminal answers are retried after a day.
    Connection::open(&st.db_path)
        .unwrap()
        .execute(
            "UPDATE auto_manage_attempts SET last_try = '2000-01-01T00:00:00+00:00'",
            [],
        )
        .unwrap();
    assert!(should_attempt(st, "a", "/p", "h1"));
}

#[test]
fn skip_stickiness_follows_the_contract() {
    for sticky in [
        "already_managed",
        "revoked",
        "scan_rejected",
        "secrets_detected",
    ] {
        assert!(skip_is_sticky(Some(sticky)), "{sticky}");
    }
    for fleeting in ["auto_manage_disabled", "status_pending", "status_blocked"] {
        assert!(!skip_is_sticky(Some(fleeting)), "{fleeting}");
    }
}

#[tokio::test]
async fn local_revocation_of_the_private_id_blocks_the_upload() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "pdf");
    env.state
        .set_sync_state(OWNER_NAMESPACE_KEY, "adam")
        .unwrap();
    scenery_core::revocation::upsert_revocation(&env.state, "skill", "adam~pdf", None).unwrap();
    let m = env.managed_response("adam~pdf", "0.1.0", "manage", 0);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.skipped, 1, "{s:?}");
    m.assert();
    assert_eq!(
        get_attempt(&env.state, "pdf", &dir.to_string_lossy())
            .unwrap()
            .status,
        "revoked"
    );
    assert!(!dir.join(".scenery-managed.json").exists());
}

#[tokio::test]
async fn a_revoked_catalog_skill_with_the_same_bare_slug_does_not_block_a_private_conversion() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "pdf");
    env.state
        .set_sync_state(OWNER_NAMESPACE_KEY, "adam")
        .unwrap();
    env.seed_registry_version("pdf", "0.1.0");
    // Catalog `pdf` is revoked org-wide; adam's `adam~pdf` is a different skill.
    scenery_core::revocation::upsert_revocation(&env.state, "skill", "pdf", None).unwrap();
    let m = env.managed_response("adam~pdf", "0.1.0", "manage", 1);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.managed, 1, "{s:?}");
    assert_eq!(marker_file(&dir)["skill_id"], "adam~pdf");
}

#[tokio::test]
async fn local_holds_are_surfaced_without_any_directive_and_pruned_when_the_skill_is_gone() {
    let env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "silent");
    fs::write(dir.join("id_ed25519"), "key").unwrap();
    let items = env.scan();

    // An older backend (no directives at all) must not make the hold silent.
    note_local_holds(&env.state, &items);
    let att = attention_list(&env.state);
    assert_eq!(att.len(), 1);
    assert_eq!(att[0].reason.as_deref(), Some("secrets_detected"));

    // The user removes the skill: the stale row goes away.
    fs::remove_dir_all(&dir).unwrap();
    note_local_holds(&env.state, &[]);
    assert!(attention_list(&env.state).is_empty());
}

// ── The periodic sync must never treat a private skill as "unknown" ─────────

/// Regression: `check_skill_updates` used to send the FOLDER name (`pdf`) to
/// `/skills/status`; the registry only knows `adam~pdf`, answered "unknown", and
/// the lifecycle phase uninstalled the skill every sync tick (destroying the
/// owner's edits, dropping provenance and, in strict mode, bypassing the
/// drift guard through a reinstall storm).
#[tokio::test]
async fn two_sync_ticks_leave_a_mapped_private_skill_installed_edited_and_marked() {
    let mut env = Env::new().await;
    env.state
        .set_sync_state("managed_paths_mode", "quarantine")
        .unwrap();
    env.state
        .record_skill_local_name("adam~pdf", "pdf")
        .unwrap();
    env.state
        .record_pending_auto_managed_takeover(
            "pdf",
            &env.agents().join("pdf").to_string_lossy(),
            "adam~pdf",
            "uuid-7",
        )
        .unwrap();
    ManagedPathsPusher::new(&env.state)
        .push_skill("pdf", None, b"---\nname: pdf\n---\nconverted", &[])
        .unwrap();
    env.state.clear_pending_adopt_takeover("pdf").unwrap();
    // The internal install row the reconciler keeps for it.
    env.seed_registry_version("pdf", "0.1.0");
    let root = env.state.root_dir.join("skills/pdf");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root.join("versions/0.1.0").as_std_path(),
        root.join("active").as_std_path(),
    )
    .unwrap();
    Connection::open(&env.state.db_path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO installed_skills \
             (skill_id, active_version, install_root, current_status, deactivated) \
             VALUES ('pdf', '0.1.0', ?1, 'active', 0)",
            [root.as_str()],
        )
        .unwrap();

    // The owner keeps working.
    let dir = env.agents().join("pdf");
    fs::write(dir.join("SKILL.md"), "---\nname: pdf\n---\nMY EDITS").unwrap();

    // The registry answers "unknown" for every id it is asked about, including
    // the private one: that is what an anonymous status call gets.
    let status = env
        .server
        .mock("POST", "/skills/status")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"statuses":{},"unknown":["pdf","adam~pdf"]}"#)
        .expect(0) // private skills are never sent to the anonymous endpoint
        .create();
    let detail = env
        .server
        .mock("GET", Matcher::Regex("^/skills/pdf".into()))
        .with_status(404)
        .expect(0)
        .create();
    for _tick in 0..2 {
        let state = Arc::clone(&env.state);
        let url = env.server.url();
        let changes = tokio::task::spawn_blocking(move || {
            let registry = RegistryClient::new(url);
            let policy = scenery_core::policy::MockPolicyClient::new();
            scenery_core::updater::check_skill_updates(&state, &registry, &policy).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(changes, 0);

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "---\nname: pdf\n---\nMY EDITS",
            "the owner's edits survive"
        );
        assert!(root.join("versions/0.1.0").exists(), "install not removed");
        let row: i64 = Connection::open(&env.state.db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'pdf'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row, 1);
        let mk = marker_file(&dir);
        assert_eq!(mk["origin"], "auto_managed");
        assert_eq!(mk["skill_id"], "adam~pdf");
    }
    status.assert();
    detail.assert();
}

#[tokio::test]
async fn a_mapped_non_private_skill_is_asked_about_by_registry_id_and_never_uninstalled_on_unknown()
{
    let mut env = Env::new().await;
    env.state.record_skill_local_name("x-bob", "x").unwrap();
    env.seed_registry_version("x", "1.0.0");
    let root = env.state.root_dir.join("skills/x");
    Connection::open(&env.state.db_path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO installed_skills \
             (skill_id, active_version, install_root, current_status, deactivated) \
             VALUES ('x', '1.0.0', ?1, 'active', 0)",
            [root.as_str()],
        )
        .unwrap();
    let status = env
        .server
        .mock("POST", "/skills/status")
        .match_body(Matcher::Regex(r#""x-bob""#.into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"statuses":{},"unknown":["x-bob"]}"#)
        .expect(1)
        .create();
    let state = Arc::clone(&env.state);
    let url = env.server.url();
    let changes = tokio::task::spawn_blocking(move || {
        scenery_core::updater::check_skill_updates(
            &state,
            &RegistryClient::new(url),
            &scenery_core::policy::MockPolicyClient::new(),
        )
        .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(changes, 0);
    status.assert();
    assert!(root.join("versions/1.0.0").exists());
}

#[tokio::test]
async fn a_backend_skip_or_hold_never_downgrades_a_local_hold_and_is_noted_once() {
    let env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "held-one");
    fs::write(dir.join(".env"), "T=1").unwrap();
    let items = env.scan();
    for action in ["skip", "hold", "skip", "skip"] {
        let mut d = directive(&dir, action);
        d.reason = Some("secrets_detected".into());
        process_response(&env.ctx(true), response(vec![d]), &items).await;
        let att = attention_list(&env.state);
        assert_eq!(att.len(), 1, "still listed as held after {action}");
        assert_eq!(att[0].status, "held");
        assert_eq!(att[0].reason.as_deref(), Some("secrets_detected"));
    }
    let n: i64 = Connection::open(&env.state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'skill_held'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "one audit event per hold, not per tick");
}

#[tokio::test]
async fn caps_count_attempts_so_a_failing_backend_gets_at_most_five_calls_per_tick() {
    let mut env = Env::new().await;
    let mut ds = Vec::new();
    for i in 0..9 {
        let d = env.user_skill(&env.agents(), &format!("storm{i}"));
        ds.push(directive(&d, "manage"));
    }
    let m = env
        .server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(503)
        .expect(MAX_CONVERSIONS_PER_TICK)
        .create();
    let items = env.scan();
    let s = process_response(&env.ctx(true), response(ds), &items).await;
    m.assert();
    assert_eq!(s.failed, MAX_CONVERSIONS_PER_TICK, "{s:?}");
}

#[tokio::test]
async fn a_failed_install_clears_the_pending_takeover() {
    let mut env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "noinstall");
    // No registry version seeded and nothing to fetch: the install fails.
    let _m = env.managed_response("adam~noinstall", "0.1.0", "manage", 1);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    assert_eq!(s.failed, 1, "{s:?}");
    assert!(env
        .state
        .pending_adopt_takeover_source("noinstall")
        .unwrap()
        .is_none());
    assert!(!dir.join(".scenery-managed.json").exists());
}

// ── Uninstall removes an own skill (own_uninstall) ─────────────────────

#[cfg(unix)]
fn make_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Snapshot of (relative path, bytes, exec bit) for a tree.
fn snapshot(root: &Path) -> Vec<(String, Vec<u8>, bool)> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, bool)>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                go(root, &p, out);
            } else {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                out.push((rel, fs::read(&p).unwrap(), exec));
            }
        }
    }
    let mut v = Vec::new();
    go(root, root, &mut v);
    v.sort();
    v
}

async fn convert_pdf(env: &mut Env) -> PathBuf {
    let dir = env.user_skill(&env.agents(), "pdf");
    #[cfg(unix)]
    {
        fs::write(dir.join("references/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        make_exec(&dir.join("references/run.sh"));
    }
    env.seed_registry_version("pdf", "0.1.0");
    let m = env.managed_response("adam~pdf", "0.1.0", "manage", 1);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.managed, 1, "{s:?}");
    dir
}

#[tokio::test]
async fn unedited_uninstall_removes_everything_with_no_backup_and_no_restore() {
    let mut env = Env::new().await;
    let dir = convert_pdf(&mut env).await;
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("COMPILED"));

    // `scenery skill uninstall pdf` (registry id or folder name).
    let (removed, own) =
        super::super::own_uninstall::uninstall_projection_detailed(&env.state, "adam~pdf").unwrap();
    assert!(removed);
    let own = own.expect("an auto-managed own skill");
    assert!(!own.local_edits, "tree equals what Scenery wrote");
    assert!(own.skill_id.contains("pdf"), "{own:?}");

    // Gone from the device and the AI clients; the original is NOT put back.
    assert!(!dir.exists(), "removed, not restored");
    assert!(fs::symlink_metadata(env.claude().join("pdf")).is_err());
    let conn = Connection::open(&env.state.db_path).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM managed_path_markers", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0, "no tracked marker rows left");
    // No conversion journal left to replay on a whole-product uninstall.
    assert!(
        scenery_core::restore_journal::RestoreJournal::for_state(&env.state)
            .read_all()
            .unwrap()
            .is_empty()
    );
    // An unedited own skill needs no removed-own backup (the registry has it).
    assert!(removed_own_backups(&env).is_empty());
    // Sticky state is gone: nothing blocks managing a recreated folder.
    let attempts: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_manage_attempts", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 0);
}

#[tokio::test]
async fn edited_uninstall_removes_the_files_but_keeps_a_removed_own_backup() {
    let mut env = Env::new().await;
    let dir = convert_pdf(&mut env).await;
    fs::write(dir.join("SKILL.md"), "MY NEW EDITS").unwrap();

    let (_, own) =
        super::super::own_uninstall::uninstall_projection_detailed(&env.state, "pdf").unwrap();
    assert!(own.expect("own").local_edits);

    assert!(!dir.exists());
    assert!(fs::symlink_metadata(env.claude().join("pdf")).is_err());
    let kept = removed_own_backups(&env);
    assert!(
        kept.iter()
            .any(|p| p.ends_with("removed-own/pdf/SKILL.md")
                && fs::read(p).unwrap() == b"MY NEW EDITS"),
        "edits survive in removed-own: {kept:?}"
    );
}

#[tokio::test]
async fn uninstall_with_an_unknown_baseline_backs_up_fail_safe() {
    let mut env = Env::new().await;
    let dir = convert_pdf(&mut env).await;
    Connection::open(&env.state.db_path)
        .unwrap()
        .execute("UPDATE managed_path_markers SET tree_hash = NULL", [])
        .unwrap();
    let (_, own) =
        super::super::own_uninstall::uninstall_projection_detailed(&env.state, "pdf").unwrap();
    assert!(own.expect("own").local_edits, "unknown counts as edited");
    assert!(!dir.exists());
    assert!(!removed_own_backups(&env).is_empty());
}

#[tokio::test]
async fn uninstall_never_deletes_a_tracked_or_unmanaged_skill() {
    let env = Env::new().await;
    let dir = env.user_skill(&env.agents(), "devrepo");
    fs::create_dir_all(dir.join(".git")).unwrap();
    let before = snapshot(&dir);
    let acted = super::super::own_uninstall::uninstall_projection(&env.state, "devrepo").unwrap();
    assert!(!acted);
    assert_eq!(snapshot(&dir), before);
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Ensure an `installed_skills` row for `pdf` (the updater / CLI uninstall key).
fn ensure_install_row(env: &Env) {
    let root = env.state.root_dir.join("skills/pdf");
    Connection::open(&env.state.db_path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO installed_skills \
             (skill_id, active_version, install_root, current_status, deactivated) \
             VALUES ('pdf', '0.1.0', ?1, 'active', 0)",
            [root.as_str()],
        )
        .unwrap();
}

fn removed_own_backups(env: &Env) -> Vec<PathBuf> {
    walk_files(
        &env.state
            .root_dir
            .join("restore-backups")
            .into_std_path_buf(),
    )
    .into_iter()
    .filter(|p| p.to_string_lossy().contains("removed-own"))
    .collect()
}

#[tokio::test]
async fn registry_deleted_teardown_never_restores_and_takes_no_backup_when_unedited() {
    let mut env = Env::new().await;
    crate::managed_paths::register_core_hooks();
    let dir = convert_pdf(&mut env).await;
    ensure_install_row(&env);

    // The updater's "unknown to the registry" path is `uninstall_skill`.
    let r = scenery_core::installer::uninstall_skill(&env.state, "pdf").unwrap();
    assert!(r.is_some());

    assert!(
        !dir.exists(),
        "revoke-style teardown removes, never restores"
    );
    assert!(fs::symlink_metadata(env.claude().join("pdf")).is_err());
    assert!(
        removed_own_backups(&env).is_empty(),
        "an unedited own skill needs no removed-own backup"
    );
}

#[tokio::test]
async fn registry_deleted_teardown_of_an_edited_own_skill_backs_up_and_does_not_restore() {
    let mut env = Env::new().await;
    crate::managed_paths::register_core_hooks();
    let dir = convert_pdf(&mut env).await;
    fs::write(dir.join("SKILL.md"), "MY EDITS").unwrap();
    ensure_install_row(&env);

    scenery_core::installer::uninstall_skill(&env.state, "pdf").unwrap();

    assert!(!dir.exists());
    assert!(!removed_own_backups(&env).is_empty());
}

#[tokio::test]
async fn user_uninstall_through_the_core_hook_removes_and_reports_the_own_skill() {
    let mut env = Env::new().await;
    crate::managed_paths::register_core_hooks();
    let dir = convert_pdf(&mut env).await;
    ensure_install_row(&env);
    let done = scenery_core::installer::uninstall_skill_by_user(&env.state, "pdf")
        .unwrap()
        .expect("installed");
    assert!(!done.own.expect("own skill reported").local_edits);
    assert!(!dir.exists(), "no restore of the original");
    assert!(fs::symlink_metadata(env.claude().join("pdf")).is_err());
}

#[tokio::test]
async fn user_uninstall_of_a_catalog_skill_reports_no_own_skill() {
    let env = Env::new().await;
    crate::managed_paths::register_core_hooks();
    ensure_install_row(&env);
    let done = scenery_core::installer::uninstall_skill_by_user(&env.state, "pdf")
        .unwrap()
        .expect("installed");
    assert!(done.own.is_none(), "nothing for the backend to archive");
}

#[tokio::test]
async fn recreating_the_folder_after_uninstall_is_managed_again() {
    let mut env = Env::new().await;
    crate::managed_paths::register_core_hooks();
    let dir = convert_pdf(&mut env).await;
    ensure_install_row(&env);
    scenery_core::installer::uninstall_skill_by_user(&env.state, "pdf").unwrap();
    assert!(!dir.exists());
    // The user recreates the skill with new content: converted like a new one,
    // and a later uninstall backs up nothing (no stale state from cycle one).
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: pdf\n---\nSECOND original\n",
    )
    .unwrap();
    env.seed_registry_version("pdf", "0.1.0");
    let m = env.managed_response("adam~pdf", "0.1.0", "manage", 1);
    let items = env.scan();
    let s = process_response(
        &env.ctx(true),
        response(vec![directive(&dir, "manage")]),
        &items,
    )
    .await;
    m.assert();
    assert_eq!(s.managed, 1, "{s:?}");
    assert!(dir.join(".scenery-managed.json").exists());

    let (_, own) =
        super::super::own_uninstall::uninstall_projection_detailed(&env.state, "pdf").unwrap();
    assert!(!own.expect("own").local_edits);
    assert!(!dir.exists());
}
