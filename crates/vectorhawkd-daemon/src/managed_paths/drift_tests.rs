//! F3 drift detection tests.
//!
//! These exercise the pure classify/quarantine/marker plumbing.  The HTTP
//! report path is covered indirectly by the backend's
//! `tests/test_managed_paths_f3.py` integration tests.

use super::*;
use crate::managed_paths::marker::{insert_db_marker, ManagedPathMarker};
use crate::managed_paths::ENV_MUTEX;
use rusqlite::Connection;
use std::fs;
use tempfile::TempDir;

fn fresh_conn() -> (TempDir, Connection) {
    let td = TempDir::new().unwrap();
    let db = td.path().join("state.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE managed_path_markers (
            path TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            slug TEXT NOT NULL,
            installation_id TEXT,
            source_sha256 TEXT NOT NULL,
            migrated_at TEXT NOT NULL
        );",
    )
    .unwrap();
    (td, conn)
}

#[test]
fn hex_sha256_matches_pusher_output() {
    // hashes must agree with the F2 pusher's hex_sha256 so an in-place file
    // written by F2 reads back as Clean on the next drift scan.
    let h = hex_sha256(b"hello");
    assert_eq!(
        h,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn classify_clean_when_skill_matches_marker() {
    let td = TempDir::new().unwrap();
    let skill_dir = td.path().join("hello");
    fs::create_dir_all(&skill_dir).unwrap();
    let body = b"---\nname: hello\n---\nGreet";
    fs::write(skill_dir.join("SKILL.md"), body).unwrap();

    let marker = ManagedPathMarker {
        path: skill_dir.to_string_lossy().to_string(),
        kind: "skill".to_string(),
        slug: "hello".to_string(),
        installation_id: None,
        source_sha256: hex_sha256(body),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    let outcome = classify(&marker);
    assert_eq!(outcome.status, DriftStatus::Clean);
}

#[test]
fn classify_drifted_when_skill_edited() {
    let td = TempDir::new().unwrap();
    let skill_dir = td.path().join("hello");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), b"original").unwrap();

    let marker = ManagedPathMarker {
        path: skill_dir.to_string_lossy().to_string(),
        kind: "skill".to_string(),
        slug: "hello".to_string(),
        installation_id: None,
        source_sha256: hex_sha256(b"original"),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    // Mutate the file.
    fs::write(skill_dir.join("SKILL.md"), b"hand-edited").unwrap();

    let outcome = classify(&marker);
    assert_eq!(outcome.status, DriftStatus::Drifted);
    assert!(outcome.current_hash.is_some());
    assert_ne!(
        outcome.current_hash.as_deref(),
        Some(marker.source_sha256.as_str())
    );
}

#[test]
fn classify_deleted_when_skill_file_missing() {
    let td = TempDir::new().unwrap();
    let skill_dir = td.path().join("gone");
    // Directory empty (no SKILL.md).
    fs::create_dir_all(&skill_dir).unwrap();

    let marker = ManagedPathMarker {
        path: skill_dir.to_string_lossy().to_string(),
        kind: "skill".to_string(),
        slug: "gone".to_string(),
        installation_id: None,
        source_sha256: "abc".to_string(),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    let outcome = classify(&marker);
    assert_eq!(outcome.status, DriftStatus::Deleted);
    assert!(outcome.current_hash.is_none());
}

#[test]
fn classify_plugin_drift() {
    let td = TempDir::new().unwrap();
    let plugin_dir = td.path().join("my-plugin");
    let inner = plugin_dir.join(".claude-plugin");
    fs::create_dir_all(&inner).unwrap();
    fs::write(inner.join("plugin.json"), b"{\"name\":\"my-plugin\"}").unwrap();

    let marker = ManagedPathMarker {
        path: plugin_dir.to_string_lossy().to_string(),
        kind: "plugin".to_string(),
        slug: "my-plugin".to_string(),
        installation_id: None,
        source_sha256: hex_sha256(b"{\"name\":\"my-plugin\"}"),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    assert_eq!(classify(&marker).status, DriftStatus::Clean);

    fs::write(inner.join("plugin.json"), b"{\"name\":\"hacked\"}").unwrap();
    assert_eq!(classify(&marker).status, DriftStatus::Drifted);
}

#[test]
fn classify_mcp_drift_via_claude_json() {
    let td = TempDir::new().unwrap();
    let claude_json = td.path().join(".claude.json");
    let entry = serde_json::json!({"command":"vectorhawk","args":["mcp","serve","--server","fs"]});
    let root = serde_json::json!({"mcpServers": {"fs": entry}});
    fs::write(&claude_json, serde_json::to_vec_pretty(&root).unwrap()).unwrap();

    let virtual_path = format!("{}:fs", claude_json.display());
    let marker = ManagedPathMarker {
        path: virtual_path,
        kind: "mcp".to_string(),
        slug: "fs".to_string(),
        installation_id: None,
        source_sha256: hex_sha256(entry.to_string().as_bytes()),
        migrated_at: "2026-05-24T00:00:00Z".to_string(),
    };
    assert_eq!(classify(&marker).status, DriftStatus::Clean);

    // Hand-edit the entry.
    let new_root = serde_json::json!({"mcpServers": {"fs": {"command":"rogue"}}});
    fs::write(&claude_json, serde_json::to_vec_pretty(&new_root).unwrap()).unwrap();
    assert_eq!(classify(&marker).status, DriftStatus::Drifted);

    // Remove the entry entirely.
    let empty_root = serde_json::json!({"mcpServers": {}});
    fs::write(
        &claude_json,
        serde_json::to_vec_pretty(&empty_root).unwrap(),
    )
    .unwrap();
    assert_eq!(classify(&marker).status, DriftStatus::Deleted);
}

#[test]
fn split_mcp_path_recovers_components() {
    let virtual_path = "/Users/x/.claude.json:filesystem";
    let (json, slug) = split_mcp_path(virtual_path).unwrap();
    assert_eq!(json.to_string_lossy(), "/Users/x/.claude.json");
    assert_eq!(slug, "filesystem");
}

#[test]
fn quarantine_skill_moves_dir_aside() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    // Test in a fake $HOME so the real ~/.claude isn't touched.
    let td = TempDir::new().unwrap();
    let fake_home = td.path().join("home");
    let skill_dir = fake_home.join(".claude").join("skills").join("hello");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), b"edited").unwrap();

    let prev_home = std::env::var_os("HOME");
    // SAFETY: drift module's dirs::home_dir() reads HOME — set it for this test.
    std::env::set_var("HOME", &fake_home);

    let outcome = DriftOutcome {
        slug: "hello".to_string(),
        kind: "skill".to_string(),
        path: skill_dir.to_string_lossy().to_string(),
        expected_hash: "abc".to_string(),
        current_hash: Some("def".to_string()),
        status: DriftStatus::Drifted,
    };
    let dest = quarantine_item(&outcome).unwrap();

    // The original path no longer holds SKILL.md.
    assert!(!skill_dir.join("SKILL.md").exists());
    // The quarantine destination does.
    assert!(dest.join("SKILL.md").exists());
    // dest path includes the quarantine root.
    assert!(dest.to_string_lossy().contains(".vectorhawk-quarantine"));

    // Restore env so we don't poison parallel tests.
    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
}

#[test]
fn quarantine_mcp_writes_entry_snapshot_and_removes() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let td = TempDir::new().unwrap();
    let fake_home = td.path().join("home");
    fs::create_dir_all(fake_home.join(".claude")).unwrap();
    let claude_json = fake_home.join(".claude.json");
    let root = serde_json::json!({
        "mcpServers": {
            "fs": {"command":"vectorhawk","args":["mcp","serve","--server","fs"]},
            "preserved": {"command":"keep-me"}
        }
    });
    fs::write(&claude_json, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
    // Hand-edit the fs entry so it's "drifted".
    let root2 = serde_json::json!({
        "mcpServers": {
            "fs": {"command":"rogue"},
            "preserved": {"command":"keep-me"}
        }
    });
    fs::write(&claude_json, serde_json::to_vec_pretty(&root2).unwrap()).unwrap();

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &fake_home);

    let virtual_path = format!("{}:fs", claude_json.display());
    let outcome = DriftOutcome {
        slug: "fs".to_string(),
        kind: "mcp".to_string(),
        path: virtual_path,
        expected_hash: "x".to_string(),
        current_hash: Some("y".to_string()),
        status: DriftStatus::Drifted,
    };
    let dest = quarantine_item(&outcome).unwrap();

    // Snapshot exists.
    assert!(dest.is_file());
    let snap: serde_json::Value = serde_json::from_slice(&fs::read(&dest).unwrap()).unwrap();
    assert_eq!(snap.get("command").and_then(|v| v.as_str()), Some("rogue"));

    // Entry removed from claude.json, preserved entry untouched.
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_json).unwrap()).unwrap();
    assert!(after.get("mcpServers").unwrap().get("fs").is_none());
    assert!(after.get("mcpServers").unwrap().get("preserved").is_some());

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
}

