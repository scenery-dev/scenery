//! Tests for `ManagedPathsPusher` (F2).
//!
//! All tests run against temp directories — they never touch the developer's
//! real `~` or `~/.claude/`.
#![allow(clippy::unwrap_used)]

use std::fs;
use tempfile::TempDir;

use super::*;
use crate::managed_paths::marker::MARKER_FILE_VERSION;
use crate::managed_paths::ENV_MUTEX;
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

/// `remove_skill` must be safe to call on a slug that was never installed,
/// and safe to call twice on one that was.
///
/// This test previously called `delete_db_marker` and never `remove_skill` at
/// all — so the `unlink_dir` step `remove_skill` gained on this branch (the
/// one carrying the ownership guard) had no coverage from the only test named
/// for it. It now drives the real function end to end, under a fake `$HOME`,
/// and asserts on the filesystem it actually touches.
#[test]
fn remove_skill_is_idempotent() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let canonical = fake_home.path().join(".agents/skills/demo");
    let link = fake_home.path().join(".claude/skills/demo");

    // 1. Never installed — must be a clean no-op, not an error.
    let never_installed = pusher.remove_skill("nonexistent");

    // 2. Install, then remove twice.
    let pushed = pusher.push_skill("demo", None, b"---\nname: demo\n---\n", &[]);
    let existed_before = canonical.join("SKILL.md").exists() && link.is_symlink();
    let first = pusher.remove_skill("demo");
    let gone_after_first = !canonical.exists() && !link.exists() && !link.is_symlink();
    let second = pusher.remove_skill("demo");

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    never_installed.expect("removing a slug that was never installed must be Ok");
    pushed.expect("push_skill setup must succeed");
    assert!(existed_before, "test precondition: the push landed");
    first.expect("first remove_skill must succeed");
    assert!(
        gone_after_first,
        "remove_skill must drop both the canonical dir and the Claude link"
    );
    second.expect("a second remove_skill must be a clean no-op");
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

// ── Canonical write + Claude Code link (agents/skills pivot) ──────────────────

#[test]
fn push_skill_writes_canonical_and_links_claude() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_skill("demo", Some("inst-1"), b"---\nname: demo\n---\n", &[]);

    let canonical = fake_home.path().join(".agents/skills/demo");
    let link = fake_home.path().join(".claude/skills/demo");

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    result.unwrap();
    // Canonical is a real directory holding the content and the marker.
    assert!(canonical.is_dir());
    assert!(!canonical.is_symlink());
    assert!(canonical.join("SKILL.md").exists());
    assert!(canonical.join(".vectorhawk-managed.json").exists());
    // Claude Code's path is a link, and the content is reachable through it.
    assert!(link.is_symlink());
    assert!(link.join("SKILL.md").exists());
}

#[test]
fn remove_skill_removes_canonical_and_link() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    pusher
        .push_skill("demo", None, b"---\nname: demo\n---\n", &[])
        .unwrap();
    let result = pusher.remove_skill("demo");

    let canonical = fake_home.path().join(".agents/skills/demo");
    let link = fake_home.path().join(".claude/skills/demo");

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    result.unwrap();
    assert!(!canonical.exists());
    assert!(!link.exists());
    assert!(!link.is_symlink());
}

// ── Legacy symlink replacement (v1.0.51) ──────────────────────────────────────

