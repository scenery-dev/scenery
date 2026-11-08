//! Revoke teardown must be idempotent and must run on EVERY snapshot.
//!
//! The client-projected content (`~/.agents/skills/<slug>`, the
//! `~/.claude/skills/<slug>` link, the `~/.claude.json` entry) is removed
//! non-fatally, so a snapshot redelivery has to finish a partial teardown. The
//! redelivery gate may only silence the duplicate log line and audit event.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use scenery_core::revocation;
use scenery_core::state::{AppState, McpInstallRow};
use scenery_mcp::aggregator::BackendRegistry;

use crate::managed_paths::{BundledSkill, ManagedPathsPusher, PluginBundle, ENV_MUTEX};
use crate::sync::sse_client::{RevocationRecord, SyncEvent};

// ── Helpers ───────────────────────────────────────────────────────────────────

struct FakeHome {
    tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl FakeHome {
    fn new() -> Self {
        let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        Self {
            tmp,
            prev,
            _guard: guard,
        }
    }
    fn path(&self) -> &Path {
        self.tmp.path()
    }
    fn agents(&self, slug: &str) -> PathBuf {
        self.path().join(".agents/skills").join(slug)
    }
    fn link(&self, slug: &str) -> PathBuf {
        self.path().join(".claude/skills").join(slug)
    }
    fn projected(&self, slug: &str) -> bool {
        self.agents(slug).exists() || self.link(slug).symlink_metadata().is_ok()
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        if let Some(v) = self.prev.take() {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }
    }
}

fn new_state() -> (tempfile::TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    (tmp, Arc::new(AppState::bootstrap_in(root).unwrap()))
}

/// Seed the internal install (rows + `skills/<id>/versions/<v>`), optionally
/// active.
fn seed_skill(state: &AppState, id: &str, version: &str, active: bool) {
    let install_root = state.root_dir.join("skills").join(id);
    let vdir = install_root.join("versions").join(version);
    std::fs::create_dir_all(vdir.as_std_path()).unwrap();
    std::fs::write(vdir.join("SKILL.md").as_std_path(), "x").unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO skill_versions (skill_id, version, install_path, source_type) \
         VALUES (?1, ?2, ?3, 'copy')",
        rusqlite::params![id, version, vdir.as_str()],
    )
    .unwrap();
    if active {
        #[cfg(target_family = "unix")]
        std::os::unix::fs::symlink(
            vdir.as_std_path(),
            install_root.join("active").as_std_path(),
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO installed_skills \
             (skill_id, active_version, install_root, current_status, deactivated) \
             VALUES (?1, ?2, ?3, 'active', 0)",
            rusqlite::params![id, version, install_root.as_str()],
        )
        .unwrap();
    }
}

/// Project `slug` onto the fake home (`~/.agents/skills` + Claude link).
fn project(state: &AppState, slug: &str) {
    ManagedPathsPusher::new(state)
        .push_skill(slug, None, b"---\nname: x\n---\n", &[])
        .unwrap();
}

fn revoke_event(ty: &str, key: &str, version: Option<&str>) -> SyncEvent {
    SyncEvent::Revoke {
        artifact_type: ty.to_string(),
        artifact_key: key.to_string(),
        version: version.map(str::to_string),
    }
}

fn snapshot(revs: &[(&str, &str, Option<&str>)]) -> SyncEvent {
    SyncEvent::Snapshot {
        installations: vec![],
        mcp_installations: None,
        plugin_installations: vec![],
        revocations: Some(
            revs.iter()
                .map(|(t, k, v)| RevocationRecord {
                    artifact_type: (*t).to_string(),
                    artifact_key: (*k).to_string(),
                    version: v.map(str::to_string),
                    signed: None,
                })
                .collect(),
        ),
        org_policy: None,
        org_id: None,
    }
}

async fn run(event: SyncEvent, state: &Arc<AppState>) {
    run_with(event, state, &Arc::new(BackendRegistry::new()), true).await;
}

