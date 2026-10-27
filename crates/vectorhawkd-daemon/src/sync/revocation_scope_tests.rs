//! Revocation coverage for plugin-bundled skills (F6) and version-scoped
//! revocations (F15).
//!
//! F6: a skill bundled in a VectorHawk-published plugin used to be downloaded
//! and bundled with no revocation check, and a plugin installed *before* the
//! skill was revoked kept carrying it. F15: plugin and MCP install gates
//! passed no version, so a version-scoped revoke never blocked anything, and
//! revoke teardown ignored the version (revoke/reinstall flapping).
//!
//! Every test points `$HOME` at a temp dir (the plugin marketplace lives
//! under `~/.claude/plugins`) and uses a temp state root. The daemon test
//! binary already forces `VECTORHAWK_DISABLE_KEYCHAIN=1` via its ctor.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use rusqlite::Connection;
use uuid::Uuid;

use vectorhawkd_core::revocation;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::aggregator::BackendRegistry;

use crate::managed_paths::ENV_MUTEX;
use crate::sync::sse_client::{PluginInstallationRecord, PluginSkillRef, SyncEvent};

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

    fn plugin_src(&self, slug: &str) -> PathBuf {
        self.path()
            .join(".claude/plugins/marketplaces/vectorhawk/plugins")
            .join(slug)
    }

    fn plugin_cache(&self, slug: &str, version: &str) -> PathBuf {
        self.path()
            .join(".claude/plugins/cache/vectorhawk")
            .join(slug)
            .join(version)
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

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-revocation-scope-tests-{label}-{nanos}")),
    )
    .expect("temp path utf-8")
}

fn new_state(label: &str) -> Arc<AppState> {
    Arc::new(AppState::bootstrap_in(temp_root(label)).unwrap())
}

fn cleanup(state: &AppState) {
    let _ = std::fs::remove_dir_all(&state.root_dir);
}

/// Seed `skills/<id>/versions/<version>/SKILL.md` with `body`, plus the
/// SQLite rows. When `active`, also point `active/` at it.
fn seed_skill_version(state: &AppState, skill_id: &str, version: &str, body: &str, active: bool) {
    let install_root = state.root_dir.join("skills").join(skill_id);
    let version_dir = install_root.join("versions").join(version);
    std::fs::create_dir_all(version_dir.as_std_path()).unwrap();
    std::fs::write(version_dir.join("SKILL.md").as_std_path(), body).unwrap();

    let conn = Connection::open(&state.db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO skill_versions (skill_id, version, install_path, source_type) \
         VALUES (?1, ?2, ?3, 'copy')",
        rusqlite::params![skill_id, version, version_dir.as_str()],
    )
    .unwrap();
    if active {
        let active_dir = install_root.join("active");
        let _ = std::fs::remove_file(active_dir.as_std_path());
        #[cfg(target_family = "unix")]
        std::os::unix::fs::symlink(version_dir.as_std_path(), active_dir.as_std_path()).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO installed_skills \
             (skill_id, active_version, install_root, current_status, deactivated) \
             VALUES (?1, ?2, ?3, 'active', 0)",
            rusqlite::params![skill_id, version, install_root.as_str()],
        )
        .unwrap();
    }
}

fn skill_ref(skill_id: &str, version: &str) -> PluginSkillRef {
    PluginSkillRef {
        skill_id: skill_id.to_string(),
        version: version.to_string(),
    }
}

fn install_plugin_event(slug: &str, version: &str, skills: Vec<PluginSkillRef>) -> SyncEvent {
    SyncEvent::InstallPlugin {
        installation_id: Uuid::new_v4(),
        plugin_slug: slug.to_string(),
        plugin_name: slug.to_string(),
        description: "test plugin".to_string(),
        version: version.to_string(),
        author: "VectorHawk".to_string(),
        skills,
    }
}

fn revoke_event(artifact_type: &str, key: &str, version: Option<&str>) -> SyncEvent {
    SyncEvent::Revoke {
        artifact_type: artifact_type.to_string(),
        artifact_key: key.to_string(),
        version: version.map(str::to_string),
    }
}

