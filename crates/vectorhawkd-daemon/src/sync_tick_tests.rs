//! Unit tests for the three gaps fixed in the registry sync tick:
//!
//! - GAP-04: UpdateCheckCache is populated by `run_sync_tick`.
//! - GAP-05: Unsynced ratings are flushed to the registry on each tick.
//! - GAP-06: Unmanaged MCP server scan buffers `unmanaged_server_detected` events.

#![allow(clippy::unwrap_used)]

use camino::Utf8PathBuf;
use mockito::Server;
use rusqlite::Connection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use vectorhawkd_core::{
    audit::SqliteAuditBuffer,
    ratings::{get_unsynced_ratings, record_rating},
    registry::RegistryClient,
    state::AppState,
};
use vectorhawkd_mcp::tools::UpdateCheckCache;

use crate::managed_paths::ENV_MUTEX;
use crate::run_sync_tick;

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!("vh-daemon-sync-{label}-{nanos}")))
        .expect("temp path utf-8")
}

fn empty_cache() -> UpdateCheckCache {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── GAP-04: UpdateCheckCache population ───────────────────────────────────────

/// When the registry reports a newer version for an installed skill,
/// `run_sync_tick` should populate the `UpdateCheckCache` with that version.
#[test]
fn gap04_update_cache_populated_when_newer_version_available() {
    let root = temp_root("gap04-newer");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // Insert a skill row at version 1.0.0 so the cache population query finds it.
    {
        let conn = Connection::open(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO installed_skills (skill_id, active_version, install_root, current_status)
             VALUES ('cache-skill', '1.0.0', '/fake/path', 'active')",
            [],
        )
        .unwrap();
    }

    let mut server = Server::new();
    let registry_url = server.url();

    // approved-servers: return empty list
    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    // check_skill_updates calls POST /skills/status then GET /portal/skills/{id}
    let _status_mock = server
        .mock("POST", "/skills/status")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"statuses":{"cache-skill":{"status":"active","latest_version":"2.0.0"}},"unknown":[]}"#)
        .expect_at_least(0)
        .create();

    // fetch_skill_detail for cache population
    let _detail_mock = server
        .mock("GET", "/portal/skills/cache-skill")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"skill_id":"cache-skill","name":"Cache Skill","latest_version":"2.0.0"}"#)
        .expect_at_least(0)
        .create();

    // Ratings + exec stats: return 200 for any POST calls (no data, so may not be called)
    let _ratings_mock = server
        .mock("POST", "/api/runner/skill-ratings")
        .with_status(200)
        .expect_at_least(0)
        .create();
    let _stats_mock = server
        .mock("POST", "/api/runner/execution-stats")
        .with_status(200)
        .expect_at_least(0)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    // Cache should have an entry for cache-skill with latest_version = Some(2.0.0).
    let guard = cache.lock().unwrap();
    assert!(
        guard.contains_key("cache-skill"),
        "update cache should have an entry for cache-skill"
    );
    let entry = &guard["cache-skill"];
    assert!(
        entry.latest_version.is_some(),
        "latest_version should be Some when registry has a newer version"
    );
    assert_eq!(
        entry.latest_version.as_ref().unwrap().to_string(),
        "2.0.0",
        "latest_version should match registry response"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// When there are no installed skills, `run_sync_tick` completes without error
/// and the cache remains empty.
#[test]
fn gap04_update_cache_empty_when_no_installed_skills() {
    let root = temp_root("gap04-empty");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let mut server = Server::new();
    let registry_url = server.url();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    assert!(
        cache.lock().unwrap().is_empty(),
        "cache should be empty when no skills are installed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ── bug-revoke-loop: revocation snapshot reconcile with NO reconciler ────────

/// `vectorhawk mcp sync` (`cmd_mcp_sync`) is a one-shot CLI invocation with
/// no reconciler running — it always calls `run_sync_tick` with
/// `snapshot_tx = None` (there is no `SyncController`/event channel to feed
/// outside the long-running daemon). Before this fix, the entire snapshot
/// fetch — including the durable `revocations` list — was skipped whenever
/// `snapshot_tx` was `None`, so this command could never lift a stale local
/// revocation gate from an admin "unblock"; only a daemon restart (which
/// re-fetches a snapshot on SSE connect) or waiting out the next 5-minute
/// in-daemon tick would. This proves the revocation half of the snapshot is
/// now applied unconditionally, independent of whether a reconciler exists
/// to hand the rest of the snapshot to.
#[test]
fn revocation_snapshot_applied_even_with_no_reconciler_present() {
    let root = temp_root("revloop-mcp-sync-unrevoke");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // A device_id must be on disk for `run_sync_tick` to even attempt the
    // snapshot fetch (mirrors a real paired device; `vectorhawk mcp sync`
    // on an unpaired box has nothing to reconcile).
    state
        .set_sync_state("device_id", "mcp-sync-device")
        .unwrap();

    let mut server = Server::new();
    let registry_url = server.url();

    // `fetch_sync_snapshot` requires an auth token to be set on the
    // RegistryClient — step 0 of `run_sync_tick` does that FROM the stored
    // tokens row, so one must exist for this path to even reach the fetch.
    vectorhawkd_core::auth::save_tokens(
        &state,
        &registry_url,
        "fake-access-tok",
        "fake-refresh-tok",
    )
    .unwrap();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    // The backend's current, authoritative state: nothing is revoked right
    // now (the admin already unrevoked it — this is exactly what
    // `GET /api/sync/snapshot` returns per the bug report: `revocations: []`).
    let _snapshot_mock = server
        .mock("GET", "/api/sync/snapshot")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"installations":[],"mcp_installations":[],"plugin_installations":[],"revocations":[]}"#,
        )
        .create();

    // Simulate the STALE local state: this device revoked "ghost-skill"
    // earlier (e.g. via a live `revoke` SSE event while the daemon was
    // running) and never got the news that it was lifted.
    vectorhawkd_core::revocation::upsert_revocation(&state, "skill", "ghost-skill", None).unwrap();
    assert!(
        vectorhawkd_core::revocation::is_revoked(&state, "skill", "ghost-skill", Some("1.0.0"))
            .unwrap(),
        "precondition: locally revoked before the tick"
    );

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    // The exact call `cmd_mcp_sync` makes: no reconciler, so `snapshot_tx`
    // is `None`.
    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    assert!(
        !vectorhawkd_core::revocation::is_revoked(&state, "skill", "ghost-skill", Some("1.0.0"))
            .unwrap(),
        "a `vectorhawk mcp sync` tick with no reconciler must still apply \
         the durable revocation list and lift the stale local gate"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ── GAP-05: Ratings flush ─────────────────────────────────────────────────────

/// An unsynced rating stored in SQLite is uploaded on `run_sync_tick` and the
/// row is marked synced (not returned by `get_unsynced_ratings` afterward).
#[test]
fn gap05_unsynced_rating_flushed_on_tick() {
    let root = temp_root("gap05-flush");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // Record an unsynced rating.
    {
        let conn = Connection::open(&state.db_path).unwrap();
        record_rating(&conn, "rated-skill", "1.0.0", "up").unwrap();
    }

    let mut server = Server::new();
    let registry_url = server.url();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    // Registry must accept the ratings upload.
    let ratings_mock = server
        .mock("POST", "/api/runner/skill-ratings")
        .with_status(200)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    ratings_mock.assert();

    // The rating should now be marked synced — get_unsynced_ratings returns empty.
    let conn = Connection::open(&state.db_path).unwrap();
    let remaining = get_unsynced_ratings(&conn).unwrap();
    assert!(
        remaining.is_empty(),
        "rating should be marked synced after tick"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// When there are no unsynced ratings, the ratings endpoint is NOT called.
#[test]
fn gap05_no_upload_when_no_ratings() {
    let root = temp_root("gap05-empty");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let mut server = Server::new();
    let registry_url = server.url();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    // Ratings endpoint should NOT be called.
    let ratings_mock = server
        .mock("POST", "/api/runner/skill-ratings")
        .with_status(200)
        .expect(0)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    ratings_mock.assert();

    let _ = std::fs::remove_dir_all(&root);
}

// ── GAP-06: Unmanaged server scan ─────────────────────────────────────────────

/// When a synthetic AI client config contains a non-vectorhawk MCP server,
/// `run_sync_tick` should buffer an `unmanaged_server_detected` audit event.
///
/// We inject a config file by writing a fake `~/.claude.json`-shaped file to a
/// temp path and temporarily redirecting `HOME` so `dirs::home_dir()` returns
/// that path.
#[test]
fn gap06_unmanaged_server_buffered_as_audit_event() {
    let root = temp_root("gap06-unmanaged");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // Build a fake home directory with a .claude.json that has an unmanaged entry.
    let fake_home = root.join("fakehome");
    std::fs::create_dir_all(&fake_home).unwrap();
    let claude_json_path = fake_home.join(".claude.json");
    let config = serde_json::json!({
        "mcpServers": {
            "vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]},
            "shadow-tool": {"command": "npx", "args": ["-y", "shadow-mcp-server"]}
        }
    });
    std::fs::write(
        &claude_json_path,
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    // Redirect HOME so detect_ai_clients() finds the fake config.
    // Hold the shared ENV_MUTEX for the entire test body to prevent races with
    // other HOME-mutating tests across the crate when cargo runs tests in parallel.
    let _home_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let original_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", fake_home.as_str());

    let mut server = Server::new();
    let registry_url = server.url();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    // Restore HOME before any assertions that might panic.
    if let Some(h) = original_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }

    // At least one `unmanaged_server_detected` audit event should be in the DB.
    let conn = Connection::open(&state.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'unmanaged_server_detected'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(
        count >= 1,
        "expected at least one unmanaged_server_detected event, got {count}"
    );

    // Verify the payload contains the expected server name.
    let payload_json: String = conn
        .query_row(
            "SELECT payload FROM audit_events WHERE event_type = 'unmanaged_server_detected' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(
        payload["server_name"].as_str().unwrap(),
        "shadow-tool",
        "audit event payload should name the unmanaged server"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// When all MCP servers in an AI client config are managed (i.e. only
/// `vectorhawk`), no `unmanaged_server_detected` events are buffered.
#[test]
fn gap06_no_events_when_all_servers_managed() {
    let root = temp_root("gap06-managed");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let fake_home = root.join("fakehome");
    std::fs::create_dir_all(&fake_home).unwrap();
    let claude_json_path = fake_home.join(".claude.json");
    let config = serde_json::json!({
        "mcpServers": {
            "vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]}
        }
    });
    std::fs::write(
        &claude_json_path,
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let _home_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let original_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", fake_home.as_str());

    let mut server = Server::new();
    let registry_url = server.url();

    let _approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    let registry = RegistryClient::new(&registry_url);
    let audit_buf = SqliteAuditBuffer::new(Arc::new(RegistryClient::new(&registry_url)), &state);
    let cache = empty_cache();

    run_sync_tick(
        &registry,
        &audit_buf,
        &state.db_path,
        &state.root_dir,
        &cache,
        None,
    )
    .unwrap();

    if let Some(h) = original_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    drop(_home_guard);

    let conn = Connection::open(&state.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'unmanaged_server_detected'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        count, 0,
        "no unmanaged_server_detected events when only vectorhawk is configured"
    );

    let _ = std::fs::remove_dir_all(&root);
}
