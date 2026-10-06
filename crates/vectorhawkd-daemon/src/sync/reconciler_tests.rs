//! Unit tests for the reconciler snapshot-diff logic.
#![allow(clippy::unwrap_used)]

use super::{build_derived_events_blocking, load_local_skill_state, skill_lock, SkillLockMap};
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

/// Seed a skill plus its on-disk active/ symlink under the state root so
/// the build_derived_events filesystem check sees it as installed.
fn seed_skill_with_fs(state: &AppState, skill_id: &str, version: &str, deactivated: bool) {
    let conn = Connection::open(&state.db_path).unwrap();
    seed_skill(&conn, skill_id, version, deactivated);

    let install_root = state.root_dir.join("skills").join(skill_id);
    let version_dir = install_root.join("versions").join(version);
    let active_dir = install_root.join("active");
    std::fs::create_dir_all(version_dir.as_std_path()).unwrap();
    // Active symlink only exists when not deactivated.
    if !deactivated {
        if active_dir.exists() || active_dir.is_symlink() {
            let _ = std::fs::remove_file(active_dir.as_std_path());
        }
        #[cfg(target_family = "unix")]
        std::os::unix::fs::symlink(version_dir.as_std_path(), active_dir.as_std_path()).unwrap();
    }
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
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
            ..
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
    seed_skill_with_fs(&state, "installed-skill", "1.0.0", false);

    // Both sides already agree (backend's row is "installed" and the skill
    // is locally present) — nothing to converge.
    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "installed-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "installed".to_string(),
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert!(
        events.is_empty(),
        "already-installed skill with backend row in 'installed' → no events"
    );

    cleanup(&root);
}

#[test]
fn snapshot_emits_install_when_locally_present_but_backend_not_yet_installed() {
    // Cross-registry reconciliation: the daemon previously paired with a
    // different backend and has the skill on disk. The new backend's
    // desired-state row is still "desired" (Queued in the portal) because
    // it has never received an "installed" PATCH from this daemon. The
    // reconciler must emit an Install event so the install handler short-
    // circuits via check_version_local and PATCHes "installed" with the
    // new installation_id.
    let root = temp_root("snapshot-cross-registry");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    seed_skill_with_fs(&state, "carryover-skill", "1.0.0", false);

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "carryover-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "desired".to_string(),
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
            ..
        } => {
            assert_eq!(*installation_id, iid);
            assert_eq!(skill_id, "carryover-skill");
            assert_eq!(version, "1.0.0");
        }
        other => panic!("expected Install, got {other:?}"),
    }

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
        source: None,
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
        source: None,
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
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "ghost skill already absent → no events");

    cleanup(&root);
}

#[test]
fn snapshot_mixed_batch_generates_correct_events() {
    let root = temp_root("snapshot-mixed");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    // install-me is not local (desired → Install)
    seed_skill_with_fs(&state, "deactivate-me", "1.0.0", false); // active → Deactivate
    seed_skill_with_fs(&state, "purge-me", "1.0.0", false); // present → Purge
    seed_skill_with_fs(&state, "keep-me", "2.0.0", false); // active, installed → no-op

    let installations = vec![
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "install-me".to_string(),
            version: "1.0.0".to_string(),
            state: "desired".to_string(),
            source: None,
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "deactivate-me".to_string(),
            version: "1.0.0".to_string(),
            state: "deactivated".to_string(),
            source: None,
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "purge-me".to_string(),
            version: "1.0.0".to_string(),
            state: "removed".to_string(),
            source: None,
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "keep-me".to_string(),
            version: "2.0.0".to_string(),
            state: "installed".to_string(),
            source: None,
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
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
            ..
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
    seed_skill_with_fs(&state, "already-here", "1.0.0", false);

    let installations = vec![InstallationRecord {
        installation_id: install_id(),
        skill_id: "already-here".to_string(),
        version: "1.0.0".to_string(),
        state: "installed".to_string(),
        source: None,
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
            source: None,
        },
        InstallationRecord {
            installation_id: install_id(),
            skill_id: "empty-row".to_string(),
            version: String::new(),
            state: "desired".to_string(),
            source: None,
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
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);
    assert!(events.is_empty(), "error-state rows → no auto-retry");

    cleanup(&root);
}

// ── Phantom-artifact source propagation tests ────────────────────────────────

#[test]
fn snapshot_propagates_migrated_local_source_to_install_event() {
    // When a snapshot record carries source="migrated:local", the derived
    // Install event must carry the same value so the do_install backstop can
    // use the explicit signal rather than falling back to the version sentinel.
    let root = temp_root("snapshot-migrated-local-source");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    // No local skill present — should derive an Install event.

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "handoff".to_string(),
        version: "0.0.0".to_string(),
        state: "installed".to_string(),
        source: Some("migrated:local".to_string()),
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1, "should derive one Install event");
    match &events[0] {
        SyncEvent::Install {
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
                "source must be propagated from InstallationRecord to Install event"
            );
        }
        other => panic!("expected Install, got {other:?}"),
    }

    cleanup(&root);
}