async fn run_with(
    event: SyncEvent,
    state: &Arc<AppState>,
    registry: &Arc<BackendRegistry>,
    with_pusher: bool,
) {
    let server = mockito::Server::new_async().await;
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let locks: super::SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let mut tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    let pusher = with_pusher.then(|| Arc::new(ManagedPathsPusher::new(state)));
    super::dispatch_event(
        event,
        state,
        &server.url(),
        &sem,
        &locks,
        &stats,
        &mut tasks,
        registry,
        tx,
        pusher,
    )
    .await;
    while let Some(res) = tasks.join_next().await {
        assert!(res.is_ok(), "reconciler task must not panic");
    }
}

fn audits(state: &AppState, event_type: &str) -> Vec<serde_json::Value> {
    let conn = Connection::open(&state.db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT payload FROM audit_events WHERE event_type = ?1 ORDER BY id")
        .unwrap();
    stmt.query_map([event_type], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

fn skill_row_exists(state: &AppState, id: &str) -> bool {
    Connection::open(&state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE skill_id = ?1",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

// ── Skills ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn leftover_native_dir_and_link_are_removed_on_redelivery() {
    // A previous teardown deleted the internal dir and rows, then failed to
    // remove the client projection. The block is cached; redelivery must
    // finish the job, and say so (audit with repeat_pass).
    let home = FakeHome::new();
    let (_t, state) = new_state();
    revocation::upsert_revocation(&state, "skill", "ghost", None).unwrap();
    project(&state, "ghost");
    assert!(home.agents("ghost").exists() && home.link("ghost").is_symlink());

    run(snapshot(&[("skill", "ghost", None)]), &state).await;

    assert!(!home.projected("ghost"), "leftover native dir/link must go");
    let a = audits(&state, "skill_revoked");
    assert_eq!(a.len(), 1, "a repeat pass that removed something audits");
    assert_eq!(a[0]["repeat_pass"], true);
    assert_eq!(a[0]["removed_locally"], true);

    run(snapshot(&[("skill", "ghost", None)]), &state).await;
    assert_eq!(
        audits(&state, "skill_revoked").len(),
        1,
        "now clean: silent"
    );
}

#[tokio::test]
async fn a_dangling_claude_link_is_removed_on_redelivery() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    revocation::upsert_revocation(&state, "skill", "dangling", None).unwrap();
    project(&state, "dangling");
    // Crash between steps: canonical dir gone, link left dangling.
    std::fs::remove_dir_all(home.agents("dangling")).unwrap();
    assert!(home.link("dangling").symlink_metadata().is_ok());

    run(snapshot(&[("skill", "dangling", None)]), &state).await;
    assert!(home.link("dangling").symlink_metadata().is_err());
}

#[tokio::test]
async fn revoke_removes_the_projection_under_the_local_slug() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    state
        .record_skill_local_name("reg-skill", "local-skill")
        .unwrap();
    // Installs live under the local slug; the revoke arrives with the
    // registry id and is mapped by `dispatch_event`.
    seed_skill(&state, "local-skill", "1.0.0", true);
    project(&state, "local-skill");

    run(revoke_event("skill", "reg-skill", None), &state).await;

    assert!(
        !home.projected("local-skill"),
        "the projection is keyed by the local slug, not the registry id"
    );
    assert!(!skill_row_exists(&state, "local-skill"));
}

#[tokio::test]
async fn version_scoped_revoke_of_an_inactive_version_keeps_the_active_one() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "vs", "1.0.0", true);
    seed_skill(&state, "vs", "2.0.0", false);
    project(&state, "vs");
    let v2 = state.root_dir.join("skills/vs/versions/2.0.0");

    run(snapshot(&[("skill", "vs", Some("2.0.0"))]), &state).await;
    assert!(!v2.exists(), "the revoked version's files go");
    assert!(
        skill_row_exists(&state, "vs"),
        "active 1.0.0 stays installed"
    );
    assert!(home.projected("vs"), "and stays projected");

    // Redelivery with nothing left is silent.
    run(snapshot(&[("skill", "vs", Some("2.0.0"))]), &state).await;
    assert_eq!(audits(&state, "skill_revoked").len(), 1);

    // Revoking the active version purges everything, projection included.
    run(snapshot(&[("skill", "vs", Some("1.0.0"))]), &state).await;
    assert!(!skill_row_exists(&state, "vs"));
    assert!(!home.projected("vs"));
}

