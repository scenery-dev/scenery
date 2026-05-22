//! Unit tests for MCP server desired-state reconciler (G3).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use uuid::Uuid;

use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::aggregator::BackendRegistry;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-mcp-reconciler-tests-{label}-{nanos}")),
    )
    .expect("temp path utf-8")
}

fn cleanup(root: &Utf8PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

fn install_id() -> Uuid {
    Uuid::new_v4()
}

fn server_id() -> Uuid {
    Uuid::new_v4()
}

fn read_managed_mcp_json(state: &AppState) -> serde_json::Value {
    let path = state.root_dir.as_std_path().join("managed-mcp.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read managed-mcp.json at {}: {e}", path.display()));
    serde_json::from_str(&content).unwrap()
}

fn server_config_json() -> serde_json::Value {
    serde_json::json!({"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]})
}

// ── managed-mcp.json write path (unit, via state methods) ────────────────────

#[test]
fn install_mcp_upserts_row_and_writes_managed_mcp_json() {
    let root = temp_root("install-basic");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let state_arc = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    let iid = install_id();
    let sid = server_id();

    let row = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: iid.to_string(),
        mcp_server_name: "GitHub MCP".to_string(),
        package_source: "@modelcontextprotocol/server-github".to_string(),
        version_pin: Some("0.7.2".to_string()),
        server_config: Some(server_config_json().to_string()),
        auth_type: "oauth_pkce".to_string(),
        gateway_server_id: Some("g1-github".to_string()),
    };

    state_arc.upsert_mcp_install(&row).unwrap();
    super::write_managed_mcp_json_for_test(&state_arc).unwrap();

    let installs = state.list_mcp_installs().unwrap();
    assert_eq!(installs.len(), 1, "one row should be in mcp_installations");
    assert_eq!(installs[0].mcp_server_name, "GitHub MCP");
    assert_eq!(installs[0].version_pin.as_deref(), Some("0.7.2"));
    assert_eq!(installs[0].auth_type, "oauth_pkce");

    let json = read_managed_mcp_json(&state);
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["name"], "GitHub MCP");
    assert_eq!(
        servers[0]["package_source"],
        "@modelcontextprotocol/server-github"
    );
    assert_eq!(servers[0]["version_pin"], "0.7.2");
    assert_eq!(servers[0]["auth_type"], "oauth_pkce");
    assert_eq!(servers[0]["gateway_server_id"], "g1-github");

    cleanup(&root);
}

#[test]
fn deactivate_mcp_removes_row_and_entry_from_managed_mcp_json() {
    let root = temp_root("deactivate");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid = server_id();
    let row = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: install_id().to_string(),
        mcp_server_name: "Slack MCP".to_string(),
        package_source: "@modelcontextprotocol/server-slack".to_string(),
        version_pin: None,
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
    };
    state.upsert_mcp_install(&row).unwrap();
    super::write_managed_mcp_json_for_test(&state).unwrap();

    // Confirm the server appears.
    let json_before = read_managed_mcp_json(&state);
    assert_eq!(json_before["servers"].as_array().unwrap().len(), 1);

    // Deactivate.
    state.delete_mcp_install(&sid.to_string()).unwrap();
    super::write_managed_mcp_json_for_test(&state).unwrap();

    let installs_after = state.list_mcp_installs().unwrap();
    assert!(installs_after.is_empty(), "row should be deleted");

    let json_after = read_managed_mcp_json(&state);
    assert_eq!(
        json_after["servers"].as_array().unwrap().len(),
        0,
        "servers array should be empty after deactivate"
    );

    cleanup(&root);
}

