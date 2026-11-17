//! Policy-gated, redacted `args` in the MCP discovery detail (plan 4.2.1).
#![allow(clippy::unwrap_used)]

use super::*;
use camino::Utf8PathBuf;
use scenery_core::state::AutoManagePolicy;
use scenery_mcp::setup::ConfigFormat;
use std::fs;

fn state() -> (AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
    (AppState::bootstrap_in(root).unwrap(), tmp)
}

fn client(dir: &std::path::Path, servers: serde_json::Value) -> ClientConfig {
    fs::create_dir_all(dir).unwrap();
    let config_path = dir.join("config.json");
    fs::write(
        &config_path,
        serde_json::to_string(&serde_json::json!({ "mcpServers": servers })).unwrap(),
    )
    .unwrap();
    ClientConfig {
        name: "Cursor".into(),
        config_path,
        mcp_key: "mcpServers".into(),
        already_configured: false,
        format: ConfigFormat::Json,
    }
}

fn policy(state: &AppState, submit: bool) {
    state
        .cache_auto_manage_policy(&AutoManagePolicy {
            auto_manage_skills: true,
            auto_submit_mcp: submit,
        })
        .unwrap();
}

fn servers() -> serde_json::Value {
    serde_json::json!({
        "everything": {
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-everything@1.2.3", "--token", "ARGSECRET-opaque"],
            "env": {"E2E_SECRET": "SCN_E2E_9f3a"}
        },
        "local": {"command": "/Users/alice/bin/my-server", "args": ["--key", "ARGSECRET-two", "--port", "9000"]},
        "remote": {"url": "https://user:URLSECRET@mcp.example.com/path?k=URLSECRET2"}
    })
}

fn by_key<'a>(items: &'a [McpDiscoveryItem], key: &str) -> &'a McpDiscoveryItem {
    items.iter().find(|i| i.detail.server_key == key).unwrap()
}

#[test]
fn args_are_withheld_until_the_backend_says_auto_submit_is_on() {
    let tmp = tempfile::tempdir().unwrap();
    let c = client(tmp.path(), servers());
    let (st, _g) = state();

    // Never told: the first batch goes without args.
    let items = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    assert!(items.iter().all(|i| i.detail.args.is_none()));
    // But the non-secret shape fields are there.
    assert_eq!(
        by_key(&items, "everything").detail.source_kind.as_deref(),
        Some("public_package")
    );
    assert_eq!(
        by_key(&items, "local").detail.source_kind.as_deref(),
        Some("local_path")
    );
    assert_eq!(
        by_key(&items, "remote").detail.source_kind.as_deref(),
        Some("remote_url")
    );
    assert_eq!(by_key(&items, "everything").detail.arg_count, 4);

    // Told "off": still none.
    policy(&st, false);
    let items = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    assert!(items.iter().all(|i| i.detail.args.is_none()));

    // Told "on": redacted args appear.
    policy(&st, true);
    let items = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    assert_eq!(
        by_key(&items, "everything").detail.args.as_deref(),
        Some(
            &[
                "-y",
                "@modelcontextprotocol/server-everything@1.2.3",
                "--token",
                "<redacted>"
            ]
            .map(String::from)[..]
        )
    );
    assert_eq!(
        by_key(&items, "local").detail.args.as_deref(),
        Some(&["--key", "<redacted>", "--port", "9000"].map(String::from)[..])
    );
    // A remote server has no command, so no args.
    assert!(by_key(&items, "remote").detail.args.is_none());
}

#[test]
fn no_secret_value_appears_anywhere_in_the_serialized_batch_policy_on_or_off() {
    let tmp = tempfile::tempdir().unwrap();
    let c = client(tmp.path(), servers());
    let (st, _g) = state();
    for submit in [None, Some(false), Some(true)] {
        if let Some(on) = submit {
            policy(&st, on);
        }
        let items =
            collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
        let wire = serde_json::to_string(&items).unwrap();
        for secret in ["SCN_E2E_9f3a", "ARGSECRET", "URLSECRET", "alice"] {
            assert!(
                !wire.contains(secret),
                "{secret} leaked (policy {submit:?}): {wire}"
            );
        }
        // Env var NAMES are sent, values never.
        assert!(wire.contains("E2E_SECRET"));
    }
}

#[test]
fn a_package_is_split_for_the_registry_fetch_and_a_command_string_is_handled() {
    let tmp = tempfile::tempdir().unwrap();
    let c = client(
        tmp.path(),
        serde_json::json!({
            "pinned": {"command": "npx", "args": ["-y", "@scope/pkg@2.0.0"]},
            "oneliner": {"command": "npx -y @scope/other --api-key sk-ant-abcdefghijklmnopqrstuvwx"}
        }),
    );
    let (st, _g) = state();
    policy(&st, true);
    let items = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    let p = by_key(&items, "pinned").detail.package.clone().unwrap();
    assert_eq!(
        (p.ecosystem.as_str(), p.name.as_str(), p.version.as_deref()),
        ("npm", "@scope/pkg", Some("2.0.0"))
    );

    let one = &by_key(&items, "oneliner").detail;
    assert_eq!(one.command.as_deref(), Some("npx"));
    assert_eq!(
        one.args.as_deref(),
        Some(&["-y", "@scope/other", "--api-key", "<redacted>"].map(String::from)[..])
    );
    assert!(!serde_json::to_string(&items).unwrap().contains("sk-ant"));
}

#[test]
fn a_cached_policy_flip_changes_the_canonical_hash_only_by_adding_args() {
    let tmp = tempfile::tempdir().unwrap();
    let c = client(
        tmp.path(),
        serde_json::json!({"s": {"command": "npx", "args": ["-y", "pkg"]}}),
    );
    let (st, _g) = state();
    let before = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    policy(&st, true);
    let after = collect_mcp_discoveries(std::slice::from_ref(&c), &st, "https://app.scenery.dev");
    assert_eq!(
        before[0].source_path, after[0].source_path,
        "same item, not a new one"
    );
    assert_ne!(before[0].canonical_hash, after[0].canonical_hash);
}
