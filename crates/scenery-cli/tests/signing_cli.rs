//! End-to-end CLI checks for the 2.2.0 trust and approval commands, run
//! against the real `scenery` binary with an isolated HOME.
#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use camino::Utf8PathBuf;
use scenery_core::state::AppState;
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

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn trust_reset_needs_a_terminal() {
    let env = Env::new();
    let out = env.run(&["trust", "reset", "https://registry.example"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("controlling terminal"));
}

#[test]
fn device_approval_commands_are_gone() {
    let env = Env::new();
    for args in [
        &["pending"][..],
        &["approve", "GitHub"],
        &["deny", "GitHub"],
        &["approvals", "list"],
        &["approvals", "revoke", "abcd"],
    ] {
        let out = env.run(args);
        assert!(!out.status.success(), "{args:?} must not exist");
        assert!(
            text(&out.stderr).contains("unrecognized subcommand"),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
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
