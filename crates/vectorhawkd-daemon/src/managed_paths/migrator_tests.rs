#![allow(clippy::unwrap_used)]

use super::*;
use crate::managed_paths::ENV_MUTEX;
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

// ── ~/.claude/skills → ~/.agents/skills migration ───────────────────────────

#[test]
fn migrates_managed_skill_from_claude_to_agents_and_relinks() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    // A pre-pivot managed skill: real dir under .claude/skills with a marker.
    let old = fake_home.path().join(".claude/skills/demo");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("SKILL.md"), b"---\nname: demo\n---\n").unwrap();
    std::fs::write(old.join(".vectorhawk-managed.json"), br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#).unwrap();

    // An unmanaged, user-authored skill that must be left completely alone.
    let user_skill = fake_home.path().join(".claude/skills/mine");
    std::fs::create_dir_all(&user_skill).unwrap();
    std::fs::write(user_skill.join("SKILL.md"), b"user content").unwrap();

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE managed_path_markers (
            path TEXT NOT NULL, kind TEXT NOT NULL, slug TEXT NOT NULL,
            installation_id TEXT, source_sha256 TEXT NOT NULL,
            migrated_at TEXT NOT NULL, PRIMARY KEY (path))",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_path_markers VALUES (?1,'skill','demo',NULL,'abc','2026-01-01T00:00:00Z')",
        [old.to_string_lossy().to_string()],
    )
    .unwrap();

    let result = super::migrate_skills_to_agents_dir(&conn);

    let canonical = fake_home.path().join(".agents/skills/demo");
    let new_key: String = conn
        .query_row(
            "SELECT path FROM managed_path_markers WHERE slug='demo'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(result.unwrap(), 1);
    // Content moved to canonical.
    assert!(canonical.join("SKILL.md").exists());
    assert!(!canonical.is_symlink());
    // Claude path is now a link.
    assert!(old.is_symlink());
    assert!(old.join("SKILL.md").exists());
    // SQLite now keys on the canonical path.
    assert_eq!(new_key, canonical.to_string_lossy().to_string());
    // The user's own skill is untouched and still a real directory.
    assert!(!user_skill.is_symlink());
    assert_eq!(
        std::fs::read(user_skill.join("SKILL.md")).unwrap(),
        b"user content"
    );
}

// ── Helpers for the ~/.claude/skills → ~/.agents/skills tests ────────────────

/// Restore `$HOME` after a test that mutated it.
struct HomeGuard(Option<std::ffi::OsString>);

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", path);
        HomeGuard(prev)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn markers_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE managed_path_markers (
            path TEXT NOT NULL, kind TEXT NOT NULL, slug TEXT NOT NULL,
            installation_id TEXT, source_sha256 TEXT NOT NULL,
            migrated_at TEXT NOT NULL, PRIMARY KEY (path))",
    )
    .unwrap();
    conn
}

/// Create a VectorHawk-managed skill directory at `dir`.
fn write_managed_skill(dir: &std::path::Path, slug: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {slug}\n---\n{body}"),
    )
    .unwrap();
    fs::write(
        dir.join(".vectorhawk-managed.json"),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();
}

fn insert_marker(conn: &rusqlite::Connection, path: &std::path::Path, slug: &str, sha: &str) {
    conn.execute(
        "INSERT INTO managed_path_markers VALUES (?1,'skill',?2,NULL,?3,'2026-01-01T00:00:00Z')",
        rusqlite::params![path.to_string_lossy().to_string(), slug, sha],
    )
    .unwrap();
}

