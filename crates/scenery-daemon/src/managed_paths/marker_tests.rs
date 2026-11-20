#![allow(clippy::unwrap_used)]

use super::*;
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_db() -> (Connection, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_path_markers (
            path TEXT NOT NULL,
            kind TEXT NOT NULL,
            slug TEXT NOT NULL,
            installation_id TEXT,
            source_sha256 TEXT NOT NULL,
            migrated_at TEXT NOT NULL,
            PRIMARY KEY (path)
        );
        CREATE TABLE IF NOT EXISTS skill_link_copies (
            path TEXT PRIMARY KEY, canonical TEXT NOT NULL,
            recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .unwrap();
    (conn, dir)
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn write_and_read_file_marker_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let hash = format!("abc{}", nanos());
    write_file_marker(
        dir.path(),
        Some("install-123"),
        &hash,
        "2026-05-24T00:00:00Z",
    )
    .unwrap();
    let marker = read_file_marker(dir.path()).unwrap().unwrap();
    assert_eq!(marker.marker_version, MARKER_FILE_VERSION);
    assert_eq!(marker.installation_id.as_deref(), Some("install-123"));
    assert_eq!(marker.source_sha256, hash);
    assert_eq!(marker.migrated_at, "2026-05-24T00:00:00Z");
}

#[test]
fn read_file_marker_returns_none_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let result = read_file_marker(dir.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn read_file_marker_tolerates_malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let marker_path = dir.path().join(".scenery-managed.json");
    std::fs::write(&marker_path, b"{not valid json{{").unwrap();
    // Should return None without propagating an error.
    let result = read_file_marker(dir.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn write_and_read_file_marker_version_mismatch_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    // Write a marker with an unexpected version — should still deserialise.
    let content = r#"{"marker_version":99,"installation_id":null,"source_sha256":"aabbcc","migrated_at":"2026-01-01T00:00:00Z"}"#;
    std::fs::write(dir.path().join(".scenery-managed.json"), content).unwrap();
    let marker = read_file_marker(dir.path()).unwrap().unwrap();
    assert_eq!(marker.marker_version, 99);
}

#[test]
fn db_marker_insert_and_is_already_marked() {
    let (conn, _dir) = temp_db();
    let marker = ManagedPathMarker {
        path: "/home/user/.claude/skills/my-skill".to_string(),
        kind: "skill".to_string(),
        slug: "my-skill".to_string(),
        installation_id: Some("inst-abc".to_string()),
        source_sha256: "deadbeef".to_string(),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    insert_db_marker(&conn, &marker).unwrap();
    assert!(is_already_marked(&conn, "/home/user/.claude/skills/my-skill").unwrap());
    assert!(!is_already_marked(&conn, "/home/user/.claude/skills/other").unwrap());
}

#[test]
fn db_marker_insert_is_idempotent() {
    let (conn, _dir) = temp_db();
    let marker = ManagedPathMarker {
        path: "/home/user/.claude.json:my-mcp".to_string(),
        kind: "mcp".to_string(),
        slug: "my-mcp".to_string(),
        installation_id: None,
        source_sha256: "cafebabe".to_string(),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    insert_db_marker(&conn, &marker).unwrap();
    // Second call should not fail (INSERT OR IGNORE).
    insert_db_marker(&conn, &marker).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM managed_path_markers", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn db_marker_update_installation_id() {
    let (conn, _dir) = temp_db();
    let marker = ManagedPathMarker {
        path: "/tmp/test-path".to_string(),
        kind: "skill".to_string(),
        slug: "test".to_string(),
        installation_id: None,
        source_sha256: "1234".to_string(),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    insert_db_marker(&conn, &marker).unwrap();
    update_db_marker_installation_id(&conn, "/tmp/test-path", "new-inst-id").unwrap();
    let stored: Option<String> = conn
        .query_row(
            "SELECT installation_id FROM managed_path_markers WHERE path = ?1",
            ["/tmp/test-path"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored.as_deref(), Some("new-inst-id"));
}

// ── db_owns_path / db_owns_link_copy (SEC1 follow-up) ─────────────────────────

fn skill_row(conn: &Connection, dir: &std::path::Path, hash: &str, inst: Option<&str>) {
    insert_db_marker(
        conn,
        &ManagedPathMarker {
            path: dir.to_string_lossy().to_string(),
            kind: "skill".into(),
            slug: "demo".into(),
            installation_id: inst.map(str::to_string),
            source_sha256: hash.into(),
            migrated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
}

#[test]
fn db_owns_path_needs_a_non_legacy_row_and_ignores_the_marker_file() {
    let (conn, dir) = temp_db();
    let skill = dir.path().join("demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), b"x").unwrap();
    std::fs::write(skill.join(".scenery-managed.json"), b"{}").unwrap();
    // Forged marker file, no row.
    assert!(!db_owns_path(&conn, "skill", &skill));
    // Legacy row (no installation id, no provenance): proves nothing.
    skill_row(&conn, &skill, "old", None);
    assert!(!db_owns_path(&conn, "skill", &skill));
    conn.execute("DELETE FROM managed_path_markers", [])
        .unwrap();
    // A catalog row with an installation id owns the path, even edited.
    skill_row(&conn, &skill, "old", Some("inst-1"));
    assert!(db_owns_path(&conn, "skill", &skill));
}

#[test]
fn db_owns_link_copy_requires_an_owned_canonical_and_a_matching_copy() {
    let (conn, dir) = temp_db();
    let canonical = dir.path().join("agents/demo");
    let link = dir.path().join("claude/demo");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::create_dir_all(&link).unwrap();
    std::fs::write(canonical.join("SKILL.md"), b"v1").unwrap();
    std::fs::write(link.join("SKILL.md"), b"v1").unwrap();
    // No row for canonical.
    assert!(!db_owns_link_copy(&conn, &canonical, &link));
    skill_row(
        &conn,
        &canonical,
        &crate::managed_paths::drift::hex_sha256(b"v1"),
        Some("inst-1"),
    );
    assert!(db_owns_link_copy(&conn, &canonical, &link));
    // User content under the same slug is not a copy of ours.
    std::fs::write(link.join("SKILL.md"), b"mine").unwrap();
    assert!(!db_owns_link_copy(&conn, &canonical, &link));
}