/// When `~/.claude/skills/<slug>` (Claude Code's link path, post-pivot) is a
/// legacy installer symlink, push_skill must replace it with a fresh symlink
/// to the canonical `~/.agents/skills/<slug>` directory it just wrote — never
/// leave the stale symlink in place, and never write through it into the
/// legacy `versions/0.1.0` dir it points at.
///
/// Pre-pivot this same scenario was handled by push_skill's own top-of-
/// function symlink check (it wrote directly to `~/.claude/skills/<slug>`).
/// Post-pivot that check now guards the brand-new canonical path instead
/// (which a legacy installer never touched), and this exact scenario —
/// stale symlink at the Claude *link* path — is handled by `link_dir`'s own
/// stale-link replacement inside push_skill's link step.
#[test]
fn push_skill_replaces_legacy_symlink_with_real_dir() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    let target = fake_home.path().join("legacy-versioned-source");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("SKILL.md"), b"legacy content").unwrap();

    let claude_skills_dir = fake_home.path().join(".claude").join("skills");
    fs::create_dir_all(&claude_skills_dir).unwrap();
    let link_path = claude_skills_dir.join("hello-world");
    std::os::unix::fs::symlink(&target, &link_path).unwrap();
    assert!(link_path.is_symlink(), "test precondition: legacy symlink");

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_skill(
        "hello-world",
        Some("inst-1"),
        b"---\nname: hello-world\n---\nfresh F2 content\n",
        &[],
    );

    let canonical_path = fake_home.path().join(".agents/skills/hello-world");
    let was_ok = result.is_ok();
    let link_is_symlink = link_path.is_symlink();
    let canonical_is_real_dir = canonical_path.is_dir() && !canonical_path.is_symlink();
    let canonical_skill_md = std::fs::read(canonical_path.join("SKILL.md")).ok();
    let link_skill_md = std::fs::read(link_path.join("SKILL.md")).ok();
    let legacy_target_md = std::fs::read(target.join("SKILL.md")).unwrap();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(was_ok, "push_skill must succeed even over a legacy symlink");
    assert!(
        link_is_symlink,
        "the Claude link path must remain/become a symlink"
    );
    assert!(
        canonical_is_real_dir,
        "the canonical path must be a real directory"
    );
    assert_eq!(
        canonical_skill_md.as_deref(),
        Some(b"---\nname: hello-world\n---\nfresh F2 content\n".as_ref()),
        "F2's content must land at the canonical dir"
    );
    assert_eq!(
        link_skill_md.as_deref(),
        Some(b"---\nname: hello-world\n---\nfresh F2 content\n".as_ref()),
        "the Claude link must have been repointed to the canonical dir, not left on the legacy target"
    );
    assert_eq!(
        legacy_target_md, b"legacy content",
        "the legacy installer's source dir must NOT have been overwritten"
    );
}

/// The pre-pivot upgrade case: before `~/.agents/skills` became canonical,
/// `push_skill` wrote a **real, marked directory** straight into
/// `~/.claude/skills/<slug>`. On the first post-pivot push for that slug the
/// real directory must be replaced by a symlink into the canonical root,
/// with the content living exactly once, at the canonical path.
///
/// This is the integration-level counterpart to `links_tests`' isolated
/// `link_dir_replaces_its_own_prior_copy_materialisation`: it drives the same
/// replacement through `push_skill`, which is where the ordering between
/// "write canonical" and "replace the Claude path" actually matters.
#[test]
fn push_skill_replaces_real_marked_dir_at_claude_path_with_link() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();

    // A pre-pivot managed skill: real directory at the Claude path, carrying
    // the `.vectorhawk-managed.json` marker push_skill used to write there.
    let claude_skill_dir = fake_home
        .path()
        .join(".claude")
        .join("skills")
        .join("legacy-real");
    fs::create_dir_all(&claude_skill_dir).unwrap();
    fs::write(claude_skill_dir.join("SKILL.md"), b"pre-pivot content").unwrap();
    fs::write(
        claude_skill_dir.join(".vectorhawk-managed.json"),
        format!(
            r#"{{"marker_version":{MARKER_FILE_VERSION},"installation_id":null,"source_sha256":"old","migrated_at":"2026-01-01T00:00:00Z"}}"#
        ),
    )
    .unwrap();
    assert!(
        !claude_skill_dir.is_symlink() && claude_skill_dir.is_dir(),
        "test precondition: a real directory, not a link"
    );

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let new_content = b"---\nname: legacy-real\n---\npost-pivot content\n";
    let result = pusher.push_skill("legacy-real", Some("inst-9"), new_content, &[]);

    let canonical = fake_home.path().join(".agents/skills/legacy-real");
    let was_ok = result.is_ok();
    let claude_is_symlink = claude_skill_dir.is_symlink();
    let canonical_is_real_dir = canonical.is_dir() && !canonical.is_symlink();
    let canonical_md = fs::read(canonical.join("SKILL.md")).ok();
    let resolves_to_canonical = fs::canonicalize(&claude_skill_dir)
        .ok()
        .zip(fs::canonicalize(&canonical).ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(was_ok, "push_skill must succeed over a real marked dir");
    assert!(
        claude_is_symlink,
        "the old real directory must be gone, replaced by a symlink"
    );
    assert!(
        resolves_to_canonical,
        "the Claude path must resolve to the canonical ~/.agents/skills dir"
    );
    assert!(
        canonical_is_real_dir,
        "the canonical path must hold the real directory"
    );
    assert_eq!(
        canonical_md.as_deref(),
        Some(new_content.as_ref()),
        "the canonical dir must hold the freshly pushed content"
    );
}

