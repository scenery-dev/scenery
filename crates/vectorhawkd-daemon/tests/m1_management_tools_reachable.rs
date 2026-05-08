//! GAP-01 regression test — management tools must be reachable through the daemon.
//!
//! Before this fix, `RealBackend::list_tools` returned only the stub echo/ping
//! tools from `BackendRegistry`. The `vectorhawk_*` management tools defined in
//! `vectorhawkd_mcp::tools::build_tool_list` were never called, so AI clients
//! never saw them.
//!
//! This test:
//!   1. Spawns `vectorhawkd` and `vectorhawkd-shim`.
//!   2. Performs an `initialize` + `tools/list` round-trip via the shim.
//!   3. Asserts that ALL expected `vectorhawk_*` management tools are present.
//!   4. Sends `tools/call` with `vectorhawk_list` and asserts no error.
//!
//! # Running
//!
//! Marked `#[ignore]` — requires pre-built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m1_management_tools_reachable \
//!     -- --include-ignored --nocapture
//! ```

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn release_bin(name: &str) -> PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo test");
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .expect("daemon crate should have a parent")
        .parent()
        .expect("crates/ should have a parent (workspace root)")
        .to_path_buf();
    workspace_root.join("target").join("release").join(name)
}

fn daemon_socket_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("vectorhawk").join("agent.sock");
        }
    }
    let data_dir = dirs::data_dir().expect("dirs::data_dir() should succeed");
    data_dir.join("VectorHawk").join("agent.sock")
}

fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn remove_socket_if_present(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn send_rpc(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut std::io::BufReader<std::process::ChildStdout>,
    request: Value,
) -> Value {
    use std::io::{BufRead, Write};
    let payload = serde_json::to_string(&request).unwrap();
    writeln!(stdin, "{payload}").expect("write to shim stdin");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read from shim stdout");
    serde_json::from_str(line.trim()).expect("shim stdout should be valid JSON")
}

fn kill_stale_daemon() {
    let _ = Command::new("pkill").args(["-x", "vectorhawkd"]).status();
    std::thread::sleep(Duration::from_millis(300));
}

/// GAP-01 regression: all `vectorhawk_*` management tools must appear in
/// `tools/list` and `vectorhawk_list` must be callable without error.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn gap01_management_tools_present_and_callable() {
    let daemon_bin = release_bin("vectorhawkd");
    let shim_bin = release_bin("vectorhawkd-shim");

    assert!(
        daemon_bin.exists(),
        "daemon binary not found at {daemon_bin:?} — run cargo build --workspace --release"
    );
    assert!(
        shim_bin.exists(),
        "shim binary not found at {shim_bin:?} — run cargo build --workspace --release"
    );

    let socket_path = daemon_socket_path();
    kill_stale_daemon();
    remove_socket_if_present(&socket_path);

    let mut daemon = Command::new(&daemon_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd");

    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    let mut shim = Command::new(&shim_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd-shim");

    let mut stdin = shim.stdin.take().expect("shim stdin");
    let stdout_raw = shim.stdout.take().expect("shim stdout");
    let mut stdout = std::io::BufReader::new(stdout_raw);

    // ── Initialize ────────────────────────────────────────────────────────────

    let init_resp = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "gap01-test", "version": "0.0.1" }
            }
        }),
    );
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert!(
        init_resp.get("result").is_some() && init_resp.get("error").is_none(),
        "initialize must succeed; got: {init_resp}"
    );

    // ── tools/list — assert management tools are present ─────────────────────

    let list_resp = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    assert_eq!(list_resp["id"], 2);
    assert!(
        list_resp.get("result").is_some() && list_resp.get("error").is_none(),
        "tools/list must succeed; got: {list_resp}"
    );

    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tools array");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // GAP-01: all management tools must be present
    let required_tools = [
        "vectorhawk_list",
        "vectorhawk_search",
        "vectorhawk_install",
        "vectorhawk_info",
        "vectorhawk_validate",
        "vectorhawk_import",
        "vectorhawk_uninstall",
        "vectorhawk_update",
    ];
    // These are only present when logged in; just assert the others
    for required in &required_tools {
        assert!(
            tool_names.contains(required),
            "GAP-01: expected tool '{required}' in tools/list but got: {tool_names:?}"
        );
    }

    // ── tools/call vectorhawk_list — must return without error ───────────────

    let call_resp = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "vectorhawk_list",
                "arguments": {}
            }
        }),
    );
    assert_eq!(call_resp["id"], 3);
    assert!(
        call_resp.get("result").is_some() && call_resp.get("error").is_none(),
        "vectorhawk_list call must succeed; got: {call_resp}"
    );

    let content = call_resp["result"]["content"]
        .as_array()
        .expect("result.content must be an array");
    assert!(!content.is_empty(), "vectorhawk_list must return content");

    // is_error must not be true
    let is_error = call_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(!is_error, "vectorhawk_list must not return isError=true");

    // ── Cleanup ───────────────────────────────────────────────────────────────

    drop(stdin);
    kill_child(&mut shim);
    kill_child(&mut daemon);
}