#[tokio::test]
async fn offline_revoke_new_to_the_cache_applies_and_audits_as_first_pass() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "offline", "1.0.0", true);
    project(&state, "offline");

    run(snapshot(&[("skill", "offline", None)]), &state).await;

    assert!(!home.projected("offline"));
    assert!(!skill_row_exists(&state, "offline"));
    let a = audits(&state, "skill_revoked");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["repeat_pass"], false);
}

#[tokio::test]
async fn unrevoke_reinstall_rerevoke_removes_the_content_again() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "flip", "1.0.0", true);
    project(&state, "flip");
    run(revoke_event("skill", "flip", None), &state).await;
    assert!(!home.projected("flip"));

    run(
        SyncEvent::Unrevoke {
            artifact_type: "skill".into(),
            artifact_key: "flip".into(),
            version: None,
            signed: None,
        },
        &state,
    )
    .await;
    seed_skill(&state, "flip", "1.0.0", true);
    project(&state, "flip");
    assert!(home.projected("flip"), "precondition: reinstalled");

    run(revoke_event("skill", "flip", None), &state).await;
    assert!(!home.projected("flip"));
    assert!(!skill_row_exists(&state, "flip"));
    assert_eq!(audits(&state, "skill_revoked").len(), 2);
}

#[tokio::test]
async fn redelivering_an_already_clean_revoke_emits_no_duplicate_audit() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "clean", "1.0.0", true);
    project(&state, "clean");

    for _ in 0..4 {
        run(snapshot(&[("skill", "clean", None)]), &state).await;
    }
    assert_eq!(audits(&state, "skill_revoked").len(), 1);
    assert!(!home.projected("clean"));
}

#[test]
fn gate_fails_open_when_the_revocation_cache_cannot_be_listed() {
    let (_t, state) = new_state();
    revocation::upsert_revocation(&state, "skill", "a", None).unwrap();
    let entries = vec![revocation::RevocationEntry {
        artifact_type: "skill".into(),
        artifact_key: "a".into(),
        version: None,
    }];
    assert_eq!(
        crate::sync::revoke_gate::entries_already_cached(&state, &entries),
        vec![true]
    );
    Connection::open(&state.db_path)
        .unwrap()
        .execute_batch("DROP TABLE artifact_revocations")
        .unwrap();
    assert_eq!(
        crate::sync::revoke_gate::entries_already_cached(&state, &entries),
        vec![false],
        "unreadable cache = treat as new (log + audit), never as already applied"
    );
}

#[tokio::test]
async fn teardown_still_runs_when_the_revocation_cache_is_unreadable() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "blind", "1.0.0", true);
    project(&state, "blind");
    Connection::open(&state.db_path)
        .unwrap()
        .execute_batch("DROP TABLE artifact_revocations")
        .unwrap();

    run(snapshot(&[("skill", "blind", None)]), &state).await;

    assert!(!home.projected("blind"));
    assert!(!skill_row_exists(&state, "blind"));
}

// ── Plugin bundling the skill ─────────────────────────────────────────────────

#[tokio::test]
async fn a_plugin_that_bundles_the_skill_is_stripped_and_redelivery_is_silent() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    crate::managed_paths::install_plugin_bundle(&PluginBundle {
        slug: "kit".into(),
        name: "kit".into(),
        description: String::new(),
        version: "1.0.0".into(),
        author: "Scenery".into(),
        skills: vec![BundledSkill {
            skill_id: "tool".into(),
            version: "1.0.0".into(),
            skill_md: b"---\nname: tool\n---\n".to_vec(),
            files: vec![],
        }],
        files: vec![],
        executables: vec![],
    })
    .unwrap();
    let bundled = home
        .path()
        .join(".claude/plugins/marketplaces/scenery/plugins/kit/skills/tool");
    assert!(bundled.exists(), "precondition");

    run(snapshot(&[("skill", "tool", None)]), &state).await;
    assert!(!bundled.exists(), "the bundled copy must be stripped");
    let a = audits(&state, "skill_revoked");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["stripped_from_plugins"], serde_json::json!(["kit"]));

    run(snapshot(&[("skill", "tool", None)]), &state).await;
    assert_eq!(audits(&state, "skill_revoked").len(), 1);
}

