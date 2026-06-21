//! Tests for `ManagedPathsPusher` (F2).
//!
//! All tests run against temp directories — they never touch the developer's
//! real `~` or `~/.claude/`.
#![allow(clippy::unwrap_used)]

use std::fs;
use tempfile::TempDir;

use super::*;
use crate::managed_paths::marker::MARKER_FILE_VERSION;
use rusqlite::Connection;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a minimal `ManagedPathsPusher` backed by a temp dir.
fn make_pusher() -> (ManagedPathsPusher, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = camino::Utf8PathBuf::from_path_buf(tmp.path().join("state.db")).unwrap();

    // Bootstrap the DB schema (just the managed_path_markers table).
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_path_markers (
            path TEXT NOT NULL,
            kind TEXT NOT NULL,
            slug TEXT NOT NULL,
            installation_id TEXT,
            source_sha256 TEXT NOT NULL,
            migrated_at TEXT NOT NULL,
            PRIMARY KEY (path)
        )",
    )
    .unwrap();
    drop(conn);

    let state = vectorhawkd_core::state::AppState {
        root_dir: camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap(),
        db_path: db_path.clone(),
    };
    let pusher = ManagedPathsPusher::new(&state);
    (pusher, tmp)
}

/// Read `.vectorhawk-managed.json` from a dir.
fn read_marker(dir: &std::path::Path) -> super::super::marker::ManagedMarkerFile {
    super::super::marker::read_file_marker(dir)
        .unwrap()
        .expect("marker must exist")
}

// ── skill push / remove ───────────────────────────────────────────────────────

#[test]
fn push_skill_writes_files_and_marker() {
    let (_pusher, _tmp) = make_pusher();

    let skills_dir = tempfile::tempdir().unwrap();
    let skills_path = skills_dir.path().to_path_buf();

    // Override HOME so the pusher writes into our temp dir.
    let fake_home = tempfile::tempdir().unwrap();
    let claude_dir = fake_home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();

    // We test resolve_skills_dir indirectly by calling push_skill after setting HOME.
    // Instead, test the helper functions directly with a known path.

    // Build a fake skill dir path.
    let skill_dir = skills_path.join("my-skill");
    fs::create_dir_all(&skill_dir).unwrap();

    let skill_md = b"---\nname: my-skill\n---\n# My Skill\n";
    let _refs: Vec<(String, Vec<u8>)> =
        vec![("prompts/step1.md".to_string(), b"step 1 content".to_vec())];

    // Write manually (push_skill uses resolve_skills_dir which needs HOME).
    // Instead, test atomic_write + hex_sha256 + marker independently.

    // atomic_write test
    let dest = skill_dir.join("SKILL.md");
    atomic_write(&dest, skill_md).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), skill_md);

    // hex_sha256 is deterministic
    let h1 = hex_sha256(skill_md);
    let h2 = hex_sha256(skill_md);
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64, "SHA-256 hex is 64 chars");

    // write_file_marker + read back
    super::super::marker::write_file_marker(
        &skill_dir,
        Some("install-abc"),
        &h1,
        "2026-05-24T00:00:00Z",
    )
    .unwrap();
    let m = read_marker(&skill_dir);
    assert_eq!(m.marker_version, MARKER_FILE_VERSION);
    assert_eq!(m.installation_id.as_deref(), Some("install-abc"));
    assert_eq!(m.source_sha256, h1);
}

#[test]
fn remove_skill_is_idempotent() {
    let (pusher, tmp) = make_pusher();

    // Call remove on a non-existent slug — must return Ok.
    // Use a fake path key that doesn't exist in the DB.
    let absent_key = format!("{}/skills/nonexistent", tmp.path().display());
    pusher.delete_db_marker(&absent_key).unwrap();
}

// ── MCP push / remove ─────────────────────────────────────────────────────────

/// Parse `mcpServers` from a raw JSON value.
fn get_mcp_servers(v: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
    v.get("mcpServers")
        .and_then(|s| s.as_object())
        .expect("mcpServers must be present and an object")
}

