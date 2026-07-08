//! Unit tests for the ownership / origin classifier.

use super::*;
use serde_json::json;
use std::collections::BTreeMap;

fn marketplaces(entries: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    for (k, v) in entries {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

fn official_entry(install_location: &str) -> Value {
    json!({
        "source": { "source": "github", "repo": ANTHROPIC_OFFICIAL_REPO },
        "installLocation": install_location,
    })
}

// ── Constants / trivial predicates ──────────────────────────────────────────────

#[test]
fn excluded_plugin_dirs_match() {
    assert!(is_excluded_plugin_dir("marketplaces"));
    assert!(is_excluded_plugin_dir("cache"));
    assert!(is_excluded_plugin_dir("data"));
    assert!(!is_excluded_plugin_dir("my-plugin"));
}

#[test]
fn only_vectorhawk_key_is_owned() {
    assert!(is_vectorhawk_mcp_key("vectorhawk"));
    assert!(!is_vectorhawk_mcp_key("slack"));
    assert!(!is_vectorhawk_mcp_key("Vectorhawk"));
}

#[test]
fn marker_presence_decides_vectorhawk_managed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!is_vectorhawk_managed(dir.path()));
    std::fs::write(dir.path().join(MANAGED_MARKER_FILENAME), "{}").unwrap();
    assert!(is_vectorhawk_managed(dir.path()));
}

// ── marketplace.json entry parsing ──────────────────────────────────────────────

#[test]
fn relative_source_detected() {
    assert!(plugin_source_is_relative(
        &json!({ "source": "./plugins/ralph-loop" })
    ));
    assert!(!plugin_source_is_relative(&json!({
        "source": { "source": "git-subdir", "url": "https://github.com/x/y.git" }
    })));
    assert!(!plugin_source_is_relative(
        &json!({ "source": "plugins/foo" })
    ));
    assert!(!plugin_source_is_relative(&json!({})));
}

#[test]
fn anthropic_author_detected() {
    assert!(author_is_anthropic(
        &json!({ "author": { "name": "Anthropic" } })
    ));
    assert!(!author_is_anthropic(
        &json!({ "author": { "name": "42Crunch" } })
    ));
    assert!(!author_is_anthropic(&json!({})));
}

#[test]
fn official_marketplace_lookup_is_pure() {
    let mkts = marketplaces(&[
        (
            "claude-plugins-official",
            official_entry("/x/claude-plugins-official"),
        ),
        (
            "acme-mp",
            json!({ "source": { "source": "github", "repo": "acme/acme-mp" } }),
        ),
    ]);
    assert!(is_official_marketplace_in("claude-plugins-official", &mkts));
    assert!(!is_official_marketplace_in("acme-mp", &mkts));
    assert!(!is_official_marketplace_in("missing", &mkts));
}

// ── classify_plugin_with (the tri-state core) ───────────────────────────────────

#[test]
fn anthropic_bundled_plugin_is_native() {
    let entry = json!({
        "name": "ralph-loop",
        "author": { "name": "Anthropic", "email": "support@anthropic.com" },
        "source": "./plugins/ralph-loop",
    });
    assert_eq!(
        classify_plugin_with(true, &entry, false),
        Origin::AnthropicNative
    );
}

#[test]
fn external_plugin_in_official_marketplace_is_custom() {
    // Lives in the official marketplace, but external source + non-Anthropic author.
    let entry = json!({
        "name": "42crunch-api-security-testing",
        "author": { "name": "42Crunch" },
        "source": { "source": "git-subdir", "url": "https://github.com/42Crunch-AI/x.git" },
    });
    assert_eq!(classify_plugin_with(true, &entry, false), Origin::Custom);
}

#[test]
fn plugin_in_third_party_marketplace_is_custom() {
    // Even a relative source + Anthropic-looking author is Custom if the
    // marketplace is not the official one.
    let entry = json!({
        "name": "spoofed",
        "author": { "name": "Anthropic" },
        "source": "./plugins/spoofed",
    });
    assert_eq!(classify_plugin_with(false, &entry, false), Origin::Custom);
}

#[test]
fn marker_wins_over_native_signals() {
    let entry = json!({
        "author": { "name": "Anthropic" },
        "source": "./plugins/x",
    });
    assert_eq!(
        classify_plugin_with(true, &entry, true),
        Origin::VectorHawkManaged
    );
}

