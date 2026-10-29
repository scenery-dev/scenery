use super::*;
use crate::managed_paths::marker::{insert_db_marker, write_file_marker, ManagedPathMarker};
use scenery_core::restore_journal::{JournalEntry, JournalOp, JournalSource};
use std::fs;

fn temp_state() -> (AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

fn marketplaces(plugins: &Path) -> Map<String, Value> {
    let mut m = Map::new();
    for (name, repo) in [
        (
            "claude-plugins-official",
            "anthropics/claude-plugins-official",
        ),
        ("acme-mp", "acme/acme-mp"),
    ] {
        m.insert(
            name.to_string(),
            serde_json::json!({
                "source": {"source": "github", "repo": repo},
                "installLocation": plugins.join("marketplaces").join(name),
            }),
        );
    }
    m
}

/// Seed a plugin dir with a manifest, our sidecar, a marker row and a journal entry.
fn adopt(state: &AppState, dir: &Path, slug: &str) {
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::write(
        dir.join(".claude-plugin/plugin.json"),
        format!(r#"{{"name":"{slug}"}}"#),
    )
    .unwrap();
    write_file_marker(dir, None, "hash-1", "2026-01-01T00:00:00Z").unwrap();
    let conn = Connection::open(state.db_path.as_std_path()).unwrap();
    insert_db_marker(
        &conn,
        &ManagedPathMarker {
            path: dir.to_string_lossy().to_string(),
            kind: "plugin".into(),
            slug: slug.into(),
            installation_id: None,
            source_sha256: "hash-1".into(),
            migrated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    RestoreJournal::for_state(state)
        .append(
            JournalEntry::new(
                JournalOp::FileReplace,
                JournalSource::Native,
                dir.to_string_lossy(),
            )
            .with_slug(slug)
            .with_detail(serde_json::json!({"canonical_hash": "hash-1"})),
        )
        .unwrap();
}

fn row_count(state: &AppState) -> i64 {
    Connection::open(state.db_path.as_std_path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM managed_path_markers", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn cleanup_removes_our_sidecar_row_and_journal_entry_and_leaves_files_untouched() {
    let (state, tmp) = temp_state();
    let plugins = tmp.path().join("plugins");
    let official = plugins.join("marketplaces/claude-plugins-official/external_plugins/github");
    let custom = plugins.join("marketplaces/acme-mp/plugins/acme-tool");
    adopt(&state, &official, "github");
    adopt(&state, &custom, "acme-tool");
    // An unrelated file in the marketplace must not be touched.
    let manifest = official.join(".claude-plugin/plugin.json");
    let manifest_before = fs::read(&manifest).unwrap();

    let mkts = marketplaces(&plugins);
    let report = cleanup_native_markers_in(&state, &plugins, Some(&mkts)).unwrap();
    assert_eq!(
        report,
        CleanupReport {
            sidecars_removed: 1,
            marker_rows_removed: 1,
            journal_entries_removed: 1
        }
    );

    assert!(!official.join(MANAGED_MARKER_FILENAME).exists());
    assert_eq!(fs::read(&manifest).unwrap(), manifest_before);
    // Non-official adoption is untouched.
    assert!(custom.join(MANAGED_MARKER_FILENAME).exists());
    assert_eq!(row_count(&state), 1);
    let journal = RestoreJournal::for_state(&state).read_all().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].slug.as_deref(), Some("acme-tool"));

    // Idempotent.
    let again = cleanup_native_markers_in(&state, &plugins, Some(&mkts)).unwrap();
    assert!(again.is_noop());
}

#[test]
fn cleanup_leaves_a_sidecar_that_is_not_ours() {
    let (state, tmp) = temp_state();
    let plugins = tmp.path().join("plugins");
    let official = plugins.join("marketplaces/claude-plugins-official/external_plugins/github");
    adopt(&state, &official, "github");
    // Replace our sidecar with one the plugin shipped itself.
    fs::write(official.join(MANAGED_MARKER_FILENAME), "{}").unwrap();

    let mkts = marketplaces(&plugins);
    let report = cleanup_native_markers_in(&state, &plugins, Some(&mkts)).unwrap();
    assert_eq!(report.sidecars_removed, 0);
    assert_eq!(report.marker_rows_removed, 1);
    assert_eq!(
        fs::read_to_string(official.join(MANAGED_MARKER_FILENAME)).unwrap(),
        "{}"
    );
}

#[test]
fn cleanup_does_nothing_when_marketplaces_unreadable() {
    let (state, tmp) = temp_state();
    let plugins = tmp.path().join("plugins");
    let official = plugins.join("marketplaces/claude-plugins-official/external_plugins/github");
    adopt(&state, &official, "github");
    let report = cleanup_native_markers_in(&state, &plugins, None).unwrap();
    assert!(report.is_noop());
    assert_eq!(row_count(&state), 1);
}
