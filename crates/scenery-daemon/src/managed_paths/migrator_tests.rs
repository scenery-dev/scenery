#![allow(clippy::unwrap_used)]

use super::*;
use scenery_core::state::AppState;
use std::fs;

/// Bootstrap a minimal AppState (with the F1 managed_path_markers table) in a
/// temp dir and return it alongside the TempDir guard so it is not dropped.
fn temp_state() -> (AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    (state, tmp)
}

fn plugin_item(slug: &str, dir: &std::path::Path) -> MigrationItem {
    let source_path = dir.join(slug);
    let manifest_dir = source_path.join(".claude-plugin");
    fs::create_dir_all(&manifest_dir).unwrap();
    let manifest = manifest_dir.join("plugin.json");
    fs::write(&manifest, r#"{"name":"test"}"#).unwrap();
    MigrationItem {
        kind: ItemKind::Plugin,
        slug: slug.to_string(),
        source_path: source_path.clone(),
        files: vec![manifest],
        canonical_hash: "deadbeef1234".to_string(),
        payload: serde_json::json!({"manifest_json": {"name": "test"}}),
    }
}

// ── Backup tests ──────────────────────────────────────────────────────────────

#[test]
fn backup_plugin_item_copies_files() {
    let tmp = tempfile::tempdir().unwrap();
    let item = plugin_item("backup-skill", tmp.path());
    let backup_root = tmp.path().join("backup");

    backup_item(&item, &backup_root).unwrap();

    let backed_up = backup_root
        .join("plugins")
        .join("backup-skill")
        .join(".claude-plugin")
        .join("plugin.json");
    assert!(backed_up.exists(), "plugin.json should be backed up");
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
    let item = plugin_item("idem-skill", tmp.path());
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

// ── Replace-with-managed: adoption marks the item as Scenery-managed ─────────

/// B3: adopting a custom skill takes local ownership — it writes the
/// `.scenery-managed.json` marker in place (the "managed copy") and a DB
/// marker row, so the drift reconciler now governs it. Runs offline: with no
/// auth token the backend POST is skipped, but ownership is still taken.
#[tokio::test]
async fn migrate_item_marks_adopted_plugin_as_managed() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = plugin_item("adopt-me", tmp.path());
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(migrated, "a fresh custom skill should be newly adopted");

    // Local ownership marker written in place (replace-with-managed).
    let marker = item
        .source_path
        .join(scenery_mcp::ownership::MANAGED_MARKER_FILENAME);
    assert!(
        marker.exists(),
        "adopted skill must carry the managed marker"
    );
    assert!(
        scenery_mcp::ownership::is_scenery_managed(&item.source_path),
        "adopted skill dir must classify as Scenery-managed"
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

// ── Revoked artifacts are left for the reconciler ───────────────────────────

#[tokio::test]
async fn migrate_item_skips_a_revoked_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    scenery_core::revocation::upsert_revocation(&state, "plugin", "bad-plugin", None).unwrap();
    let mut item = plugin_item("bad-plugin", tmp.path());
    item.kind = ItemKind::Plugin;

    let client = reqwest::Client::new();
    let migrated = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        "http://unused",
        &client,
    )
    .await
    .unwrap();
    assert!(!migrated);
    assert_eq!(row_count(&state), 0);
}

#[tokio::test]
async fn migrate_item_still_adopts_when_only_another_artifact_or_version_is_revoked() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    // Other skill revoked; and a version-scoped revoke of this one (the
    // migration publishes its own new version, so it is not blocked).
    scenery_core::revocation::upsert_revocation(&state, "skill", "someone-else", None).unwrap();
    scenery_core::revocation::upsert_revocation(&state, "skill", "fine-skill", Some("9.9.9"))
        .unwrap();
    let item = plugin_item("fine-skill", tmp.path());

    let client = reqwest::Client::new();
    let migrated = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        "http://unused",
        &client,
    )
    .await
    .unwrap();
    assert!(migrated);
}