// ── MCP ───────────────────────────────────────────────────────────────────────

fn mcp_row(sid: &str) -> McpInstallRow {
    McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: uuid::Uuid::new_v4().to_string(),
        mcp_server_name: "GitHub MCP".to_string(),
        package_source: "@org/server".to_string(),
        version_pin: None,
        server_config: Some(
            serde_json::json!({"command": "npx", "args": ["-y", "server"], "env": {}}).to_string(),
        ),
        auth_type: "none".to_string(),
        gateway_server_id: None,
        gateway_url: None,
    }
}

#[tokio::test]
async fn mcp_config_entry_is_removed_even_when_writing_managed_mcp_json_fails() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    let sid = uuid::Uuid::new_v4().to_string();
    state.upsert_mcp_install(&mcp_row(&sid)).unwrap();
    let claude_json = home.path().join(".claude.json");
    std::fs::write(
        &claude_json,
        serde_json::json!({"mcpServers": {
            "github-mcp": {"command": "scenery"},
            "someone-elses": {"command": "x"}
        }})
        .to_string(),
    )
    .unwrap();
    // Make `managed-mcp.json` unwritable: its temp path is a directory.
    let tmp_path = state.root_dir.join("managed-mcp.json.tmp");
    std::fs::create_dir_all(tmp_path.as_std_path()).unwrap();

    run(revoke_event("mcp_server", &sid, None), &state).await;

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert!(
        cfg["mcpServers"].get("github-mcp").is_none(),
        "the client config entry must go even though the json write failed"
    );
    assert!(cfg["mcpServers"].get("someone-elses").is_some());
    assert!(state.list_mcp_installs().unwrap().is_empty());

    // Redelivery with the write now possible repairs managed-mcp.json, so a
    // daemon restart cannot reload the revoked server from it.
    std::fs::remove_dir_all(tmp_path.as_std_path()).unwrap();
    run(snapshot(&[("mcp_server", &sid, None)]), &state).await;
    let managed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(state.root_dir.join("managed-mcp.json").as_std_path()).unwrap(),
    )
    .unwrap();
    assert_eq!(managed["servers"], serde_json::json!([]));
}

#[tokio::test]
async fn mcp_redelivery_with_nothing_left_is_silent() {
    let _home = FakeHome::new();
    let (_t, state) = new_state();
    let sid = uuid::Uuid::new_v4().to_string();
    state.upsert_mcp_install(&mcp_row(&sid)).unwrap();

    for _ in 0..3 {
        run(snapshot(&[("mcp_server", &sid, None)]), &state).await;
    }
    assert_eq!(audits(&state, "mcp_server_revoked").len(), 1);
}