#[test]
fn snapshot_propagates_none_source_when_backend_omits_it() {
    // Old backends do not include source in snapshot records.  The Install
    // event must carry source=None so the backstop falls back to the version
    // sentinel / marker checks.
    let root = temp_root("snapshot-none-source");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    let iid = install_id();
    let installations = vec![InstallationRecord {
        installation_id: iid,
        skill_id: "normal-skill".to_string(),
        version: "1.0.0".to_string(),
        state: "desired".to_string(),
        source: None,
    }];

    let events = build_derived_events_blocking(installations, &state);

    assert_eq!(events.len(), 1);
    match &events[0] {
        SyncEvent::Install { source, .. } => {
            assert!(
                source.is_none(),
                "source must be None when backend omits it"
            );
        }
        other => panic!("expected Install, got {other:?}"),
    }

    cleanup(&root);
}

// ── Per-skill mutex tests ─────────────────────────────────────────────────────

#[test]
fn skill_lock_returns_same_mutex_for_same_id() {
    let map: SkillLockMap =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let a = skill_lock(&map, "hello-world");
    let b = skill_lock(&map, "hello-world");
    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "same skill_id must yield the same Arc<Mutex>",
    );
}

#[test]
fn skill_lock_returns_distinct_mutex_for_different_ids() {
    let map: SkillLockMap =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let a = skill_lock(&map, "alpha");
    let b = skill_lock(&map, "beta");
    assert!(
        !std::sync::Arc::ptr_eq(&a, &b),
        "different skill_ids must yield different mutexes",
    );
}

#[tokio::test]
async fn skill_lock_serializes_same_skill_fifo() {
    // Two tasks racing for the same skill's mutex must run sequentially in
    // submission order, not interleave. This is the property that prevents
    // the install/deactivate race the snapshot cross-check used to catch
    // reactively.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{sleep, Duration};

    let map: SkillLockMap =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let interleave = std::sync::Arc::new(AtomicUsize::new(0));
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

    let lock1 = skill_lock(&map, "shared");
    let lock2 = skill_lock(&map, "shared");
    let i1 = interleave.clone();
    let i2 = interleave.clone();
    let o1 = observed.clone();
    let o2 = observed.clone();

    let t1 = tokio::spawn(async move {
        let _g = lock1.lock_owned().await;
        // Bump entry counter; if anyone else entered concurrently we'd see 2.
        let in_count = i1.fetch_add(1, Ordering::SeqCst) + 1;
        o1.lock().unwrap().push("t1-start");
        assert_eq!(in_count, 1, "t1 must hold the lock exclusively");
        sleep(Duration::from_millis(40)).await;
        i1.fetch_sub(1, Ordering::SeqCst);
        o1.lock().unwrap().push("t1-end");
    });

    // Submit t2 a moment later so the FIFO ordering is unambiguous.
    sleep(Duration::from_millis(5)).await;

    let t2 = tokio::spawn(async move {
        let _g = lock2.lock_owned().await;
        let in_count = i2.fetch_add(1, Ordering::SeqCst) + 1;
        o2.lock().unwrap().push("t2-start");
        assert_eq!(in_count, 1, "t2 must hold the lock exclusively");
        sleep(Duration::from_millis(5)).await;
        i2.fetch_sub(1, Ordering::SeqCst);
        o2.lock().unwrap().push("t2-end");
    });

    t1.await.unwrap();
    t2.await.unwrap();

    let seen = observed.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["t1-start", "t1-end", "t2-start", "t2-end"],
        "same-skill tasks must execute end-to-end in submission order",
    );
}