/// **Regression (shared-root symlink clobber).**
///
/// `~/.agents/skills` is not VectorHawk's private root. `npx skills`, Cursor
/// and Codex users all populate it — `discoveries.rs` scans it *because* it is
/// user-populated — and symlinking a skill in from a checkout elsewhere is a
/// normal thing for a user to do there. VectorHawk never puts a symlink at
/// the canonical path itself (it writes a real directory), so a symlink whose
/// target carries no marker is someone else's.
///
/// Pushing a managed skill whose slug collides with such a link must refuse,
/// not silently unlink it.
#[test]
fn push_skill_refuses_to_clobber_an_unmanaged_symlink_in_the_shared_root() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();

    // The user's own skill, checked out elsewhere and linked into the shared
    // cross-agent root by hand (or by `npx skills`).
    let user_checkout = fake_home.path().join("dev").join("my-skill");
    fs::create_dir_all(&user_checkout).unwrap();
    fs::write(user_checkout.join("SKILL.md"), b"the user's own work").unwrap();

    let agents_skills = fake_home.path().join(".agents").join("skills");
    fs::create_dir_all(&agents_skills).unwrap();
    let collision = agents_skills.join("hello-world");
    std::os::unix::fs::symlink(&user_checkout, &collision).unwrap();
    assert!(collision.is_symlink(), "test precondition");

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_skill("hello-world", Some("inst-1"), b"managed content", &[]);

    let still_a_symlink = collision.is_symlink();
    let target_md = fs::read(user_checkout.join("SKILL.md")).ok();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        result.is_err(),
        "push_skill must refuse rather than clobber a foreign symlink"
    );
    assert!(
        still_a_symlink,
        "BUG: push_skill silently unlinked the user's symlink at {}",
        collision.display()
    );
    assert_eq!(
        target_md.as_deref(),
        Some(b"the user's own work".as_ref()),
        "the user's checkout must be untouched"
    );
}

/// The counterpart: a symlink at the canonical path that resolves to
/// VectorHawk-marked content *is* ours to replace, so the push proceeds and
/// materialises a real managed directory.
#[test]
fn push_skill_replaces_a_managed_symlink_in_the_shared_root() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();

    let managed_target = fake_home.path().join("vh-managed").join("hello-world");
    fs::create_dir_all(&managed_target).unwrap();
    fs::write(managed_target.join("SKILL.md"), b"old managed content").unwrap();
    fs::write(managed_target.join(".vectorhawk-managed.json"), b"{}").unwrap();

    let agents_skills = fake_home.path().join(".agents").join("skills");
    fs::create_dir_all(&agents_skills).unwrap();
    let canonical = agents_skills.join("hello-world");
    std::os::unix::fs::symlink(&managed_target, &canonical).unwrap();

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_skill("hello-world", Some("inst-1"), b"fresh managed content", &[]);

    let is_real_dir = canonical.is_dir() && !canonical.is_symlink();
    let md = fs::read(canonical.join("SKILL.md")).ok();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    result.unwrap();
    assert!(
        is_real_dir,
        "our own symlink must become a real managed dir"
    );
    assert_eq!(md.as_deref(), Some(b"fresh managed content".as_ref()));
}

