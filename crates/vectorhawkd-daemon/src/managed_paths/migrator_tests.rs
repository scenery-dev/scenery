#![allow(clippy::unwrap_used)]

use super::*;
use std::fs;
use vectorhawkd_core::state::AppState;

/// Bootstrap a minimal AppState (with the F1 managed_path_markers table) in a
/// temp dir and return it alongside the TempDir guard so it is not dropped.
fn temp_state() -> (AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

fn skill_item(slug: &str, dir: &std::path::Path) -> MigrationItem {
    let source_path = dir.join(slug);
    fs::create_dir_all(&source_path).unwrap();
    let skill_md = source_path.join("SKILL.md");
    fs::write(&skill_md, "---\nname: test\n---\n").unwrap();
    MigrationItem {
        kind: ItemKind::Skill,
        slug: slug.to_string(),
        source_path: source_path.clone(),
        files: vec![skill_md],
        canonical_hash: "deadbeef1234".to_string(),
        payload: serde_json::json!({"skill_md": "---\nname: test\n---\n"}),
    }
}

// ── Backup tests ──────────────────────────────────────────────────────────────

#[test]
fn backup_skill_item_copies_files() {
    let tmp = tempfile::tempdir().unwrap();
    let item = skill_item("backup-skill", tmp.path());
    let backup_root = tmp.path().join("backup");

    backup_item(&item, &backup_root).unwrap();

    let backed_up_skill_md = backup_root
        .join("skills")
        .join("backup-skill")
        .join("SKILL.md");
    assert!(backed_up_skill_md.exists(), "SKILL.md should be backed up");
}

#[test]
fn backup_mcp_item_copies_claude_json() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_json = tmp.path().join(".claude.json");
    fs::write(&claude_json, r#"{"mcpServers":{"my-mcp":{}}}"#).unwrap();

    let item = MigrationItem {
        kind: ItemKind::Mcp,
        slug: "my-mcp".to_string(),
        source_path: std::path::PathBuf::from(format!("{}:my-mcp", claude_json.display())),
        files: vec![claude_json.clone()],
        canonical_hash: "aabb".to_string(),
        payload: serde_json::json!({"mcp_config": {}}),
    };

    let backup_root = tmp.path().join("backup");
    backup_item(&item, &backup_root).unwrap();

    assert!(backup_root.join("claude.json").exists());
}

#[test]
fn backup_mcp_item_idempotent_for_claude_json() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_json = tmp.path().join(".claude.json");
    let original_content = r#"{"mcpServers":{"a":{},"b":{}}}"#;
    fs::write(&claude_json, original_content).unwrap();

    let backup_root = tmp.path().join("backup");

    for slug in ["a", "b"] {
        let item = MigrationItem {
            kind: ItemKind::Mcp,
            slug: slug.to_string(),
            source_path: std::path::PathBuf::from(format!("{}:{slug}", claude_json.display())),
            files: vec![claude_json.clone()],
            canonical_hash: "xx".to_string(),
            payload: serde_json::json!({}),
        };
        backup_item(&item, &backup_root).unwrap();
    }

    // File should exist exactly once, with the original content.
    let backed_up = fs::read_to_string(backup_root.join("claude.json")).unwrap();
    assert_eq!(backed_up, original_content);
}

// ── Idempotency via SQLite ────────────────────────────────────────────────────

#[tokio::test]
async fn migrate_item_idempotent_second_run_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = skill_item("idem-skill", tmp.path());
    let backup_root = tmp.path().join("backup");

    // Pre-seed the marker so the first call is also a no-op for this test.
    {
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let marker = crate::managed_paths::marker::ManagedPathMarker {
            path: item.source_path.to_string_lossy().to_string(),
            kind: "skill".to_string(),
            slug: item.slug.clone(),
            installation_id: None,
            source_sha256: item.canonical_hash.clone(),
            migrated_at: "2026-05-24T00:00:00Z".to_string(),
        };
        crate::managed_paths::marker::insert_db_marker(&conn, &marker).unwrap();
    }

    // Neither call should return Err or count as a migration.
    let client = reqwest::Client::new();
    let result = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(!result, "already-marked item should return false");
}

// ── Audit event ───────────────────────────────────────────────────────────────

#[test]
fn buffer_audit_event_writes_row() {
    let tmp = tempfile::tempdir().unwrap();
    let item = skill_item("audit-skill", tmp.path());
    let conn = rusqlite::Connection::open(tmp.path().join("test.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            uploaded INTEGER NOT NULL DEFAULT 0
        )",
    )
    .unwrap();

    buffer_audit_event(&conn, &item, Some("inst-xyz")).unwrap();

    let event_type: String = conn
        .query_row("SELECT event_type FROM audit_events LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(event_type, "managed_path_migrated");
}

// ── copy_dir_recursive ────────────────────────────────────────────────────────

#[test]
fn copy_dir_recursive_copies_nested_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), "hello").unwrap();
    fs::write(src.join("sub").join("b.txt"), "world").unwrap();

    let dest = tmp.path().join("dest");
    copy_dir_recursive(&src, &dest).unwrap();

    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "hello");
    assert_eq!(
        fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
        "world"
    );
}
