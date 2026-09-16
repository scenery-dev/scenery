//! Unit tests for the plugin desired-state snapshot reconciler (RB1).
//!
//! The daemon already fully handles the live `install_plugin` SSE delta
//! (`SyncEvent::InstallPlugin` → `spawn_install_plugin` → the local
//! `vectorhawk` Claude Code marketplace). These tests cover the new
//! `plugin_installations` snapshot key: a plugin installed before a device
//! registered (or while a live delta was dropped) is only ever delivered
//! through the snapshot, so the daemon must converge it the same way it
//! converges MCP snapshot state — reusing the exact same install/deactivate
//! code paths the live event uses.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::aggregator::BackendRegistry;

use super::{dispatch_event, skill_lock, SkillLockMap};
use crate::managed_paths::ENV_MUTEX;
use crate::sync::sse_client::{PluginInstallationRecord, PluginSkillRef, SyncEvent};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Guard that points `$HOME` at a fresh temp dir for the duration of the test,
/// restoring the previous value on drop. Serialized via `ENV_MUTEX` since
/// `$HOME` is process-global — same pattern as `plugin_marketplace_tests.rs`.
struct FakeHome {
    _tmp: tempfile::TempDir,
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
            _tmp: tmp,
            prev,
            _guard: guard,
        }
    }

    fn path(&self) -> &std::path::Path {
        self._tmp.path()
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

fn install_id() -> Uuid {
    Uuid::new_v4()
}

fn make_record(slug: &str, iid: Uuid, state: &str) -> PluginInstallationRecord {
    PluginInstallationRecord {
        installation_id: iid,
        plugin_slug: slug.to_string(),
        plugin_name: slug.to_string(),
        description: "test plugin".to_string(),
        version: "1.0.0".to_string(),
        author: "VectorHawk".to_string(),
        skills: vec![PluginSkillRef {
            skill_id: "some-skill".to_string(),
            version: "1.0.0".to_string(),
        }],
        state: state.to_string(),
    }
}

/// Seed `installed_plugins.json` directly (bypassing `install_plugin_bundle`,
/// which also downloads skill content) so tests can control local state
/// precisely. Mirrors the wire shape `plugin_marketplace::upsert_installed_
/// plugins` writes.
fn seed_installed_plugins(home: &std::path::Path, entries: &[(&str, &str)]) {
    let path = home.join(".claude/plugins/installed_plugins.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut plugins = serde_json::Map::new();
    for (key, version) in entries {
        plugins.insert(
            key.to_string(),
            serde_json::json!([{
                "scope": "user",
                "installPath": "/fake/path",
                "version": version,
                "installedAt": "2026-01-01T00:00:00Z",
                "lastUpdated": "2026-01-01T00:00:00Z",
            }]),
        );
    }
    let root = serde_json::json!({ "version": 2, "plugins": plugins });
    std::fs::write(&path, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
}

fn read_installed_plugins(home: &std::path::Path) -> serde_json::Value {
    let path = home.join(".claude/plugins/installed_plugins.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

// ── list_governed_plugins ───────────────────────────────────────────────────

#[test]
fn list_governed_plugins_scopes_to_vectorhawk_marketplace() {
    let home = FakeHome::new();
    seed_installed_plugins(
        home.path(),
        &[
            ("superpowers@vectorhawk", "5.0.7"),
            ("other-plugin@official", "1.2.0"),
        ],
    );

    let governed = crate::managed_paths::list_governed_plugins().unwrap();

    assert_eq!(governed.len(), 1, "only vectorhawk-marketplace entries");
    assert_eq!(
        governed.get("superpowers").map(String::as_str),
        Some("5.0.7")
    );
    assert!(
        !governed.contains_key("other-plugin"),
        "foreign-marketplace plugin must never be reported as governed"
    );
}

// ── Snapshot plugin reconciliation ──────────────────────────────────────────

#[test]
fn snapshot_plugin_not_installed_produces_one_install_event() {
    let home = FakeHome::new();
    let _ = home; // no local installed_plugins.json at all

    let records = vec![make_record("superpowers", install_id(), "desired")];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    assert_eq!(
        diff.events.len(),
        1,
        "one desired, not-installed plugin → one event"
    );
    assert!(diff.silent_purges.is_empty());
    match &diff.events[0] {
        SyncEvent::InstallPlugin { plugin_slug, .. } => {
            assert_eq!(plugin_slug, "superpowers");
        }
        other => panic!("expected InstallPlugin, got {other:?}"),
    }
}

#[test]
fn snapshot_plugin_already_installed_is_noop() {
    let home = FakeHome::new();
    seed_installed_plugins(home.path(), &[("superpowers@vectorhawk", "1.0.0")]);

    let records = vec![make_record("superpowers", install_id(), "installed")];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    assert!(
        diff.events.is_empty(),
        "already-installed plugin present in snapshot → no-op"
    );
    assert!(diff.silent_purges.is_empty());
}

#[test]
fn snapshot_plugin_empty_array_is_noop_for_existing_installs() {
    // Backwards-compat: old backend does not emit `plugin_installations`. The
    // SSE parser defaults it to an empty vec (see sse_client.rs `#[serde(default)]`).
    // dispatch_event guards this before calling the async wrapper, but the
    // blocking function must also be a safe no-op when called directly.
    let home = FakeHome::new();
    seed_installed_plugins(home.path(), &[("superpowers@vectorhawk", "1.0.0")]);

    let diff = super::build_derived_plugin_events_blocking_for_test(vec![]);

    assert!(diff.events.is_empty(), "empty snapshot → no events");
    assert!(diff.silent_purges.is_empty());
    // Existing install must be left completely untouched.
    let governed = crate::managed_paths::list_governed_plugins().unwrap();
    assert_eq!(
        governed.get("superpowers").map(String::as_str),
        Some("1.0.0"),
        "empty snapshot must not wipe existing governed plugin installs"
    );
}

#[test]
fn snapshot_plugin_orphan_is_queued_for_silent_purge_and_diff_is_pure() {
    // Scenario: daemon was offline; backend deleted plugin B's installation
    // row from the catalog entirely. Snapshot now only contains plugin A.
    // Plugin B (governed) must be queued for a silent purge (no event — see
    // `PluginDiff` doc comment). A plugin the user installed manually from a
    // different marketplace must never be classified as governed at all.
    //
    // RB1 fix round 2: the diff function itself must not touch the
    // filesystem — actual removal is executed later by the caller through
    // the locked `spawn_purge_plugin` handler. This test asserts both the
    // classification AND that `installed_plugins.json` is byte-for-byte
    // unchanged immediately after the diff call.
    let home = FakeHome::new();
    seed_installed_plugins(
        home.path(),
        &[
            ("plugin-a@vectorhawk", "1.0.0"),
            ("plugin-b@vectorhawk", "1.0.0"),
            ("manual-plugin@official", "2.0.0"),
        ],
    );
    let before = read_installed_plugins(home.path());

    let records = vec![make_record("plugin-a", install_id(), "installed")];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    assert!(
        diff.events.is_empty(),
        "plugin A already installed and is the only snapshot entry → no events"
    );
    assert_eq!(
        diff.silent_purges,
        vec!["plugin-b".to_string()],
        "only the governed orphan (plugin-b) is queued for a silent purge — \
         plugin A is in the snapshot, and manual-plugin was never governed"
    );

    let after = read_installed_plugins(home.path());
    assert_eq!(
        before, after,
        "the diff function must not mutate installed_plugins.json — orphan \
         removal is deferred to the locked spawn_purge_plugin handler"
    );
}

#[test]
fn snapshot_plugin_deactivated_state_emits_event_without_mutating_filesystem() {
    let home = FakeHome::new();
    seed_installed_plugins(home.path(), &[("superpowers@vectorhawk", "1.0.0")]);
    let before = read_installed_plugins(home.path());

    let iid = install_id();
    let records = vec![make_record("superpowers", iid, "deactivated")];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    assert_eq!(diff.events.len(), 1);
    assert!(diff.silent_purges.is_empty());
    match &diff.events[0] {
        SyncEvent::DeactivatePlugin {
            plugin_slug,
            installation_id,
        } => {
            assert_eq!(plugin_slug, "superpowers");
            assert_eq!(*installation_id, iid);
        }
        other => panic!("expected DeactivatePlugin, got {other:?}"),
    }

    // RB1 fix round 2: the diff must not remove the install itself — that
    // now happens only once the emitted DeactivatePlugin event reaches the
    // locked `spawn_deactivate_plugin` handler.
    let after = read_installed_plugins(home.path());
    assert_eq!(
        before, after,
        "the diff function must not mutate installed_plugins.json for a \
         deactivated plugin — only the locked handler may"
    );
}

#[test]
fn snapshot_plugin_removed_state_is_queued_for_silent_purge_and_diff_is_pure() {
    let home = FakeHome::new();
    seed_installed_plugins(home.path(), &[("superpowers@vectorhawk", "1.0.0")]);
    let before = read_installed_plugins(home.path());

    let records = vec![make_record("superpowers", install_id(), "removed")];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    assert!(
        diff.events.is_empty(),
        "removed state purges without emitting an event"
    );
    assert_eq!(diff.silent_purges, vec!["superpowers".to_string()]);

    let after = read_installed_plugins(home.path());
    assert_eq!(
        before, after,
        "the diff function must not mutate installed_plugins.json for a \
         removed plugin — only the locked spawn_purge_plugin handler may"
    );
}

#[test]
fn build_derived_plugin_events_blocking_never_touches_filesystem() {
    // Explicit, comprehensive purity check (RB1 fix round 2 review ask):
    // seed one governed plugin in every branch the diff can take — install,
    // no-op, deactivate, silent removed-purge, and an orphan (absent from
    // records) — and assert `installed_plugins.json` is byte-identical
    // before and after, and that the diff never even creates the
    // marketplace directory tree (which only `install_plugin_bundle`/
    // `uninstall_plugin_bundle` write to).
    let home = FakeHome::new();
    seed_installed_plugins(
        home.path(),
        &[
            ("already@vectorhawk", "1.0.0"),
            ("to-deactivate@vectorhawk", "1.0.0"),
            ("to-remove@vectorhawk", "1.0.0"),
            ("orphan@vectorhawk", "1.0.0"),
        ],
    );
    let before = read_installed_plugins(home.path());

    let records = vec![
        make_record("new-plugin", install_id(), "desired"),
        make_record("already", install_id(), "installed"),
        make_record("to-deactivate", install_id(), "deactivated"),
        make_record("to-remove", install_id(), "removed"),
        // "orphan" deliberately absent — exercises the orphan branch.
    ];

    let diff = super::build_derived_plugin_events_blocking_for_test(records);

    // Sanity: every branch actually produced *something*, so this is a
    // meaningful test of purity, not a trivial no-op check.
    assert_eq!(
        diff.events.len(),
        2,
        "install(new-plugin) + deactivate(to-deactivate)"
    );
    assert_eq!(
        diff.silent_purges.len(),
        2,
        "orphan(orphan) + removed-purge(to-remove)"
    );

    let after = read_installed_plugins(home.path());
    assert_eq!(
        before, after,
        "the diff function must never mutate installed_plugins.json, \
         regardless of which branch (install/no-op/deactivate/removed/orphan) \
         it takes"
    );

    let marketplace_plugins_dir = home
        .path()
        .join(".claude/plugins/marketplaces/vectorhawk/plugins");
    assert!(
        !marketplace_plugins_dir.exists(),
        "the diff function must never touch the marketplace filesystem tree \
         — only install_plugin_bundle/uninstall_plugin_bundle may, and only \
         under the caller's 'plugin:<slug>' lock"
    );
}

// ── Concurrency: live install_plugin race vs. snapshot-derived install ─────
//
// Review finding (fix round 1): spawn_install_plugin/spawn_deactivate_plugin
// took no lock, unlike skills and MCP which route every handler (live AND
// snapshot-derived) through the shared `skill_locks: &SkillLockMap`. RB1
// adds a second trigger (the periodic snapshot poll in `run_sync_tick`) that
// can race the live `install_plugin` SSE delta for the same slug —
// `install_plugin_bundle` does an unlocked `remove_dir_all` + rewrite of the
// plugin source tree, so two concurrent installs could interleave and mix
// versions. Fixed by keying a lock as `"plugin:<slug>"` (namespaced so it
// can never collide with a skill_id or mcp_server_id lock) and acquiring it
// in both `spawn_install_plugin` and `spawn_deactivate_plugin`, before the
// semaphore — same ordering `spawn_install` already uses for skills.

fn temp_root(label: &str) -> camino::Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    camino::Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-plugin-reconciler-tests-{label}-{nanos}")),
    )
    .expect("temp path utf-8")
}

#[tokio::test]
async fn concurrent_live_and_snapshot_install_for_same_slug_are_serialized() {
    let home = FakeHome::new();

    // Registry mock server: every HTTP call this path makes (the imported-
    // plugin content fetch, the PATCH status callback) is best-effort /
    // error-tolerant — an unmocked mockito route responds quickly (501)
    // rather than hanging, so no explicit mocks are required for the
    // assertions below.
    let server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let root = temp_root("concurrent-install");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    let skill_locks: SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // A single-permit semaphore, held by the test up front: forces the first
    // spawned install to block *after* acquiring the plugin lock, giving a
    // deterministic window to prove the second (snapshot-derived) install
    // for the same slug is blocked by the lock rather than by scheduling
    // luck.
    let sem = Arc::new(tokio::sync::Semaphore::new(1));
    let held_permit = Arc::clone(&sem).acquire_owned().await.unwrap();

    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let backend_registry = Arc::new(BackendRegistry::new());
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();

    let slug = "racer";

    let live_event = SyncEvent::InstallPlugin {
        installation_id: install_id(),
        plugin_slug: slug.to_string(),
        plugin_name: slug.to_string(),
        description: "test plugin".to_string(),
        version: "1.0.0".to_string(),
        author: "VectorHawk".to_string(),
        skills: vec![],
    };

    // Dispatch the live SSE event: spawns task A, which acquires the
    // "plugin:racer" lock (uncontended) and then blocks on the semaphore
    // (held by the test) before doing any real install work.
    dispatch_event(
        live_event,
        &state,
        &registry_url,
        &sem,
        &skill_locks,
        &stats,
        &mut install_tasks,
        &backend_registry,
        list_changed_tx.clone(),
        None,
    )
    .await;

    // Give task A's spawned future a chance to run up to the semaphore wait.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // The plugin lock for this slug must now be held by task A. This is the
    // direct RED/GREEN signal: before the fix, spawn_install_plugin never
    // touched any lock, so this assertion would fail (try_lock would
    // succeed on a lock nobody was holding).
    let lock = skill_lock(&skill_locks, &format!("plugin:{slug}"));
    assert!(
        lock.try_lock().is_err(),
        "task A (live install_plugin event) must hold the plugin lock while \
         blocked on the semaphore"
    );

    // Dispatch a snapshot containing a desired record for the SAME slug.
    // It is not yet installed locally (task A never got past the
    // semaphore), so the diff spawns a second InstallPlugin — task B — for
    // "racer", exactly the race the review flagged (live delta + periodic
    // snapshot poll converging on the same plugin concurrently).
    let snapshot_event = SyncEvent::Snapshot {
        installations: vec![],
        mcp_installations: vec![],
        plugin_installations: vec![make_record(slug, install_id(), "desired")],
    };
    dispatch_event(
        snapshot_event,
        &state,
        &registry_url,
        &sem,
        &skill_locks,
        &stats,
        &mut install_tasks,
        &backend_registry,
        list_changed_tx.clone(),
        None,
    )
    .await;

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // Task B must be queued behind the SAME lock task A holds — still true
    // after dispatching the snapshot-derived install.
    assert!(
        lock.try_lock().is_err(),
        "the plugin lock must still be held after dispatching the \
         snapshot-derived install for the same slug — proves both the live \
         and snapshot-derived paths serialize on one shared lock"
    );

    // Release the semaphore: task A proceeds, installs, releases the lock;
    // task B then acquires it and runs (redundantly but safely — the
    // "already installed" check happens before spawning, not after
    // acquiring the lock, matching the MCP diff's own idempotent-upsert
    // precedent). Neither task may ever run concurrently with the other.
    drop(held_permit);

    let mut completed = 0;
    while let Some(res) = install_tasks.join_next().await {
        assert!(res.is_ok(), "install task must not panic");
        completed += 1;
    }
    assert_eq!(
        completed, 2,
        "both the live and snapshot-derived installs must run to completion"
    );

    // Final state: one consistent, fully-written install — not corrupted by
    // interleaved concurrent writes (install_plugin_bundle does an unlocked
    // remove_dir_all + rewrite of the plugin source tree).
    let governed = crate::managed_paths::list_governed_plugins().unwrap();
    assert_eq!(
        governed.get(slug).map(String::as_str),
        Some("1.0.0"),
        "plugin must end up installed at a single consistent version"
    );
    let plugin_json = home
        .path()
        .join(".claude/plugins/marketplaces/vectorhawk/plugins/racer/.claude-plugin/plugin.json");
    assert!(
        plugin_json.exists(),
        "plugin.json must exist and be fully written, not torn by an \
         interleaved concurrent install"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ── Concurrency: live install race vs. snapshot-derived orphan purge ───────
//
// Review finding (fix round 2): build_derived_plugin_events_blocking used to
// call uninstall_plugin_bundle directly and *unlocked* for orphans,
// "deactivated", and "removed" — racing a locked live install_plugin writing
// the same plugin tree. Fixed by making the diff function pure (compute
// only) and moving every removal (orphan, deactivated, removed) behind the
// same "plugin:<slug>" lock spawn_install_plugin/spawn_deactivate_plugin use
// — orphans and "removed" rows go through the new spawn_purge_plugin (no
// PATCH callback, mirroring the MCP diff's own unlocked-but-atomic-SQL
// orphan/"removed" handling), while "deactivated" still goes through the
// existing DeactivatePlugin event.

#[tokio::test]
async fn concurrent_live_install_and_snapshot_derived_orphan_purge_for_same_slug_are_serialized() {
    let home = FakeHome::new();
    // "furnace" is already governed+installed locally (simulating a prior
    // run); "decoy" is also governed+installed and stays in the snapshot as
    // already-installed (state="installed") purely so the records list is
    // non-empty without spawning any extra work for it.
    seed_installed_plugins(
        home.path(),
        &[
            ("furnace@vectorhawk", "1.0.0"),
            ("decoy@vectorhawk", "1.0.0"),
        ],
    );

    let server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let root = temp_root("concurrent-orphan");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    let skill_locks: SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let sem = Arc::new(tokio::sync::Semaphore::new(1));
    let held_permit = Arc::clone(&sem).acquire_owned().await.unwrap();

    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let backend_registry = Arc::new(BackendRegistry::new());
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();

    let slug = "furnace";

    // A live event re-installs "furnace" (e.g. an admin pushed an update).
    // The held semaphore forces it to block right after acquiring the
    // "plugin:furnace" lock, before doing any real install work.
    let live_event = SyncEvent::InstallPlugin {
        installation_id: install_id(),
        plugin_slug: slug.to_string(),
        plugin_name: slug.to_string(),
        description: "test plugin".to_string(),
        version: "1.0.0".to_string(),
        author: "VectorHawk".to_string(),
        skills: vec![],
    };
    dispatch_event(
        live_event,
        &state,
        &registry_url,
        &sem,
        &skill_locks,
        &stats,
        &mut install_tasks,
        &backend_registry,
        list_changed_tx.clone(),
        None,
    )
    .await;

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    let lock = skill_lock(&skill_locks, &format!("plugin:{slug}"));
    assert!(
        lock.try_lock().is_err(),
        "the live install must hold the plugin lock while blocked on the semaphore"
    );

    // A snapshot poll lands concurrently in which "furnace" is entirely
    // absent (its installation row was deleted from the backend catalog
    // while offline) — an orphan. "decoy" stays in the snapshot as
    // already-installed so the records list isn't empty (which would early-
    // return and skip orphan detection).
    let snapshot_event = SyncEvent::Snapshot {
        installations: vec![],
        mcp_installations: vec![],
        plugin_installations: vec![make_record("decoy", install_id(), "installed")],
    };
    dispatch_event(
        snapshot_event,
        &state,
        &registry_url,
        &sem,
        &skill_locks,
        &stats,
        &mut install_tasks,
        &backend_registry,
        list_changed_tx.clone(),
        None,
    )
    .await;

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // The snapshot-derived orphan purge for "furnace" must be queued behind
    // the SAME lock the live install holds — not racing it.
    assert!(
        lock.try_lock().is_err(),
        "the plugin lock must still be held after dispatching the snapshot- \
         derived orphan purge for the same slug — proves the live install \
         and the snapshot-derived purge serialize on one shared lock"
    );

    drop(held_permit);

    let mut completed = 0;
    while let Some(res) = install_tasks.join_next().await {
        assert!(res.is_ok(), "task must not panic");
        completed += 1;
    }
    assert_eq!(
        completed, 2,
        "both the live install and the snapshot-derived orphan purge must \
         run to completion (decoy was already installed — no task for it)"
    );

    // Final state must be fully consistent either way — not a torn mix of
    // the install's writes and the purge's remove_dir_all. Execution order
    // here is deterministic (install first, purge second, since the purge
    // was blocked behind the install's lock), so the purge's removal wins.
    let governed = crate::managed_paths::list_governed_plugins().unwrap();
    assert!(
        !governed.contains_key(slug),
        "furnace ends up fully purged (the orphan purge ran after the \
         install, serialized — not interleaved with it)"
    );
    let plugin_dir = home
        .path()
        .join(".claude/plugins/marketplaces/vectorhawk/plugins/furnace");
    assert!(
        !plugin_dir.exists(),
        "furnace's plugin directory must be cleanly removed, not left as a \
         torn mix of the install's writes and the purge's remove_dir_all"
    );
    // decoy was untouched throughout.
    assert!(governed.contains_key("decoy"));

    let _ = std::fs::remove_dir_all(&root);
}

// ── End-to-end: dispatch_event actually performs the deferred mutation ─────
//
// Now that build_derived_plugin_events_blocking is pure (fix round 2), this
// confirms the full production pipeline — dispatch_event → PluginDiff →
// spawn_deactivate_plugin / spawn_purge_plugin — still converges local state
// correctly for all three removal cases in one snapshot, non-concurrently.

#[tokio::test]
async fn dispatch_event_snapshot_converges_deactivated_and_orphan_and_removed_plugins() {
    let home = FakeHome::new();
    seed_installed_plugins(
        home.path(),
        &[
            ("to-deactivate@vectorhawk", "1.0.0"),
            ("to-remove@vectorhawk", "1.0.0"),
            ("orphan@vectorhawk", "1.0.0"),
            ("keep@vectorhawk", "1.0.0"),
        ],
    );

    let server = mockito::Server::new_async().await;
    let registry_url = server.url();
    let root = temp_root("e2e-plugin-removal");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    let skill_locks: SkillLockMap =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let backend_registry = Arc::new(BackendRegistry::new());
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();

    let snapshot_event = SyncEvent::Snapshot {
        installations: vec![],
        mcp_installations: vec![],
        plugin_installations: vec![
            make_record("to-deactivate", install_id(), "deactivated"),
            make_record("to-remove", install_id(), "removed"),
            make_record("keep", install_id(), "installed"),
            // "orphan" deliberately absent.
        ],
    };
    dispatch_event(
        snapshot_event,
        &state,
        &registry_url,
        &sem,
        &skill_locks,
        &stats,
        &mut install_tasks,
        &backend_registry,
        list_changed_tx.clone(),
        None,
    )
    .await;

    let mut completed = 0;
    while let Some(res) = install_tasks.join_next().await {
        assert!(res.is_ok(), "task must not panic");
        completed += 1;
    }
    assert_eq!(
        completed, 3,
        "deactivate(to-deactivate) + purge(to-remove) + purge(orphan) — \
         keep needs no task, it's already installed"
    );

    let governed = crate::managed_paths::list_governed_plugins().unwrap();
    assert!(!governed.contains_key("to-deactivate"));
    assert!(!governed.contains_key("to-remove"));
    assert!(!governed.contains_key("orphan"));
    assert!(governed.contains_key("keep"));

    let _ = std::fs::remove_dir_all(&root);
}
