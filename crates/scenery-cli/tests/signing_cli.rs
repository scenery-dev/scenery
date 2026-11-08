//! End-to-end CLI checks for the 2.2.0 trust and approval commands, run
//! against the real `scenery` binary with an isolated HOME.
#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use camino::Utf8PathBuf;
use scenery_core::state::{AppState, McpInstallRow};
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};

struct Env {
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn state_dir(&self) -> Utf8PathBuf {
        let base = if cfg!(target_os = "macos") {
            self.home.path().join("Library/Application Support")
        } else {
            self.home.path().join("xdg")
        };
        Utf8PathBuf::from_path_buf(base.join("Scenery")).unwrap()
    }

    fn state(&self) -> AppState {
        AppState::bootstrap_in(self.state_dir()).unwrap()
    }

    /// Run the binary with no controlling terminal (new session), exactly
    /// like a daemon or an AI-client subprocess.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_scenery"));
        cmd.args(args)
            .env("HOME", self.home.path())
            .env("XDG_DATA_HOME", self.home.path().join("xdg"))
            .env_remove("SCENERY_REGISTRY_URL")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: `setsid` is async-signal-safe and runs in the forked child
        // before exec; it only detaches the controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.output().unwrap()
    }
}

fn seed_pending(state: &AppState, name: &str) {
    let cfg = r#"{"command":"npx","args":["-y","pkg"],"env":{"TOKEN":"x"}}"#;
    let def = scenery_core::approvals::definition_from_config(&serde_json::from_str(cfg).unwrap())
        .unwrap();
    let row = McpInstallRow {
        mcp_server_id: format!("srv-{name}"),
        installation_id: format!("00000000-0000-0000-0000-00000000000{}", name.len()),
        mcp_server_name: name.to_string(),
        package_source: "npm".into(),
        version_pin: None,
        server_config: Some(cfg.into()),
        auth_type: "none".into(),
        gateway_server_id: None,
        gateway_url: None,
    };
    state
        .upsert_mcp_install_gated(&row, Some(&def.approval_hash()), Some("pending"), None)
        .unwrap();
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn approve_refuses_without_a_controlling_terminal_and_approves_nothing() {
    let env = Env::new();
    let state = env.state();
    seed_pending(&state, "GitHub");

    let out = env.run(&["approve", "GitHub"]);
    assert!(!out.status.success(), "must fail: {}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("controlling terminal"),
        "stderr: {}",
        text(&out.stderr)
    );
    assert!(scenery_core::approvals::list_approvals(&state)
        .unwrap()
        .is_empty());
    assert_eq!(
        scenery_core::approvals::list_pending(&state).unwrap().len(),
        1
    );

    // There is no --yes bypass.
    let out = env.run(&["approve", "GitHub", "--yes"]);
    assert!(!out.status.success());
    assert!(scenery_core::approvals::list_approvals(&state)
        .unwrap()
        .is_empty());
}

#[test]
fn revoke_and_trust_reset_also_need_a_terminal() {
    let env = Env::new();
    let state = env.state();
    let def = scenery_core::approvals::StdioDefinition::new("npx".into(), vec![], vec![]);
    scenery_core::approvals::insert_approval(&state, &def, "x").unwrap();
    let hash = def.approval_hash();
    let out = env.run(&["approvals", "revoke", &hash[..8]]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("controlling terminal"));
    assert!(scenery_core::approvals::is_approved(&state, &hash).unwrap());

    let out = env.run(&["trust", "reset", "https://registry.example"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("controlling terminal"));
}

#[test]
fn pending_lists_the_exact_command_and_signer_free_servers() {
    let env = Env::new();
    seed_pending(&env.state(), "GitHub");
    let out = env.run(&["pending"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    for needle in ["GitHub", "npx -y pkg", "TOKEN", "scenery approve"] {
        assert!(s.contains(needle), "{needle} missing from:\n{s}");
    }
    // Env VALUES are never printed.
    assert!(!s.contains("\"x\""));
}

#[test]
fn deny_drops_the_pending_entry() {
    let env = Env::new();
    let state = env.state();
    seed_pending(&state, "GitHub");
    let out = env.run(&["deny", "GitHub", "--registry-url", "http://127.0.0.1:9"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(scenery_core::approvals::list_pending(&state)
        .unwrap()
        .is_empty());
    assert!(state.list_mcp_installs().unwrap().is_empty());
}

#[test]
fn non_https_and_pathful_registry_urls_are_refused() {
    let env = Env::new();
    for url in [
        "http://evil.example",
        "https://evil.example/x",
        "ftp://x",
        "https://u:p@x.example",
    ] {
        let out = env.run(&["doctor", "--registry-url", url]);
        assert!(!out.status.success(), "{url} must be refused");
        assert!(
            text(&out.stderr).to_lowercase().contains("registry url"),
            "{url}: {}",
            text(&out.stderr)
        );
    }
    // Via the environment too (a poisoned shell profile or `.envrc`).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scenery"));
    cmd.args(["doctor"])
        .env("HOME", env.home.path())
        .env("XDG_DATA_HOME", env.home.path().join("xdg"))
        .env("SCENERY_REGISTRY_URL", "http://evil.example")
        .stdin(Stdio::null());
    assert!(!cmd.output().unwrap().status.success());
}

#[test]
fn trust_pin_requires_a_matching_fingerprint() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let env = Env::new();
    let key = scenery_core::signed::testing::trusted(3, "scn-x-a");
    let mut server = mockito::Server::new();
    let origin = server.url();
    let _m = server
        .mock("GET", "/api/runner/signing-keys")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({"iss": origin, "keys": [
                {"kid": "scn-x-a", "alg": "ed25519", "public_key": URL_SAFE_NO_PAD.encode(key.pubkey), "status": "active"}
            ]})
            .to_string(),
        )
        .expect_at_least(2)
        .create();

    let bad = env.run(&["trust", "pin", &origin, "--fingerprint", &"0".repeat(64)]);
    assert!(!bad.status.success());
    assert!(scenery_core::trust::keys_for_origin(&env.state(), &origin).is_empty());

    let fp = scenery_core::trust::fingerprint(&key.pubkey);
    let ok = env.run(&["trust", "pin", &origin, "--fingerprint", &fp]);
    assert!(ok.status.success(), "{}", text(&ok.stderr));
    let keys = scenery_core::trust::keys_for_origin(&env.state(), &origin);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].pubkey, key.pubkey);
}
