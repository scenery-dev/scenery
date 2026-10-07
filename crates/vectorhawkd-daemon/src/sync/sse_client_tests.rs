//! Unit tests for the SSE event parser.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use super::{dispatch_event, is_device_revoked_body, parse_sync_event};

#[test]
fn parses_snapshot_event() {
    let data = r#"{"installations":[{"installation_id":"550e8400-e29b-41d4-a716-446655440000","skill_id":"my-skill","version":"1.0.0","state":"desired"}]}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            installations,
            mcp_installations,
            plugin_installations,
        } => {
            assert_eq!(installations.len(), 1);
            assert_eq!(installations[0].skill_id, "my-skill");
            assert_eq!(installations[0].version, "1.0.0");
            assert_eq!(installations[0].state, "desired");
            assert!(
                mcp_installations.is_none(),
                "old-format snapshot has no mcp_installations key → None, not Some(empty)"
            );
            assert!(
                plugin_installations.is_empty(),
                "old-format snapshot has no plugin_installations key → default empty vec"
            );
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn parses_snapshot_event_with_empty_mcp_installations_as_some_empty() {
    // A present-but-empty "mcp_installations": [] (what the current backend
    // sends for a device with zero MCP installs — see
    // vectorhawk-backend/backend/app/routers/portal_sync.py _get_mcp_snapshot,
    // which always includes the key) must parse to `Some(vec![])`, not
    // `None`. The two have different meanings to the reconciler: `None`
    // means "old backend, no data, leave existing installs alone"; `Some(
    // vec![])` means "this device desires zero MCP servers, reconcile
    // everything away."
    let data = r#"{"installations":[],"mcp_installations":[]}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            mcp_installations, ..
        } => {
            let records = mcp_installations
                .expect("a present empty mcp_installations array must parse to Some, not None");
            assert!(
                records.is_empty(),
                "the inner vec must be empty — Some(vec![]) means zero desired MCP servers"
            );
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn parses_snapshot_event_with_deleted_server_tombstone() {
    // Wire shape the backend sends for an MCP server whose catalog row was
    // hard-deleted (admin delete): mcp_server_name/package_source are
    // empty strings, auth_type is "none", gateway fields are null, and
    // state is "deactivated" — see _get_mcp_snapshot's tombstone branch in
    // vectorhawk-backend/backend/app/routers/portal_sync.py. The runner must
    // parse this without erroring; the reconciler resolves the real
    // aggregator key from its OWN local row (not from this tombstone's
    // empty name) before tearing the server down.
    let data = r#"{
        "installations": [],
        "mcp_installations": [
            {
                "installation_id": "550e8400-e29b-41d4-a716-446655440030",
                "mcp_server_id": "550e8400-e29b-41d4-a716-446655440031",
                "mcp_server_name": "",
                "package_source": "",
                "version_pin": null,
                "server_config": null,
                "auth_type": "none",
                "gateway_server_id": null,
                "gateway_url": null,
                "state": "deactivated"
            }
        ]
    }"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            mcp_installations, ..
        } => {
            let records = mcp_installations.expect("tombstone snapshot must parse to Some");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].mcp_server_name, "");
            assert_eq!(records[0].package_source, "");
            assert_eq!(records[0].auth_type, "none");
            assert_eq!(records[0].state, "deactivated");
            assert!(records[0].gateway_url.is_none());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn parses_snapshot_event_with_plugin_installations() {
    // Wire shape matches `build_install_plugin_payload` in the backend
    // (backend/app/services/plugin_install_payload.py) plus the extra
    // "state" field `_get_plugin_snapshot` adds — see RB1 brief.
    let data = r#"{
        "installations": [],
        "plugin_installations": [
            {
                "installation_id": "550e8400-e29b-41d4-a716-446655440020",
                "plugin_slug": "superpowers",
                "plugin_name": "Superpowers",
                "description": "Core skills library",
                "version": "5.0.7",
                "author": "VectorHawk",
                "skills": [{"skill_id": "tdd", "version": "1.0.0"}],
                "state": "desired"
            }
        ]
    }"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            plugin_installations,
            ..
        } => {
            assert_eq!(plugin_installations.len(), 1);
            let p = &plugin_installations[0];
            assert_eq!(p.plugin_slug, "superpowers");
            assert_eq!(p.plugin_name, "Superpowers");
            assert_eq!(p.version, "5.0.7");
            assert_eq!(p.author, "VectorHawk");
            assert_eq!(p.state, "desired");
            assert_eq!(p.skills.len(), 1);
            assert_eq!(p.skills[0].skill_id, "tdd");
            assert_eq!(p.skills[0].version, "1.0.0");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn parses_install_event() {
    let data = r#"{"installation_id":"550e8400-e29b-41d4-a716-446655440001","skill_id":"new-skill","version":"2.3.0"}"#;
    let event = parse_sync_event("install", data).unwrap();
    match event {
        super::SyncEvent::Install {
            skill_id,
            version,
            source,
            ..
        } => {
            assert_eq!(skill_id, "new-skill");
            assert_eq!(version, "2.3.0");
            assert!(
                source.is_none(),
                "source must be None when backend omits it"
            );
        }
        other => panic!("expected Install, got {other:?}"),
    }
}

#[test]
fn parses_install_event_with_migrated_local_source() {
    // Newer backends include source="migrated:local" in the install event payload.
    // The runner must parse and propagate it for the phantom-artifact backstop.
    let data = r#"{"installation_id":"550e8400-e29b-41d4-a716-446655440001","skill_id":"handoff","version":"0.0.0","source":"migrated:local"}"#;
    let event = parse_sync_event("install", data).unwrap();
    match event {
        super::SyncEvent::Install {
            skill_id,
            version,
            source,
            ..
        } => {
            assert_eq!(skill_id, "handoff");
            assert_eq!(version, "0.0.0");
            assert_eq!(
                source.as_deref(),
                Some("migrated:local"),
                "source must be parsed from the install event payload"
            );
        }
        other => panic!("expected Install, got {other:?}"),
    }
}

#[test]
fn parses_deactivate_event() {
    let data =
        r#"{"installation_id":"550e8400-e29b-41d4-a716-446655440002","skill_id":"old-skill"}"#;
    let event = parse_sync_event("deactivate", data).unwrap();
    match event {
        super::SyncEvent::Deactivate { skill_id, .. } => {
            assert_eq!(skill_id, "old-skill");
        }
        other => panic!("expected Deactivate, got {other:?}"),
    }
}

#[test]
fn parses_purge_event() {
    let data =
        r#"{"installation_id":"550e8400-e29b-41d4-a716-446655440003","skill_id":"gone-skill"}"#;
    let event = parse_sync_event("purge", data).unwrap();
    match event {
        super::SyncEvent::Purge { skill_id, .. } => {
            assert_eq!(skill_id, "gone-skill");
        }
        other => panic!("expected Purge, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_event_type() {
    let result = parse_sync_event("unknown_type", r#"{"foo":"bar"}"#);
    assert!(result.is_err(), "unknown event type should return an error");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("unknown_type"),
        "error should name the event type"
    );
}

#[test]
fn rejects_malformed_json() {
    let result = parse_sync_event("install", "not-json");
    assert!(result.is_err(), "bad JSON should return an error");
}

#[test]
fn snapshot_with_multiple_records() {
    let data = r#"{
        "installations": [
            {"installation_id":"550e8400-e29b-41d4-a716-446655440010","skill_id":"skill-a","version":"1.0.0","state":"desired"},
            {"installation_id":"550e8400-e29b-41d4-a716-446655440011","skill_id":"skill-b","version":"2.0.0","state":"deactivated"}
        ]
    }"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            installations,
            mcp_installations: _,
            plugin_installations: _,
        } => {
            assert_eq!(installations.len(), 2);
            assert_eq!(installations[0].skill_id, "skill-a");
            assert_eq!(installations[1].state, "deactivated");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

// ── inference_policy_update dispatch ────────────────────────────────────────

/// Boot a real `AppState` (SQLite + dirs) under a fresh temp dir, matching the
/// pattern used elsewhere for `dispatch_event`-style tests (see
/// `auth_dispatch_tests.rs::reload_returns_inactive_without_token_and_is_idempotent`).
fn bootstrap_state() -> (vectorhawkd_core::state::AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = vectorhawkd_core::state::AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

#[tokio::test]
async fn inference_policy_update_flips_flag_and_persists() {
    use std::sync::atomic::Ordering;

    let (state, _tmp) = bootstrap_state();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut last_event_id = None;

    // Precondition: fresh AppState starts with the kill switch off, proving
    // the assertions below actually observe a flip rather than a no-op.
    assert!(
        !state.block_third_party_inference.load(Ordering::Relaxed),
        "fresh AppState must start with block_third_party_inference = false"
    );
    assert_eq!(
        state.get_sync_state("block_third_party_inference").unwrap(),
        None,
        "fresh AppState must have no persisted block_third_party_inference value"
    );

    dispatch_event(
        "inference_policy_update",
        r#"{"org_id":"default","enabled":true,"updated_at":"2026-01-01T00:00:00Z"}"#,
        &None,
        &mut last_event_id,
        &state,
        &tx,
    )
    .await
    .unwrap();

    assert!(
        state.block_third_party_inference.load(Ordering::Relaxed),
        "live atomic must flip to true"
    );
    assert_eq!(
        state
            .get_sync_state("block_third_party_inference")
            .unwrap()
            .as_deref(),
        Some("true"),
        "sync_state must persist the new value"
    );

    // Flip back to false — proves the handler isn't a one-shot / write-once
    // and correctly tracks the live value both ways.
    dispatch_event(
        "inference_policy_update",
        r#"{"org_id":"default","enabled":false,"updated_at":"2026-01-01T00:01:00Z"}"#,
        &None,
        &mut last_event_id,
        &state,
        &tx,
    )
    .await
    .unwrap();

    assert!(
        !state.block_third_party_inference.load(Ordering::Relaxed),
        "live atomic must flip back to false"
    );
    assert_eq!(
        state
            .get_sync_state("block_third_party_inference")
            .unwrap()
            .as_deref(),
        Some("false"),
        "sync_state must persist the flip back to false"
    );
}

// ── device revocation (kill switch) ─────────────────────────────────────────

#[test]
fn device_revoked_body_detected_in_fastapi_http_exception_shape() {
    // FastAPI's HTTPException(detail={"code": "device_revoked", ...}) wire
    // shape: the dict passed to `detail` lands nested one level under the
    // top-level "detail" key.
    assert!(is_device_revoked_body(
        r#"{"detail":{"code":"device_revoked","detail":"Device has been revoked"}}"#
    ));
}

#[test]
fn device_revoked_body_also_detected_in_flat_shape() {
    // Tolerate a hypothetical future flatter error shape too — this is a
    // best-effort optional fast path, not a hard contract with one route.
    assert!(is_device_revoked_body(r#"{"code":"device_revoked"}"#));
}

#[test]
fn ordinary_403_body_is_not_mistaken_for_revocation() {
    assert!(!is_device_revoked_body(
        r#"{"detail":"Admin access required"}"#
    ));
    assert!(!is_device_revoked_body(""));
    assert!(!is_device_revoked_body("not json at all"));
}

/// End-to-end: a 403 carrying the device-revoked signal must stop the SSE
/// client's `run()` loop outright — no retry, no backoff sleep — rather
/// than treating it like any other transient error. Modeled on
/// `sync_controller_hook_tests::sse_client_reconnects_with_fresh_device_id_
/// after_sync_state_change`, which drives `sse_client::run` directly against
/// a mockito server for the same reason: this must hold independent of
/// `SyncController`.
#[tokio::test]
async fn revoked_device_stops_the_sse_loop_instead_of_retrying() {
    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let revoked_attempt = server
        .mock("GET", "/api/sync/events")
        .with_status(403)
        .with_body(r#"{"detail":{"code":"device_revoked","detail":"Device has been revoked"}}"#)
        .expect(1)
        .create_async()
        .await;

    // If the fix regresses to "retry like any other error", this second
    // mock would start getting hit after the 1s backoff — `expect(0)`
    // below would then fail the test.
    let would_retry = server
        .mock("GET", "/api/sync/events")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect(0)
        .create_async()
        .await;

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, _connected_rx) = tokio::sync::watch::channel(false);
    let live_token = Arc::new(tokio::sync::RwLock::new("tok".to_string()));
    let config = crate::sync::SyncConfig {
        registry_url: registry_url.clone(),
        token: "tok".to_string(),
        device_id: "dev-revoked".to_string(),
        last_event_id: None,
        pusher: None,
        live_token,
    };

    // `run()` must return on its own — no abort needed — because a
    // revocation is terminal for this task, not a thing to sleep through.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::sync::sse_client::run(config, state, event_tx, connected_tx),
    )
    .await
    .expect("run() must return promptly on a revoked-device 403, not loop forever");

    revoked_attempt.assert_async().await;
    would_retry.assert_async().await;
}

/// Beyond just stopping the loop (above), a revoked-device 403 must clear
/// the local device identity and stamp `device_revoked_at` — see
/// `AppState::mark_device_revoked`'s doc comment for why this matters:
/// without it, the daemon's own unattended register-on-startup path would
/// have a stale (now-dead) `device_uuid`/`device_id` sitting in
/// `sync_state` forever, and `auth status`/`doctor` would have nothing to
/// tell the user.
#[tokio::test]
async fn revoked_device_clears_local_identity_and_stamps_marker() {
    let (state, _tmp) = bootstrap_state();
    state.set_sync_state("device_uuid", "uuid-revoked").unwrap();
    state.set_sync_state("device_id", "dev-revoked").unwrap();
    let state = Arc::new(state);

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    server
        .mock("GET", "/api/sync/events")
        .with_status(403)
        .with_body(r#"{"detail":{"code":"device_revoked","detail":"Device has been revoked"}}"#)
        .create_async()
        .await;

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, _connected_rx) = tokio::sync::watch::channel(false);
    let live_token = Arc::new(tokio::sync::RwLock::new("tok".to_string()));
    let config = crate::sync::SyncConfig {
        registry_url: registry_url.clone(),
        token: "tok".to_string(),
        device_id: "dev-revoked".to_string(),
        last_event_id: None,
        pusher: None,
        live_token,
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::sync::sse_client::run(config, Arc::clone(&state), event_tx, connected_tx),
    )
    .await
    .expect("run() must return promptly on a revoked-device 403");

    assert_eq!(state.get_sync_state("device_uuid").unwrap(), None);
    assert_eq!(state.get_sync_state("device_id").unwrap(), None);
    assert!(
        state.get_sync_state("device_revoked_at").unwrap().is_some(),
        "device_revoked_at must be stamped"
    );
}