#[tokio::test]
async fn migrate_item_treats_backend_403_artifact_revoked_as_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let mut server = mockito::Server::new_async().await;
    scenery_core::auth::save_tokens(&state, &server.url(), "tok", "rtok").unwrap();
    let m = server
        .mock("POST", "/portal/managed-paths/migrate")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(r#"{"detail":{"code":"artifact_revoked","message":"revoked"}}"#)
        .expect(1)
        .create_async()
        .await;
    let item = plugin_item("late", tmp.path());

    let client = reqwest::Client::new();
    let migrated = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        &server.url(),
        &client,
    )
    .await
    .unwrap();

    assert!(!migrated, "not an error and not adopted");
    m.assert_async().await;
    assert_eq!(row_count(&state), 0, "no marker written");
}

/// Contract: the backend reports a revoked item as HTTP 200 with the item in
/// `errors` and `migrated: []` (backend b039687) — never as a 403.
#[tokio::test]
async fn migrate_item_treats_200_with_artifact_revoked_error_as_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let mut server = mockito::Server::new_async().await;
    scenery_core::auth::save_tokens(&state, &server.url(), "tok", "rtok").unwrap();
    let m = server
        .mock("POST", "/portal/managed-paths/migrate")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"migrated":[],"errors":[{"kind":"skill","slug":"late","message":"artifact_revoked: This artifact has been revoked by your organization"}]}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let item = plugin_item("late", tmp.path());
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, &server.url(), &client)
        .await
        .unwrap();

    assert!(!migrated);
    m.assert_async().await;
    assert_eq!(row_count(&state), 0, "no marker");
    assert!(
        !backup_root.join("skills").join("late").exists(),
        "no backup left"
    );
    assert!(!item
        .source_path
        .join(scenery_mcp::ownership::MANAGED_MARKER_FILENAME)
        .exists());
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_type = 'managed_path_migrated'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "no audit event");
}

/// Any other per-item error stops the marker too (and is retried next start).
#[tokio::test]
async fn migrate_item_stops_on_any_per_item_backend_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let mut server = mockito::Server::new_async().await;
    scenery_core::auth::save_tokens(&state, &server.url(), "tok", "rtok").unwrap();
    let _m = server
        .mock("POST", "/portal/managed-paths/migrate")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"migrated":[],"errors":[{"kind":"skill","slug":"oops","message":"compile failed"}]}"#)
        .create_async()
        .await;
    let item = plugin_item("oops", tmp.path());

    let client = reqwest::Client::new();
    let result = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        &server.url(),
        &client,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(row_count(&state), 0);
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn insert_row(state: &AppState, path: &std::path::Path, kind: &str, slug: &str, sha: &str) {
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    crate::managed_paths::marker::insert_db_marker(
        &conn,
        &crate::managed_paths::marker::ManagedPathMarker {
            path: path.to_string_lossy().to_string(),
            kind: kind.to_string(),
            slug: slug.to_string(),
            installation_id: None,
            source_sha256: sha.to_string(),
            migrated_at: "2026-01-01T00:00:00Z".to_string(),
        },
    )
    .unwrap();
}

fn row_count(state: &AppState) -> i64 {
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM managed_path_markers", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// F13 (reviewer's bypass): same for plugin dirs.
#[tokio::test]
async fn migrate_item_adopts_plugin_with_forged_marker_and_no_row() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let pdir = tmp.path().join("acme-tool");
    fs::create_dir_all(pdir.join(".claude-plugin")).unwrap();
    let manifest = pdir.join(".claude-plugin").join("plugin.json");
    fs::write(&manifest, r#"{"name":"acme-tool"}"#).unwrap();
    fs::write(
        pdir.join(scenery_mcp::ownership::MANAGED_MARKER_FILENAME),
        "{}",
    )
    .unwrap();
    let item = MigrationItem {
        kind: ItemKind::Plugin,
        slug: "acme-tool".to_string(),
        source_path: pdir.clone(),
        files: vec![manifest],
        canonical_hash: sha256_hex(br#"{"name":"acme-tool"}"#),
        payload: serde_json::json!({"manifest_json": {"name": "acme-tool"}}),
    };
    let client = reqwest::Client::new();
    let result = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        "http://unused",
        &client,
    )
    .await
    .unwrap();
    assert!(
        result,
        "a forged marker must not hide a plugin from adoption"
    );
}

/// A DB-tracked path whose content changed is drift's business (drift reports
/// it to the backend); F1 must not adopt it a second time.
#[tokio::test]
async fn migrate_item_skips_tracked_path_even_when_hash_differs() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = plugin_item("drifted", tmp.path());
    insert_row(&state, &item.source_path, "skill", "drifted", "0000");
    let client = reqwest::Client::new();
    let result = migrate_item(
        &item,
        &tmp.path().join("backup"),
        &state,
        "http://unused",
        &client,
    )
    .await
    .unwrap();
    assert!(!result);
    assert_eq!(row_count(&state), 1);
}