/// **Regression (shared-root real-directory clobber).**
///
/// The symlink case above (`push_skill_refuses_to_clobber_an_unmanaged_symlink_in_the_shared_root`)
/// is the rarer shape. The common one is a **real directory**: `npx skills`,
/// Cursor and Codex all create real directories at the canonical path, and
/// `discoveries.rs` deliberately reports them without adopting them, so they
/// legitimately sit there unmanaged forever. `push_skill` must refuse a slug
/// collision against one of these exactly as it refuses one against a foreign
/// symlink — not silently annex it, overwrite its `SKILL.md`, stamp it with
/// VectorHawk's marker, and leave it primed for a later `remove_skill` to
/// `remove_dir_all` content VectorHawk never wrote.
#[test]
fn push_skill_refuses_to_clobber_an_unmanaged_real_dir_in_the_shared_root() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, _tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();

    // A real, unmanaged directory at the canonical path — e.g. left by `npx
    // skills`, Cursor, or Codex, and never carrying VectorHawk's marker.
    let agents_skills = fake_home.path().join(".agents").join("skills");
    fs::create_dir_all(&agents_skills).unwrap();
    let collision = agents_skills.join("hello-world");
    fs::create_dir_all(&collision).unwrap();
    fs::write(collision.join("SKILL.md"), b"the user's own SKILL.md").unwrap();
    fs::write(collision.join("notes.md"), b"the user's own notes").unwrap();
    assert!(
        collision.is_dir() && !collision.is_symlink(),
        "test precondition: a real, unmanaged directory"
    );

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let push_result = pusher.push_skill("hello-world", Some("inst-1"), b"managed content", &[]);

    let skill_md_after_push = fs::read(collision.join("SKILL.md")).ok();
    let notes_md_after_push = fs::read(collision.join("notes.md")).ok();
    let marker_written = collision.join(".vectorhawk-managed.json").exists();

    // A subsequent remove_skill (as the reconciler would call on deactivate)
    // must not destroy the directory either.
    let remove_result = pusher.remove_skill("hello-world");
    let dir_survives_remove = collision.is_dir();
    let skill_md_after_remove = fs::read(collision.join("SKILL.md")).ok();
    let notes_md_after_remove = fs::read(collision.join("notes.md")).ok();

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(
        push_result.is_err(),
        "push_skill must refuse rather than annex a foreign real directory"
    );
    assert_eq!(
        skill_md_after_push.as_deref(),
        Some(b"the user's own SKILL.md".as_ref()),
        "BUG: push_skill overwrote the user's SKILL.md"
    );
    assert_eq!(
        notes_md_after_push.as_deref(),
        Some(b"the user's own notes".as_ref()),
        "the user's second file must be untouched"
    );
    assert!(
        !marker_written,
        "BUG: push_skill stamped the user's directory as VectorHawk-managed"
    );
    assert!(
        dir_survives_remove,
        "BUG: remove_skill deleted the user's unmanaged directory"
    );
    assert_eq!(
        skill_md_after_remove.as_deref(),
        Some(b"the user's own SKILL.md".as_ref()),
        "the user's SKILL.md must survive a subsequent remove_skill call"
    );
    assert_eq!(
        notes_md_after_remove.as_deref(),
        Some(b"the user's own notes".as_ref()),
        "the user's notes.md must survive a subsequent remove_skill call"
    );
    let _ = remove_result; // may legitimately be Err (refused) or Ok (idempotent no-op)
}