fn mcp_marker_rows(state: &AppState) -> Vec<String> {
    let conn = Connection::open(&state.db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT slug FROM managed_path_markers WHERE kind = 'mcp' ORDER BY slug")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[tokio::test]
async fn revoke_removes_scenery_owned_entry_even_when_the_row_is_gone() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    let pusher = ManagedPathsPusher::new(&state);
    // A leftover Scenery-pushed entry whose mcp_installations row is gone.
    pusher.push_mcp("github-mcp", None, false).unwrap();
    // A live install elsewhere (has a row), and entries Scenery does not own.
    let mut live = mcp_row("live-server-id");
    live.mcp_server_name = "Live One".into();
    state.upsert_mcp_install(&live).unwrap();
    pusher.push_mcp("live-one", None, false).unwrap();
    // A pushed entry the user has since replaced with their own config.
    pusher.push_mcp("user-edited", None, false).unwrap();
    let claude_json = home.path().join(".claude.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    cfg["mcpServers"]["user-edited"] = serde_json::json!({"command": "my-own-thing"});
    cfg["mcpServers"]["someone-elses"] = serde_json::json!({"command": "x"});
    std::fs::write(&claude_json, cfg.to_string()).unwrap();

    let gone = uuid::Uuid::new_v4().to_string();
    run(revoke_event("mcp_server", &gone, None), &state).await;

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    let servers = &cfg["mcpServers"];
    assert!(
        servers.get("github-mcp").is_none(),
        "orphaned entry must go"
    );
    assert!(servers.get("live-one").is_some(), "live install untouched");
    assert_eq!(servers["user-edited"]["command"], "my-own-thing");
    assert!(servers.get("someone-elses").is_some());
    // A user-edited entry keeps its content, but Scenery relinquishes the
    // stale marker row.
    assert!(!mcp_marker_rows(&state).contains(&"user-edited".to_string()));
    // The orphan's marker row is gone; the live one's stays.
    assert!(!mcp_marker_rows(&state).contains(&"github-mcp".to_string()));
    assert!(mcp_marker_rows(&state).contains(&"live-one".to_string()));
}

#[tokio::test]
async fn install_verified_version_refuses_a_revoked_skill() {
    let _home = FakeHome::new();
    let (_t, state) = new_state();
    scenery_core::revocation::upsert_revocation(&state, "skill", "whole", None).unwrap();
    scenery_core::revocation::upsert_revocation(&state, "skill", "scoped", Some("1.0.0")).unwrap();

    for (id, ver) in [("whole", "2.0.0"), ("scoped", "1.0.0")] {
        let err = super::install_verified_version(
            uuid::Uuid::new_v4(),
            id,
            ver,
            &state,
            "http://127.0.0.1:1",
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("revoked"), "{id}@{ver}: {err}");
        assert!(
            !state.root_dir.join("skills").join(id).exists(),
            "nothing may be installed"
        );
    }
}

#[tokio::test]
async fn sweep_skips_a_slug_whose_row_appears_after_the_initial_read() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    let pusher = ManagedPathsPusher::new(&state);
    pusher.push_mcp("github-mcp", None, false).unwrap();

    // A reinstall lands between the sweep's live-row read and its removal.
    let st = Arc::clone(&state);
    let removed = pusher
        .sweep_orphaned_mcp_entries(&move || {
            st.upsert_mcp_install(&mcp_row(&uuid::Uuid::new_v4().to_string()))
                .unwrap();
        })
        .unwrap();

    assert!(removed.is_empty(), "a live install must not be swept");
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join(".claude.json")).unwrap())
            .unwrap();
    assert!(cfg["mcpServers"].get("github-mcp").is_some());
    assert!(mcp_marker_rows(&state).contains(&"github-mcp".to_string()));
}

// ── Plugin revoke: redelivery must not churn the user's files ─────────────────

fn bundle() -> PluginBundle {
    PluginBundle {
        slug: "kit".into(),
        name: "kit".into(),
        description: String::new(),
        version: "1.0.0".into(),
        author: "Scenery".into(),
        skills: vec![BundledSkill {
            skill_id: "tool".into(),
            version: "1.0.0".into(),
            skill_md: b"---\nname: tool\n---\n".to_vec(),
            files: vec![],
        }],
        files: vec![],
        executables: vec![],
    }
}

fn stamp(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(p).unwrap(),
        std::fs::metadata(p).unwrap().modified().unwrap(),
    )
}

#[tokio::test]
async fn redelivered_revoke_of_a_removed_plugin_does_not_rewrite_json_files() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    crate::managed_paths::install_plugin_bundle(&bundle()).unwrap();

    run(snapshot(&[("plugin", "kit", None)]), &state).await;
    assert!(!crate::managed_paths::plugin_has_local_state("kit").unwrap());

    // The user's own compact settings.json, which a rewrite would reformat.
    let settings = home.path().join(".claude/settings.json");
    std::fs::write(
        &settings,
        r#"{"theme":"dark","enabledPlugins":{"other@x":true}}"#,
    )
    .unwrap();
    let installed = home.path().join(".claude/plugins/installed_plugins.json");
    let manifest = home
        .path()
        .join(".claude/plugins/marketplaces/scenery/.claude-plugin/marketplace.json");
    let before = [stamp(&settings), stamp(&installed), stamp(&manifest)];
    std::thread::sleep(std::time::Duration::from_millis(30));

    run(snapshot(&[("plugin", "kit", None)]), &state).await;
    run(snapshot(&[("plugin", "kit", None)]), &state).await;

    let after = [stamp(&settings), stamp(&installed), stamp(&manifest)];
    assert_eq!(before, after, "bytes and mtimes must be untouched");
    assert_eq!(audits(&state, "plugin_revoked").len(), 1);
}