// ── F9: the migrate POST only ever carries allowlisted fields ────────────────

/// Defense in depth: even if a scanner bug hands `post_migrate` a raw
/// payload, only allowlisted fields leave the machine.
#[tokio::test]
async fn post_migrate_strips_non_allowlisted_payload_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();
    scenery_core::auth::save_tokens(&state, &registry_url, "tok", "rtok").unwrap();

    let bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let cap = bodies.clone();
    let _m = server
        .mock("POST", "/portal/managed-paths/migrate")
        .match_request(move |req| {
            cap.lock()
                .unwrap()
                .push(String::from_utf8_lossy(req.body().unwrap()).to_string());
            true
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"migrated":[{"installation_id":"i-1","is_new":true}],"errors":[]}"#)
        .expect_at_least(1)
        .create_async()
        .await;

    let claude_json = tmp.path().join(".claude.json");
    fs::write(&claude_json, "{}").unwrap();
    let mcp = MigrationItem {
        kind: ItemKind::Mcp,
        slug: "raw".to_string(),
        source_path: std::path::PathBuf::from(format!("{}:raw", claude_json.display())),
        files: vec![claude_json.clone()],
        canonical_hash: "h".to_string(),
        payload: serde_json::json!({
            "mcp_config": {
                "command": "npx",
                "args": ["--token", "ARGSECRET"],
                "env": {"K": "ghp_ENVSECRET"},
                "headers": {"Authorization": "Bearer HDRSECRET"},
                "url": "https://u:USERINFOSECRET@h.example/PATHSECRET",
                "transport": "stdio",
                "env_keys": ["K"],
                "arg_count": 2
            },
            "extra": "EXTRASECRET"
        }),
    };
    let pdir = tmp.path().join("p");
    fs::create_dir_all(pdir.join(".claude-plugin")).unwrap();
    let manifest = pdir.join(".claude-plugin").join("plugin.json");
    fs::write(&manifest, "{}").unwrap();
    let plugin = MigrationItem {
        kind: ItemKind::Plugin,
        slug: "p".to_string(),
        source_path: pdir,
        files: vec![manifest],
        canonical_hash: "h2".to_string(),
        payload: serde_json::json!({"manifest_json": {
            "name": "p", "version": "1", "description": "d",
            "mcpServers": {"x": {"env": {"K": "PLUGINSECRET"}}}
        }}),
    };

    let client = reqwest::Client::new();
    for item in [&mcp, &plugin] {
        migrate_item(
            item,
            &tmp.path().join("backup"),
            &state,
            &registry_url,
            &client,
        )
        .await
        .unwrap();
    }

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "both items must have been POSTed");
    for b in bodies.iter() {
        for secret in [
            "ARGSECRET",
            "ghp_ENVSECRET",
            "HDRSECRET",
            "USERINFOSECRET",
            "PATHSECRET",
            "EXTRASECRET",
            "PLUGINSECRET",
        ] {
            assert!(!b.contains(secret), "POST body leaked {secret}: {b}");
        }
    }
    let mcp_body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let cfg = &mcp_body["items"][0]["payload"]["mcp_config"];
    assert_eq!(cfg["env_keys"], serde_json::json!(["K"]));
    assert_eq!(cfg["transport"], "stdio");
    let plugin_body: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    assert_eq!(
        plugin_body["items"][0]["payload"]["manifest_json"],
        serde_json::json!({"name": "p", "version": "1", "description": "d"})
    );
}