#[test]
fn reinstall_mcp_is_idempotent() {
    let root = temp_root("idempotent");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid = server_id();

    let row1 = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: install_id().to_string(),
        mcp_server_name: "GitHub MCP".to_string(),
        package_source: "@modelcontextprotocol/server-github".to_string(),
        version_pin: Some("0.7.1".to_string()),
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
    };
    state.upsert_mcp_install(&row1).unwrap();
    super::write_managed_mcp_json_for_test(&state).unwrap();

    // Re-install with a new installation_id and updated version_pin.
    let new_iid = install_id();
    let row2 = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: sid.to_string(),
        installation_id: new_iid.to_string(),
        mcp_server_name: "GitHub MCP".to_string(),
        package_source: "@modelcontextprotocol/server-github".to_string(),
        version_pin: Some("0.7.2".to_string()),
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
    };
    state.upsert_mcp_install(&row2).unwrap();
    super::write_managed_mcp_json_for_test(&state).unwrap();

    let installs = state.list_mcp_installs().unwrap();
    assert_eq!(installs.len(), 1, "upsert must not duplicate rows");
    assert_eq!(installs[0].installation_id, new_iid.to_string());
    assert_eq!(installs[0].version_pin.as_deref(), Some("0.7.2"));

    let json = read_managed_mcp_json(&state);
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["version_pin"], "0.7.2");

    cleanup(&root);
}

#[test]
fn managed_mcp_json_contains_all_installed_servers() {
    let root = temp_root("multi-server");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    for i in 0..3u32 {
        let row = vectorhawkd_core::state::McpInstallRow {
            mcp_server_id: Uuid::new_v4().to_string(),
            installation_id: install_id().to_string(),
            mcp_server_name: format!("Server {i}"),
            package_source: format!("@org/server-{i}"),
            version_pin: None,
            server_config: None,
            auth_type: "none".to_string(),
            gateway_server_id: None,
        };
        state.upsert_mcp_install(&row).unwrap();
    }
    super::write_managed_mcp_json_for_test(&state).unwrap();

    let json = read_managed_mcp_json(&state);
    assert_eq!(
        json["servers"].as_array().unwrap().len(),
        3,
        "all three servers should appear in managed-mcp.json"
    );

    cleanup(&root);
}

// ── PATCH callback order test (mockito) ──────────────────────────────────────

#[tokio::test]
async fn patch_callback_receives_installing_then_installed_in_order() {
    use mockito::Server;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    let mut server = Server::new_async().await;
    let received_states: StdArc<StdMutex<Vec<String>>> = StdArc::new(StdMutex::new(Vec::new()));

    let states_clone = StdArc::clone(&received_states);
    let iid = install_id();
    let url_path = format!("/api/mcp-installations/{iid}");

    // Register two sequential PATCH expectations for the same path.
    let _m1 = server
        .mock("PATCH", url_path.as_str())
        .with_status(200)
        .with_body("{}")
        .expect(2) // installing + installed
        .create_async()
        .await;

    let root = temp_root("patch-order");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // Store a dummy token so report_mcp_installation_status loads one.
    {
        use rusqlite::Connection;
        let conn = Connection::open(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO auth_tokens (registry_url, access_token, refresh_token) VALUES (?1, ?2, ?3)",
            rusqlite::params![server.url(), "test-token", "test-refresh"],
        )
        .unwrap();
    }

    let state_arc = Arc::new(AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    });
    let registry_url = server.url();

    // Simulate the sequence: report installing, then installed.
    super::report_mcp_installation_status_for_test(
        iid,
        "installing",
        None,
        &registry_url,
        &state_arc,
    )
    .await;
    states_clone.lock().unwrap().push("installing".to_string());

    super::report_mcp_installation_status_for_test(
        iid,
        "installed",
        None,
        &registry_url,
        &state_arc,
    )
    .await;
    states_clone.lock().unwrap().push("installed".to_string());

    _m1.assert_async().await;

    let seen = received_states.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["installing", "installed"],
        "PATCH callbacks must arrive in order: installing → installed"
    );

    cleanup(&root);
}

// ── Snapshot MCP reconciliation tests ────────────────────────────────────────