#[tokio::test]
async fn skill_lock_does_not_serialize_different_skills() {
    use tokio::time::{sleep, Duration};

    let map: SkillLockMap =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let alpha = skill_lock(&map, "alpha");
    let beta = skill_lock(&map, "beta");

    let start = std::time::Instant::now();

    let t1 = tokio::spawn(async move {
        let _g = alpha.lock_owned().await;
        sleep(Duration::from_millis(50)).await;
    });
    let t2 = tokio::spawn(async move {
        let _g = beta.lock_owned().await;
        sleep(Duration::from_millis(50)).await;
    });

    t1.await.unwrap();
    t2.await.unwrap();

    let elapsed = start.elapsed();
    // If they were serialized this would be ~100ms; running in parallel
    // it lands well under 90ms even on a busy CI box.
    assert!(
        elapsed < Duration::from_millis(90),
        "different-skill locks should not serialize (took {elapsed:?})",
    );
}

// ── marker_present_for_slug short-circuit (v1.0.53) ──────────────────────────

/// `marker_present_for_slug` returns true when a `kind='skill'` row exists for
/// the slug, false otherwise. Used by `do_install` to short-circuit the
/// download step for skills that were imported by F1 (no artifact in the
/// registry — trying to download flaps the row into `error`).
#[tokio::test]
async fn marker_present_returns_true_when_f2_marker_exists() {
    use std::sync::Arc;
    let root = temp_root("marker-present");
    let state = Arc::new(AppState::bootstrap_in(root.clone()).unwrap());

    // Initially absent.
    assert!(!super::marker_present_for_slug(&state, "demo").await);

    // Seed a marker row directly.
    let conn = Connection::open(&state.db_path).unwrap();
    conn.execute(
        "INSERT INTO managed_path_markers (path, kind, slug, source_sha256, migrated_at) \
         VALUES ('/x/demo', 'skill', 'demo', 'abc', 't')",
        [],
    )
    .unwrap();

    assert!(super::marker_present_for_slug(&state, "demo").await);
    // Other slugs still absent.
    assert!(!super::marker_present_for_slug(&state, "other").await);
    // Wrong kind doesn't match.
    conn.execute(
        "INSERT INTO managed_path_markers (path, kind, slug, source_sha256, migrated_at) \
         VALUES ('/x/mcp-slug', 'mcp', 'mcp-only', 'def', 't')",
        [],
    )
    .unwrap();
    assert!(!super::marker_present_for_slug(&state, "mcp-only").await);

    cleanup(&root);
}

// ── Bug A regression: F2 (`~/.claude/skills/<id>`) projection cleanup on
// Purge / per-device kill-switch Deactivate ───────────────────────────────────
//
// `purge_skill_blocking` and `handle_deactivate` already invoke
// `ManagedPathsPusher::remove_skill` (wired in since v1.0.51, commit
// 896dafb) — but until now nothing exercised that wiring above the
// pusher-unit level (`managed_paths/pusher_tests.rs` tests `remove_skill`
// directly, never through the reconciler handlers that are the documented
// call site). These tests close that gap end-to-end and pin the required
// invariant: the F2 projection comes down on Purge/Deactivate regardless of
// `managed_paths_mode` — that KV only gates F3's drift scanner
// (`managed_paths/drift.rs`), never F2's `remove_skill`, so "warn" must not
// suppress removal.

