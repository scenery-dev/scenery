#![allow(clippy::unwrap_used)]

use super::*;
use scenery_daemon::managed_paths::discoveries::DiscoveryItem;
use std::fs;

use crate::HOME_ENV_LOCK as ENV_MUTEX;

pub(crate) struct Fixture {
    _tmp: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) tmpdir: PathBuf,
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let tmpdir = tmp.path().join("tmpdir");
        fs::create_dir_all(&tmpdir).unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("XDG_DATA_HOME");
        for name in ["notes", "deploy"] {
            let dir = home.join(".agents/skills").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
            )
            .unwrap();
        }
        let cursor = home.join(".cursor");
        fs::create_dir_all(&cursor).unwrap();
        fs::write(
            cursor.join("mcp.json"),
            r#"{"mcpServers":{"gh":{"command":"npx","args":["-y","@x/gh"]}}}"#,
        )
        .unwrap();
        let plugin = home.join(".claude/plugins/p1");
        fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
        fs::write(
            plugin.join(".claude-plugin/plugin.json"),
            r#"{"name":"p1","version":"1.0.0","description":"d"}"#,
        )
        .unwrap();
        Self {
            _tmp: tmp,
            home,
            tmpdir,
            prev_home,
            prev_xdg,
            _lock: lock,
        }
    }

    pub(crate) fn env(&self, euid: u32) -> ScanEnv {
        ScanEnv {
            euid,
            home: self.home.clone(),
            now: OffsetDateTime::now_utc(),
            token: None,
            save_dir: self.tmpdir.clone(),
            backoff: vec![Duration::from_millis(1), Duration::from_millis(1)],
            // Tests never open a real browser.
            open_browser: |_| false,
            login_timeout: Duration::from_secs(10),
            watch_signals: false,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

/// Drive the async scan from a sync test that holds the env lock.
pub(crate) fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

pub(crate) fn args(dry_run: bool, json: bool) -> ScanArgs {
    ScanArgs {
        once: true,
        login: false,
        token_file: None,
        registry_url: "https://app.scenery.dev".to_string(),
        dry_run,
        json,
        quiet: false,
        out: None,
        from_file: None,
    }
}

pub(crate) fn item(kind: &str, slug: &str) -> DiscoveryItem {
    DiscoveryItem {
        kind: kind.to_string(),
        slug: slug.to_string(),
        source_path: "~/x".to_string(),
        canonical_hash: "h".to_string(),
        detail: None,
        local: None,
    }
}

#[test]
fn scan_refuses_root() {
    let fx = Fixture::new();
    let mut out = Vec::new();
    let code = block(run_to(args(true, false), &fx.env(0), &mut out)).unwrap();
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert!(!scenery_core::state::AppState::resolve_root_dir()
        .unwrap()
        .exists());
}

#[test]
fn scan_skips_when_runner_present() {
    let fx = Fixture::new();
    let root = scenery_core::state::AppState::resolve_root_dir().unwrap();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("state.db"), b"").unwrap();
    let mut server = mockito::Server::new();
    let session = server.mock("GET", mockito::Matcher::Any).expect(0).create();
    let reports = server
        .mock("POST", mockito::Matcher::Any)
        .expect(0)
        .create();
    let mut a = args(false, false);
    a.registry_url = server.url();
    let mut env = fx.env(501);
    env.token = Some("scn_scan_abcdef".to_string());
    let mut out = Vec::new();
    let code = block(run_to(a, &env, &mut out)).unwrap();
    assert_eq!(code, 0);
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("already installed"));
    session.assert();
    reports.assert();
}

#[test]
fn dry_run_prints_summary() {
    let fx = Fixture::new();
    let mut out = Vec::new();
    let code = block(run_to(args(true, false), &fx.env(501), &mut out)).unwrap();
    assert_eq!(code, 0);
    let text = String::from_utf8(out).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "Found 1 MCP servers, 2 skills, 1 plugins."
    );
    let rows = text
        .lines()
        .filter(|l| ["mcp", "skill", "plugin"].iter().any(|k| l.starts_with(k)))
        .count();
    assert_eq!(rows, 4);
}

#[test]
fn summary_truncates_at_50() {
    let items: Vec<DiscoveryItem> = (0..60)
        .map(|i| item("skill", &format!("s{i:02}")))
        .collect();
    let text = summary::render(&items, false);
    let rows = text.lines().filter(|l| l.starts_with("skill")).count();
    assert_eq!(rows, 50);
    assert!(text.contains("… and 10 more"));
    assert_eq!(summary::render(&items, true).lines().count(), 1);
}

#[test]
fn json_matches_wire_shape() {
    let fx = Fixture::new();
    let mut out = Vec::new();
    let code = block(run_to(args(true, true), &fx.env(501), &mut out)).unwrap();
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["format"], "scenery-scan-report/1");
    let id = v["device"]["scan_identity"].as_str().unwrap();
    assert_eq!(id.len(), 64);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(v["device"]["platform"], std::env::consts::OS);
    assert_eq!(v["device"]["arch"], std::env::consts::ARCH);
    // dry run never carries MCP args
    let mcp = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "mcp")
        .unwrap();
    assert!(mcp["detail"].get("args").is_none());
}

#[test]
fn save_is_private() {
    let fx = Fixture::new();
    let rep = report::build(vec![item("skill", "a")], fx.env(501).now).unwrap();
    let path = fx.home.join("r.json");
    report::save(&rep, Some(&path)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(report::save(&rep, Some(&path)).is_err());
}