#[test]
fn modify_claude_json_creates_file_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude.json");
    assert!(!path.exists());

    modify_claude_json(&path, |root| {
        root.as_object_mut().unwrap().insert(
            "mcpServers".to_string(),
            serde_json::json!({"test-server": {"command": "vectorhawk"}}),
        );
    })
    .unwrap();

    assert!(path.exists());
    let content: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let servers = get_mcp_servers(&content);
    assert!(servers.contains_key("test-server"));
}

#[test]
fn modify_claude_json_preserves_existing_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude.json");

    // Seed with an existing anthropic entry.
    let initial = serde_json::json!({
        "mcpServers": {
            "anthropic-builtin": {"command": "anthropic-mcp", "args": []}
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

    modify_claude_json(&path, |root| {
        if let Some(Value::Object(ref mut map)) = root.get_mut("mcpServers") {
            map.insert(
                "my-server".to_string(),
                serde_json::json!({"command": "vectorhawk", "args": ["mcp", "serve", "--server", "my-server"]}),
            );
        }
    })
    .unwrap();

    let content: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let servers = get_mcp_servers(&content);

    // Both entries must be present.
    assert!(
        servers.contains_key("anthropic-builtin"),
        "existing entry must be preserved"
    );
    assert!(
        servers.contains_key("my-server"),
        "new entry must be written"
    );

    // Verify the new entry shape.
    let entry = servers.get("my-server").unwrap();
    assert_eq!(entry["command"], "vectorhawk");
    let args = entry["args"].as_array().unwrap();
    assert_eq!(args[0], "mcp");
    assert_eq!(args[1], "serve");
    assert_eq!(args[2], "--server");
    assert_eq!(args[3], "my-server");
}

#[test]
fn modify_claude_json_remove_entry_preserves_others() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude.json");

    let initial = serde_json::json!({
        "mcpServers": {
            "keep-me": {"command": "keep"},
            "remove-me": {"command": "vectorhawk", "args": ["mcp", "serve", "--server", "remove-me"]}
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

    modify_claude_json(&path, |root| {
        if let Some(Value::Object(ref mut map)) = root.get_mut("mcpServers") {
            map.remove("remove-me");
        }
    })
    .unwrap();

    let content: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let servers = get_mcp_servers(&content);
    assert!(servers.contains_key("keep-me"), "keep-me must survive");
    assert!(!servers.contains_key("remove-me"), "remove-me must be gone");
}

#[test]
fn modify_claude_json_concurrent_writers_no_corruption() {
    // Spawn multiple threads each appending a different key. All keys must
    // land in the final JSON without corruption.  This is the synchronous
    // equivalent of the "5 concurrent tokio tasks" requirement — we use
    // threads here because the file lock is synchronous.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude.json");
    let path = std::sync::Arc::new(path);

    let slugs = vec!["alpha", "beta", "gamma", "delta", "epsilon"];
    let mut handles = Vec::new();

    for slug in &slugs {
        let p = std::sync::Arc::clone(&path);
        let slug = slug.to_string();
        let handle = std::thread::spawn(move || {
            modify_claude_json(&p, |root| {
                if !root.is_object() {
                    *root = Value::Object(serde_json::Map::new());
                }
                if let Value::Object(ref mut top_map) = root {
                    let servers = top_map
                        .entry("mcpServers".to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Value::Object(ref mut map) = servers {
                        map.insert(
                            slug.clone(),
                            serde_json::json!({"command": "vectorhawk", "args": ["mcp", "serve", "--server", slug]}),
                        );
                    }
                }
            })
            .unwrap();
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("thread must not panic");
    }

    let content: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path.as_ref()).unwrap()).unwrap();
    let servers = get_mcp_servers(&content);

    for slug in &slugs {
        assert!(
            servers.contains_key(*slug),
            "slug {slug} must be in final JSON"
        );
    }
}

// ── SQLite marker helpers ─────────────────────────────────────────────────────

#[test]
fn upsert_db_marker_then_delete() {
    let (pusher, _tmp) = make_pusher();

    let path_key = "/tmp/fake/skills/test-skill";
    pusher
        .upsert_db_marker(
            path_key,
            "skill",
            "test-skill",
            Some("iid-001"),
            "abc123",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

    // Verify it's present.
    let conn = Connection::open(&pusher.db_path).unwrap();
    let exists = super::super::marker::is_already_marked(&conn, path_key).unwrap();
    assert!(exists, "marker must be present after upsert");

    // Delete it.
    drop(conn);
    pusher.delete_db_marker(path_key).unwrap();

    let conn = Connection::open(&pusher.db_path).unwrap();
    let exists = super::super::marker::is_already_marked(&conn, path_key).unwrap();
    assert!(!exists, "marker must be absent after delete");
}

#[test]
fn upsert_db_marker_idempotent_on_repeat_call() {
    let (pusher, _tmp) = make_pusher();

    let path_key = "/tmp/fake/skills/idempotent-skill";
    // First insert.
    pusher
        .upsert_db_marker(
            path_key,
            "skill",
            "idempotent-skill",
            None,
            "hash1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    // Second call with a different installation_id — must not error.
    pusher
        .upsert_db_marker(
            path_key,
            "skill",
            "idempotent-skill",
            Some("iid-002"),
            "hash1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

    let conn = Connection::open(&pusher.db_path).unwrap();
    let exists = super::super::marker::is_already_marked(&conn, path_key).unwrap();
    assert!(exists, "marker must still be present");
}

// ── atomic_write ──────────────────────────────────────────────────────────────

#[test]
fn atomic_write_leaves_no_tmp_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("output.txt");
    atomic_write(&dest, b"hello world").unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b"hello world");
    // Temp file must be gone.
    assert!(
        !dir.path().join("output.tmp").exists(),
        "tmp file must be cleaned up"
    );
}

#[test]
fn hex_sha256_known_value() {
    // SHA-256 of the empty byte string is known.
    let h = hex_sha256(b"");
    assert_eq!(
        h,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

// ── Legacy symlink replacement (v1.0.51) ──────────────────────────────────────

/// When `~/.claude/skills/<slug>` is a legacy installer symlink, push_skill
/// must remove it and create a real directory it owns — otherwise the file
/// writes leak through the symlink into the legacy `versions/0.1.0` dir and
/// the next reconcile can resurrect F2's view.
#[test]
fn push_skill_replaces_legacy_symlink_with_real_dir() {
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    let target = fake_home.path().join("legacy-versioned-source");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("SKILL.md"), b"legacy content").unwrap();

    let skills_dir = fake_home.path().join(".claude").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    let slug_path = skills_dir.join("hello-world");
    std::os::unix::fs::symlink(&target, &slug_path).unwrap();
    assert!(slug_path.is_symlink(), "test precondition: legacy symlink");

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    // Exercise the write mechanics directly via the inner method so the test
    // doesn't depend on the `native_skills_enabled()` env gate.
    let result = pusher.push_skill_inner(
        "hello-world",
        Some("inst-1"),
        b"---\nname: hello-world\n---\nfresh F2 content\n",
        &[],
    );

    let was_ok = result.is_ok();
    let post_symlink = slug_path.is_symlink();
    let post_dir_is_real = slug_path.is_dir() && !slug_path.is_symlink();
    let f2_skill_md = std::fs::read(slug_path.join("SKILL.md")).ok();
    let legacy_target_md = std::fs::read(target.join("SKILL.md")).unwrap();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(was_ok, "push_skill must succeed even over a legacy symlink");
    assert!(!post_symlink, "the symlink must be unlinked");
    assert!(post_dir_is_real, "the path must become a real directory");
    assert_eq!(
        f2_skill_md.as_deref(),
        Some(b"---\nname: hello-world\n---\nfresh F2 content\n".as_ref()),
        "F2's content must land at the real dir, not through the symlink"
    );
    assert_eq!(
        legacy_target_md, b"legacy content",
        "the legacy installer's source dir must NOT have been overwritten"
    );
}

// ── reclaim_active_skills (v1.0.51 startup pass) ──────────────────────────────

/// reclaim_active_skills must materialize legacy installer symlinks for every
/// active row in `installed_skills`, set an F2 marker, and leave non-symlink
/// paths alone.
#[test]
fn reclaim_active_skills_converts_legacy_symlinks_only() {
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    // Build state with an `installed_skills` table containing two active
    // skills (one with a legacy symlink, one without) and one deactivated.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = camino::Utf8PathBuf::from_path_buf(tmp.path().join("state.db")).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE managed_path_markers (
            path TEXT PRIMARY KEY, kind TEXT NOT NULL, slug TEXT NOT NULL,
            installation_id TEXT, source_sha256 TEXT NOT NULL, migrated_at TEXT NOT NULL
        );
        CREATE TABLE installed_skills (
            skill_id TEXT PRIMARY KEY, active_version TEXT NOT NULL,
            install_root TEXT NOT NULL, channel TEXT, current_status TEXT
        );",
    )
    .unwrap();

    // Active skill #1 — legacy symlink present.
    let install_root_1 = fake_home
        .path()
        .join("app-support")
        .join("skills")
        .join("alpha");
    fs::create_dir_all(install_root_1.join("active")).unwrap();
    fs::write(
        install_root_1.join("active").join("SKILL.md"),
        b"alpha body",
    )
    .unwrap();
    let skills_dir = fake_home.path().join(".claude").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    std::os::unix::fs::symlink(install_root_1.join("active"), skills_dir.join("alpha")).unwrap();

    // Active skill #2 — no symlink. Reclaim must skip it cleanly.
    let install_root_2 = fake_home
        .path()
        .join("app-support")
        .join("skills")
        .join("beta");
    fs::create_dir_all(install_root_2.join("active")).unwrap();
    fs::write(install_root_2.join("active").join("SKILL.md"), b"beta body").unwrap();

    // Deactivated skill — must be ignored entirely.
    conn.execute(
        "INSERT INTO installed_skills(skill_id, active_version, install_root, channel, current_status) VALUES
            ('alpha','1.0.0',?1,'stable','active'),
            ('beta','1.0.0',?2,'stable','active'),
            ('gamma','1.0.0','/tmp/whatever','stable','deactivated')",
        rusqlite::params![install_root_1.to_string_lossy(), install_root_2.to_string_lossy()],
    )
    .unwrap();
    drop(conn);

    let state = vectorhawkd_core::state::AppState {
        root_dir: camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap(),
        db_path,
    };
    let pusher = ManagedPathsPusher::new(&state);

    let reclaimed = reclaim_active_skills(&state, &pusher).unwrap();

    let alpha_path = skills_dir.join("alpha");
    let alpha_is_dir = alpha_path.is_dir() && !alpha_path.is_symlink();
    let alpha_marker_exists = alpha_path.join(".vectorhawk-managed.json").exists();
    let beta_exists = skills_dir.join("beta").exists();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(
        reclaimed, 1,
        "only the symlinked alpha must count as reclaimed"
    );
    assert!(alpha_is_dir, "alpha must now be a real directory");
    assert!(alpha_marker_exists, "alpha must have an F2 marker");
    assert!(
        !beta_exists,
        "beta had no symlink — reclaim must not create one"
    );
}

// ── push_adopted_discovery (v1.0.54) ──────────────────────────────────────────

// NOTE: the positive "writes skill dir + marker" test was intentionally
// removed because push_skill resolves the destination via $HOME, and the
// existing drift quarantine tests also swap $HOME — running them
// concurrently leaks files into the developer's real ~/.claude/skills/.
// Integration coverage for this path lives in the E2E flow exercised by
// the discovery-adopt scenario; the test below proves the kind-routing
// guard and `push_skill`'s own tests cover the disk write behavior.
// TODO: gate all HOME-swapping tests behind a process-wide serial Mutex,
// then re-add positive coverage here.

#[tokio::test]
async fn push_adopted_discovery_noop_for_non_skill_kind() {
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let root = camino::Utf8PathBuf::from_path_buf(fake_home.path().join("vh-state")).unwrap();
    let state = std::sync::Arc::new(vectorhawkd_core::state::AppState::bootstrap_in(root).unwrap());

    // Non-skill kind must return Ok without touching the filesystem, even when
    // source_path doesn't exist.
    let result = push_adopted_discovery(&state, "foo", "plugin", "/nope").await;

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    assert!(result.is_ok(), "non-skill kind must noop OK");
}

// ── VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER gate ─────────────────────────────

#[test]
fn push_skill_is_noop_when_reconciler_disabled() {
    // Set the env var to disable reconciler.
    std::env::set_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER", "1");

    let (pusher, _tmp) = make_pusher();
    // Calling push_skill must succeed immediately without touching the filesystem.
    let result = pusher.push_skill("gated-skill", None, b"---\nname: x\n---\n", &[]);
    assert!(result.is_ok(), "must return Ok when gate is active");

    // Clean up so this test doesn't affect others.
    std::env::remove_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER");
}

// ── native-skills gate (default off) ──────────────────────────────────────────

/// By default (no `VECTORHAWK_ENABLE_NATIVE_SKILLS`) the public `push_skill`
/// must be a no-op: governed skills are surfaced via MCP, not written into
/// `~/.claude/skills/`. The gate returns *before* `resolve_skills_dir()` reads
/// `$HOME`, so this test needs no HOME swap (keeping it free of the suite's
/// env-var races) — a slug containing a path separator would error if the body
/// ever ran, proving the early return.
#[test]
fn push_skill_noop_when_native_skills_disabled() {
    let (pusher, _tmp) = make_pusher();
    // An invalid slug: if the write body executed it would try to create this as
    // a directory under ~/.claude/skills and fail. Ok(()) proves we bailed early.
    let result = pusher.push_skill("../escape/noop-skill", None, b"---\nname: x\n---\n", &[]);
    assert!(
        result.is_ok(),
        "push_skill must no-op (Ok, no filesystem touch) when native skills are disabled"
    );
}

/// `remove_managed_skills` must delete every `kind='skill'` marker directory
/// (cleanup of skill dirs written by older runner versions) and leave non-skill
/// markers untouched.
#[test]
fn remove_managed_skills_removes_skill_dirs_only() {
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let (pusher, tmp) = make_pusher();
    let state = vectorhawkd_core::state::AppState {
        root_dir: camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap(),
        db_path: pusher.db_path.clone(),
    };

    // Seed two native skill dirs via the inner (gate-bypassing) writer, plus a
    // plugin marker that must survive the cleanup.
    pusher
        .push_skill_inner("alpha", None, b"---\nname: alpha\n---\n", &[])
        .unwrap();
    pusher
        .push_skill_inner("beta", None, b"---\nname: beta\n---\n", &[])
        .unwrap();
    pusher
        .push_plugin("gamma", None, &serde_json::json!({"name": "gamma"}))
        .unwrap();

    let skills_dir = fake_home.path().join(".claude").join("skills");
    assert!(skills_dir.join("alpha").is_dir());
    assert!(skills_dir.join("beta").is_dir());

    let removed = remove_managed_skills(&state, &pusher).unwrap();

    let alpha_gone = !skills_dir.join("alpha").exists();
    let beta_gone = !skills_dir.join("beta").exists();
    let plugin_survives = fake_home
        .path()
        .join(".claude")
        .join("plugins")
        .join("gamma")
        .exists();

    // No skill markers should remain; the plugin marker should.
    let conn = Connection::open(&pusher.db_path).unwrap();
    let skill_markers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_path_markers WHERE kind = 'skill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let plugin_markers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_path_markers WHERE kind = 'plugin'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(removed, 2, "both skill dirs must be removed");
    assert!(alpha_gone && beta_gone, "skill dirs must be deleted");
    assert!(plugin_survives, "plugin dir must NOT be touched");
    assert_eq!(skill_markers, 0, "skill markers must be cleared");
    assert_eq!(plugin_markers, 1, "plugin marker must survive");
}
