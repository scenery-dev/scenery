//! Integration tests for `vectorhawk mcp sync` and `vectorhawk mcp backends`.
//!
//! `mcp sync` runs an in-process registry sync tick; `mcp backends` lists
//! the stub backend registry.  Both must exit 0 and produce useful output.
//!
//! These tests call the command handler functions directly (not as a subprocess)
//! so they run in `cargo test` without a pre-built binary.

#![allow(clippy::unwrap_used)]

// ── mcp backends ─────────────────────────────────────────────────────────────

#[test]
fn build_stub_registry_returns_empty_registry() {
    use vectorhawkd_daemon::build_stub_registry;
    use vectorhawkd_mcp::aggregator::BackendRegistry;

    // The M0 stub backend (echo + ping) was removed once real backend
    // registration shipped via managed-mcp.json. build_stub_registry now
    // just constructs an empty registry that gets populated by
    // load_managed_mcp_into_registry at startup.
    let registry: BackendRegistry = build_stub_registry();
    assert!(
        registry.list_backends().is_empty(),
        "build_stub_registry must produce an empty registry now"
    );
}

// ── mcp sync ─────────────────────────────────────────────────────────────────

/// Verify that `run_sync_tick` completes without panicking when the registry
/// is unreachable.  The sync loop is designed to log-and-continue on HTTP
/// errors, so the function must return Ok(()) even if all HTTP calls fail.
#[test]
fn mcp_sync_succeeds_when_registry_unreachable() {
    use std::sync::Arc;
    use vectorhawkd_core::{audit::SqliteAuditBuffer, registry::RegistryClient, state::AppState};
    use vectorhawkd_daemon::run_sync_tick;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = camino::Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-sync-test-{nanos}")),
    )
    .unwrap();
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    // Point at an unreachable port — all HTTP calls must fail gracefully.
    let registry = Arc::new(RegistryClient::new("http://127.0.0.1:1"));
    let audit = Arc::new(SqliteAuditBuffer::new(Arc::clone(&registry), &state));

    let update_cache: vectorhawkd_mcp::tools::UpdateCheckCache =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let result = run_sync_tick(
        &registry,
        &audit,
        &state.db_path,
        &state.root_dir,
        &update_cache,
    );
    assert!(
        result.is_ok(),
        "run_sync_tick must return Ok even when the registry is unreachable: {result:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Verify that `run_sync_tick` calls the registry audit flush and approved-
/// server endpoints when the registry is reachable.
#[test]
fn mcp_sync_calls_registry_endpoints_when_reachable() {
    use std::sync::Arc;
    use vectorhawkd_core::{audit::SqliteAuditBuffer, registry::RegistryClient, state::AppState};
    use vectorhawkd_daemon::run_sync_tick;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = camino::Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-sync-reachable-{nanos}")),
    )
    .unwrap();
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let mut server = mockito::Server::new();

    // The sync tick calls POST /audit/events (audit flush) and
    // GET /mcp/approved-servers (approved server list).
    // Both return minimal valid responses.
    let _audit_mock = server
        .mock("POST", "/audit/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"flushed":0}"#)
        .create();

    let approved_mock = server
        .mock("GET", "/api/runner/approved-servers")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"servers":[]}"#)
        .create();

    let registry = Arc::new(RegistryClient::new(server.url()));
    let audit = Arc::new(SqliteAuditBuffer::new(Arc::clone(&registry), &state));

    let update_cache: vectorhawkd_mcp::tools::UpdateCheckCache =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let result = run_sync_tick(
        &registry,
        &audit,
        &state.db_path,
        &state.root_dir,
        &update_cache,
    );
    assert!(result.is_ok(), "run_sync_tick must succeed: {result:?}");

    // The approved-servers endpoint must have been called.
    approved_mock.assert();

    let _ = std::fs::remove_dir_all(&root);
}
