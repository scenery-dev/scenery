//! Unit tests for the SSE event parser.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use super::{dispatch_event, is_device_revoked_body, is_user_revoked_body, parse_sync_event};

#[test]
fn parses_snapshot_event() {
    let data = r#"{"installations":[{"installation_id":"550e8400-e29b-41d4-a716-446655440000","skill_id":"my-skill","version":"1.0.0","state":"desired"}]}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            installations,
            mcp_installations,
            plugin_installations,
            revocations,
            org_policy,
            org_id: _,
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
            assert!(
                revocations.is_none(),
                "old-format snapshot has no revocations key → None, so the local \
                 revocation cache must be left untouched, not wiped"
            );
            assert!(
                org_policy.is_none(),
                "old-format snapshot has no org_policy key → None, so sync_state \
                 and the live block_third_party atomic must be left untouched"
            );
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn parses_snapshot_event_with_empty_mcp_installations_as_some_empty() {
    // A present-but-empty "mcp_installations": [] (what the current backend
    // sends for a device with zero MCP installs — see
    // scenery-backend/backend/app/routers/portal_sync.py _get_mcp_snapshot,
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
    // scenery-backend/backend/app/routers/portal_sync.py. The runner must
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
                "author": "Scenery",
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
            assert_eq!(p.author, "Scenery");
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
fn parses_revoke_event_whole_artifact() {
    let data = r#"{"artifact_type":"skill","artifact_key":"code-review"}"#;
    let event = parse_sync_event("revoke", data).unwrap();
    match event {
        super::SyncEvent::Revoke {
            artifact_type,
            artifact_key,
            version,
        } => {
            assert_eq!(artifact_type, "skill");
            assert_eq!(artifact_key, "code-review");
            assert!(
                version.is_none(),
                "an omitted version means the whole artifact, every version"
            );
        }
        other => panic!("expected Revoke, got {other:?}"),
    }
}

#[test]
fn parses_unrevoke_event() {
    // Bug 2 fix: live "unblock" push, mirroring the `revoke` event shape.
    let data = r#"{"artifact_type":"skill","artifact_key":"code-review","version":null}"#;
    let event = parse_sync_event("unrevoke", data).unwrap();
    match event {
        super::SyncEvent::Unrevoke {
            artifact_type,
            artifact_key,
            version,
            signed: _,
        } => {
            assert_eq!(artifact_type, "skill");
            assert_eq!(artifact_key, "code-review");
            assert!(version.is_none());
        }
        other => panic!("expected Unrevoke, got {other:?}"),
    }
}

#[test]
fn unknown_event_type_is_logged_and_skipped_not_fatal() {
    // Precondition for both `revoke` and `unrevoke` being safely additive:
    // a runner that predates them must treat the unrecognized event name as
    // a no-op, not a crash or a reason to drop the connection. Confirmed
    // identical in the shipped v1.0.99 tag (same `other => bail!` arm).
    assert!(parse_sync_event("some_future_event_type", "{}").is_err());
}

#[test]
fn parses_revoke_event_scoped_to_one_version() {
    let data = r#"{"artifact_type":"skill","artifact_key":"code-review","version":"1.2.3","revoked_at":"2026-10-06T12:00:00Z"}"#;
    let event = parse_sync_event("revoke", data).unwrap();
    match event {
        super::SyncEvent::Revoke {
            artifact_type,
            artifact_key,
            version,
        } => {
            assert_eq!(artifact_type, "skill");
            assert_eq!(artifact_key, "code-review");
            assert_eq!(version, Some("1.2.3".to_string()));
        }
        other => panic!("expected Revoke, got {other:?}"),
    }
}

#[test]
fn parses_revoke_event_for_mcp_server_and_plugin_artifact_types() {
    for (artifact_type, key) in [
        ("mcp_server", "550e8400-e29b-41d4-a716-446655440099"),
        ("plugin", "release-notes-bundle"),
    ] {
        let data = format!(r#"{{"artifact_type":"{artifact_type}","artifact_key":"{key}"}}"#);
        let event = parse_sync_event("revoke", &data).unwrap();
        match event {
            super::SyncEvent::Revoke {
                artifact_type: at,
                artifact_key: ak,
                ..
            } => {
                assert_eq!(at, artifact_type);
                assert_eq!(ak, key);
            }
            other => panic!("expected Revoke, got {other:?}"),
        }
    }
}

#[test]
fn parses_snapshot_event_with_revocations_list() {
    let data = r#"{"installations":[],"revocations":[{"artifact_type":"skill","artifact_key":"blocked-skill"},{"artifact_type":"mcp_server","artifact_key":"srv-1","version":"0.2.0"}]}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot { revocations, .. } => {
            let revs = revocations.expect("present key must parse to Some(..)");
            assert_eq!(revs.len(), 2);
            assert_eq!(revs[0].artifact_type, "skill");
            assert_eq!(revs[0].artifact_key, "blocked-skill");
            assert!(revs[0].version.is_none());
            assert_eq!(revs[1].version, Some("0.2.0".to_string()));
        }
        other => panic!("expected Snapshot, got {other:?}"),
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
            revocations: _,
            org_policy: _,
            org_id: None,
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
fn bootstrap_state() -> (scenery_core::state::AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = scenery_core::state::AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

/// An unsigned (or forged) org-policy push changes nothing.
#[tokio::test]
async fn unsigned_inference_policy_update_is_ignored() {
    use std::sync::atomic::Ordering;
    let (state, _tmp) = bootstrap_state();
    crate::test_support::pair_device(&state, "http://127.0.0.1:9");
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut last_event_id = None;
    dispatch_event(
        "inference_policy_update",
        r#"{"org_id":"default","enabled":true}"#,
        &None,
        &mut last_event_id,
        &state,
        &tx,
    )
    .await
    .unwrap();
    assert!(!state.block_third_party_inference.load(Ordering::Relaxed));
    assert_eq!(
        state.get_sync_state("block_third_party_inference").unwrap(),
        None
    );
}

#[tokio::test]
async fn inference_policy_update_flips_flag_and_persists() {
    use std::sync::atomic::Ordering;

    let (state, _tmp) = bootstrap_state();
    let origin = "http://127.0.0.1:9";
    crate::test_support::pair_device(&state, origin);
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
        &format!(
            r#"{{"org_id":"default","enabled":true,"signed":{}}}"#,
            crate::test_support::sign_org_policy(origin, "audit", true)
        ),
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
        &format!(
            r#"{{"org_id":"default","enabled":false,"signed":{}}}"#,
            crate::test_support::sign_org_policy(origin, "audit", false)
        ),
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

// ── org-policy snapshot reconcile (Card 3) ──────────────────────────────────

#[test]
fn snapshot_parses_org_policy_block() {
    let data = r#"{
        "installations": [],
        "org_policy": {
            "managed_paths_mode": "quarantine",
            "block_third_party_inference": true
        }
    }"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot { org_policy, .. } => {
            let policy = org_policy.expect("org_policy key present must parse to Some");
            assert_eq!(policy.managed_paths_mode.as_deref(), Some("quarantine"));
            assert_eq!(policy.block_third_party_inference, Some(true));
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn snapshot_without_org_policy_key_parses_to_none() {
    // Backward compat: an older backend's snapshot (or one with nothing to
    // report) omits `org_policy` entirely — must parse cleanly to `None`,
    // not fail deserialization and not default to some concrete policy.
    let data = r#"{"installations": []}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot { org_policy, .. } => {
            assert!(org_policy.is_none());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn snapshot_org_policy_partial_fields_parse_independently() {
    // A backend might report only one flag's current value in a given
    // snapshot. The other field must parse to `None` ("no change"), not
    // fail deserialization or default to `false`.
    let data = r#"{
        "installations": [],
        "org_policy": { "block_third_party_inference": true }
    }"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot { org_policy, .. } => {
            let policy = org_policy.unwrap();
            assert_eq!(policy.managed_paths_mode, None);
            assert_eq!(policy.block_third_party_inference, Some(true));
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
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

/// An admin revoking the USER is terminal too. The backend checks the user
/// before the device on `/sync/events`, so a revoked user's daemon gets
/// 403 `{"code":"user_revoked"}` rather than `device_revoked`. That must
/// stop the loop the same way (user revoke cascades to every device), not
/// fall into the generic 403 backoff and retry every 60s forever.
#[tokio::test]
async fn revoked_user_stops_the_sse_loop_and_clears_local_identity() {
    let (state, _tmp) = bootstrap_state();
    state
        .set_sync_state("device_uuid", "uuid-user-revoked")
        .unwrap();
    state
        .set_sync_state("device_id", "dev-user-revoked")
        .unwrap();
    let state = Arc::new(state);

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let revoked_attempt = server
        .mock("GET", "/api/sync/events")
        .with_status(403)
        .with_body(r#"{"detail":{"code":"user_revoked","detail":"User has been revoked"}}"#)
        .expect(1)
        .create_async()
        .await;

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, _connected_rx) = tokio::sync::watch::channel(false);
    let live_token = Arc::new(tokio::sync::RwLock::new("tok".to_string()));
    let config = crate::sync::SyncConfig {
        registry_url: registry_url.clone(),
        token: "tok".to_string(),
        device_id: "dev-user-revoked".to_string(),
        last_event_id: None,
        pusher: None,
        live_token,
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::sync::sse_client::run(config, Arc::clone(&state), event_tx, connected_tx),
    )
    .await
    .expect("run() must return promptly on a revoked-user 403, not back off forever");

    revoked_attempt.assert_async().await;
    assert_eq!(state.get_sync_state("device_uuid").unwrap(), None);
    assert_eq!(state.get_sync_state("device_id").unwrap(), None);
}

#[test]
fn user_revoked_body_detected() {
    assert!(is_user_revoked_body(
        r#"{"detail":{"code":"user_revoked","detail":"User has been revoked"}}"#
    ));
    assert!(!is_user_revoked_body(
        r#"{"detail":{"code":"device_revoked"}}"#
    ));
    assert!(!is_user_revoked_body("not json"));
}

// ── bug-revoke-loop: bounded retry on a 401/refresh/401 cycle ─────────────────

/// Scripted fake backend over a raw TCP listener so the tests can record the
/// time of every request (mockito cannot). Each `/api/sync/events` GET pops
/// the next scripted status (the last one repeats); refresh always succeeds.
struct FakeBackend {
    url: String,
    event_times: Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
    refreshes: Arc<std::sync::atomic::AtomicUsize>,
}

async fn fake_backend(script: Vec<(u16, &'static str)>) -> FakeBackend {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let event_times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (times, refr) = (Arc::clone(&event_times), Arc::clone(&refreshes));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let (status, body) = if req.starts_with("POST /portal/auth/refresh") {
                refr.fetch_add(1, Ordering::SeqCst);
                (
                    200,
                    r#"{"access_token":"new-tok","refresh_token":"new-refresh"}"#,
                )
            } else {
                let mut t = times.lock().unwrap();
                let idx = t.len().min(script.len() - 1);
                t.push(std::time::Instant::now());
                script[idx]
            };
            let ctype = if body.is_empty() {
                "text/event-stream"
            } else {
                "application/json"
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    FakeBackend {
        url,
        event_times,
        refreshes,
    }
}

fn test_policy() -> crate::sync::sse_client::BackoffPolicy {
    use std::time::Duration;
    crate::sync::sse_client::BackoffPolicy {
        init: Duration::from_millis(20),
        max: Duration::from_millis(80),
        degraded_init: Duration::from_millis(150),
        degraded_max: Duration::from_millis(300),
    }
}

fn sync_config(url: &str) -> crate::sync::SyncConfig {
    crate::sync::SyncConfig {
        registry_url: url.to_string(),
        token: "stale".to_string(),
        device_id: "dev-1".to_string(),
        last_event_id: None,
        pusher: None,
        live_token: Arc::new(tokio::sync::RwLock::new("stale".to_string())),
    }
}

/// Kill-switch reliability: a burst of post-refresh 401s (backend redeploy)
/// must NOT stop the client. It enters degraded mode with capped backoff
/// (never retrying without a sleep) and reconnects on its own once the
/// backend returns 200; the degraded marker is stamped, then cleared.
#[tokio::test]
async fn post_refresh_401_burst_degrades_then_recovers_without_restart() {
    use scenery_core::auth::save_tokens;
    use std::time::Duration;

    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);
    // 8 x 401 (streak trips at the 4th), then a clean (empty) stream.
    let mut script = vec![(401u16, r#"{"detail":"nope"}"#); 8];
    script.push((200, ""));
    let fake = fake_backend(script).await;
    save_tokens(&state, &fake.url, "stale", "refresh-tok").unwrap();

    let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, mut connected_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(crate::sync::sse_client::run_with_policy(
        sync_config(&fake.url),
        Arc::clone(&state),
        event_tx,
        connected_tx,
        test_policy(),
    ));

    tokio::time::timeout(Duration::from_secs(10), connected_rx.wait_for(|c| *c))
        .await
        .expect("client must reconnect on its own after the 401 burst")
        .unwrap();
    assert!(!task.is_finished(), "401s must never stop the SSE task");

    let times = fake.event_times.lock().unwrap().clone();
    assert!(times.len() >= 9, "expected 9 attempts, got {}", times.len());
    let p = test_policy();
    // No retry without a backoff sleep: every gap honours the schedule
    // (small slack for timer granularity).
    for (i, w) in times.windows(2).take(9).enumerate() {
        let gap = w[1].duration_since(w[0]);
        let min = if i < 3 { p.init } else { p.degraded_init };
        assert!(
            gap + Duration::from_millis(10) >= min,
            "gap {i} too short: {gap:?}"
        );
    }
    // Bounded: refresh happened once per 401, never in a storm.
    assert!(fake.refreshes.load(std::sync::atomic::Ordering::SeqCst) <= 8);
    task.abort();
}

/// While degraded the marker is visible to `auth status`/`doctor`, and is
/// cleared after recovery.
#[tokio::test]
async fn degraded_marker_set_then_cleared_on_recovery() {
    use scenery_core::auth::save_tokens;
    use std::time::Duration;

    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);
    let mut script = vec![(401u16, "{}"); 6];
    script.push((200, ""));
    let fake = fake_backend(script).await;
    save_tokens(&state, &fake.url, "stale", "refresh-tok").unwrap();

    let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, mut connected_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(crate::sync::sse_client::run_with_policy(
        sync_config(&fake.url),
        Arc::clone(&state),
        event_tx,
        connected_tx,
        test_policy(),
    ));
    // Attempt 5+ is degraded; poll for the marker before recovery.
    let mut seen = false;
    for _ in 0..200 {
        if state
            .get_sync_state(crate::sync::sse_client::SSE_DEGRADED_KEY)
            .unwrap()
            .is_some()
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(seen, "degraded marker must be stamped");
    tokio::time::timeout(Duration::from_secs(10), connected_rx.wait_for(|c| *c))
        .await
        .unwrap()
        .unwrap();
    // Marker is cleared on the Reconnect that follows the empty stream.
    let mut cleared = false;
    for _ in 0..200 {
        if state
            .get_sync_state(crate::sync::sse_client::SSE_DEGRADED_KEY)
            .unwrap()
            .is_none()
        {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(cleared, "degraded marker must clear after a clean connect");
    task.abort();
}

/// Even after the streak trips, an explicit 403 `device_revoked` still
/// stops the task permanently (the only terminal signals are explicit 403s).
#[tokio::test]
async fn device_revoked_403_still_stops_after_degraded_401s() {
    use scenery_core::auth::save_tokens;
    use std::time::Duration;

    let (state, _tmp) = bootstrap_state();
    let state = Arc::new(state);
    let mut script = vec![(401u16, "{}"); 5];
    script.push((403, r#"{"detail":{"code":"device_revoked"}}"#));
    let fake = fake_backend(script).await;
    save_tokens(&state, &fake.url, "stale", "refresh-tok").unwrap();

    let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
    let (connected_tx, _crx) = tokio::sync::watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(10),
        crate::sync::sse_client::run_with_policy(
            sync_config(&fake.url),
            Arc::clone(&state),
            event_tx,
            connected_tx,
            test_policy(),
        ),
    )
    .await
    .expect("device_revoked must stop the loop");
    assert_eq!(fake.event_times.lock().unwrap().len(), 6);
}

/// A revoked slug's source dir must never be copied into the managed dirs,
/// even briefly; a non-revoked one still is (control).
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn discovery_adopted_does_not_copy_a_revoked_slug() {
    let _guard = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let (state, _tmp) = bootstrap_state();
    scenery_core::revocation::upsert_revocation(&state, "skill", "revoked-adopt", None).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut last_event_id = None;
    for slug in ["revoked-adopt", "fine-adopt"] {
        let src = home.path().join("src").join(slug);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), format!("---\nname: {slug}\n---\n")).unwrap();
        let data = serde_json::json!({
            "slug": slug, "kind": "skill", "source_path": src.to_string_lossy(),
        })
        .to_string();
        dispatch_event(
            "discovery_adopted",
            &data,
            &None,
            &mut last_event_id,
            &state,
            &tx,
        )
        .await
        .unwrap();
    }

    let fine = home.path().join(".agents/skills/fine-adopt");
    for _ in 0..100 {
        if fine.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let fine_pushed = fine.exists();
    let revoked_copied = home.path().join(".agents/skills/revoked-adopt").exists()
        || home
            .path()
            .join(".claude/skills/revoked-adopt")
            .symlink_metadata()
            .is_ok();

    if let Some(v) = prev {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    assert!(fine_pushed, "control: a non-revoked adopt is still pushed");
    assert!(!revoked_copied, "a revoked slug must not be copied");
}

// ── M5: byte-level line splitting ────────────────────────────────────────────

use super::{snapshot_event_from_json, LineSplitter, SseLine, MAX_SSE_LINE};

fn lines(chunks: &[&[u8]]) -> Vec<SseLine> {
    let mut s = LineSplitter::default();
    let mut out = Vec::new();
    for c in chunks {
        out.extend(s.push(c).unwrap());
    }
    out
}

#[test]
fn a_three_byte_character_split_across_chunks_is_not_an_error() {
    // "€" is E2 82 AC; cut it after each byte.
    let full = "data: price €5\n".as_bytes().to_vec();
    let at = full.iter().position(|b| *b == 0xE2).unwrap();
    for cut in [at + 1, at + 2] {
        let got = lines(&[&full[..cut], &full[cut..]]);
        assert_eq!(
            got,
            vec![SseLine::Text("data: price €5".to_string())],
            "cut at {cut}"
        );
    }
    // One byte at a time.
    let bytes: Vec<&[u8]> = full.chunks(1).collect();
    assert_eq!(
        lines(&bytes),
        vec![SseLine::Text("data: price €5".to_string())]
    );
}

#[test]
fn crlf_split_across_chunks_yields_one_clean_line() {
    let got = lines(&[b"data: a\r", b"\ndata: b\r\n", b"\r\n"]);
    assert_eq!(
        got,
        vec![
            SseLine::Text("data: a".into()),
            SseLine::Text("data: b".into()),
            SseLine::Text(String::new()),
        ]
    );
}

#[test]
fn invalid_utf8_line_is_flagged_and_the_stream_continues() {
    let got = lines(&[b"data: \xff\xfe\n", b"data: ok\n"]);
    assert_eq!(
        got,
        vec![SseLine::Invalid, SseLine::Text("data: ok".into())]
    );
}

#[test]
fn an_unterminated_line_over_the_cap_is_an_error() {
    let mut s = LineSplitter::default();
    let chunk = vec![b'x'; 1024 * 1024];
    let mut result = Ok(Vec::new());
    for _ in 0..=(MAX_SSE_LINE / chunk.len()) {
        result = s.push(&chunk);
        if result.is_err() {
            break;
        }
    }
    assert!(result.is_err(), "line cap must trip");
    // A long line that IS terminated, under the cap, is fine.
    let mut s = LineSplitter::default();
    let mut ok = vec![b'y'; 1024 * 1024];
    ok.push(b'\n');
    assert_eq!(s.push(&ok).unwrap().len(), 1);
}

// ── M1: unsafe names from the wire are dropped ───────────────────────────────

#[test]
fn events_with_unsafe_names_are_rejected() {
    for (ev, data) in [
        (
            "install",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"../../Documents/x","version":"1.0.0"}"#,
        ),
        (
            "install",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"ok","version":"../1"}"#,
        ),
        (
            "purge",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"/Users/u/work"}"#,
        ),
        (
            "deactivate",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":".."}"#,
        ),
        (
            "deactivate_plugin",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","plugin_slug":"a/b"}"#,
        ),
        (
            "install_plugin",
            r#"{"installation_id":"00000000-0000-0000-0000-000000000001","plugin_slug":"p","version":"1.0.0","skills":[{"skill_id":"../x","version":"1"}]}"#,
        ),
        (
            "revoke",
            r#"{"artifact_type":"skill","artifact_key":"../../x"}"#,
        ),
        (
            "unrevoke",
            r#"{"artifact_type":"plugin","artifact_key":"ok","version":"../1"}"#,
        ),
    ] {
        assert!(parse_sync_event(ev, data).is_err(), "{ev}: {data}");
    }
    // Sane names still parse.
    assert!(parse_sync_event(
        "purge",
        r#"{"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"code-review"}"#
    )
    .is_ok());
}

#[test]
fn snapshot_drops_only_the_bad_records_and_keeps_none_vs_empty() {
    let data = r#"{
        "installations":[
            {"installation_id":"00000000-0000-0000-0000-000000000001","skill_id":"good","version":"1.0.0","state":"installed"},
            {"installation_id":"00000000-0000-0000-0000-000000000002","skill_id":"../evil","version":"1.0.0","state":"installed"},
            {"installation_id":"00000000-0000-0000-0000-000000000003","skill_id":"fine","version":"1.0.0","state":"installed","local_slug":"/abs"}
        ],
        "plugin_installations":[
            {"installation_id":"00000000-0000-0000-0000-000000000004","plugin_slug":"p/../q","version":"1.0.0","state":"installed"}
        ],
        "revocations":[
            {"artifact_type":"skill","artifact_key":"../x"},
            {"artifact_type":"skill","artifact_key":"ok-skill"}
        ]
    }"#;
    match snapshot_event_from_json(data).unwrap() {
        super::SyncEvent::Snapshot {
            installations,
            mcp_installations,
            plugin_installations,
            revocations,
            ..
        } => {
            assert_eq!(installations.len(), 1);
            assert_eq!(installations[0].skill_id, "good");
            assert!(plugin_installations.is_empty());
            // The key was absent: still None, not Some(empty).
            assert!(mcp_installations.is_none());
            // Present list: filtered, still Some.
            assert_eq!(revocations.unwrap().len(), 1);
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
    // A present-but-all-bad revocation list stays Some(empty), not None.
    let all_bad =
        r#"{"installations":[],"revocations":[{"artifact_type":"skill","artifact_key":".."}]}"#;
    match snapshot_event_from_json(all_bad).unwrap() {
        super::SyncEvent::Snapshot { revocations, .. } => {
            assert_eq!(revocations.map(|r| r.len()), Some(0));
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
}