/// Dispatch one event and wait for every task it spawned. Every HTTP call
/// on these paths (status PATCH, imported-plugin content fetch) is
/// best-effort, so an unmocked mockito server (fast 501) is enough.
async fn run(event: SyncEvent, state: &Arc<AppState>, backend_registry: &Arc<BackendRegistry>) {
    let server = mockito::Server::new_async().await;
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let locks: super::SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    super::dispatch_event(
        event,
        state,
        &server.url(),
        &sem,
        &locks,
        &stats,
        &mut install_tasks,
        backend_registry,
        list_changed_tx,
        None,
    )
    .await;
    while let Some(res) = install_tasks.join_next().await {
        assert!(res.is_ok(), "reconciler task must not panic");
    }
}

fn governed_version(slug: &str) -> Option<String> {
    crate::managed_paths::list_governed_plugins()
        .unwrap()
        .get(slug)
        .cloned()
}

// ── F6: plugin install gate for bundled skills ──────────────────────────────

#[tokio::test]
async fn plugin_install_skips_a_revoked_bundled_skill_and_installs_the_rest() {
    let home = FakeHome::new();
    let state = new_state("f6-skip-revoked");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "evil", "1.0.0", "evil body", true);
    seed_skill_version(&state, "good", "1.0.0", "good body", true);
    revocation::upsert_revocation(&state, "skill", "evil", None).unwrap();

    run(
        install_plugin_event(
            "bundle",
            "1.0.0",
            vec![skill_ref("evil", "1.0.0"), skill_ref("good", "1.0.0")],
        ),
        &state,
        &reg,
    )
    .await;

    assert_eq!(governed_version("bundle").as_deref(), Some("1.0.0"));
    let src = home.plugin_src("bundle");
    assert!(src.join("skills/good/SKILL.md").exists());
    assert!(
        !src.join("skills/evil").exists(),
        "a revoked skill must never be bundled into a plugin"
    );
    assert!(!home
        .plugin_cache("bundle", "1.0.0")
        .join("skills/evil")
        .exists());
    cleanup(&state);
}

#[tokio::test]
async fn plugin_install_skips_a_bundled_skill_revoked_at_the_referenced_version() {
    let home = FakeHome::new();
    let state = new_state("f6-skip-version");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "evil", "1.0.0", "evil body", true);
    revocation::upsert_revocation(&state, "skill", "evil", Some("1.0.0")).unwrap();

    run(
        install_plugin_event("bundle", "1.0.0", vec![skill_ref("evil", "1.0.0")]),
        &state,
        &reg,
    )
    .await;

    assert!(
        !home.plugin_src("bundle").join("skills/evil").exists(),
        "a skill revoked at the referenced version must not be bundled"
    );
    cleanup(&state);
}

#[tokio::test]
async fn plugin_install_bundles_the_referenced_version_not_whatever_is_active() {
    // Reviewer bypass: the gate checked the referenced version but the
    // bundle was read from `skills/<id>/active`, which can be a different
    // (revoked) version.
    let home = FakeHome::new();
    let state = new_state("f6-bundle-referenced");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "tool", "1.0.0", "one-point-oh", false);
    seed_skill_version(&state, "tool", "2.0.0", "two-point-oh REVOKED", true);
    revocation::upsert_revocation(&state, "skill", "tool", Some("2.0.0")).unwrap();

    run(
        install_plugin_event("bundle", "1.0.0", vec![skill_ref("tool", "1.0.0")]),
        &state,
        &reg,
    )
    .await;

    let bundled = std::fs::read_to_string(home.plugin_src("bundle").join("skills/tool/SKILL.md"))
        .expect("the unrevoked referenced version must be bundled");
    assert_eq!(
        bundled, "one-point-oh",
        "the plugin must carry exactly the version that passed the gate"
    );
    cleanup(&state);
}

// ── F6: revoking a skill strips it from plugins installed earlier ───────────

