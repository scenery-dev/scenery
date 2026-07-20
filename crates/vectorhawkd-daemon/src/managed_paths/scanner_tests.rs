#![allow(clippy::unwrap_used)]

use super::*;
use std::fs;

fn write_skill_dir(base: &std::path::Path, slug: &str, skill_md: &str) -> std::path::PathBuf {
    let dir = base.join(slug);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    dir
}

fn write_plugin_dir(base: &std::path::Path, slug: &str, manifest: &str) -> std::path::PathBuf {
    let dir = base.join(slug).join(".claude-plugin");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.json"), manifest).unwrap();
    dir.parent().unwrap().to_path_buf()
}

fn write_claude_json(base: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = base.join(".claude.json");
    fs::write(&path, content).unwrap();
    path
}

// ── Skills ────────────────────────────────────────────────────────────────────

#[test]
fn scan_skills_empty_dir_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).unwrap();
    let items = scan_skills_dir(&skills).unwrap();
    assert!(items.is_empty());
}

#[test]
fn scan_skills_nonexistent_dir_returns_empty() {
    let items = scan_skills_dir(std::path::Path::new("/tmp/does-not-exist-managed-paths")).unwrap();
    assert!(items.is_empty());
}

#[test]
fn scan_skills_one_valid_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).unwrap();
    write_skill_dir(&skills, "my-skill", "---\nname: my-skill\n---\nHelp text");

    let items = scan_skills_dir(&skills).unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.slug, "my-skill");
    assert_eq!(item.kind, ItemKind::Skill);
    assert!(!item.canonical_hash.is_empty());
    assert!(item.files.iter().any(|f| f.ends_with("SKILL.md")));
}

#[test]
fn scan_skills_missing_skill_md_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).unwrap();
    // Dir with no SKILL.md — should be skipped.
    fs::create_dir_all(skills.join("bad-skill")).unwrap();
    // Dir with SKILL.md — should be found.
    write_skill_dir(&skills, "good-skill", "---\nname: good-skill\n---");

    let items = scan_skills_dir(&skills).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].slug, "good-skill");
}

#[test]
fn scan_skills_files_in_root_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).unwrap();
    // A plain file — should be skipped (not a dir).
    fs::write(skills.join("README.md"), "hi").unwrap();

    let items = scan_skills_dir(&skills).unwrap();
    assert!(items.is_empty());
}

/// Pins the fix for the self-adoption bug: since the pivot to
/// `~/.agents/skills`, VectorHawk writes real skill content into a canonical
/// dir and leaves a *directory symlink* at `~/.claude/skills/<slug>`. The
/// fixture reproduces that exact production shape — the canonical dir holds
/// both `SKILL.md` and the `.vectorhawk-managed.json` marker, and
/// `skills_dir` holds only a symlink pointing at it — so this also proves the
/// marker is resolved *through* the link (`fs::metadata`/`Path::join` follow
/// symlinks), not just checked against the link path itself.
///
/// An unmanaged sibling skill in the same `skills_dir` is asserted present so
/// this test cannot pass against a scanner that returns nothing at all.
#[test]
fn scan_skills_skips_vectorhawk_managed_symlink_but_keeps_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).unwrap();

    // Canonical dir (simulates ~/.agents/skills/<slug>): real content + marker.
    let canonical_root = tmp.path().join("agents-skills");
    let canonical_dir = write_skill_dir(
        &canonical_root,
        "managed-skill",
        "---\nname: managed-skill\n---",
    );
    fs::write(
        canonical_dir.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        r#"{"marker_version":1,"installation_id":null,"source_sha256":"abc","migrated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Directory symlink at ~/.claude/skills/<slug> — the exact production shape.
    let link_path = skills.join("managed-skill");
    std::os::unix::fs::symlink(&canonical_dir, &link_path).unwrap();

    // Unmanaged sibling skill, a real dir directly under skills_dir.
    write_skill_dir(&skills, "native-skill", "---\nname: native-skill\n---");

    let items = scan_skills_dir(&skills).unwrap();
    let slugs: Vec<&str> = items.iter().map(|i| i.slug.as_str()).collect();

    assert_eq!(
        items.len(),
        1,
        "only the unmanaged sibling should be returned: {slugs:?}"
    );
    assert!(
        slugs.contains(&"native-skill"),
        "unmanaged sibling must still be discovered: {slugs:?}"
    );
    assert!(
        !slugs.contains(&"managed-skill"),
        "VectorHawk-managed symlink must be skipped: {slugs:?}"
    );
}

// ── Plugins ───────────────────────────────────────────────────────────────────

#[test]
fn scan_plugins_excludes_anthropic_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    // Create all three excluded dirs with plugin manifests — none should appear.
    for dir_name in ["marketplaces", "cache", "data"] {
        let plugin_dir = plugins.join(dir_name).join(".claude-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"name":"x"}"#).unwrap();
    }

    let items = scan_plugins_dir(&plugins).unwrap();
    assert!(items.is_empty());
}

#[test]
fn scan_plugins_missing_manifest_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    fs::create_dir_all(plugins.join("no-manifest")).unwrap();
    write_plugin_dir(
        &plugins,
        "has-manifest",
        r#"{"name":"has-manifest","version":"1.0.0"}"#,
    );

    let items = scan_plugins_dir(&plugins).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].slug, "has-manifest");
}

#[test]
fn scan_plugins_valid_plugin_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_plugin_dir(
        &plugins,
        "my-plugin",
        r#"{"name":"my-plugin","version":"1.0.0"}"#,
    );

    let items = scan_plugins_dir(&plugins).unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.slug, "my-plugin");
    assert_eq!(item.kind, ItemKind::Plugin);
    assert!(!item.canonical_hash.is_empty());
}

