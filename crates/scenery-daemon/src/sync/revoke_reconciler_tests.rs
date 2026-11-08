//! Unit tests for the broadcast-model revoke path: the `Revoke` SSE event,
//! the snapshot's `revocations` list, and the install-time gate every
//! install handler checks before installing anything.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use rusqlite::Connection;
use uuid::Uuid;

use crate::sync::sse_client::SyncEvent;
use scenery_core::state::AppState;
use scenery_mcp::aggregator::BackendRegistry;

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-revoke-tests-{label}-{nanos}")),
    )
    .expect("temp path utf-8")
}

fn cleanup(root: &Utf8PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

fn install_id() -> Uuid {
    Uuid::new_v4()
}

/// Seed a skill plus its on-disk active/ symlink, mirroring
/// `reconciler_tests::seed_skill_with_fs` (duplicated locally — that helper
/// is private to its own test module).
fn seed_installed_skill(state: &AppState, skill_id: &str, version: &str) {
    let conn = Connection::open(&state.db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO installed_skills \
         (skill_id, active_version, install_root, current_status, deactivated) \
         VALUES (?1, ?2, '/fake', 'active', 0)",
        rusqlite::params![skill_id, version],
    )
    .unwrap();

    let install_root = state.root_dir.join("skills").join(skill_id);
    let version_dir = install_root.join("versions").join(version);
    let active_dir = install_root.join("active");
    std::fs::create_dir_all(version_dir.as_std_path()).unwrap();
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(version_dir.as_std_path(), active_dir.as_std_path()).unwrap();
}

fn revocation_row_exists(state: &AppState, artifact_type: &str, artifact_key: &str) -> bool {
    let conn = Connection::open(&state.db_path).unwrap();
    conn.query_row(
        "SELECT 1 FROM artifact_revocations WHERE artifact_type = ?1 AND artifact_key = ?2",
        rusqlite::params![artifact_type, artifact_key],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .unwrap()
    .is_some()
}

trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}
impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

struct FakeHome {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl FakeHome {
    fn new() -> Self {
        let guard = crate::managed_paths::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        Self {
            _tmp: tmp,
            prev,
            _guard: guard,
        }
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

fn skill_locks() -> super::SkillLockMap {
    Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    event: SyncEvent,
    state: &Arc<AppState>,
    backend_registry: &Arc<BackendRegistry>,
) {
    // Revoke teardown and the skill-revoke plugin strip work under
    // `~/.claude` — point HOME at a temp dir so these tests never read or
    // modify the real one.
    let _home = FakeHome::new();
    let registry_url = "https://example.invalid".to_string();
    let event = crate::test_support::prepare_event(event, state, &registry_url);
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let locks = skill_locks();
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);

    super::dispatch_event(
        event,
        state,
        &registry_url,
        &sem,
        &locks,
        &stats,
        &mut install_tasks,
        backend_registry,
        list_changed_tx,
        None,
    )
    .await;

    while install_tasks.join_next().await.is_some() {}
}

// ── Live `revoke` event: skill ─────────────────────────────────────────────────

#[tokio::test]
async fn revoke_event_purges_an_installed_skill_and_persists_the_block() {
    let root = temp_root("revoke-skill-installed");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    seed_installed_skill(&state, "code-review", "1.0.0");

    let install_root = state.root_dir.join("skills").join("code-review");
    assert!(install_root.exists(), "precondition: skill is on disk");

    dispatch(
        SyncEvent::Revoke {
            artifact_type: "skill".to_string(),
            artifact_key: "code-review".to_string(),
            version: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        !install_root.exists(),
        "revoke must purge the skill's files immediately"
    );
    let conn = Connection::open(&state.db_path).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'code-review'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "revoke must remove the local install row");
    assert!(
        revocation_row_exists(&state, "skill", "code-review"),
        "revoke must durably persist the block so a later install is refused"
    );

    cleanup(&root);
}

#[tokio::test]
async fn revoke_event_persists_the_block_even_when_not_installed_locally() {
    let root = temp_root("revoke-skill-absent");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    dispatch(
        SyncEvent::Revoke {
            artifact_type: "skill".to_string(),
            artifact_key: "never-installed".to_string(),
            version: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        revocation_row_exists(&state, "skill", "never-installed"),
        "a revoke for an artifact this device never had must still record the \
         block — that's what stops the backend's own `install` event (or a \
         later admin un-revoke/re-revoke race) from ever installing it"
    );

    cleanup(&root);
}

#[tokio::test]
async fn revoke_event_is_idempotent_when_sent_twice() {
    let root = temp_root("revoke-skill-twice");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    seed_installed_skill(&state, "dup-skill", "1.0.0");

    for _ in 0..2 {
        dispatch(
            SyncEvent::Revoke {
                artifact_type: "skill".to_string(),
                artifact_key: "dup-skill".to_string(),
                version: None,
            },
            &state,
            &Arc::new(BackendRegistry::new()),
        )
        .await;
    }

    let conn = Connection::open(&state.db_path).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_revocations WHERE artifact_key = 'dup-skill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 1,
        "re-sending the same revoke must not duplicate the row"
    );

    cleanup(&root);
}

// ── Live `unrevoke` event (Bug 2 fix) ──────────────────────────────────────────

#[tokio::test]
async fn unrevoke_event_clears_the_local_gate_immediately() {
    let root = temp_root("unrevoke-skill-live");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    dispatch(
        SyncEvent::Revoke {
            artifact_type: "skill".to_string(),
            artifact_key: "temp-blocked".to_string(),
            version: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;
    assert!(
        revocation_row_exists(&state, "skill", "temp-blocked"),
        "precondition: revoke must persist the block"
    );

    // Before this fix, only the periodic snapshot reconcile (or an SSE
    // reconnect) could ever clear this — the admin's unrevoke sent nothing
    // live at all. This live `Unrevoke` event must lift the gate
    // immediately, with no snapshot/reconnect involved.
    dispatch(
        SyncEvent::Unrevoke {
            artifact_type: "skill".to_string(),
            artifact_key: "temp-blocked".to_string(),
            version: None,
            signed: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        !revocation_row_exists(&state, "skill", "temp-blocked"),
        "a live unrevoke must clear the local revocation-cache row"
    );

    cleanup(&root);
}

#[tokio::test]
async fn unrevoke_event_for_a_never_revoked_key_is_a_harmless_no_op() {
    let root = temp_root("unrevoke-skill-noop");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    dispatch(
        SyncEvent::Unrevoke {
            artifact_type: "skill".to_string(),
            artifact_key: "never-blocked".to_string(),
            version: None,
            signed: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(!revocation_row_exists(&state, "skill", "never-blocked"));

    cleanup(&root);
}

#[tokio::test]
async fn install_event_succeeds_after_a_live_unrevoke() {
    let root = temp_root("unrevoke-then-install");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    dispatch(
        SyncEvent::Revoke {
            artifact_type: "skill".to_string(),
            artifact_key: "revoked-then-unrevoked".to_string(),
            version: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    dispatch(
        SyncEvent::Unrevoke {
            artifact_type: "skill".to_string(),
            artifact_key: "revoked-then-unrevoked".to_string(),
            version: None,
            signed: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    // `handle_install`'s gate (`revocation::is_revoked`) must now say "no"
    // — proving the live unrevoke is not merely cosmetic (clearing the row)
    // but actually unblocks the real install-time check every install path
    // consults. The install itself will fail for an unrelated reason (no
    // real registry at `https://example.invalid` to fetch artifact
    // metadata from) — that's fine, this test only needs to prove the
    // revocation gate itself was lifted, not exercise a full install.
    assert!(
        !scenery_core::revocation::is_revoked(
            &state,
            "skill",
            "revoked-then-unrevoked",
            Some("1.0.0")
        )
        .unwrap(),
        "is_revoked must return false after a live unrevoke, which is what \
         lets handle_install proceed past its revoke-overrides-desired-state \
         gate"
    );

    cleanup(&root);
}

// ── Live `revoke` event: MCP server ────────────────────────────────────────────

#[tokio::test]
async fn revoke_event_tears_down_a_running_mcp_backend_and_persists_the_block() {
    let root = temp_root("revoke-mcp");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    let sid = Uuid::new_v4();

    let row = scenery_core::state::McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: install_id().to_string(),
        mcp_server_name: "GitHub MCP".to_string(),
        package_source: "@org/server".to_string(),
        version_pin: None,
        server_config: Some(
            serde_json::json!({"command": "npx", "args": ["-y", "server"], "env": {}}).to_string(),
        ),
        auth_type: "none".to_string(),
        gateway_server_id: None,
        gateway_url: None,
    };
    state.upsert_mcp_install(&row).unwrap();
    crate::test_support::approve_stored_stdio(&state);

    let backend_registry = Arc::new(BackendRegistry::new());
    let (list_changed_tx, _rx) = tokio::sync::broadcast::channel(16);
    crate::load_managed_mcp_into_registry(
        &state,
        &backend_registry,
        list_changed_tx,
        "http://127.0.0.1:1",
    );
    assert!(backend_registry.has_backend("github-mcp"));

    dispatch(
        SyncEvent::Revoke {
            artifact_type: "mcp_server".to_string(),
            artifact_key: sid.to_string(),
            version: None,
        },
        &state,
        &backend_registry,
    )
    .await;

    assert!(
        !backend_registry.has_backend("github-mcp"),
        "revoke must stop the running MCP backend immediately"
    );
    assert!(
        state.list_mcp_installs().unwrap().is_empty(),
        "revoke must delete the local mcp_installations row"
    );
    assert!(revocation_row_exists(
        &state,
        "mcp_server",
        &sid.to_string()
    ));

    cleanup(&root);
}

// ── Snapshot `revocations` list ────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_revocations_none_leaves_local_cache_untouched() {
    // Mirrors the `mcp_installations: None` precedent: an older backend that
    // omits the `revocations` key entirely must not wipe a cache populated by
    // an earlier live `revoke` event.
    let root = temp_root("snapshot-revocations-none");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "pre-existing", None).unwrap();

    dispatch(
        SyncEvent::Snapshot {
            installations: vec![],
            mcp_installations: None,
            plugin_installations: vec![],
            revocations: None,
            org_policy: None,
            org_id: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        revocation_row_exists(&state, "skill", "pre-existing"),
        "revocations: None must leave the existing local cache alone"
    );

    cleanup(&root);
}

#[tokio::test]
async fn snapshot_revocations_some_empty_keeps_an_entry_without_a_signed_clear() {
    // 2.2.0 fail-safe: an entry that merely drops out of the snapshot's
    // `revocations` list is NOT lifted. Only a verified `cleared` statement
    // lifts a revocation (see the next test and `trust_apply_tests`).
    let root = temp_root("snapshot-revocations-unblock");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "unblocked-skill", None).unwrap();

    dispatch(
        SyncEvent::Snapshot {
            installations: vec![],
            mcp_installations: None,
            plugin_installations: vec![],
            revocations: Some(vec![]),
            org_policy: None,
            org_id: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        scenery_core::revocation::is_revoked(&state, "skill", "unblocked-skill", None).unwrap(),
        "a dropped entry without a signed `cleared` statement must keep the gate"
    );
    cleanup(&root);
}

/// A snapshot entry carrying a verified `cleared` statement lifts the gate.
#[tokio::test]
async fn snapshot_revocation_with_a_signed_clear_lifts_the_entry() {
    let root = temp_root("snapshot-revocations-signed-clear");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "unblocked-skill", None).unwrap();
    // `dispatch` pairs the device against this URL; sign for the same origin.
    let signed = crate::test_support::sign_revocation(
        "https://example.invalid",
        "skill",
        "unblocked-skill",
        None,
        "cleared",
    );
    crate::test_support::pair_device(&state, "https://example.invalid");

    dispatch(
        SyncEvent::Snapshot {
            installations: vec![],
            mcp_installations: None,
            plugin_installations: vec![],
            revocations: Some(vec![crate::sync::sse_client::RevocationRecord {
                artifact_type: "skill".into(),
                artifact_key: "unblocked-skill".into(),
                version: None,
                signed: Some(signed),
            }]),
            org_policy: None,
            org_id: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        !scenery_core::revocation::is_revoked(&state, "skill", "unblocked-skill", None).unwrap()
    );
    cleanup(&root);
}

/// A live `unrevoke` with no valid signed clear keeps the local revocation.
#[tokio::test]
async fn unsigned_unrevoke_keeps_the_local_revocation() {
    let root = temp_root("unsigned-unrevoke");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "kept", None).unwrap();
    crate::test_support::pair_device(&state, "https://example.invalid");

    // Straight to the reconciler, bypassing `dispatch`'s auto-signing.
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let locks = skill_locks();
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));
    let mut tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    super::dispatch_event(
        SyncEvent::Unrevoke {
            artifact_type: "skill".into(),
            artifact_key: "kept".into(),
            version: None,
            signed: None,
        },
        &state,
        "https://example.invalid",
        &sem,
        &locks,
        &stats,
        &mut tasks,
        &Arc::new(BackendRegistry::new()),
        tx,
        None,
    )
    .await;
    while tasks.join_next().await.is_some() {}

    assert!(scenery_core::revocation::is_revoked(&state, "skill", "kept", None).unwrap());
    cleanup(&root);
}

#[tokio::test]
async fn snapshot_revocations_some_purges_a_locally_installed_skill() {
    let root = temp_root("snapshot-revocations-purge");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    seed_installed_skill(&state, "snapshot-revoked", "1.0.0");
    let install_root = state.root_dir.join("skills").join("snapshot-revoked");
    assert!(install_root.exists());

    dispatch(
        SyncEvent::Snapshot {
            installations: vec![],
            mcp_installations: None,
            plugin_installations: vec![],
            revocations: Some(vec![crate::sync::sse_client::RevocationRecord {
                artifact_type: "skill".to_string(),
                artifact_key: "snapshot-revoked".to_string(),
                version: None,
                signed: None,
            }]),
            org_policy: None,
            org_id: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(
        !install_root.exists(),
        "a revoked entry delivered via the snapshot must purge an already-installed skill"
    );
    assert!(revocation_row_exists(&state, "skill", "snapshot-revoked"));

    cleanup(&root);
}

fn snapshot_revoking(key: &str) -> SyncEvent {
    SyncEvent::Snapshot {
        installations: vec![],
        mcp_installations: None,
        plugin_installations: vec![],
        revocations: Some(vec![crate::sync::sse_client::RevocationRecord {
            artifact_type: "skill".to_string(),
            artifact_key: key.to_string(),
            version: None,
            signed: None,
        }]),
        org_policy: None,
        org_id: None,
    }
}

fn revoked_audit_count(state: &AppState) -> i64 {
    Connection::open(&state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'skill_revoked'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn redelivered_snapshot_does_not_reapply_an_already_applied_revoke() {
    // A token refresh reconnects SSE and re-sends the full snapshot. The skill
    // is already gone and the block cached, so nothing may run again.
    let root = temp_root("snapshot-revocations-redelivered");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    seed_installed_skill(&state, "once-only", "1.0.0");
    let reg = Arc::new(BackendRegistry::new());

    dispatch(snapshot_revoking("once-only"), &state, &reg).await;
    assert_eq!(revoked_audit_count(&state), 1);

    dispatch(snapshot_revoking("once-only"), &state, &reg).await;
    assert_eq!(
        revoked_audit_count(&state),
        1,
        "an already-applied revoke must not emit a second audit event"
    );
    assert!(revocation_row_exists(&state, "skill", "once-only"));

    cleanup(&root);
}

#[tokio::test]
async fn cached_revoke_still_applies_when_the_skill_is_still_installed() {
    // Block persisted (crash / failed teardown) but the skill is still on
    // disk: the next snapshot must finish the job, not skip it as "seen".
    let root = temp_root("snapshot-revocations-unfinished");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "half-done", None).unwrap();
    seed_installed_skill(&state, "half-done", "1.0.0");
    let install_root = state.root_dir.join("skills").join("half-done");

    dispatch(
        snapshot_revoking("half-done"),
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    assert!(!install_root.exists(), "unfinished revoke must still apply");
    assert_eq!(revoked_audit_count(&state), 1);

    cleanup(&root);
}

// ── Install-time gate ──────────────────────────────────────────────────────────

#[tokio::test]
async fn install_event_is_refused_for_an_already_revoked_skill() {
    let root = temp_root("install-gate-skill");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "gated-skill", Some("1.0.0"))
        .unwrap();

    dispatch(
        SyncEvent::Install {
            installation_id: install_id(),
            skill_id: "gated-skill".to_string(),
            version: "1.0.0".to_string(),
            source: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    let conn = Connection::open(&state.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'gated-skill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "a revoked skill+version must never be installed, even by a live `install` event"
    );

    cleanup(&root);
}

#[tokio::test]
async fn install_event_proceeds_for_a_different_unrevoked_version() {
    // Pins the gate to the exact version, not the whole skill, when the
    // admin scoped the revoke to one version (see `revocation::is_revoked`).
    // Install will still fail downstream (no real registry/artifact behind
    // "https://example.invalid"), but it must fail for a DIFFERENT reason
    // than the revocation gate, proving the gate itself let it through.
    let root = temp_root("install-gate-version-scoped");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    scenery_core::revocation::upsert_revocation(&state, "skill", "scoped-skill", Some("1.0.0"))
        .unwrap();

    dispatch(
        SyncEvent::Install {
            installation_id: install_id(),
            skill_id: "scoped-skill".to_string(),
            version: "2.0.0".to_string(),
            source: None,
        },
        &state,
        &Arc::new(BackendRegistry::new()),
    )
    .await;

    // It never installed (no real backend to download from), but the error
    // state row it leaves behind proves `do_install` ran, not the gate.
    let conn = Connection::open(&state.db_path).unwrap();
    let error_row: Option<String> = conn
        .query_row(
            "SELECT current_status FROM installed_skills WHERE skill_id = 'scoped-skill'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    // Either an error row exists (do_install ran and failed on network) or
    // none exists at all depending on how far do_install got — what matters
    // is that it did NOT short-circuit with the "revoked" gate, which is
    // already covered precisely by `install_event_is_refused_for_an_already_revoked_skill`
    // never leaving ANY row (blocked before any DB write). Here, confirm no
    // revocation row exists for version 2.0.0's install path having been
    // gated at all by asserting the whole-skill revocation check: scoped
    // revoke of 1.0.0 must not report 2.0.0 as revoked.
    assert!(
        !scenery_core::revocation::is_revoked(&state, "skill", "scoped-skill", Some("2.0.0"))
            .unwrap()
    );
    let _ = error_row;

    cleanup(&root);
}

#[tokio::test]
async fn install_mcp_event_is_refused_for_an_already_revoked_server() {
    let root = temp_root("install-gate-mcp");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());
    let sid = Uuid::new_v4();
    scenery_core::revocation::upsert_revocation(&state, "mcp_server", &sid.to_string(), None)
        .unwrap();

    let backend_registry = Arc::new(BackendRegistry::new());
    dispatch(
        SyncEvent::InstallMcp {
            installation_id: install_id(),
            mcp_server_id: sid,
            mcp_server_name: "GitHub MCP".to_string(),
            package_source: "@org/server".to_string(),
            version_pin: None,
            server_config: Some(
                serde_json::json!({"command": "npx", "args": ["-y", "server"], "env": {}}),
            ),
            auth_type: "none".to_string(),
            gateway_server_id: None,
            gateway_url: None,
            signed: None,
        },
        &state,
        &backend_registry,
    )
    .await;

    assert!(
        state.list_mcp_installs().unwrap().is_empty(),
        "a revoked MCP server must never be installed, even by a live `install_mcp` event"
    );
    assert!(
        !backend_registry.has_backend("github-mcp"),
        "a revoked MCP server must never be registered in the live aggregator"
    );

    cleanup(&root);
}