/// Set `HOME` to a fresh temp dir for the duration of `f`, holding the
/// crate-wide `ENV_MUTEX` so parallel tests never race on the env var, and
/// always restoring the previous value afterward even if `f` panics.
fn with_fake_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(fake_home.path())));

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    match result {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

#[test]
fn purge_skill_blocking_removes_f2_claude_skills_projection() {
    let root = temp_root("purge-f2-removes");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    {
        let conn = Connection::open(&state.db_path).unwrap();
        seed_skill(&conn, "demo", "1.0.0", false);
    }

    with_fake_home(|home| {
        let pusher = crate::managed_paths::ManagedPathsPusher::new(&state);
        pusher
            .push_skill("demo", Some("inst-1"), b"---\nname: demo\n---\n", &[])
            .unwrap();

        let canonical = home.join(".agents/skills/demo");
        let link = home.join(".claude/skills/demo");
        assert!(
            canonical.is_dir(),
            "precondition: F2 projection must exist before purge"
        );
        assert!(
            link.is_symlink(),
            "precondition: Claude Code link must exist before purge"
        );

        super::purge_skill_blocking(&state, "demo").expect("purge_skill_blocking should succeed");

        assert!(
            !canonical.exists(),
            "BUG: Purge left the F2 skill dir orphaned at ~/.agents/skills/demo"
        );
        assert!(
            !link.exists() && !link.is_symlink(),
            "BUG: Purge left the ~/.claude/skills/demo link orphaned"
        );
    });

    cleanup(&root);
}

#[test]
fn purge_skill_blocking_removes_f2_projection_even_when_mode_is_warn() {
    let root = temp_root("purge-f2-warn-mode");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    {
        let conn = Connection::open(&state.db_path).unwrap();
        seed_skill(&conn, "demo", "1.0.0", false);
    }
    // F4's org-level policy mode — must not gate F2 removal on an explicit
    // security action (Purge). Only F3's drift scanner reads this key.
    state.set_sync_state("managed_paths_mode", "warn").unwrap();

    with_fake_home(|home| {
        let pusher = crate::managed_paths::ManagedPathsPusher::new(&state);
        pusher
            .push_skill("demo", Some("inst-1"), b"---\nname: demo\n---\n", &[])
            .unwrap();

        let canonical = home.join(".agents/skills/demo");
        let link = home.join(".claude/skills/demo");

        super::purge_skill_blocking(&state, "demo").expect("purge_skill_blocking should succeed");

        assert!(
            !canonical.exists(),
            "BUG: managed_paths_mode=warn suppressed F2 removal during Purge"
        );
        assert!(
            !link.exists() && !link.is_symlink(),
            "BUG: managed_paths_mode=warn left the Claude Code link in place after Purge"
        );
    });

    cleanup(&root);
}

#[test]
fn purge_skill_blocking_does_not_touch_foreign_unmanaged_projection() {
    let root = temp_root("purge-f2-foreign-untouched");
    let state = AppState::bootstrap_in(root.clone()).unwrap();

    with_fake_home(|home| {
        // A real, unmanaged directory at the canonical path — e.g. left by
        // `npx skills`, Cursor, or Codex — never carrying VectorHawk's
        // `.vectorhawk-managed.json` marker. Purge must never touch it.
        let agents_skills = home.join(".agents").join("skills");
        std::fs::create_dir_all(&agents_skills).unwrap();
        let foreign = agents_skills.join("demo");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("SKILL.md"), b"the user's own SKILL.md").unwrap();

        super::purge_skill_blocking(&state, "demo").expect(
            "purge_skill_blocking must not fail just because F2 refused to remove a foreign dir",
        );

        assert!(
            foreign.is_dir(),
            "BUG: Purge deleted a directory VectorHawk never marked as managed"
        );
        assert_eq!(
            std::fs::read(foreign.join("SKILL.md")).ok().as_deref(),
            Some(b"the user's own SKILL.md".as_ref()),
            "BUG: Purge modified content inside a foreign, unmanaged directory"
        );
    });

    cleanup(&root);
}