#[test]
fn policy_mode_parses_known_strings() {
    assert_eq!(PolicyMode::from_str_or_default("warn"), PolicyMode::Warn);
    assert_eq!(
        PolicyMode::from_str_or_default("quarantine"),
        PolicyMode::Quarantine
    );
    assert_eq!(
        PolicyMode::from_str_or_default("approve_required"),
        PolicyMode::ApproveRequired
    );
    assert_eq!(
        PolicyMode::from_str_or_default("audit_only"),
        PolicyMode::AuditOnly
    );
    // Unknown → audit_only default.
    assert_eq!(
        PolicyMode::from_str_or_default("nonsense"),
        PolicyMode::AuditOnly
    );
}

// ── Link integrity ────────────────────────────────────────────────────────────

/// RAII guard restoring `$HOME` even if the test body panics.
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

#[test]
fn link_integrity_false_when_link_replaced_by_real_dir() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    // User replaced the link with their own real directory — not
    // VectorHawk-managed (no marker file), so it can never read as intact.
    let link = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("SKILL.md"), b"tampered").unwrap();

    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    assert!(!result.unwrap());
}

#[test]
fn link_integrity_true_when_link_intact() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    let link = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&canonical, &link).unwrap();

    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    assert!(result.unwrap());
}

#[test]
fn link_integrity_true_when_link_absent() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    // No `.claude/skills/demo` at all — a surfacing failure the pusher heals
    // on the next reconcile, not tampering.

    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    assert!(result.unwrap());
}