// ── Restore journal (ONE ledger) ────────────────────────────────────────────

/// F1 takeovers must also land in the unified restore journal, source=native,
/// pointing at the SAME backup the legacy `.scenery-backup/` manifest uses
/// — not a second copy.
#[tokio::test]
async fn migrate_item_appends_native_restore_journal_entry_for_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _guard) = temp_state();
    let item = plugin_item("journal-skill", tmp.path());
    let backup_root = tmp.path().join("backup");

    let client = reqwest::Client::new();
    let migrated = migrate_item(&item, &backup_root, &state, "http://unused", &client)
        .await
        .unwrap();
    assert!(migrated);

    let journal = scenery_core::restore_journal::RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "one restore-journal entry per migrated item"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.op,
        scenery_core::restore_journal::JournalOp::FileReplace
    );
    assert_eq!(
        entry.source,
        scenery_core::restore_journal::JournalSource::Native
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
            .join("plugins")
            .join("journal-skill")
            .to_string_lossy()
            .to_string(),
        "restore journal must point at the SAME .scenery-backup/ location, not a new copy"
    );
    assert!(
        std::path::Path::new(backup_path)
            .join(".claude-plugin")
            .join("plugin.json")
            .exists(),
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

    let journal = scenery_core::restore_journal::RestoreJournal::for_state(&state);
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(
        entry.op,
        scenery_core::restore_journal::JournalOp::ConfigEdit
    );
    assert_eq!(
        entry.source,
        scenery_core::restore_journal::JournalSource::Native
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
    let item = plugin_item("audit-skill", tmp.path());
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

/// Helpers for the marketplace-adoption tests below.
fn marketplace_plugin_item(home: &std::path::Path, mkt: &str, slug: &str) -> MigrationItem {
    let dir = home
        .join(".claude/plugins/marketplaces")
        .join(mkt)
        .join("external_plugins")
        .join(slug);
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    let manifest = dir.join(".claude-plugin/plugin.json");
    fs::write(&manifest, format!(r#"{{"name":"{slug}"}}"#)).unwrap();
    MigrationItem {
        kind: ItemKind::Plugin,
        slug: slug.to_string(),
        source_path: dir,
        files: vec![manifest],
        canonical_hash: "h".to_string(),
        payload: serde_json::json!({"manifest_json": {"name": slug}}),
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // HOME is process-global; serialised via ENV_MUTEX
async fn external_plugin_in_official_marketplace_is_not_adopted_but_third_party_is() {
    let _env = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let plugins = home.join(".claude/plugins");
    let mut known = serde_json::Map::new();
    for (name, repo) in [
        (
            "claude-plugins-official",
            "anthropics/claude-plugins-official",
        ),
        ("acme-mp", "acme/acme-mp"),
    ] {
        known.insert(
            name.to_string(),
            serde_json::json!({
                "source": {"source": "github", "repo": repo},
                "installLocation": plugins.join("marketplaces").join(name),
            }),
        );
    }
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("known_marketplaces.json"),
        serde_json::Value::Object(known).to_string(),
    )
    .unwrap();
    let official = marketplace_plugin_item(&home, "claude-plugins-official", "github");
    let custom = marketplace_plugin_item(&home, "acme-mp", "acme-tool");

    let (state, _g) = temp_state();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let client = reqwest::Client::new();
    let backup_root = tmp.path().join("backup");
    let off = migrate_item(&official, &backup_root, &state, "http://unused", &client).await;
    let cus = migrate_item(&custom, &backup_root, &state, "http://unused", &client).await;
    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        !off.unwrap(),
        "official-marketplace plugin must not be adopted"
    );
    assert!(!official.source_path.join(".scenery-managed.json").exists());
    assert!(
        cus.unwrap(),
        "third-party marketplace plugin stays adoptable"
    );
    assert!(custom.source_path.join(".scenery-managed.json").exists());
}
