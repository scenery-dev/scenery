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

// ── Replace-with-managed: adoption marks the item as VectorHawk-managed ─────────

/// B3: adopting a custom skill takes local ownership — it writes the
/// `.vectorhawk-managed.json` marker in place (the "managed copy") and a DB
/// marker row, so the drift reconciler now governs it. Runs offline: with no
/// auth token the backend POST is skipped, but ownership is still taken.
#[tokio::test]
async fn migrate_item_marks_adopted_skill_as_managed() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = skill_item("adopt-me", tmp.path());
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(migrated, "a fresh custom skill should be newly adopted");

    // Local ownership marker written in place (replace-with-managed).
    let marker = item
        .source_path
        .join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME);
    assert!(
        marker.exists(),
        "adopted skill must carry the managed marker"
    );
    assert!(
        vectorhawkd_mcp::ownership::is_vectorhawk_managed(&item.source_path),
        "adopted skill dir must classify as VectorHawk-managed"
    );

    // DB marker row present (authoritative idempotency key).
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    assert!(
        crate::managed_paths::marker::is_already_marked(
            &conn,
            &item.source_path.to_string_lossy(),
        )
        .unwrap(),
        "adopted skill must have a managed_path_markers row"
    );
}

/// Pins the fix for the self-adoption bug (step "0.5" in `migrate_item`):
/// a `MigrationItem` whose `source_path` already carries
/// `.vectorhawk-managed.json` must never be adopted — no backup POST, no DB
/// marker row, and `migrate_item` must return `Ok(false)` rather than
/// `Ok(true)`. Without this check the idempotency check in step 1 is not
/// sufficient, because since the `.agents` pivot the DB marker is keyed on
/// the canonical path while the scanner can hand back a different (symlink)
/// `source_path` for the same content.
#[tokio::test]
async fn migrate_item_does_not_adopt_vectorhawk_managed_source() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = skill_item("already-managed", tmp.path());
    fs::write(
        item.source_path
            .join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        r#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let result = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();

    assert!(
        !result,
        "an already-managed source_path must never be (re)adopted"
    );

    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    assert!(
        !crate::managed_paths::marker::is_already_marked(
            &conn,
            &item.source_path.to_string_lossy(),
        )
        .unwrap(),
        "no managed_path_markers row should be inserted for already-managed content"
    );
}

// ── Restore journal (ONE ledger) ────────────────────────────────────────────

/// F1 takeovers must also land in the unified restore journal, source=native,
/// pointing at the SAME backup the legacy `.vectorhawk-backup/` manifest uses
/// — not a second copy.
#[tokio::test]
async fn migrate_item_appends_native_restore_journal_entry_for_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = skill_item("journal-skill", tmp.path());
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(migrated);

    let journal = vectorhawkd_core::restore_journal::RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "one restore-journal entry per migrated item"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.op,
        vectorhawkd_core::restore_journal::JournalOp::FileReplace
    );
    assert_eq!(
        entry.source,
        vectorhawkd_core::restore_journal::JournalSource::Native
    );
    assert_eq!(entry.slug.as_deref(), Some("journal-skill"));
    assert_eq!(entry.target_path, item.source_path.to_string_lossy());

    let backup_path = entry
        .backup_path
        .as_ref()
        .expect("native takeover must carry a backup_path");
    assert_eq!(
        backup_path,
        &backup_root
            .join("skills")
            .join("journal-skill")
            .to_string_lossy()
            .to_string(),
        "restore journal must point at the SAME .vectorhawk-backup/ location, not a new copy"
    );
    assert!(
        std::path::Path::new(backup_path).join("SKILL.md").exists(),
        "the backup this entry points at must actually exist on disk"
    );
}

/// Same as above, for an `Mcp` item: op=config_edit, target_path is the real
/// `~/.claude.json` path (not the scanner's virtual `path:slug` key), and
/// detail carries `server_key`/`mcp_key` so a later precise removal is
/// possible without re-deriving it from `slug` alone.
#[tokio::test]
async fn migrate_item_appends_native_restore_journal_entry_for_mcp() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
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

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(migrated);

    let journal = vectorhawkd_core::restore_journal::RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(
        entry.op,
        vectorhawkd_core::restore_journal::JournalOp::ConfigEdit
    );
    assert_eq!(
        entry.source,
        vectorhawkd_core::restore_journal::JournalSource::Native
    );
    assert_eq!(
        entry.target_path,
        claude_json.to_string_lossy(),
        "target_path must be the real claude.json path, not the virtual path:slug key"
    );
    assert_eq!(entry.detail["server_key"], "my-mcp");
    assert_eq!(entry.detail["mcp_key"], "mcpServers");
    assert_eq!(
        entry.backup_path.as_deref(),
        Some(backup_root.join("claude.json").to_string_lossy().as_ref())
    );
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
