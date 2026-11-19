#![allow(clippy::unwrap_used)]

// Smoke test: reconciler builds without panicking.
use super::*;
use scenery_core::state::AppState;

#[test]
fn managed_paths_reconciler_new_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = Arc::new(AppState::bootstrap_in(root).unwrap());
    let reconciler = ManagedPathsReconciler::new(state, "https://example.com".to_string());
    assert!(reconciler.is_ok());
}

/// Regression (board card "legacy startup migrator re-adopts tracked/auto-managed
/// skill after restart"): the startup migration must never touch a skill under
/// `~/.claude/skills`, whether Scenery tracks it (auto-managed in place, marker
/// row) or not. Skills belong to the auto-manage converter. A "restart" is a
/// second `migrate_existing` run: no backup, no POST to `/migrate`, no marker
/// row added, the skill bytes untouched.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn startup_migration_leaves_tracked_and_auto_managed_skills_alone_across_restarts() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let root = camino::Utf8PathBuf::from_path_buf(home.path().join("state")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let state = Arc::new(AppState::bootstrap_in(root).unwrap());

    let skills = home.path().join(".claude/skills");
    let mk = |slug: &str, body: &str| {
        let d = skills.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
        d
    };
    // `scntest-a5`: auto-managed in place (a marker row keys on its real dir).
    let tracked = mk("scntest-a5", "---\nname: scntest-a5\n---\nbody");
    {
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        marker::insert_db_marker(
            &conn,
            &marker::ManagedPathMarker {
                path: tracked.to_string_lossy().to_string(),
                kind: "skill".to_string(),
                slug: "scntest-a5".to_string(),
                installation_id: None,
                source_sha256: "abc".to_string(),
                migrated_at: "2026-01-01T00:00:00Z".to_string(),
            },
        )
        .unwrap();
    }
    // A skill nothing tracks yet: auto-manage's job, not the migrator's.
    let loose = mk("loose", "---\nname: loose\n---\nbody");

    let mut server = mockito::Server::new_async().await;
    let migrate = server
        .mock("POST", "/portal/managed-paths/migrate")
        .expect(0)
        .create_async()
        .await;

    let rows = |state: &AppState| -> i64 {
        rusqlite::Connection::open(&state.db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM managed_path_markers", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let before = rows(&state);
    let tracked_bytes = std::fs::read(tracked.join("SKILL.md")).unwrap();
    let loose_bytes = std::fs::read(loose.join("SKILL.md")).unwrap();

    for _restart in 0..2 {
        let reconciler = ManagedPathsReconciler::new(state.clone(), server.url()).unwrap();
        let report = reconciler.migrate_existing().await.unwrap();
        assert!(report.errors.is_empty());
    }

    migrate.assert_async().await;
    assert_eq!(rows(&state), before, "no marker row added or duplicated");
    assert!(
        !home.path().join(".claude/.scenery-backup").exists(),
        "no backup run directory is created for skills"
    );
    assert_eq!(
        std::fs::read(tracked.join("SKILL.md")).unwrap(),
        tracked_bytes
    );
    assert_eq!(std::fs::read(loose.join("SKILL.md")).unwrap(), loose_bytes);
    assert!(!tracked.join(".scenery-managed.json").exists());
    assert!(!loose.join(".scenery-managed.json").exists());

    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}
