//! Unit tests for the reconciler snapshot-diff logic.
#![allow(clippy::unwrap_used)]

use super::{build_derived_events_blocking, load_local_skill_state};
use crate::sync::sse_client::{InstallationRecord, SyncEvent};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use vectorhawkd_core::state::AppState;

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-reconciler-tests-{label}-{nanos}")),
    )
    .expect("temp path utf-8")
}

fn cleanup(root: &Utf8PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

fn install_id() -> Uuid {
    Uuid::new_v4()
}

/// Seed an installed_skills row with optional deactivated flag.
fn seed_skill(conn: &Connection, skill_id: &str, version: &str, deactivated: bool) {
    conn.execute(
        "INSERT OR REPLACE INTO installed_skills \
         (skill_id, active_version, install_root, current_status, deactivated) \
         VALUES (?1, ?2, '/fake', 'active', ?3)",
        rusqlite::params![skill_id, version, if deactivated { 1i64 } else { 0i64 }],
    )
    .unwrap();
}

// ── load_local_skill_state tests ──────────────────────────────────────────────

#[test]
fn local_state_empty_when_no_skills() {
    let root = temp_root("local-empty");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();

    let local = load_local_skill_state(&conn);
    assert!(local.is_empty(), "no skills → empty map");

    cleanup(&root);
}

#[test]
fn local_state_reflects_active_skill() {
    let root = temp_root("local-active");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();

    seed_skill(&conn, "skill-a", "1.0.0", false);
    let local = load_local_skill_state(&conn);

    assert!(local.contains_key("skill-a"));
    let (ver, deactivated) = &local["skill-a"];
    assert_eq!(ver, "1.0.0");
    assert!(!deactivated);

    cleanup(&root);
}

#[test]
fn local_state_reflects_deactivated_skill() {
    let root = temp_root("local-deactivated");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();

    seed_skill(&conn, "skill-b", "2.0.0", true);
    let local = load_local_skill_state(&conn);

    let (ver, deactivated) = &local["skill-b"];
    assert_eq!(ver, "2.0.0");
    assert!(deactivated);

    cleanup(&root);
}

// ── build_derived_events_blocking tests ──────────────────────────────────────

#[test]
fn snapshot_installs_missing_desired_skill() {
    let root = temp_root("snapshot-install");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    // No locally installed skills.

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "new-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "desired".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
        } => {
            assert_eq!(*installation_id, iid);
            assert_eq!(skill_id, "new-skill");
            assert_eq!(version, "1.0.0");
        }
        other => panic!("expected Install, got {other:?}"),
    }

    cleanup(&root);
}

#[test]
fn snapshot_no_op_when_skill_already_installed() {
    let root = temp_root("snapshot-noop");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    seed_skill(&conn, "installed-skill", "1.0.0", false);
    drop(conn);

    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "installed-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "desired".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert!(
        events.is_empty(),
        "already-installed desired skill → no events"
    );

    cleanup(&root);
}

#[test]
fn snapshot_deactivates_active_skill_when_state_is_deactivated() {
    let root = temp_root("snapshot-deactivate");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    seed_skill(&conn, "active-skill", "1.0.0", false);
    drop(conn);

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "active-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "deactivated".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Deactivate { skill_id, .. } => {
            assert_eq!(skill_id, "active-skill");
        }
        other => panic!("expected Deactivate, got {other:?}"),
    }

    cleanup(&root);
}

#[test]
fn snapshot_purges_present_skill_when_state_is_removed() {
    let root = temp_root("snapshot-purge");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    seed_skill(&conn, "old-skill", "1.0.0", false);
    drop(conn);

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "old-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "removed".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Purge { skill_id, .. } => {
            assert_eq!(skill_id, "old-skill");
        }
        other => panic!("expected Purge, got {other:?}"),
    }

    cleanup(&root);
}

#[test]
fn snapshot_no_purge_when_skill_not_locally_present() {
    let root = temp_root("snapshot-no-purge");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    // Skill not in local DB.

    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "ghost-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "removed".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "ghost skill already absent → no events");

    cleanup(&root);
}

#[test]
fn snapshot_mixed_batch_generates_correct_events() {
    let root = temp_root("snapshot-mixed");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    // install-me is not local (desired → Install)
    seed_skill(&conn, "deactivate-me", "1.0.0", false); // active → Deactivate
    seed_skill(&conn, "purge-me", "1.0.0", false); // present → Purge
    seed_skill(&conn, "keep-me", "2.0.0", false); // active, desired → no-op
    drop(conn);

    let installations = vec![
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "install-me".to_string(),
            version: "1.0.0".to_string(),
            state: "desired".to_string(),
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "deactivate-me".to_string(),
            version: "1.0.0".to_string(),
            state: "deactivated".to_string(),
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "purge-me".to_string(),
            version: "1.0.0".to_string(),
            state: "removed".to_string(),
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "keep-me".to_string(),
            version: "2.0.0".to_string(),
            state: "desired".to_string(),
        },
    ];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 3, "3 action events expected (no-op skipped)");

    let installs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::Install { .. }))
        .collect();
    let deactivates: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::Deactivate { .. }))
        .collect();
    let purges: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::Purge { .. }))
        .collect();

    assert_eq!(installs.len(), 1, "one Install expected");
    assert_eq!(deactivates.len(), 1, "one Deactivate expected");
    assert_eq!(purges.len(), 1, "one Purge expected");

    cleanup(&root);
}

#[test]
fn snapshot_installed_state_treated_like_desired_when_missing_locally() {
    // Backend sees the row as "installed" but locally it isn't — the daemon
    // should install it. Previously this state was logged as "unknown" and
    // skipped, which meant a fresh-from-install device could not converge
    // until the snapshot was re-sent with state="desired" (never happens).
    let root = temp_root("snapshot-installed-state");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "needs-install".to_string(),
        version: "1.2.3".to_string(),
        state: "installed".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
        } => {
            assert_eq!(*installation_id, iid);
            assert_eq!(skill_id, "needs-install");
            assert_eq!(version, "1.2.3");
        }
        other => panic!("expected Install, got {other:?}"),
    }

    cleanup(&root);
}

#[test]
fn snapshot_installed_state_noop_when_already_present_locally() {
    let root = temp_root("snapshot-installed-noop");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    let conn = Connection::open(&state.db_path).unwrap();
    seed_skill(&conn, "already-here", "1.0.0", false);
    drop(conn);

    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "already-here".to_string(),
        version: "1.0.0".to_string(),
        state: "installed".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "installed + locally present → no events");

    cleanup(&root);
}

#[test]
fn snapshot_skips_rows_with_unresolved_latest_version() {
    // Legacy rows that bypassed POST version resolution may carry
    // version="latest"; the artifact-metadata endpoint requires a concrete
    // semver and would 404. Skip rather than spin in retries.
    let root = temp_root("snapshot-unresolved");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let installations = vec![
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "latest-row".to_string(),
            version: "latest".to_string(),
            state: "desired".to_string(),
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "empty-row".to_string(),
            version: String::new(),
            state: "desired".to_string(),
        },
    ];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "unresolved-version rows → no events");

    cleanup(&root);
}

#[test]
fn snapshot_skips_error_state_rows() {
    let root = temp_root("snapshot-error-state");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "errored".to_string(),
        version: "1.0.0".to_string(),
        state: "error".to_string(),
    }];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "error-state rows → no auto-retry");

    cleanup(&root);
}