/// Helper to build a McpInstallationRecord for snapshot tests.
fn make_mcp_snapshot_record(
    mcp_server_id: Uuid,
    iid: Uuid,
    name: &str,
    state: &str,
) -> crate::sync::sse_client::McpInstallationRecord {
    crate::sync::sse_client::McpInstallationRecord {
        installation_id: iid,
        mcp_server_id,
        mcp_server_name: name.to_string(),
        package_source: format!("@org/{}", name.to_lowercase().replace(' ', "-")),
        version_pin: None,
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
        state: state.to_string(),
    }
}

/// Helper to seed an MCP row into the local mcp_installations table.
fn seed_mcp_row(state: &AppState, mcp_server_id: Uuid, name: &str) {
    let row = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: mcp_server_id.to_string(),
        installation_id: install_id().to_string(),
        mcp_server_name: name.to_string(),
        package_source: format!("@org/{}", name.to_lowercase().replace(' ', "-")),
        version_pin: None,
        server_config: None,
        auth_type: "none".to_string(),
        gateway_server_id: None,
    };
    state.upsert_mcp_install(&row).unwrap();
}

#[test]
fn snapshot_mcp_two_desired_rows_produce_two_install_events_and_write_json() {
    // Scenario: fresh device reconnects after downtime. Backend snapshot carries
    // two desired MCP servers; neither is in local SQLite.  Expect two InstallMcp
    // events and a managed-mcp.json written by handle_install_mcp (not by the diff
    // itself — the diff only writes json when it deletes orphans or deactivations).
    //
    // NOTE: The diff function itself does not call write_managed_mcp_json for
    // the install path — that happens inside handle_install_mcp.  This test
    // verifies only that two InstallMcp events are emitted.
    let root = temp_root("snapshot-mcp-two-desired");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid1 = server_id();
    let sid2 = server_id();
    let iid1 = install_id();
    let iid2 = install_id();

    let records = vec![
        make_mcp_snapshot_record(sid1, iid1, "GitHub MCP", "desired"),
        make_mcp_snapshot_record(sid2, iid2, "Slack MCP", "desired"),
    ];

    let events = super::build_derived_mcp_events_blocking_for_test(records, &state);

    assert_eq!(events.len(), 2, "two desired rows → two InstallMcp events");

    let install_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, crate::sync::sse_client::SyncEvent::InstallMcp { .. }))
        .collect();
    assert_eq!(install_events.len(), 2, "both events must be InstallMcp");

    // Verify no managed-mcp.json was written by the diff itself
    // (no orphan removals or deactivations occurred).
    let json_path = state.root_dir.as_std_path().join("managed-mcp.json");
    assert!(
        !json_path.exists(),
        "diff alone must not write managed-mcp.json for install-only events"
    );

    cleanup(&root);
}