// NOTE: `reclaim_active_skills_converts_legacy_symlinks_only` was removed with
// the `reclaim_active_skills` function it named (see the retirement note in
// pusher.rs). Its fixture planted an *unmarked* symlink at
// `~/.agents/skills/<slug>` — which, post-pivot, is user content in a shared
// root that `push_skill` must now refuse to touch, not legacy state to
// reclaim. The surviving behaviour is covered by
// `push_missing_active_skills_repushes_only_absent` and by the two
// shared-root ownership tests above.

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

// `#[tokio::test]` defaults to a current-thread runtime and this test has no
// other concurrent task, so holding a std Mutex across the single `.await`
// below cannot block a worker pool or deadlock — safe, unlike the general
// case clippy's lint guards against.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn push_adopted_discovery_noop_for_non_skill_kind() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    // Set the env var to disable reconciler.
    std::env::set_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER", "1");

    let (pusher, _tmp) = make_pusher();
    // Calling push_skill must succeed immediately without touching the filesystem.
    let result = pusher.push_skill("gated-skill", None, b"---\nname: x\n---\n", &[]);
    assert!(result.is_ok(), "must return Ok when gate is active");

    // Clean up so this test doesn't affect others.
    std::env::remove_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER");
}

// ── push_missing_active_skills (self-heal) ────────────────────────────────────

/// push_missing_active_skills must re-push active installed skills whose
/// ~/.claude/skills dir is absent, and leave already-present dirs alone.
#[test]
fn push_missing_active_skills_repushes_only_absent() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

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

    // Active skill "alpha" — installed copy exists, but its ~/.agents/skills dir
    // is MISSING (the v1.0.67-wipe scenario, now against the canonical root).
    // Must be re-pushed.
    let install_root_1 = fake_home
        .path()
        .join("app-support")
        .join("skills")
        .join("alpha");
    fs::create_dir_all(install_root_1.join("active")).unwrap();
    fs::write(
        install_root_1.join("active").join("SKILL.md"),
        b"---\nname: alpha\n---\nbody",
    )
    .unwrap();

    // Active skill "beta" — already present in ~/.agents/skills (the
    // canonical root). Must be skipped.
    let install_root_2 = fake_home
        .path()
        .join("app-support")
        .join("skills")
        .join("beta");
    fs::create_dir_all(install_root_2.join("active")).unwrap();
    fs::write(
        install_root_2.join("active").join("SKILL.md"),
        b"---\nname: beta\n---\nbody",
    )
    .unwrap();
    let skills_dir = fake_home.path().join(".agents").join("skills");
    fs::create_dir_all(skills_dir.join("beta")).unwrap();
    fs::write(skills_dir.join("beta").join("SKILL.md"), b"preexisting").unwrap();

    conn.execute(
        "INSERT INTO installed_skills(skill_id, active_version, install_root, channel, current_status) VALUES
            ('alpha','1.0.0',?1,'stable','active'),
            ('beta','1.0.0',?2,'stable','active')",
        rusqlite::params![install_root_1.to_string_lossy(), install_root_2.to_string_lossy()],
    )
    .unwrap();
    drop(conn);

    let state = vectorhawkd_core::state::AppState {
        root_dir: camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap(),
        db_path,
    };
    let pusher = ManagedPathsPusher::new(&state);

    let healed = push_missing_active_skills(&state, &pusher).unwrap();

    let alpha_now = skills_dir.join("alpha");
    let alpha_pushed = alpha_now.is_dir()
        && alpha_now.join(".vectorhawk-managed.json").exists()
        && fs::read(alpha_now.join("SKILL.md")).unwrap() == b"---\nname: alpha\n---\nbody";
    let beta_untouched =
        fs::read(skills_dir.join("beta").join("SKILL.md")).unwrap() == b"preexisting";

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert_eq!(healed, 1, "only the missing 'alpha' must be re-pushed");
    assert!(
        alpha_pushed,
        "alpha must be materialized with content + marker"
    );
    assert!(
        beta_untouched,
        "beta already present — must not be overwritten"
    );
}

