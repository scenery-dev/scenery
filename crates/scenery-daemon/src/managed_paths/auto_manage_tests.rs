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
        collect_discoveries(&[self.agents(), self.pi()], &self.state)
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
    // The pending takeover survives, so the conversion converges later.
    assert!(env
        .state
        .pending_adopt_takeover_source("fragile")
        .unwrap()
        .is_some());

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
