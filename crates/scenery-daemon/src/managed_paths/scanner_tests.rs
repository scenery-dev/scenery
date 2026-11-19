#![allow(clippy::unwrap_used)]

use super::*;
use std::fs;

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
    // Native: external plugin within the official marketplace → also skipped
    // (the whole official marketplace is off-limits).
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

    assert_eq!(items.len(), 1, "got: {slugs:?}");
    assert!(slugs.contains("acme-tool"));
    assert!(
        !slugs.contains("code-review"),
        "native plugin must be skipped"
    );
    assert!(
        !slugs.contains("github"),
        "external plugin in the official marketplace must be skipped"
    );
    assert!(items.iter().all(|i| i.kind == ItemKind::Plugin));
}

/// F13 (reviewer's bypass): a forged marker in a plugin dir must not hide it
/// from F1 adoption. Ownership is decided by the migrator from the DB.
#[test]
fn marketplace_walk_returns_plugin_with_forged_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_known_marketplaces(&plugins, &[("acme-mp", "acme/acme-mp")]);

    let pdir = write_marketplace_plugin(
        &plugins,
        "acme-mp",
        "external_plugins",
        "acme-tool",
        r#"{"name":"acme-tool"}"#,
    );
    fs::write(
        pdir.join(scenery_mcp::ownership::MANAGED_MARKER_FILENAME),
        "{}",
    )
    .unwrap();

    let items = scan_plugin_marketplaces(&plugins).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].slug, "acme-tool");
}

/// Scenery's own governed plugins (listed in the bundled-skills record)
/// are not offered for adoption; anything else planted in our marketplace
/// directory still is.
#[test]
fn marketplace_walk_skips_recorded_governed_plugins_only() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_known_marketplaces(&plugins, &[("scenery", "scenery/scenery")]);
    for slug in ["governed", "planted"] {
        write_marketplace_plugin(
            &plugins,
            "scenery",
            "plugins",
            slug,
            &format!(r#"{{"name":"{slug}"}}"#),
        );
    }
    let record = plugins.join("marketplaces/scenery/.scenery");
    fs::create_dir_all(&record).unwrap();
    fs::write(
        record.join("bundled-skills.json"),
        r#"{"governed":{"version":"1.0.0","skills":{}}}"#,
    )
    .unwrap();

    let items = scan_plugin_marketplaces(&plugins).unwrap();
    let slugs: Vec<&str> = items.iter().map(|i| i.slug.as_str()).collect();
    assert_eq!(slugs, vec!["planted"]);
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
fn scan_claude_json_scenery_entry_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{"scenery":{"command":"scenery","args":["mcp","serve"]},"my-mcp":{"command":"npx","args":["my-mcp"]}}}"#,
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

// ── F9: migrate payloads carry no secret values ───────────────────────────────

/// Substrings that appear only in secret positions of the fixtures below.
/// None may ever appear anywhere in a serialised migrate payload.
const SECRET_MARKERS: &[&str] = &[
    "ghp_ENVSECRET",
    "HDRSECRET",
    "ARGSECRET",
    "USERINFOSECRET",
    "PATHSECRET",
    "QUERYSECRET",
    "FRAGSECRET",
    "CMDSECRET",
    "PLUGINSECRET",
    "alice",
];

fn assert_no_secrets(payload: &serde_json::Value) {
    let s = payload.to_string();
    for m in SECRET_MARKERS {
        assert!(!s.contains(m), "payload leaked {m:?}: {s}");
    }
}

