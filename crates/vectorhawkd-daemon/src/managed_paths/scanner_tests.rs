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