#[tokio::test]
async fn revoking_a_skill_strips_it_from_a_plugin_installed_before_the_revoke() {
    let home = FakeHome::new();
    let state = new_state("f6-strip-existing");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "evil", "1.0.0", "evil body", true);
    seed_skill_version(&state, "good", "1.0.0", "good body", true);

    run(
        install_plugin_event(
            "bundle",
            "1.0.0",
            vec![skill_ref("evil", "1.0.0"), skill_ref("good", "1.0.0")],
        ),
        &state,
        &reg,
    )
    .await;
    let src = home.plugin_src("bundle");
    let cache = home.plugin_cache("bundle", "1.0.0");
    assert!(src.join("skills/evil/SKILL.md").exists(), "precondition");
    assert!(cache.join("skills/evil/SKILL.md").exists(), "precondition");

    run(revoke_event("skill", "evil", None), &state, &reg).await;

    assert!(
        !src.join("skills/evil").exists(),
        "revoke must remove the skill from the plugin's marketplace source"
    );
    assert!(
        !cache.join("skills/evil").exists(),
        "revoke must remove the skill from the plugin's install cache (what Claude Code loads)"
    );
    assert!(src.join("skills/good/SKILL.md").exists());
    assert!(cache.join("skills/good/SKILL.md").exists());
    assert_eq!(governed_version("bundle").as_deref(), Some("1.0.0"));
    cleanup(&state);
}

#[tokio::test]
async fn version_scoped_skill_revoke_only_strips_plugins_bundling_that_version() {
    let home = FakeHome::new();
    let state = new_state("f6-strip-version");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "tool", "1.0.0", "v1", false);
    seed_skill_version(&state, "tool", "2.0.0", "v2", true);

    run(
        install_plugin_event("old", "1.0.0", vec![skill_ref("tool", "1.0.0")]),
        &state,
        &reg,
    )
    .await;
    run(
        install_plugin_event("new", "1.0.0", vec![skill_ref("tool", "2.0.0")]),
        &state,
        &reg,
    )
    .await;

    run(revoke_event("skill", "tool", Some("1.0.0")), &state, &reg).await;

    assert!(!home.plugin_src("old").join("skills/tool").exists());
    assert!(
        home.plugin_src("new").join("skills/tool/SKILL.md").exists(),
        "a plugin carrying an unrevoked version must keep it"
    );
    cleanup(&state);
}

#[tokio::test]
async fn revoking_a_skill_never_touches_imported_or_user_plugins() {
    let home = FakeHome::new();
    let state = new_state("f6-strip-foreign");
    let reg = Arc::new(BackendRegistry::new());

    // An imported plugin (mirrored source tree) that happens to carry a
    // `skills/evil` folder of its own — not the registry skill.
    crate::managed_paths::install_plugin_bundle(&crate::managed_paths::PluginBundle {
        slug: "imported".to_string(),
        name: "imported".to_string(),
        description: String::new(),
        version: "1.0.0".to_string(),
        author: "someone".to_string(),
        skills: vec![],
        files: vec![
            (
                ".claude-plugin/plugin.json".to_string(),
                br#"{"name":"imported"}"#.to_vec(),
            ),
            ("skills/evil/SKILL.md".to_string(), b"theirs".to_vec()),
        ],
        executables: vec![],
    })
    .unwrap();

    // A plugin the user installed from another marketplace.
    let user_skill = home
        .path()
        .join(".claude/plugins/marketplaces/official/plugins/userplug/skills/evil/SKILL.md");
    std::fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
    std::fs::write(&user_skill, "user's").unwrap();

    run(revoke_event("skill", "evil", None), &state, &reg).await;

    assert!(
        home.plugin_src("imported")
            .join("skills/evil/SKILL.md")
            .exists(),
        "an imported plugin's own same-named skill folder must be left alone"
    );
    assert!(home
        .plugin_cache("imported", "1.0.0")
        .join("skills/evil/SKILL.md")
        .exists());
    assert!(
        user_skill.exists(),
        "a user-installed plugin must never be touched"
    );
    cleanup(&state);
}

