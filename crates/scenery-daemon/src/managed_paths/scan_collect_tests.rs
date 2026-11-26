//! `scenery scan --once` collector: every kind, masked paths, no secrets,
//! the daemon's MCP rules, and no footprint on disk.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::managed_paths::discoveries::{collect_all_discoveries, extra_roots};
use crate::managed_paths::ENV_MUTEX;
use scenery_core::state::AppState;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

const REGISTRY: &str = "https://app.scenery.dev";
const TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

/// A temp `HOME` holding the brief's fixture. Holds `ENV_MUTEX` and restores
/// `HOME` / `XDG_DATA_HOME` on drop.
struct Fixture {
    tmp: tempfile::TempDir,
    home: PathBuf,
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &home);
        // The state dir must resolve under this HOME on Linux too.
        std::env::remove_var("XDG_DATA_HOME");

        skill(&home.join(".claude/skills"), "notes");
        skill(&home.join(".agents/skills"), "deploy");
        let cursor = home.join(".cursor");
        fs::create_dir_all(&cursor).unwrap();
        let mcp = serde_json::json!({"mcpServers": {"gh": {
            "command": "npx",
            "args": ["-y", "@x/gh", "--token", TOKEN],
            "env": {"GH_TOKEN": "secret-value"}
        }}});
        fs::write(cursor.join("mcp.json"), mcp.to_string()).unwrap();
        plugin(&home.join(".claude/plugins/p1"), "p1");

        Self {
            tmp,
            home,
            prev_home,
            prev_xdg,
            _lock: lock,
        }
    }

    fn collect(&self, include_args: bool) -> Vec<DiscoveryItem> {
        collect_for_scan(&self.home, REGISTRY, include_args).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        restore("HOME", &self.prev_home);
        restore("XDG_DATA_HOME", &self.prev_xdg);
    }
}

fn restore(key: &str, prev: &Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

fn skill(root: &Path, slug: &str) {
    let d = root.join(slug);
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("SKILL.md"),
        format!("---\nname: {slug}\n---\nbody\n"),
    )
    .unwrap();
}

fn plugin(dir: &Path, name: &str) {
    let m = dir.join(".claude-plugin");
    fs::create_dir_all(&m).unwrap();
    let manifest = serde_json::json!({
        "name": name,
        "version": "1.2.3",
        "description": "a plugin",
        "author": {"email": "someone@example.com"},
        "mcpServers": {"x": {"command": "x", "env": {"K": "secret-value"}}}
    });
    fs::write(m.join("plugin.json"), manifest.to_string()).unwrap();
}

fn kinds(items: &[DiscoveryItem], kind: &str) -> Vec<DiscoveryItem> {
    items.iter().filter(|i| i.kind == kind).cloned().collect()
}

#[test]
fn collects_all_kinds() {
    let fx = Fixture::new();
    let items = fx.collect(false);
    let mut skills: Vec<String> = kinds(&items, "skill").into_iter().map(|i| i.slug).collect();
    skills.sort();
    assert_eq!(skills, vec!["deploy".to_string(), "notes".to_string()]);
    assert_eq!(kinds(&items, "mcp").len(), 1, "{items:?}");
    let plugins = kinds(&items, "plugin");
    assert_eq!(plugins.len(), 1, "{items:?}");
    let p = &plugins[0];
    assert_eq!(p.slug, "p1");
    assert_eq!(
        p.detail,
        Some(serde_json::json!({
            "name": "p1",
            "version": "1.2.3",
            "description": "a plugin",
            "marketplace": null
        }))
    );
    assert!(items.iter().all(|i| i.local.is_none()));
}

#[test]
fn nested_plugin_carries_its_marketplace() {
    let fx = Fixture::new();
    let plugins = fx.home.join(".claude/plugins");
    // Without the marketplace registry every marketplace reads as native.
    let known =
        serde_json::json!({"acme": {"source": {"source": "github", "repo": "acme/plugins"}}});
    fs::write(plugins.join("known_marketplaces.json"), known.to_string()).unwrap();
    plugin(&plugins.join("marketplaces/acme/plugins/p2"), "p2");
    let items = fx.collect(false);
    let p2 = items.iter().find(|i| i.slug == "p2").unwrap();
    assert_eq!(p2.kind, "plugin");
    let d = p2.detail.as_ref().unwrap();
    assert_eq!(d["marketplace"], "acme");
    assert_eq!(d["name"], "p2");
}

#[test]
fn masks_paths() {
    let fx = Fixture::new();
    let abs = fx.tmp.path().to_string_lossy().to_string();
    let items = fx.collect(true);
    assert!(!items.is_empty());
    for i in &items {
        assert!(i.source_path.starts_with("~/"), "{}", i.source_path);
        assert!(!i.source_path.contains(&abs), "{}", i.source_path);
    }
    let json = serde_json::to_string(&items).unwrap();
    assert!(!json.contains(&abs), "absolute temp path leaked: {json}");
}