#[test]
fn scan_claude_json_payload_has_no_env_header_or_arg_values() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{
            "github":{
                "command":"/Users/alice/.nvm/versions/node/v20/bin/npx",
                "args":["-y","@modelcontextprotocol/server-github","--token","ARGSECRET"],
                "env":{"GITHUB_TOKEN":"ghp_ENVSECRET"}
            },
            "remote":{
                "type":"http",
                "url":"https://bob:USERINFOSECRET@mcp.example.com:8443/PATHSECRET?key=QUERYSECRET#FRAGSECRET",
                "headers":{"Authorization":"Bearer HDRSECRET"}
            }
        }}"#,
    );
    let items = scan_claude_json(&path).unwrap();
    assert_eq!(items.len(), 2);

    for item in &items {
        assert_no_secrets(&item.payload);
        let cfg = item.payload.get("mcp_config").expect("mcp_config key");
        let obj = cfg.as_object().unwrap();
        for k in obj.keys() {
            assert!(
                [
                    "transport",
                    "command_basename",
                    "package_identifier",
                    "url",
                    "env_keys",
                    "header_keys",
                    "arg_count"
                ]
                .contains(&k.as_str()),
                "unexpected mcp_config field {k:?}"
            );
        }
    }

    let gh = items.iter().find(|i| i.slug == "github").unwrap();
    let cfg = &gh.payload["mcp_config"];
    assert_eq!(cfg["transport"], "stdio");
    assert_eq!(cfg["command_basename"], "npx");
    assert_eq!(
        cfg["package_identifier"],
        "@modelcontextprotocol/server-github"
    );
    assert_eq!(cfg["env_keys"], serde_json::json!(["GITHUB_TOKEN"]));
    assert_eq!(cfg["arg_count"], 4);

    let remote = items.iter().find(|i| i.slug == "remote").unwrap();
    let cfg = &remote.payload["mcp_config"];
    assert_eq!(cfg["transport"], "http");
    assert_eq!(cfg["url"], "https://mcp.example.com:8443");
    assert_eq!(cfg["header_keys"], serde_json::json!(["Authorization"]));
}

/// The migrate path must not trust `Path::file_name` on an arbitrary command
/// string: a shell one-liner's "file name" is whatever follows the last `/`,
/// which can be a secret.
#[test]
fn scan_claude_json_payload_drops_unsafe_command_basename() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_claude_json(
        tmp.path(),
        r#"{"mcpServers":{
            "shell":{"command":"sh -c 'curl https://h.example/CMDSECRET'"},
            "spaced":{"command":"/opt/my tool/run CMDSECRET"}
        }}"#,
    );
    let items = scan_claude_json(&path).unwrap();
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_no_secrets(&item.payload);
        assert!(item.payload["mcp_config"]["command_basename"].is_null());
    }
}

const PLUGIN_WITH_SECRETS: &str = r#"{
    "name":"acme-tool",
    "version":"1.2.3",
    "description":"Acme tooling",
    "author":{"name":"Acme","email":"alice@acme.example"},
    "mcpServers":{"acme":{"command":"npx","args":["acme","--key","PLUGINSECRET"],
        "env":{"ACME_KEY":"PLUGINSECRET"},"headers":{"Authorization":"PLUGINSECRET"}}},
    "hooks":{"PreToolUse":[{"command":"curl https://x/PLUGINSECRET"}]}
}"#;

fn assert_plugin_payload_allowlisted(payload: &serde_json::Value) {
    assert_no_secrets(payload);
    let m = payload["manifest_json"].as_object().expect("manifest_json");
    let mut keys: Vec<&str> = m.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["description", "name", "version"]);
    assert_eq!(m["name"], "acme-tool");
    assert_eq!(m["version"], "1.2.3");
    assert_eq!(m["description"], "Acme tooling");
}

#[test]
fn scan_plugins_dir_payload_is_allowlisted() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_plugin_dir(&plugins, "acme-tool", PLUGIN_WITH_SECRETS);
    let items = scan_plugins_dir(&plugins).unwrap();
    assert_eq!(items.len(), 1);
    assert_plugin_payload_allowlisted(&items[0].payload);
}

#[test]
fn scan_plugin_marketplaces_payload_is_allowlisted() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_known_marketplaces(&plugins, &[("acme-mp", "acme/acme-mp")]);
    write_marketplace_plugin(
        &plugins,
        "acme-mp",
        "external_plugins",
        "acme-tool",
        PLUGIN_WITH_SECRETS,
    );
    let items = scan_plugin_marketplaces(&plugins).unwrap();
    assert_eq!(items.len(), 1);
    assert_plugin_payload_allowlisted(&items[0].payload);
}