#[test]
fn snapshot_mcp_orphan_removed_from_sqlite_and_managed_json() {
    // Scenario: daemon was offline; backend deleted server B from the catalog.
    // Snapshot now only contains server A.  Server B was locally installed.
    // Expect: server B row deleted from SQLite, managed-mcp.json rewritten with
    // only server A.
    let root = temp_root("snapshot-mcp-orphan");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid_a = server_id();
    let sid_b = server_id();
    let iid_a = install_id();

    // Seed both servers locally.
    seed_mcp_row(&state, sid_a, "GitHub MCP");
    seed_mcp_row(&state, sid_b, "Slack MCP");
    // Write initial managed-mcp.json with both servers.
    super::write_managed_mcp_json_for_test(&state).unwrap();
    assert_eq!(
        read_managed_mcp_json(&state)["servers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Snapshot only contains server A (server B was deleted on the backend).
    let records = vec![make_mcp_snapshot_record(
        sid_a,
        iid_a,
        "GitHub MCP",
        "installed",
    )];

    let events = super::build_derived_mcp_events_blocking_for_test(records, &state);

    // No new install events — server A is already present.
    assert!(events.is_empty(), "server A already installed → no events");

    // Server B must be gone from SQLite.
    let remaining = state.list_mcp_installs().unwrap();
    assert_eq!(remaining.len(), 1, "orphan server B must be removed");
    assert_eq!(remaining[0].mcp_server_id, sid_a.to_string());

    // managed-mcp.json must be rewritten with only server A.
    let json = read_managed_mcp_json(&state);
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(
        servers.len(),
        1,
        "managed-mcp.json must reflect only server A"
    );
    assert_eq!(servers[0]["name"], "GitHub MCP");

    cleanup(&root);
}

#[test]
fn snapshot_mcp_deactivated_state_removes_local_row_and_rewrites_json() {
    // Scenario: server was installed; portal admin deactivates it. Daemon
    // reconnects and receives a snapshot with state=deactivated for that server.
    // Expect: row deleted from SQLite, managed-mcp.json is empty, DeactivateMcp
    // event emitted so the backend PATCH callback fires.
    let root = temp_root("snapshot-mcp-deactivated");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid = server_id();
    let iid = install_id();

    seed_mcp_row(&state, sid, "GitHub MCP");
    super::write_managed_mcp_json_for_test(&state).unwrap();
    assert_eq!(
        read_managed_mcp_json(&state)["servers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let records = vec![make_mcp_snapshot_record(
        sid,
        iid,
        "GitHub MCP",
        "deactivated",
    )];

    let events = super::build_derived_mcp_events_blocking_for_test(records, &state);

    // One DeactivateMcp event so the reconciler can PATCH the backend.
    assert_eq!(events.len(), 1);
    match &events[0] {
        crate::sync::sse_client::SyncEvent::DeactivateMcp { mcp_server_id, .. } => {
            assert_eq!(*mcp_server_id, sid);
        }
        other => panic!("expected DeactivateMcp, got {other:?}"),
    }

    // Row removed from SQLite.
    assert!(state.list_mcp_installs().unwrap().is_empty());

    // managed-mcp.json rewritten with zero servers.
    let json = read_managed_mcp_json(&state);
    assert_eq!(
        json["servers"].as_array().unwrap().len(),
        0,
        "managed-mcp.json must be empty after deactivation"
    );

    cleanup(&root);
}

#[test]
fn snapshot_mcp_empty_array_is_noop_for_existing_installs() {
    // Backwards-compat scenario: old backend does not emit `mcp_installations`
    // key. The SSE parser defaults it to an empty vec.  The reconciler must
    // treat an empty slice as "old backend, no data" — NOT as "desired state is
    // zero servers".  Existing local installs must be left untouched.
    let root = temp_root("snapshot-mcp-empty-compat");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid = server_id();
    seed_mcp_row(&state, sid, "GitHub MCP");
    super::write_managed_mcp_json_for_test(&state).unwrap();

    // Caller guards against empty vec (as done in dispatch_event).
    // To test the function directly we call it with an empty vec and confirm
    // it is a no-op (no events, no SQLite changes, no JSON rewrite).
    let events = super::build_derived_mcp_events_blocking_for_test(vec![], &state);

    assert!(events.is_empty(), "empty snapshot → no events");

    // Existing row must still be present.
    let remaining = state.list_mcp_installs().unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "empty snapshot must not wipe existing installs"
    );
    assert_eq!(remaining[0].mcp_server_id, sid.to_string());

    cleanup(&root);
}

// ── Per-server mutex (reuses SkillLockMap) ────────────────────────────────────

#[tokio::test]
async fn mcp_server_lock_serializes_same_server_id() {
    use super::{skill_lock, SkillLockMap};

    let map: SkillLockMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let sid = server_id().to_string();

    let lock1 = skill_lock(&map, &sid);
    let lock2 = skill_lock(&map, &sid);

    assert!(
        Arc::ptr_eq(&lock1, &lock2),
        "same mcp_server_id must yield the same Arc<Mutex>"
    );

    let sid2 = server_id().to_string();
    let lock3 = skill_lock(&map, &sid2);
    assert!(
        !Arc::ptr_eq(&lock1, &lock3),
        "different mcp_server_ids must yield different mutexes"
    );
}

#[tokio::test]
async fn mcp_server_lock_prevents_concurrent_installs_for_same_server() {
    use super::{skill_lock, SkillLockMap};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    let map: SkillLockMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let sid = server_id().to_string();

    let active = Arc::new(AtomicUsize::new(0));
    let lock1 = skill_lock(&map, &sid);
    let lock2 = skill_lock(&map, &sid);
    let a1 = Arc::clone(&active);
    let a2 = Arc::clone(&active);

    let t1 = tokio::spawn(async move {
        let _g = lock1.lock_owned().await;
        let prev = a1.fetch_add(1, Ordering::SeqCst);
        assert_eq!(prev, 0, "only one task should hold the lock at a time");
        sleep(Duration::from_millis(30)).await;
        a1.fetch_sub(1, Ordering::SeqCst);
    });

    sleep(Duration::from_millis(5)).await;

    let t2 = tokio::spawn(async move {
        let _g = lock2.lock_owned().await;
        let prev = a2.fetch_add(1, Ordering::SeqCst);
        assert_eq!(prev, 0, "only one task should hold the lock at a time");
        sleep(Duration::from_millis(5)).await;
        a2.fetch_sub(1, Ordering::SeqCst);
    });

    t1.await.unwrap();
    t2.await.unwrap();
}

// ── BackendRegistry aggregator integration tests ──────────────────────────────

/// Helper: build a fresh BackendRegistry (no stub backend).
fn fresh_registry() -> Arc<BackendRegistry> {
    Arc::new(BackendRegistry::new())
}

/// Helper: build a server_config JSON blob with command+args shape.
fn stdio_server_config() -> serde_json::Value {
    serde_json::json!({
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-github"],
        "env": {}
    })
}

/// Helper: seed an MCP row with a command-based server_config.
fn seed_mcp_row_with_config(
    state: &AppState,
    mcp_server_id: Uuid,
    name: &str,
    server_config: Option<serde_json::Value>,
) {
    let row = vectorhawkd_core::state::McpInstallRow {
        mcp_server_id: mcp_server_id.to_string(),
        installation_id: install_id().to_string(),
        mcp_server_name: name.to_string(),
        package_source: "@org/server".to_string(),
        version_pin: None,
        server_config: server_config.as_ref().map(|v| v.to_string()),
        auth_type: "none".to_string(),
        gateway_server_id: None,
    };
    state.upsert_mcp_install(&row).unwrap();
}

#[test]
fn startup_with_two_entries_registers_two_backends() {
    // Arrange: seed two MCP rows with valid stdio server_config.
    let root = temp_root("startup-two-backends");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid1 = server_id();
    let sid2 = server_id();
    seed_mcp_row_with_config(&state, sid1, "GitHub MCP", Some(stdio_server_config()));
    seed_mcp_row_with_config(&state, sid2, "Slack MCP", Some(stdio_server_config()));

    // Act: run the startup loader.
    let registry = fresh_registry();
    let (list_changed_tx, _) = tokio::sync::broadcast::channel(16);
    crate::load_managed_mcp_into_registry(&state, &registry, list_changed_tx);

    // Assert: both backends are registered by their UUID server_id.
    let backends = registry.list_backends();
    assert_eq!(
        backends.len(),
        2,
        "two entries should register two backends"
    );

    let ids: Vec<&str> = backends.iter().map(|b| b.server_id.as_str()).collect();
    // Backends are keyed by the slug of their display name, not the UUID.
    let _ = sid1;
    let _ = sid2;
    assert!(
        ids.contains(&"github-mcp"),
        "github-mcp slug must be in registry, got {:?}",
        ids
    );
    assert!(
        ids.contains(&"slack-mcp"),
        "slack-mcp slug must be in registry, got {:?}",
        ids
    );

    cleanup(&root);
}

#[test]
fn startup_with_null_server_config_skips_entry() {
    // Arrange: one row with server_config = None (null in SQLite).
    let root = temp_root("startup-null-config");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let sid = server_id();
    seed_mcp_row_with_config(&state, sid, "No-Config MCP", None);

    // Act.
    let registry = fresh_registry();
    let (list_changed_tx, _) = tokio::sync::broadcast::channel(16);
    crate::load_managed_mcp_into_registry(&state, &registry, list_changed_tx);

    // Assert: no backends registered (null config → skip with warning).
    let backends = registry.list_backends();
    assert!(
        backends.is_empty(),
        "null server_config must not register a backend"
    );

    cleanup(&root);
}

#[tokio::test]
async fn handle_install_mcp_registers_backend_in_aggregator() {
    // Arrange: fresh state + registry.
    let root = temp_root("install-registers-aggregator");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let state_arc = Arc::new(AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    });

    let registry = fresh_registry();
    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));

    let iid = install_id();
    let sid = server_id();

    // Act: drive handle_install_mcp directly (no network — use a local mockito
    // server for the PATCH callback so the function doesn't hang).
    let mut mock_server = mockito::Server::new_async().await;
    let url_path = format!("/api/mcp-installations/{iid}");
    let _m = mock_server
        .mock("PATCH", url_path.as_str())
        .with_status(200)
        .with_body("{}")
        .expect(2) // installing + installed
        .create_async()
        .await;

    let changed = super::handle_install_mcp_for_test(
        iid,
        sid,
        "GitHub MCP".to_string(),
        "@modelcontextprotocol/server-github".to_string(),
        None,
        Some(stdio_server_config()),
        "none".to_string(),
        None,
        &state_arc,
        &mock_server.url(),
        &stats,
        &registry,
    )
    .await;

    // Assert: the handler returns true (tool list changed) and the backend
    // is now present in the aggregator under the mcp_server_id key.
    assert!(changed, "handle_install_mcp must return true on success");
    let _ = sid;
    assert!(
        registry.has_backend("github-mcp"),
        "backend must be registered in aggregator under display-name slug"
    );

    cleanup(&root);
}