fn marker_paths(conn: &rusqlite::Connection, slug: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT path FROM managed_path_markers WHERE slug = ?1 ORDER BY path")
        .unwrap();
    let rows = stmt
        .query_map([slug], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

/// Snapshot a directory tree as (relative path, kind, content) triples.
fn snapshot(root: &std::path::Path) -> Vec<(String, String)> {
    fn walk(root: &std::path::Path, rel: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(root.join(rel)) else {
            return;
        };
        for entry in entries.flatten() {
            let key = rel.join(entry.file_name());
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                out.push((
                    key.to_string_lossy().to_string(),
                    format!("link:{}", fs::read_link(&path).unwrap().display()),
                ));
            } else if meta.is_dir() {
                out.push((key.to_string_lossy().to_string(), "dir".into()));
                walk(root, &key, out);
            } else {
                out.push((
                    key.to_string_lossy().to_string(),
                    format!("file:{}", fs::read_to_string(&path).unwrap_or_default()),
                ));
            }
        }
    }
    let mut out = vec![];
    walk(root, std::path::Path::new(""), &mut out);
    out.sort();
    out
}

#[test]
fn migration_is_idempotent_with_a_real_managed_skill() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let old = fake_home.path().join(".claude/skills/demo");
    write_managed_skill(&old, "demo", "body");
    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "abc");

    let first = super::migrate_skills_to_agents_dir(&conn).unwrap();
    let after_first = snapshot(fake_home.path());
    let rows_after_first = marker_paths(&conn, "demo");

    let second = super::migrate_skills_to_agents_dir(&conn).unwrap();
    let after_second = snapshot(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");

    // First run migrates exactly one skill.
    assert_eq!(first, 1);
    assert!(canonical.join("SKILL.md").exists());
    assert!(old.is_symlink());
    assert_eq!(
        rows_after_first,
        vec![canonical.to_string_lossy().to_string()]
    );

    // Second run is a genuine no-op.
    assert_eq!(second, 0, "second run must not re-migrate");
    assert_eq!(
        after_first, after_second,
        "second run must leave disk state byte-identical (no new backup dir)"
    );
    assert!(
        !fake_home.path().join(".claude/.vectorhawk-backup").exists(),
        "an already-migrated skill must never be backed up again"
    );
    assert_eq!(
        marker_paths(&conn, "demo"),
        vec![canonical.to_string_lossy().to_string()],
        "second run must not duplicate the marker row"
    );
}

#[test]
fn migration_is_idempotent_on_empty_home() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let conn = markers_conn();
    assert_eq!(super::migrate_skills_to_agents_dir(&conn).unwrap(), 0);
    assert_eq!(super::migrate_skills_to_agents_dir(&conn).unwrap(), 0);
}

#[test]
fn migrates_several_skills_and_leaves_user_skills_alone() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let skills = fake_home.path().join(".claude/skills");
    let conn = markers_conn();

    for slug in ["alpha", "beta", "gamma"] {
        let dir = skills.join(slug);
        write_managed_skill(&dir, slug, slug);
        insert_marker(&conn, &dir, slug, "abc");
    }

    // Two unmanaged, user-authored skills — must be completely untouched.
    let mine = skills.join("mine");
    fs::create_dir_all(mine.join("nested")).unwrap();
    fs::write(mine.join("SKILL.md"), "user content").unwrap();
    fs::write(mine.join("nested/notes.md"), "notes").unwrap();
    let theirs = skills.join("theirs");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("SKILL.md"), "other user content").unwrap();

    let mine_before = snapshot(&mine);

    let migrated = super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(migrated, 3);
    for slug in ["alpha", "beta", "gamma"] {
        let canonical = fake_home.path().join(".agents/skills").join(slug);
        assert!(canonical.join("SKILL.md").exists(), "{slug} not moved");
        assert!(!canonical.is_symlink());
        assert!(skills.join(slug).is_symlink(), "{slug} not relinked");
        assert_eq!(
            marker_paths(&conn, slug),
            vec![canonical.to_string_lossy().to_string()],
            "{slug} marker not re-keyed"
        );
    }

    // User content: not moved, not linked, not backed up, no row.
    assert!(!mine.is_symlink());
    assert!(!theirs.is_symlink());
    assert_eq!(snapshot(&mine), mine_before);
    assert_eq!(
        fs::read_to_string(theirs.join("SKILL.md")).unwrap(),
        "other user content"
    );
    assert!(!fake_home.path().join(".agents/skills/mine").exists());
    assert!(!fake_home.path().join(".agents/skills/theirs").exists());
    assert!(marker_paths(&conn, "mine").is_empty());
    assert!(marker_paths(&conn, "theirs").is_empty());
    assert!(!fake_home.path().join(".claude/.vectorhawk-backup").exists());
}

/// FINDING 1 (Critical) regression: a crash between `fs::rename` and the
/// SQLite `UPDATE` leaves canonical + symlink on disk but the row still keyed
/// on the old Claude path. Before the fix, the migration loop skipped the
/// symlink entry and the row stayed stale forever.
#[test]
fn repairs_marker_stranded_by_a_crash_between_rename_and_update() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    // Post-crash disk state: content at canonical, symlink at the Claude path.
    let canonical = fake_home.path().join(".agents/skills/demo");
    write_managed_skill(&canonical, "demo", "body");
    let old = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&canonical, &old).unwrap();

    // …but the row is still keyed on the old path.
    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "abc");

    let migrated = super::migrate_skills_to_agents_dir(&conn).unwrap();

    // Nothing to move, but the row must be repaired.
    assert_eq!(migrated, 0);
    assert_eq!(
        marker_paths(&conn, "demo"),
        vec![canonical.to_string_lossy().to_string()],
        "stale row was not repaired — it would report false drift forever"
    );
    assert!(old.is_symlink(), "the link must be left alone");
    assert!(canonical.join("SKILL.md").exists());
}

