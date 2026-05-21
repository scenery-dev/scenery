//! Unit tests for MCP server desired-state reconciler (G3).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use uuid::Uuid;

use vectorhawkd_core::state::AppState;

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