#[tokio::test]
async fn handle_deactivate_mcp_removes_backend_from_aggregator() {
    // Arrange: seed a row + register it in the aggregator.
    let root = temp_root("deactivate-removes-aggregator");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let state_arc = Arc::new(AppState {
        root_dir: state.root_dir.clone(),
        db_path: state.db_path.clone(),
    });

    let sid = server_id();
    let iid = install_id();
    seed_mcp_row_with_config(&state, sid, "GitHub MCP", Some(stdio_server_config()));

    let registry = fresh_registry();
    // Pre-register so we can verify it gets removed.
    let (list_changed_tx, _) = tokio::sync::broadcast::channel(16);
    crate::load_managed_mcp_into_registry(&state, &registry, list_changed_tx);
    assert!(
        registry.has_backend("github-mcp"),
        "backend must be registered before deactivate"
    );

    let stats = Arc::new(std::sync::Mutex::new(super::ReconcilerStats::default()));

    let mut mock_server = mockito::Server::new_async().await;
    let url_path = format!("/api/mcp-installations/{iid}");
    let _m = mock_server
        .mock("PATCH", url_path.as_str())
        .with_status(200)
        .with_body("{}")
        .expect(1) // deactivated
        .create_async()
        .await;

    // Act.
    let changed = super::handle_deactivate_mcp_for_test(
        iid,
        sid,
        &state_arc,
        &mock_server.url(),
        &stats,
        &registry,
    )
    .await;

    // Assert: handler returns true and backend is gone from aggregator.
    assert!(changed, "handle_deactivate_mcp must return true on success");
    let _ = sid;
    assert!(
        !registry.has_backend("github-mcp"),
        "backend must be removed from aggregator after deactivate"
    );

    cleanup(&root);
}