#[test]
fn snapshot_reinstalls_a_governed_plugin_with_no_bundled_skill_record() {
    // A plugin written by an older daemon has no record of which registry
    // skills it bundled, so a later skill revoke could not find it. The
    // snapshot diff reinstalls it once (through the gated install path),
    // which writes the record.
    let home = FakeHome::new();
    let installed = home.path().join(".claude/plugins/installed_plugins.json");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(
        &installed,
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "plugins": { "legacy@vectorhawk": [{ "scope": "user", "version": "1.0.0" }] }
        }))
        .unwrap(),
    )
    .unwrap();

    let diff =
        super::build_derived_plugin_events_blocking_for_test(vec![PluginInstallationRecord {
            installation_id: Uuid::new_v4(),
            plugin_slug: "legacy".to_string(),
            plugin_name: "legacy".to_string(),
            description: String::new(),
            version: "1.0.0".to_string(),
            author: "VectorHawk".to_string(),
            skills: vec![skill_ref("evil", "1.0.0")],
            state: "installed".to_string(),
        }]);

    assert!(
        matches!(diff.events.as_slice(), [SyncEvent::InstallPlugin { plugin_slug, .. }] if plugin_slug == "legacy"),
        "expected one InstallPlugin for the legacy plugin, got {:?}",
        diff.events
    );
}

// ── F15: version-scoped plugin revocations ──────────────────────────────────

#[tokio::test]
async fn plugin_install_is_blocked_by_a_revoke_of_that_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-plugin-gate");
    let reg = Arc::new(BackendRegistry::new());
    revocation::upsert_revocation(&state, "plugin", "p", Some("1.2.0")).unwrap();

    run(install_plugin_event("p", "1.2.0", vec![]), &state, &reg).await;
    assert_eq!(
        governed_version("p"),
        None,
        "a version-scoped plugin revoke must block installing that version"
    );

    run(install_plugin_event("p", "1.3.0", vec![]), &state, &reg).await;
    assert_eq!(
        governed_version("p").as_deref(),
        Some("1.3.0"),
        "other versions stay installable"
    );
    cleanup(&state);
}

#[tokio::test]
async fn version_scoped_plugin_revoke_leaves_a_different_installed_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-plugin-teardown-other");
    let reg = Arc::new(BackendRegistry::new());
    run(install_plugin_event("p", "1.3.0", vec![]), &state, &reg).await;

    run(revoke_event("plugin", "p", Some("1.2.0")), &state, &reg).await;

    assert_eq!(
        governed_version("p").as_deref(),
        Some("1.3.0"),
        "revoking 1.2.0 must not tear down an installed 1.3.0"
    );
    cleanup(&state);
}

#[tokio::test]
async fn version_scoped_plugin_revoke_tears_down_that_installed_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-plugin-teardown-same");
    let reg = Arc::new(BackendRegistry::new());
    run(install_plugin_event("p", "1.2.0", vec![]), &state, &reg).await;

    run(revoke_event("plugin", "p", Some("1.2.0")), &state, &reg).await;

    assert_eq!(governed_version("p"), None);
    cleanup(&state);
}

// ── F15: version-scoped MCP revocations ─────────────────────────────────────

fn install_mcp_event(sid: Uuid, version_pin: Option<&str>) -> SyncEvent {
    SyncEvent::InstallMcp {
        installation_id: Uuid::new_v4(),
        mcp_server_id: sid,
        mcp_server_name: "Pinned MCP".to_string(),
        package_source: "@org/server".to_string(),
        version_pin: version_pin.map(str::to_string),
        // No transport: the row is written but nothing is spawned.
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
        gateway_url: None,
    }
}

fn mcp_installed(state: &AppState, sid: Uuid) -> bool {
    state
        .list_mcp_installs()
        .unwrap()
        .iter()
        .any(|r| r.mcp_server_id == sid.to_string())
}

#[tokio::test]
async fn pinned_mcp_install_is_blocked_by_a_revoke_of_the_pinned_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-mcp-pinned-same");
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    revocation::upsert_revocation(&state, "mcp_server", &sid.to_string(), Some("1.0.0")).unwrap();

    run(install_mcp_event(sid, Some("1.0.0")), &state, &reg).await;

    assert!(!mcp_installed(&state, sid));
    cleanup(&state);
}

#[tokio::test]
async fn pinned_mcp_install_of_another_version_is_allowed() {
    let _home = FakeHome::new();
    let state = new_state("f15-mcp-pinned-other");
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    revocation::upsert_revocation(&state, "mcp_server", &sid.to_string(), Some("1.0.0")).unwrap();

    run(install_mcp_event(sid, Some("2.0.0")), &state, &reg).await;

    assert!(mcp_installed(&state, sid));
    cleanup(&state);
}