#[tokio::test]
async fn partial_plugin_leftover_is_cleaned_on_redelivery() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    crate::managed_paths::install_plugin_bundle(&bundle()).unwrap();
    run(snapshot(&[("plugin", "kit", None)]), &state).await;

    // A previous attempt left only the enabledPlugins key behind.
    let settings = home.path().join(".claude/settings.json");
    std::fs::write(
        &settings,
        r#"{"enabledPlugins":{"kit@scenery":true,"other@x":true}}"#,
    )
    .unwrap();
    assert!(crate::managed_paths::plugin_has_local_state("kit").unwrap());

    run(snapshot(&[("plugin", "kit", None)]), &state).await;

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(v["enabledPlugins"].get("kit@scenery").is_none());
    assert!(v["enabledPlugins"].get("other@x").is_some());
    let a = audits(&state, "plugin_revoked");
    assert_eq!(a.len(), 2);
    assert_eq!(a[1]["repeat_pass"], true);
}

#[tokio::test]
async fn corrupt_installed_plugins_does_not_block_settings_cleanup_or_spam_audits() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    crate::managed_paths::install_plugin_bundle(&bundle()).unwrap();
    let installed = home.path().join(".claude/plugins/installed_plugins.json");
    std::fs::write(&installed, "{ not json").unwrap();

    for _ in 0..3 {
        run(snapshot(&[("plugin", "kit", None)]), &state).await;
    }

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert!(settings["enabledPlugins"].get("kit@scenery").is_none());
    assert_eq!(std::fs::read(&installed).unwrap(), b"{ not json");
    // The corrupt file stays a reported leftover on every pass...
    assert!(crate::managed_paths::plugin_has_local_state("kit").unwrap());
    // ...but only the first pass (a state change) is audited, and it records
    // that content WAS removed even though the teardown reported an error.
    let a = audits(&state, "plugin_revoked");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["removed_locally"], true);
}

#[tokio::test]
async fn corrupt_settings_json_is_left_alone_but_the_rest_is_removed_and_audited() {
    let home = FakeHome::new();
    let (_t, state) = new_state();
    crate::managed_paths::install_plugin_bundle(&bundle()).unwrap();
    let settings = home.path().join(".claude/settings.json");
    std::fs::write(&settings, "{ not json").unwrap();

    for _ in 0..3 {
        run(snapshot(&[("plugin", "kit", None)]), &state).await;
    }

    assert_eq!(std::fs::read(&settings).unwrap(), b"{ not json");
    let installed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".claude/plugins/installed_plugins.json"))
            .unwrap(),
    )
    .unwrap();
    assert!(installed["plugins"].get("kit@scenery").is_none());
    assert!(!home
        .path()
        .join(".claude/plugins/marketplaces/scenery/plugins/kit")
        .exists());
    assert!(crate::managed_paths::plugin_has_local_state("kit").unwrap());
    let a = audits(&state, "plugin_revoked");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["removed_locally"], true);
}

// ── No-op snapshot must not write outcomes ───────────────────────────────────

#[test]
fn snapshot_noop_for_an_installed_skill_writes_no_projection_outcome() {
    use crate::managed_paths::projection_outcome::read_since;
    use crate::sync::sse_client::InstallationRecord;
    let _home = FakeHome::new();
    let (_t, state) = new_state();
    seed_skill(&state, "same", "1.0.0", true);
    project(&state, "same");
    let since = chrono::Utc::now() - chrono::Duration::seconds(1);

    let events = super::build_derived_events_blocking(
        vec![InstallationRecord {
            installation_id: uuid::Uuid::new_v4(),
            skill_id: "same".into(),
            version: "1.0.0".into(),
            state: "installed".into(),
            source: None,
            local_slug: None,
        }],
        &state,
    );
    assert!(events.is_empty(), "nothing to install");
    assert_eq!(
        read_since(&state, "same", since).unwrap(),
        None,
        "the periodic diff must not churn sync_state"
    );
}