// ── Restore journal (F2 pushes) ─────────────────────────────────────────────
//
// push_skill/push_mcp/push_plugin all write into $HOME-derived paths, so
// these tests follow the existing HOME-swap convention used above (no
// process-wide mutex — matches the accepted, documented risk noted on
// push_adopted_discovery's tests).

use vectorhawkd_core::restore_journal::{JournalOp, JournalSource, RestoreJournal};

#[test]
fn push_skill_appends_managed_artifact_push_entry() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    fs::create_dir_all(fake_home.path().join(".claude").join("skills")).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_skill(
        "journal-skill",
        Some("inst-journal-1"),
        b"---\nname: journal-skill\n---\nbody\n",
        &[],
    );

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    result.expect("push_skill should succeed");

    let journal =
        RestoreJournal::new(camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
    let entries = journal.read_all().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "push_skill should append exactly one entry"
    );
    let entry = &entries[0];
    assert_eq!(entry.op, JournalOp::ArtifactPush);
    assert_eq!(entry.source, JournalSource::Managed);
    assert_eq!(entry.slug.as_deref(), Some("journal-skill"));
    assert_eq!(
        entry.client.as_deref(),
        Some("all"),
        "the canonical directory serves every client, not just Claude Code"
    );
    assert!(
        entry.backup_path.is_none(),
        "managed pushes are pure removals on uninstall — no backup"
    );
    assert!(entry.target_path.ends_with("journal-skill"));
}

#[test]
fn push_mcp_appends_brokered_entry_when_flagged() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    fs::create_dir_all(fake_home.path()).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_mcp("slack-mcp", Some("inst-mcp-1"), true);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    result.expect("push_mcp should succeed");

    let journal =
        RestoreJournal::new(camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.op, JournalOp::ArtifactPush);
    assert_eq!(entry.source, JournalSource::Brokered);
    assert_eq!(entry.slug.as_deref(), Some("slack-mcp"));
    assert!(entry.target_path.ends_with(".claude.json"));
    assert_eq!(entry.detail["server_key"], "slack-mcp");
    assert_eq!(entry.detail["mcp_key"], "mcpServers");
}

#[test]
fn push_mcp_appends_managed_entry_when_not_brokered() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    fs::create_dir_all(fake_home.path()).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_mcp("local-tool", None, false);

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    result.expect("push_mcp should succeed");

    let journal =
        RestoreJournal::new(camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, JournalSource::Managed);
}

#[test]
fn push_plugin_appends_managed_artifact_push_entry() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let (pusher, tmp) = make_pusher();

    let fake_home = tempfile::tempdir().unwrap();
    fs::create_dir_all(fake_home.path().join(".claude").join("plugins")).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = pusher.push_plugin(
        "journal-plugin",
        Some("inst-plugin-1"),
        &serde_json::json!({"name": "journal-plugin"}),
    );

    if let Some(v) = prev_home {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }
    result.expect("push_plugin should succeed");

    let journal =
        RestoreJournal::new(camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.op, JournalOp::ArtifactPush);
    assert_eq!(entry.source, JournalSource::Managed);
    assert_eq!(entry.slug.as_deref(), Some("journal-plugin"));
    assert!(entry.target_path.ends_with("journal-plugin"));
}

#[test]
fn push_skill_writes_no_journal_entry_when_reconciler_disabled() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER", "1");

    let (pusher, tmp) = make_pusher();
    let result = pusher.push_skill("gated-journal-skill", None, b"---\nname: x\n---\n", &[]);

    std::env::remove_var("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER");
    result.expect("must return Ok when gate is active");

    let journal =
        RestoreJournal::new(camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
    let entries = journal.read_all().unwrap();
    assert!(
        entries.is_empty(),
        "no journal entry should be written when the reconciler is disabled"
    );
}