// ── is_anthropic_native_in (mutation guard core) ────────────────────────────────

#[test]
fn native_path_protects_official_internal_plugins_tree() {
    let plugins = std::path::Path::new("/home/u/.claude/plugins");
    let install = "/home/u/.claude/plugins/marketplaces/claude-plugins-official";
    let mkts = marketplaces(&[("claude-plugins-official", official_entry(install))]);

    let internal = std::path::Path::new(
        "/home/u/.claude/plugins/marketplaces/claude-plugins-official/plugins/code-review",
    );
    assert!(is_anthropic_native_in(internal, plugins, Some(&mkts)));

    // external_plugins within the same official marketplace is adoptable, not native.
    let external = std::path::Path::new(
        "/home/u/.claude/plugins/marketplaces/claude-plugins-official/external_plugins/github",
    );
    assert!(!is_anthropic_native_in(external, plugins, Some(&mkts)));
}

#[test]
fn native_path_protects_cache_and_data_plumbing() {
    let plugins = std::path::Path::new("/home/u/.claude/plugins");
    let mkts = marketplaces(&[]);
    assert!(is_anthropic_native_in(
        std::path::Path::new("/home/u/.claude/plugins/cache/foo"),
        plugins,
        Some(&mkts),
    ));
    assert!(is_anthropic_native_in(
        std::path::Path::new("/home/u/.claude/plugins/data/bar"),
        plugins,
        Some(&mkts),
    ));
}

#[test]
fn skills_dir_paths_are_not_native() {
    let plugins = std::path::Path::new("/home/u/.claude/plugins");
    let mkts = marketplaces(&[]);
    assert!(!is_anthropic_native_in(
        std::path::Path::new("/home/u/.claude/skills/my-skill"),
        plugins,
        Some(&mkts),
    ));
}

#[test]
fn third_party_marketplace_tree_is_not_native() {
    let plugins = std::path::Path::new("/home/u/.claude/plugins");
    let mkts = marketplaces(&[(
        "acme-mp",
        json!({
            "source": { "source": "github", "repo": "acme/acme-mp" },
            "installLocation": "/home/u/.claude/plugins/marketplaces/acme-mp",
        }),
    )]);
    assert!(!is_anthropic_native_in(
        std::path::Path::new("/home/u/.claude/plugins/marketplaces/acme-mp/plugins/thing"),
        plugins,
        Some(&mkts),
    ));
}

#[test]
fn fail_safe_when_marketplaces_unreadable() {
    let plugins = std::path::Path::new("/home/u/.claude/plugins");
    // No marketplace map available → any marketplaces/ path is treated as native.
    assert!(is_anthropic_native_in(
        std::path::Path::new("/home/u/.claude/plugins/marketplaces/anything/plugins/x"),
        plugins,
        None,
    ));
}

// ── native_mcp_keys_at ──────────────────────────────────────────────────────────

#[test]
fn native_mcp_keys_excludes_vectorhawk_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude.json");
    std::fs::write(
        &path,
        json!({
            "mcpServers": {
                "vectorhawk": { "command": "vectorhawk" },
                "slack": { "command": "npx" },
                "jira": { "command": "npx" },
            }
        })
        .to_string(),
    )
    .unwrap();

    let keys = native_mcp_keys_at(&path);
    let expected: BTreeMap<String, ()> = [("slack".to_string(), ()), ("jira".to_string(), ())]
        .into_iter()
        .collect();
    assert_eq!(keys.len(), 2);
    for k in expected.keys() {
        assert!(keys.contains(k), "missing key {k}");
    }
    assert!(!keys.contains("vectorhawk"));
}

#[test]
fn native_mcp_keys_fail_open_on_missing_or_malformed() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file.
    assert!(native_mcp_keys_at(&dir.path().join("nope.json")).is_empty());
    // Malformed file.
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    assert!(native_mcp_keys_at(&bad).is_empty());
}

// ── ensure_not_native guard ─────────────────────────────────────────────────────

#[test]
fn ensure_not_native_allows_ordinary_paths() {
    // A path outside any Claude marketplace tree is never native → Ok.
    let dir = tempfile::tempdir().unwrap();
    assert!(ensure_not_native(dir.path()).is_ok());
    assert!(ensure_not_native(&dir.path().join("skills").join("my-skill")).is_ok());
}
