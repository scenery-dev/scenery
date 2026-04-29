//! M0 acceptance integration test — daemon boot, shim relay, protocol round-trip.
//!
//! # Overview
//!
//! This test exercises the full daemon + shim stack as required by M0 acceptance
//! criteria AC2 and AC3:
//!
//!   AC2: `vectorhawkd` daemon boots, listens on a Unix socket at the platform
//!        data directory, and holds at least one stub backend.
//!   AC3: `vectorhawkd-shim` (invoked as `vectorhawk mcp serve`) connects to
//!        the daemon socket, relays an MCP `initialize` handshake + `tools/list`
//!        + >=5 `tools/call` invocations.  The AI client does not see an error.
//!
//! # Running
//!
//! These tests are marked `#[ignore]` because they require pre-built release
//! binaries.  Build first, then run with `--include-ignored`:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m0_acceptance \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Or via the acceptance gate (preferred):
//!
//! ```text
//! bash scripts/m0_acceptance.sh
//! ```
//!
//! # Test placement
//!
//! Tests live under `crates/vectorhawkd-daemon/tests/` following the standard
//! Rust integration test layout.  The daemon crate is the natural home because
//! the daemon binary being exercised is its `[[bin]]` target.

#![allow(clippy::unwrap_used)] // integration tests may unwrap for clarity

use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the path to a release binary in `target/release/`.
///
/// The path is computed relative to the workspace root, which is two levels
/// above `crates/vectorhawkd-daemon/tests/` in the source tree.  At test
/// runtime `CARGO_MANIFEST_DIR` points at `crates/vectorhawkd-daemon/`.
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

/// Resolve the daemon Unix socket path using the same logic as
/// `vectorhawkd_core::state::AppState::socket_path()`.
///
/// macOS:  `~/Library/Application Support/VectorHawk/agent.sock`
/// Linux:  `$XDG_RUNTIME_DIR/vectorhawk/agent.sock`
///         (falls back to `~/.local/share/VectorHawk/agent.sock`)
fn daemon_socket_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("vectorhawk").join("agent.sock");
        }
    }

    // macOS default (and Linux fallback without XDG_RUNTIME_DIR).
    let data_dir = dirs::data_dir().expect("dirs::data_dir() should succeed");
    data_dir.join("VectorHawk").join("agent.sock")
}

/// Wait up to `timeout` for the socket file to appear on disk.
///
/// Returns `true` if the socket appeared within the deadline.
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

/// Remove the socket file if it exists (pre-test cleanup).
fn remove_socket_if_present(path: &PathBuf) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// Kill a running child process unconditionally and wait for it to exit.
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Send a JSON-RPC request to the shim's stdin and read one JSON line from
/// its stdout.  Framing on the stdio side is newline-delimited (the shim's
/// stdio transport is standard JSON-RPC newline framing; the internal
/// daemon-socket transport uses length-prefix framing, but that is internal).
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

// ── Pre-test helpers ──────────────────────────────────────────────────────────

/// Kill any lingering `vectorhawkd` process that might be left from a prior run.
fn kill_stale_daemon() {
    // Best-effort: ignore errors (process may not exist).
    let _ = Command::new("pkill").args(["-f", "vectorhawkd"]).status();
    std::thread::sleep(Duration::from_millis(300));
}

// ── AC2 + AC3 acceptance test ─────────────────────────────────────────────────

/// Full shim-relay round-trip: initialize + tools/list + 5x tools/call.
///
/// Verifies M0 acceptance criteria AC2 (daemon boots on socket) and AC3
/// (shim relays protocol).
///
/// Marked `#[ignore]` — requires pre-built release binaries.
/// See the module-level documentation for running instructions.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn ac2_ac3_initialize_tools_list_and_five_tool_calls() {
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

    // Pre-test cleanup.
    kill_stale_daemon();
    remove_socket_if_present(&socket_path);

    // Spawn the daemon.
    let mut daemon = Command::new(&daemon_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd");

    // Wait up to 5 seconds for the socket file to appear.
    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    // Spawn the shim with stdin/stdout piped.
    let mut shim = Command::new(&shim_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd-shim");

    let mut stdin = shim.stdin.take().expect("shim stdin");
    let stdout_raw = shim.stdout.take().expect("shim stdout");
    let mut stdout = std::io::BufReader::new(stdout_raw);

    // ---- initialize --------------------------------------------------------

    let init_response = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "m0-test", "version": "0.0.1" }
            }
        }),
    );

    assert_eq!(
        init_response["jsonrpc"], "2.0",
        "initialize response must have jsonrpc 2.0"
    );
    assert_eq!(init_response["id"], 1, "initialize response must echo id=1");
    assert!(
        init_response.get("result").is_some() && init_response.get("error").is_none(),
        "initialize must return result, not error; got: {init_response}"
    );
    assert_eq!(
        init_response["result"]["protocolVersion"], "2024-11-05",
        "initialize result must advertise correct protocol version"
    );

    // ---- tools/list --------------------------------------------------------

    let list_response = send_rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );

    assert_eq!(list_response["jsonrpc"], "2.0");
    assert_eq!(list_response["id"], 2);
    assert!(
        list_response.get("result").is_some() && list_response.get("error").is_none(),
        "tools/list must return result; got: {list_response}"
    );

    let tools = list_response["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain a 'tools' array");

    assert!(
        !tools.is_empty(),
        "daemon must expose at least one stub tool; tools array was empty"
    );

    // Use the first available tool name for the tools/call invocations.
    let tool_name = tools[0]["name"]
        .as_str()
        .expect("tool must have a 'name' string field")
        .to_string();

    // ---- 5x tools/call -----------------------------------------------------

    for i in 0..5usize {
        let call_id = (i + 3) as u64; // ids 3..7
        let call_response = send_rpc(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": call_id,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": {}
                }
            }),
        );

        assert_eq!(
            call_response["jsonrpc"], "2.0",
            "tools/call #{i} must have jsonrpc 2.0"
        );
        assert_eq!(
            call_response["id"], call_id,
            "tools/call #{i} must echo the correct id"
        );
        assert!(
            call_response.get("result").is_some() && call_response.get("error").is_none(),
            "tools/call #{i} must return result, not error; got: {call_response}"
        );

        let content = call_response["result"]["content"]
            .as_array()
            .expect("tools/call result must contain a 'content' array");
        assert!(
            !content.is_empty(),
            "tools/call #{i} content array must not be empty"
        );
    }

    // ---- Cleanup ------------------------------------------------------------

    // Drop stdin to signal EOF to the shim.
    drop(stdin);

    // Send SIGTERM to daemon and assert clean exit within 2 s.
    let _ = nix_kill(daemon.id(), libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "daemon did not exit within 2 s after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // After clean daemon shutdown the socket file must be removed.
    assert!(
        !socket_path.exists(),
        "daemon must remove socket file on clean shutdown; still exists at {socket_path:?}"
    );

    kill_child(&mut shim);
}

// ── SIGTERM helpers (unix-only) ───────────────────────────────────────────────
//
// We avoid bringing in the `nix` crate as a dev-dep (would add weight) and
// instead call the libc signal number directly via std::process.  On macOS /
// Linux SIGTERM = 15.

/// Send SIGTERM to the given PID.  No-op (returns Ok) on non-unix.
fn nix_kill(pid: u32, _signum: i32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, _signum) };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn libc_sigterm() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGTERM
    }
    #[cfg(not(unix))]
    {
        15 // conventional value; Windows support deferred to M2/M3
    }
}