// ── Plugin marketplaces (nested layout) ─────────────────────────────────────────

/// Write a nested plugin at `<plugins>/marketplaces/<mp>/<sub>/<slug>/.claude-plugin/plugin.json`.
fn write_marketplace_plugin(
    plugins: &std::path::Path,
    mp: &str,
    sub: &str,
    slug: &str,
    manifest: &str,
) -> std::path::PathBuf {
    let dir = plugins.join("marketplaces").join(mp).join(sub).join(slug);
    let cp = dir.join(".claude-plugin");
    fs::create_dir_all(&cp).unwrap();
    fs::write(cp.join("plugin.json"), manifest).unwrap();
    dir
}

fn write_known_marketplaces(plugins: &std::path::Path, entries: &[(&str, &str)]) {
    // entries: (marketplace_name, repo)
    let mut map = serde_json::Map::new();
    for (name, repo) in entries {
        let install = plugins.join("marketplaces").join(name);
        map.insert(
            (*name).to_string(),
            serde_json::json!({
                "source": { "source": "github", "repo": repo },
                "installLocation": install.to_string_lossy(),
            }),
        );
    }
    fs::create_dir_all(plugins).unwrap();
    fs::write(
        plugins.join("known_marketplaces.json"),
        serde_json::Value::Object(map).to_string(),
    )
    .unwrap();
}

#[test]
fn marketplace_walk_no_marketplaces_dir_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    assert!(scan_plugin_marketplaces(&plugins).unwrap().is_empty());
}

#[test]
fn marketplace_walk_skips_native_adopts_custom() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_known_marketplaces(
        &plugins,
        &[
            (
                "claude-plugins-official",
                "anthropics/claude-plugins-official",
            ),
            ("acme-mp", "acme/acme-mp"),
        ],
    );

    // Anthropic-native: internal plugin of the official marketplace → skipped.
    write_marketplace_plugin(
        &plugins,
        "claude-plugins-official",
        "plugins",
        "code-review",
        r#"{"name":"code-review","author":{"name":"Anthropic"}}"#,
    );
    // Custom: external plugin within the official marketplace → adopted.
    write_marketplace_plugin(
        &plugins,
        "claude-plugins-official",
        "external_plugins",
        "github",
        r#"{"name":"github","author":{"name":"GitHub"}}"#,
    );
    // Custom: plugin in a third-party marketplace → adopted.
    write_marketplace_plugin(
        &plugins,
        "acme-mp",
        "plugins",
        "acme-tool",
        r#"{"name":"acme-tool","author":{"name":"Acme"}}"#,
    );

    let items = scan_plugin_marketplaces(&plugins).unwrap();
    let slugs: std::collections::HashSet<&str> = items.iter().map(|i| i.slug.as_str()).collect();

    assert_eq!(items.len(), 2, "got: {slugs:?}");
    assert!(slugs.contains("github"));
    assert!(slugs.contains("acme-tool"));
    assert!(
        !slugs.contains("code-review"),
        "native plugin must be skipped"
    );
    assert!(items.iter().all(|i| i.kind == ItemKind::Plugin));
}

#[test]
fn marketplace_walk_skips_already_managed() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_known_marketplaces(&plugins, &[("acme-mp", "acme/acme-mp")]);

    let pdir = write_marketplace_plugin(
        &plugins,
        "acme-mp",
        "plugins",
        "acme-tool",
        r#"{"name":"acme-tool"}"#,
    );
    // Stamp it as already VectorHawk-managed.
    fs::write(
        pdir.join(vectorhawkd_mcp::ownership::MANAGED_MARKER_FILENAME),
        "{}",
    )
    .unwrap();

    assert!(scan_plugin_marketplaces(&plugins).unwrap().is_empty());
}

// ── MCP / claude.json ─────────────────────────────────────────────────────────

#[test]
fn scan_claude_json_missing_file_returns_empty() {
    let items = scan_claude_json(std::path::Path::new("/tmp/does-not-exist.json")).unwrap();
    assert!(items.is_empty());
}

#[test]
fn scan_claude_json_no_mcp_servers_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(tmp.path(), r#"{"theme":"dark"}"#);
    let items = scan_claude_json(&path).unwrap();
    assert!(items.is_empty());
}

#[test]
fn scan_claude_json_vectorhawk_entry_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{"vectorhawk":{"command":"vectorhawk","args":["mcp","serve"]},"my-mcp":{"command":"npx","args":["my-mcp"]}}}"#,
    );
    let items = scan_claude_json(&path).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].slug, "my-mcp");
    assert_eq!(items[0].kind, ItemKind::Mcp);
}

#[test]
fn scan_claude_json_multiple_entries_all_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{"server-a":{"command":"a"},"server-b":{"command":"b"}}}"#,
    );
    let items = scan_claude_json(&path).unwrap();
    assert_eq!(items.len(), 2);
    let slugs: Vec<&str> = items.iter().map(|i| i.slug.as_str()).collect();
    assert!(slugs.contains(&"server-a"));
    assert!(slugs.contains(&"server-b"));
}

#[test]
fn scan_claude_json_each_mcp_has_unique_canonical_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{"a":{"command":"cmd-a"},"b":{"command":"cmd-b"}}}"#,
    );
    let items = scan_claude_json(&path).unwrap();
    let hashes: std::collections::HashSet<&str> =
        items.iter().map(|i| i.canonical_hash.as_str()).collect();
    // Each server has a distinct config so hashes must differ.
    assert_eq!(hashes.len(), 2);
}