#[test]
fn no_secret_values() {
    let fx = Fixture::new();
    for include_args in [false, true] {
        let items = fx.collect(include_args);
        let json = serde_json::to_string(&items).unwrap();
        assert!(!json.contains(TOKEN), "token leaked: {json}");
        assert!(!json.contains("ghp_abc"), "token prefix leaked: {json}");
        assert!(!json.contains("secret-value"), "env value leaked: {json}");
        assert!(!json.contains("someone@example.com"), "{json}");
        let mcp = kinds(&items, "mcp");
        let detail = mcp[0].detail.as_ref().unwrap();
        assert_eq!(detail.get("args").is_some(), include_args, "{detail}");
    }
}

#[test]
fn matches_daemon_collector() {
    let fx = Fixture::new();
    let data = camino::Utf8PathBuf::from_path_buf(fx.tmp.path().join("data")).unwrap();
    let state = AppState::bootstrap_in(data).unwrap();
    let daemon = collect_all_discoveries(
        &extra_roots(),
        &scenery_mcp::setup::detect_ai_clients(),
        &state,
        REGISTRY,
    );
    let daemon_mcp = kinds(&daemon, "mcp");
    let scan_mcp = kinds(&fx.collect(false), "mcp");
    assert_eq!(daemon_mcp.len(), 1);
    assert_eq!(scan_mcp.len(), 1);
    assert_eq!(scan_mcp[0].detail, daemon_mcp[0].detail);
    assert_eq!(scan_mcp[0].canonical_hash, daemon_mcp[0].canonical_hash);
    assert_eq!(scan_mcp[0].source_path, daemon_mcp[0].source_path);
}

type Snapshot = BTreeMap<PathBuf, (u64, Option<SystemTime>)>;

fn snapshot(dir: &Path, out: &mut Snapshot) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let m = fs::symlink_metadata(&p).unwrap();
        out.insert(p.clone(), (m.len(), m.modified().ok()));
        if m.is_dir() {
            snapshot(&p, out);
        }
    }
}

#[test]
fn writes_nothing() {
    let fx = Fixture::new();
    let mut before = Snapshot::new();
    snapshot(&fx.home, &mut before);
    for include_args in [false, true] {
        fx.collect(include_args);
    }
    let mut after = Snapshot::new();
    snapshot(&fx.home, &mut after);
    assert_eq!(before, after);
    let root = AppState::resolve_root_dir().unwrap();
    assert!(
        root.as_std_path().starts_with(&fx.home),
        "state dir {root} not under the test HOME"
    );
    assert!(!root.exists(), "{root} was created");
}

#[test]
fn ephemeral_connection_is_in_memory() {
    let conn = scenery_core::state::ephemeral_schema_connection().unwrap();
    assert_eq!(conn.path(), Some(""), "must not be backed by a file");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM managed_path_markers", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);
}

fn mcp_item(args: Vec<String>) -> DiscoveryItem {
    DiscoveryItem {
        kind: "mcp".into(),
        slug: "c:k".into(),
        source_path: "~/.cursor/mcp.json#k".into(),
        canonical_hash: "h".into(),
        detail: Some(serde_json::json!({"args": args, "arg_count": 1})),
        local: None,
    }
}

#[test]
fn caps_args_to_backend_limits() {
    let long = "a".repeat(MAX_ARG_BYTES + 100);
    let mut many: Vec<String> = (0..MAX_ARGS + 10).map(|i| format!("x{i}")).collect();
    many[0] = long;
    let mut items = vec![mcp_item(many)];
    enforce_limits(&mut items, MAX_REPORT_ITEMS, MAX_ITEMS_BYTES);
    let args = items[0].detail.as_ref().unwrap()["args"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(args.len(), MAX_ARGS);
    assert!(args
        .iter()
        .all(|a| a.as_str().unwrap().len() <= MAX_ARG_BYTES));
}

#[test]
fn caps_item_count_and_body_size() {
    let mut items: Vec<DiscoveryItem> = (0..10).map(|_| mcp_item(vec!["y".repeat(1000)])).collect();
    enforce_limits(&mut items, 5, usize::MAX);
    assert_eq!(items.len(), 5);

    // Over the byte budget: args are dropped first, before any item is.
    let budget = serde_json::to_vec(&items).unwrap().len() - 1;
    enforce_limits(&mut items, 5, budget);
    assert_eq!(items.len(), 5);
    assert!(items
        .iter()
        .all(|i| i.detail.as_ref().unwrap().get("args").is_none()));

    // Still over: items are dropped from the end.
    let one = serde_json::to_vec(&items[..1]).unwrap().len();
    enforce_limits(&mut items, 5, one + 10);
    assert_eq!(items.len(), 1);
}