/// State 3: a real directory at the Claude path that is VectorHawk-managed
/// and byte-identical to canonical — the legitimate `LinkMode::Copy` steady
/// state (e.g. Windows without Developer Mode). `links::link_is_intact`
/// alone returns `false` here because it checks `is_symlink()` first; a
/// naive port of the brief's implementation would report `link_replaced`
/// drift every cycle on a perfectly healthy install. This must read as
/// intact.
#[test]
fn link_integrity_true_when_healthy_managed_copy() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    // The pusher writes the `.vectorhawk-managed.json` marker into the
    // *canonical* directory, and `copy_tree` necessarily carries it along
    // into the copy — so a real steady-state pair has the marker on both
    // sides, not just the copy.
    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    fs::write(
        canonical.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // A real directory (not a symlink) at the Claude path, carrying the
    // VectorHawk marker and identical content — as `link_dir` leaves behind
    // on a copy-mode host.
    let link = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("SKILL.md"), b"managed").unwrap();
    fs::write(
        link.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    assert!(!link.is_symlink());
    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    assert!(result.unwrap());
}

/// Same real-directory-with-marker shape as the healthy-copy case, but
/// `SKILL.md` itself has diverged from canonical — must NOT read as intact.
#[test]
fn link_integrity_false_when_managed_copy_diverged() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    fs::write(
        canonical.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let link = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("SKILL.md"), b"diverged").unwrap();
    fs::write(
        link.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    assert!(!result.unwrap());
}

/// Documents a deliberate blind spot introduced by matching
/// `current_hash_for`'s single-file granularity: `check_link_integrity` only
/// hashes `SKILL.md`, so a managed copy that diverges *solely* in some other
/// bundled file (e.g. a reference doc or script) is NOT detected as drift —
/// it reads as intact, same as canonical's own drift classification would.
/// This is intentional for consistency with the rest of drift detection, not
/// an oversight; this test exists so the limitation stays visible rather
/// than being silently reintroduced or "fixed" without discussion.
#[test]
fn link_integrity_true_when_managed_copy_diverges_only_in_reference_file() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let _home_guard = HomeGuard::set(fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    fs::create_dir_all(&canonical).unwrap();
    fs::write(canonical.join("SKILL.md"), b"managed").unwrap();
    fs::write(canonical.join("reference.md"), b"original reference").unwrap();
    fs::write(
        canonical.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let link = fake_home.path().join(".claude/skills/demo");
    fs::create_dir_all(&link).unwrap();
    // SKILL.md matches canonical byte-for-byte...
    fs::write(link.join("SKILL.md"), b"managed").unwrap();
    // ...but the bundled reference file has silently diverged.
    fs::write(link.join("reference.md"), b"tampered reference").unwrap();
    fs::write(
        link.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        br#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let result = super::check_link_integrity("demo", &hex_sha256(b"managed"));

    // Blind spot, not a bug: SKILL.md-only hashing cannot see this, exactly
    // like `current_hash_for` cannot see it for canonical either.
    assert!(result.unwrap());
}

#[test]
fn list_markers_returns_all_rows() {
    let (_td, conn) = fresh_conn();
    insert_db_marker(
        &conn,
        &ManagedPathMarker {
            path: "/p/one".to_string(),
            kind: "skill".to_string(),
            slug: "one".to_string(),
            installation_id: None,
            source_sha256: "h1".to_string(),
            migrated_at: "t".to_string(),
        },
    )
    .unwrap();
    insert_db_marker(
        &conn,
        &ManagedPathMarker {
            path: "/p/two".to_string(),
            kind: "mcp".to_string(),
            slug: "two".to_string(),
            installation_id: None,
            source_sha256: "h2".to_string(),
            migrated_at: "t".to_string(),
        },
    )
    .unwrap();
    let rows = list_markers(&conn).unwrap();
    assert_eq!(rows.len(), 2);
}
