//! Unit tests for the SSE event parser.
#![allow(clippy::unwrap_used)]

use super::parse_sync_event;

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