/// FINDING 2: once `push_skill` has inserted a canonical row alongside the
/// stale one, `PRIMARY KEY (path)` lets both coexist and the orphan reports
/// false drift. The repair must collapse them to exactly one row — the
/// canonical one, which carries the fresher hash.
#[test]
fn repair_drops_orphan_row_when_canonical_row_already_exists() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    write_managed_skill(&canonical, "demo", "post-push body");
    let old = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&canonical, &old).unwrap();

    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "stale-pre-push-sha");
    insert_marker(&conn, &canonical, "demo", "fresh-post-push-sha");

    super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(
        marker_paths(&conn, "demo"),
        vec![canonical.to_string_lossy().to_string()],
        "duplicate rows must collapse to one"
    );
    let sha: String = conn
        .query_row(
            "SELECT source_sha256 FROM managed_path_markers WHERE slug='demo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sha, "fresh-post-push-sha", "the fresher row must survive");
}

/// FINDING 1, second interleaving: the symlink itself was deleted, so there is
/// no directory entry left to walk. A filesystem-driven repair would miss
/// this; the DB-driven pass must still fix the row.
#[test]
fn repairs_marker_when_the_claude_symlink_is_gone() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    write_managed_skill(&canonical, "demo", "body");
    let old = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(old.parent().unwrap()).unwrap();

    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "abc");

    super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(
        marker_paths(&conn, "demo"),
        vec![canonical.to_string_lossy().to_string()]
    );
}

/// FINDING A: the repair pass must not re-key a row onto a same-slug canonical
/// directory that is not ours.
///
/// A row for an F1-adopted *user* skill at `~/.claude/skills/notes`, whose
/// directory the user then deleted, must be left alone so drift classifies it
/// DELETED and drops the marker. Re-pointing it at an unrelated
/// `~/.agents/skills/notes` produces a permanent false-drift row instead: the
/// foreign content's sha will never match the row's.
#[test]
fn repair_does_not_repoint_onto_foreign_canonical_content() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    // An unrelated directory — no `.vectorhawk-managed.json` — happens to
    // share the slug under the canonical root.
    let canonical = fake_home.path().join(".agents/skills/notes");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"someone else's notes skill").unwrap();

    // The row's Claude-side directory is gone: the user deleted it.
    let old = fake_home.path().join(".claude/skills/notes");
    fs::create_dir_all(old.parent().unwrap()).unwrap();

    let conn = markers_conn();
    insert_marker(&conn, &old, "notes", "abc");

    super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(
        marker_paths(&conn, "notes"),
        vec![old.to_string_lossy().to_string()],
        "row must NOT be repointed at foreign content — drift must still see it as DELETED"
    );
    assert_eq!(
        fs::read(canonical.join("SKILL.md")).unwrap(),
        b"someone else's notes skill",
        "the foreign directory must be left completely untouched"
    );
}

/// The same repair, when the canonical directory *is* ours, must still happen —
/// this pins that the `is_vectorhawk_managed` guard did not disable the repair.
#[test]
fn repair_still_repoints_onto_our_own_canonical_content() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/notes");
    write_managed_skill(&canonical, "notes", "ours");
    let old = fake_home.path().join(".claude/skills/notes");
    fs::create_dir_all(old.parent().unwrap()).unwrap();

    let conn = markers_conn();
    insert_marker(&conn, &old, "notes", "abc");

    super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(
        marker_paths(&conn, "notes"),
        vec![canonical.to_string_lossy().to_string()],
        "a marked canonical dir is ours — the row must still be repaired"
    );
}