#[tokio::test]
async fn unpinned_mcp_install_fails_closed_on_a_version_scoped_revoke() {
    // Reviewer bypass: with no pin the gate checked only the whole-artifact
    // row, so any version-scoped revoke was ignored.
    let _home = FakeHome::new();
    let state = new_state("f15-mcp-unpinned");
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    revocation::upsert_revocation(&state, "mcp_server", &sid.to_string(), Some("1.0.0")).unwrap();

    run(install_mcp_event(sid, None), &state, &reg).await;

    assert!(
        !mcp_installed(&state, sid),
        "an unpinned install cannot prove it avoids the revoked version — fail closed"
    );
    cleanup(&state);
}

#[tokio::test]
async fn version_scoped_mcp_revoke_leaves_a_server_pinned_to_another_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-mcp-teardown-other");
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    run(install_mcp_event(sid, Some("2.0.0")), &state, &reg).await;
    assert!(mcp_installed(&state, sid), "precondition");

    run(
        revoke_event("mcp_server", &sid.to_string(), Some("1.0.0")),
        &state,
        &reg,
    )
    .await;

    assert!(
        mcp_installed(&state, sid),
        "revoking 1.0.0 must not tear down a server pinned to 2.0.0"
    );
    cleanup(&state);
}

#[tokio::test]
async fn version_scoped_mcp_revoke_tears_down_an_unpinned_server() {
    let _home = FakeHome::new();
    let state = new_state("f15-mcp-teardown-unpinned");
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    run(install_mcp_event(sid, None), &state, &reg).await;
    assert!(mcp_installed(&state, sid), "precondition");

    run(
        revoke_event("mcp_server", &sid.to_string(), Some("1.0.0")),
        &state,
        &reg,
    )
    .await;

    assert!(!mcp_installed(&state, sid));
    cleanup(&state);
}

// ── F15: version-scoped skill revoke teardown ───────────────────────────────

#[tokio::test]
async fn version_scoped_skill_revoke_keeps_a_different_active_version() {
    let _home = FakeHome::new();
    let state = new_state("f15-skill-teardown-other");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "tool", "1.0.0", "v1", false);
    seed_skill_version(&state, "tool", "2.0.0", "v2", true);

    run(revoke_event("skill", "tool", Some("1.0.0")), &state, &reg).await;

    let skill_root = state.root_dir.join("skills").join("tool");
    assert!(
        skill_root.join("active/SKILL.md").exists(),
        "revoking 1.0.0 must not purge an active 2.0.0"
    );
    assert!(
        !skill_root.join("versions/1.0.0").exists(),
        "the revoked version's files must be removed so nothing can roll back to it"
    );
    cleanup(&state);
}

// ── Lock ordering: skill revoke racing a plugin install ─────────────────────

#[tokio::test]
async fn skill_revoke_racing_a_plugin_install_neither_deadlocks_nor_leaves_the_skill() {
    let home = FakeHome::new();
    let state = new_state("f6-race");
    let reg = Arc::new(BackendRegistry::new());
    seed_skill_version(&state, "evil", "1.0.0", "evil body", true);

    let server = mockito::Server::new_async().await;
    let locks: super::SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    let mut tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    // Hold the only permit so the install parks while holding its plugin
    // lock — the revoke then runs its skill-lock section and must reach
    // for the plugin lock without still holding the skill lock.
    let sem = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = Arc::clone(&sem).acquire_owned().await.unwrap();

    for event in [
        install_plugin_event("bundle", "1.0.0", vec![skill_ref("evil", "1.0.0")]),
        revoke_event("skill", "evil", None),
    ] {
        super::dispatch_event(
            event,
            &state,
            &server.url(),
            &sem,
            &locks,
            &stats,
            &mut tasks,
            &reg,
            list_changed_tx.clone(),
            None,
        )
        .await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }
    drop(permit);

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(res) = tasks.join_next().await {
            assert!(res.is_ok());
        }
    })
    .await
    .expect("revoke and plugin install must not deadlock");

    assert_eq!(governed_version("bundle").as_deref(), Some("1.0.0"));
    assert!(!home.plugin_src("bundle").join("skills/evil").exists());
    assert!(!home
        .plugin_cache("bundle", "1.0.0")
        .join("skills/evil")
        .exists());
    cleanup(&state);
}
