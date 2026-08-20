//! Unit tests for the SSE event parser.
#![allow(clippy::unwrap_used)]

use super::{dispatch_event, parse_sync_event};

#[test]
fn parses_snapshot_event() {
    let data = r#"{"installations":[{"installation_id":"550e8400-e29b-41d4-a716-446655440000","skill_id":"my-skill","version":"1.0.0","state":"desired"}]}"#;
    let event = parse_sync_event("snapshot", data).unwrap();
    match event {
        super::SyncEvent::Snapshot {
            installations,
            mcp_installations,
        } => {
            assert_eq!(installations.len(), 1);
            assert_eq!(installations[0].skill_id, "my-skill");
            assert_eq!(installations[0].version, "1.0.0");
            assert_eq!(installations[0].state, "desired");
            assert!(
                mcp_installations.is_empty(),
                "old-format snapshot has no mcp_installations key → default empty vec"
            );
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