/// FINDING 3: one unmigratable entry must not abort the whole run. A *file*
/// at the canonical path makes `link_dir` bail for that slug only.
#[test]
fn one_bad_entry_does_not_block_the_other_skills() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let skills = fake_home.path().join(".claude/skills");
    let canonical_root = fake_home.path().join(".agents/skills");
    fs::create_dir_all(&canonical_root).unwrap();

    let conn = markers_conn();
    for slug in ["aaa-bad", "bbb-good", "ccc-good"] {
        let dir = skills.join(slug);
        write_managed_skill(&dir, slug, slug);
        insert_marker(&conn, &dir, slug, "abc");
    }
    // Canonical path for the first (alphabetically first, so it is hit first)
    // is a regular FILE — the move is skipped and `link_dir` bails.
    fs::write(canonical_root.join("aaa-bad"), "not a directory").unwrap();

    let migrated = super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert_eq!(migrated, 2, "the two healthy skills must still migrate");
    for slug in ["bbb-good", "ccc-good"] {
        assert!(skills.join(slug).is_symlink(), "{slug} not migrated");
        assert_eq!(
            marker_paths(&conn, slug),
            vec![canonical_root.join(slug).to_string_lossy().to_string()]
        );
    }
    // The bad one is untouched and still keyed on its original path — no data
    // lost, and it will be retried next start.
    assert!(skills.join("aaa-bad").is_dir());
    assert!(!skills.join("aaa-bad").is_symlink());
    assert_eq!(
        fs::read_to_string(canonical_root.join("aaa-bad")).unwrap(),
        "not a directory"
    );
    assert_eq!(
        marker_paths(&conn, "aaa-bad"),
        vec![skills.join("aaa-bad").to_string_lossy().to_string()]
    );
}

/// FINDING 5: when the Claude-side directory is a real, byte-identical copy of
/// canonical, the migration must not tear it down and rebuild it.
///
/// The identity check now lives in `link_dir` rather than here, so this holds
/// on *every* platform and via *every* caller — and it holds more strictly than
/// before: an identical copy is recognised as settled state and left alone, so
/// not even the first backup happens. (The previous migrator-side version healed
/// the copy into a symlink on Unix, costing one backup, and only suppressed the
/// churn on a copy-mode host. Nothing is lost by leaving it: the copy's content
/// is correct by definition, and the moment it diverges — which any push does,
/// since a push rewrites canonical — `link_dir` backs it up and relinks, as
/// `stale_legacy_content_is_backed_up_and_replaced` below pins.)
#[test]
fn identical_copy_converges_and_stops_creating_backups() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    let old = fake_home.path().join(".claude/skills/demo");
    write_managed_skill(&canonical, "demo", "same body");
    write_managed_skill(&old, "demo", "same body");

    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "abc");

    let backup_root = fake_home.path().join(".claude/.vectorhawk-backup");

    // Three runs, and no run creates a backup at all.
    for run in 1..=3 {
        assert_eq!(super::migrate_skills_to_agents_dir(&conn).unwrap(), 0);
        assert!(
            !backup_root.exists(),
            "run {run} created a timestamped backup for an already-identical copy"
        );
        assert_eq!(
            marker_paths(&conn, "demo"),
            vec![canonical.to_string_lossy().to_string()],
            "run {run}: the row must be keyed on canonical"
        );
    }

    // Content is intact on both sides and the copy was left as a real dir.
    assert!(!old.is_symlink());
    assert!(canonical.join("SKILL.md").exists());
    assert_eq!(
        fs::read(old.join("SKILL.md")).unwrap(),
        fs::read(canonical.join("SKILL.md")).unwrap()
    );
}

/// The genuinely-stale legacy case must still be replaced: Claude-side content
/// differs from canonical, so it is backed up and replaced by a link.
#[test]
fn stale_legacy_content_is_backed_up_and_replaced() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    let old = fake_home.path().join(".claude/skills/demo");
    write_managed_skill(&canonical, "demo", "NEW canonical body");
    write_managed_skill(&old, "demo", "OLD stale body");

    let conn = markers_conn();
    insert_marker(&conn, &old, "demo", "abc");

    super::migrate_skills_to_agents_dir(&conn).unwrap();

    assert!(old.is_symlink(), "stale content must be replaced by a link");
    assert!(fs::read_to_string(old.join("SKILL.md"))
        .unwrap()
        .contains("NEW canonical body"));
    assert_eq!(
        marker_paths(&conn, "demo"),
        vec![canonical.to_string_lossy().to_string()]
    );
    // The displaced content is recoverable under the links backup root.
    let backups = snapshot(&fake_home.path().join(".claude/.vectorhawk-backup"));
    assert!(
        backups.iter().any(|(_, v)| v.contains("OLD stale body")),
        "stale content must be preserved in a backup: {backups:?}"
    );
}

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
