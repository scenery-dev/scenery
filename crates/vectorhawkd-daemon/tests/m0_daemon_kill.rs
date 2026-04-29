//! M0 daemon-kill fallback test — verifies AC4: killing the daemon mid-session
//! causes the shim to fall back to in-process mode within 2 seconds.
//!
//! # Overview
//!
//! M0 acceptance criterion AC4:
//!
//!   Killing the daemon mid-session causes the shim to fall back to in-process
//!   within 2 seconds; the AI client does not error.
//!
//! This test:
//!   1. Spawns `vectorhawkd` (daemon) and `vectorhawkd-shim` (shim).
//!   2. Performs a successful `initialize` + `tools/list` round-trip via the
//!      daemon socket path (confirming the live path works).
//!   3. Kills the daemon with SIGKILL (hard kill, not SIGTERM, to simulate a
//!      crash rather than an orderly shutdown).
//!   4. Sends a `tools/call` to the shim.
//!   5. Asserts the shim responds successfully within 3 seconds (2s fallback
//!      grace + 1s buffer) and that the response does NOT contain a JSON-RPC
//!      `error` field.
//!
//! # Fallback mechanism
//!
//! When the socket becomes unreachable the shim switches to `EmbeddedBackend`
//! (in-process execution).  The spec requires this transition to complete within
//! 2 seconds.  The shim SHOULD emit a `WARN`-level log message containing
//! "fallback" when it switches modes; this test validates the observable
//! behavior (successful response) but does NOT inspect stderr for the warning
//! text — that is left as an open question for Stream 5 (see notes below).
//!
//! # Running
//!
//! These tests are marked `#[ignore]` because they require pre-built release
//! binaries.  Build first, then run with `--include-ignored`:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m0_daemon_kill \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Or via the acceptance gate (preferred):
//!
//! ```text
//! bash scripts/m0_acceptance.sh
//! ```
//!
//! # Open question for Stream 5 (shim)
//!
//! The daemon-kill test asserts a successful `tools/call` response after the
//! daemon is killed.  It does NOT currently inspect the shim's stderr for the
//! expected WARN fallback message because the exact substring is not yet
//! specified (Stream 5 implements the fallback).  If Stream 5 settles on a
//! stable log message (e.g. "switching to embedded fallback"), add a
//! `stderr_contains("embedded fallback")` assertion here.

#![allow(clippy::unwrap_used)] // integration tests may unwrap for clarity

use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

// ── Helpers (duplicated from m0_acceptance.rs — each integration test binary
// is its own compilation unit so we cannot share a test-internal module) ──────

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

fn wait_for_socket(path: &PathBuf, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn remove_socket_if_present(path: &PathBuf) {
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
    let _ = Command::new("pkill").args(["-f", "vectorhawkd"]).status();
    std::thread::sleep(Duration::from_millis(300));
}

fn nix_kill(pid: u32, signum: i32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, signum) };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signum);
    }
    Ok(())
}

fn libc_sigkill() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGKILL
    }
    #[cfg(not(unix))]
    {
        9
    }
}

// ── AC4 test ──────────────────────────────────────────────────────────────────

/// Kill the daemon mid-session; assert the shim falls back and responds within
/// 3 seconds.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
/// See the module-level documentation for running instructions.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn ac4_daemon_kill_shim_falls_back_within_3s() {
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

    // Spawn daemon.
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

    // Spawn shim.
    let mut shim = Command::new(&shim_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd-shim");

    let mut stdin = shim.stdin.take().expect("shim stdin");
    let stdout_raw = shim.stdout.take().expect("shim stdout");
    let mut stdout = std::io::BufReader::new(stdout_raw);

    // ---- Establish the session (happy path) ---------------------------------

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
                "clientInfo": { "name": "kill-test", "version": "0.0.1" }
            }
        }),
    );
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert!(
        init_resp.get("result").is_some() && init_resp.get("error").is_none(),
        "pre-kill initialize must succeed; got: {init_resp}"
    );

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
        "pre-kill tools/list must succeed; got: {list_resp}"
    );

    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tools array");
    assert!(!tools.is_empty(), "at least one tool must be available");

    let tool_name = tools[0]["name"]
        .as_str()
        .expect("tool must have a name")
        .to_string();

    // ---- Kill the daemon (SIGKILL = hard crash, not orderly shutdown) --------

    nix_kill(daemon.id(), libc_sigkill()).expect("failed to SIGKILL daemon");
    // Give the kernel a moment to deliver the signal.
    std::thread::sleep(Duration::from_millis(50));
    let _ = daemon.wait();

    // ---- Post-kill tools/call: must succeed within 3 s ----------------------
    //
    // The shim has a 2 s fallback grace period.  We allow an extra second of
    // buffer for test infra overhead.

    let call_deadline = Instant::now() + Duration::from_secs(3);

    // We cannot easily impose a per-read timeout with BufReader + blocking I/O
    // here without pulling in a separate thread or async runtime.  Instead, we
    // send the request and assert the response arrives before the deadline by
    // checking elapsed time after the (blocking) read returns.
    //
    // The shim implementation is expected to complete fallback and reply before
    // the 3 s wall-clock deadline.  If it hangs the test runner's own timeout
    // (--test-timeout, default 60 s) will eventually terminate the run.

    let post_kill_resp = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": {}
            }
        }),
    );

    let elapsed = Instant::now().duration_since(call_deadline - Duration::from_secs(3));

    assert!(
        elapsed <= Duration::from_secs(3),
        "post-daemon-kill tools/call took {elapsed:?}, exceeding the 3 s budget"
    );
    assert_eq!(post_kill_resp["jsonrpc"], "2.0");
    assert_eq!(post_kill_resp["id"], 3);
    assert!(
        post_kill_resp.get("result").is_some() && post_kill_resp.get("error").is_none(),
        "post-kill tools/call must return result (shim in embedded fallback mode); got: {post_kill_resp}"
    );

    // ---- Cleanup ------------------------------------------------------------

    drop(stdin);
    kill_child(&mut shim);
}