/// `handle_deactivate` is async and must hold `HOME` steady across its one
/// `.await` (the blocking `deactivate_skill_blocking` task plus the PATCH
/// status report), so — unlike the sync Purge tests above — this can't go
/// through the sync `with_fake_home` helper. `#[tokio::test]` defaults to a
/// current-thread runtime and this test has no other concurrent task, so
/// holding the std `ENV_MUTEX` across the single `.await` below cannot
/// deadlock or block a worker pool; same justification already used in
/// `managed_paths/adopt_publish_tests.rs`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handle_deactivate_removes_f2_claude_skills_projection() {
    use std::sync::Arc;

    let _guard = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let root = temp_root("deactivate-f2-removes");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    {
        let conn = Connection::open(&state.db_path).unwrap();
        seed_skill(&conn, "demo", "1.0.0", false);
    }
    let state_arc = Arc::new(state.clone());

    let pusher = crate::managed_paths::ManagedPathsPusher::new(&state);
    pusher
        .push_skill("demo", Some("inst-1"), b"---\nname: demo\n---\n", &[])
        .unwrap();

    let canonical = fake_home.path().join(".agents/skills/demo");
    let link = fake_home.path().join(".claude/skills/demo");
    assert!(
        canonical.is_dir(),
        "precondition: F2 projection must exist before deactivate"
    );

    let mut mock_server = mockito::Server::new_async().await;
    let iid = install_id();
    let url_path = format!("/api/installations/{iid}");
    let _m = mock_server
        .mock("PATCH", url_path.as_str())
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    let changed =
        super::handle_deactivate(iid, "demo", &state_arc, &mock_server.url(), Some(&pusher)).await;

    let canonical_gone = !canonical.exists();
    let link_gone = !link.exists() && !link.is_symlink();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        changed,
        "handle_deactivate should report a tool-list change on success"
    );
    assert!(
        canonical_gone,
        "BUG: per-device kill-switch Deactivate left the F2 skill dir orphaned"
    );
    assert!(
        link_gone,
        "BUG: per-device kill-switch Deactivate left the Claude Code link orphaned"
    );

    cleanup(&root);
}

/// See `handle_deactivate_removes_f2_claude_skills_projection` above for the
/// `await_holding_lock` justification.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handle_deactivate_removes_f2_projection_even_when_mode_is_warn() {
    use std::sync::Arc;

    let _guard = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let root = temp_root("deactivate-f2-warn-mode");
    let state = AppState::bootstrap_in(root.clone()).unwrap();
    {
        let conn = Connection::open(&state.db_path).unwrap();
        seed_skill(&conn, "demo", "1.0.0", false);
    }
    // F4's org-level policy mode — must not gate F2 removal on an explicit
    // security action (the per-device kill switch). Only F3's drift scanner
    // reads this key.
    state.set_sync_state("managed_paths_mode", "warn").unwrap();
    let state_arc = Arc::new(state.clone());

    let pusher = crate::managed_paths::ManagedPathsPusher::new(&state);
    pusher
        .push_skill("demo", Some("inst-1"), b"---\nname: demo\n---\n", &[])
        .unwrap();

    let canonical = fake_home.path().join(".agents/skills/demo");
    let link = fake_home.path().join(".claude/skills/demo");

    let mut mock_server = mockito::Server::new_async().await;
    let iid = install_id();
    let url_path = format!("/api/installations/{iid}");
    let _m = mock_server
        .mock("PATCH", url_path.as_str())
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    let changed =
        super::handle_deactivate(iid, "demo", &state_arc, &mock_server.url(), Some(&pusher)).await;

    let canonical_gone = !canonical.exists();
    let link_gone = !link.exists() && !link.is_symlink();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        changed,
        "handle_deactivate should report a tool-list change on success"
    );
    assert!(
        canonical_gone,
        "BUG: managed_paths_mode=warn suppressed F2 removal during the per-device kill-switch Deactivate"
    );
    assert!(
        link_gone,
        "BUG: managed_paths_mode=warn left the Claude Code link in place after Deactivate"
    );

    cleanup(&root);
}
