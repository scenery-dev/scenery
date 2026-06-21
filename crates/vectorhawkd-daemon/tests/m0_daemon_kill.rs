//! M0 daemon-kill test — verifies the M4 contract: killing the daemon
//! mid-session causes the shim to surface a JSON-RPC error containing the
//! "daemon" install hint, not a silent in-process fallback.
//!
//! # M4 contract (replaces the M0 fallback contract)
//!
//! Up through M3 the shim transparently switched to an `EmbeddedBackend`
//! (in-process stub) when the daemon socket died. M4 deletes that
//! silent-degradation path. The new contract is:
//!
//!   Killing the daemon mid-session causes the shim to return a JSON-RPC
//!   error response (code -32001) containing a "daemon" install hint
//!   within 3 seconds. The AI client surfaces the error to the user.
//!
//! This test:
//!   1. Spawns `vectorhawk daemon run` (daemon) and `vectorhawk mcp serve` (shim).
//!   2. Performs a successful `initialize` + `tools/list` round-trip via
//!      the daemon socket path (confirming the live path works).
//!   3. Kills the daemon with SIGKILL (hard kill).
//!   4. Sends a `tools/call` to the shim.
//!   5. Asserts the shim responds within 3 s with a JSON-RPC `error` whose
//!      `message` contains the substring `daemon`. The response MUST NOT
//!      have a `result` field.
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
//! bash scripts/m0_acceptance.sh   # uses the new M4 contract
//! bash scripts/m4_acceptance.sh   # full M4 verification
//! ```

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

fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> bool {
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
    // `-x vectorhawk` for exact process-name match — see m0_acceptance::kill_stale_daemon
    // for why `-f` was wrong on Linux.
    let _ = Command::new("pkill").args(["-x", "vectorhawk"]).status();
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

/// Kill the daemon mid-session; assert the shim surfaces a JSON-RPC error
/// containing the "daemon" install hint within 3 seconds.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
/// See the module-level documentation for running instructions.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn ac4_daemon_kill_shim_returns_daemon_required_error_within_3s() {
    let daemon_bin = release_bin("vectorhawk");
    let shim_bin = release_bin("vectorhawk");

    assert!(
        daemon_bin.exists(),
        "vectorhawk binary not found at {daemon_bin:?} — run cargo build --workspace --release"
    );
    assert!(
        shim_bin.exists(),
        "vectorhawk binary not found at {shim_bin:?} — run cargo build --workspace --release"
    );

    let socket_path = daemon_socket_path();

    kill_stale_daemon();
    remove_socket_if_present(&socket_path);

    // Spawn daemon.
    let mut daemon = Command::new(&daemon_bin)
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawk daemon run");

    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    // Spawn shim.
    let mut shim = Command::new(&shim_bin)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawk mcp serve");

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
        post_kill_resp.get("error").is_some() && post_kill_resp.get("result").is_none(),
        "post-kill tools/call must return a JSON-RPC error (shim in DaemonRequired mode); got: {post_kill_resp}"
    );
    let error_msg = post_kill_resp["error"]["message"]
        .as_str()
        .expect("error.message must be a string")
        .to_string();
    assert!(
        error_msg.to_lowercase().contains("daemon"),
        "error message must contain 'daemon'; got: {error_msg}"
    );
    assert_eq!(
        post_kill_resp["error"]["code"], -32001i64,
        "error code must be -32001 (DAEMON_UNREACHABLE)"
    );

    // ---- Cleanup ------------------------------------------------------------

    drop(stdin);
    kill_child(&mut shim);
}

// ── M6.5: EmbeddedBackend + HybridModelClient wiring ─────────────────────────

/// Verify that `EmbeddedBackend::with_model_client` accepts a
/// `HybridModelClient` built from owned `Box<dyn ModelClient>` values.
///
/// This is a unit-level smoke test: it does not spawn any process but
/// confirms that the M6.2 generification (removing the `'a` lifetime from
/// `HybridModelClient`) works end-to-end with the `EmbeddedBackend` builder.
///
/// Not marked `#[ignore]` — runs without any external binaries.
#[tokio::test]
async fn embedded_backend_accepts_hybrid_model_client() {
    use std::io::Cursor;
    use std::sync::Arc;
    use vectorhawkd_core::model::{MockModelClient, ModelClient};
    use vectorhawkd_mcp::{
        backend::{Backend, EmbeddedBackend},
        sampling::{HybridModelClient, McpSamplingClient},
    };

    // Build a McpSamplingClient that immediately returns an EOF so that if
    // any sampling request is ever attempted it fails cleanly rather than
    // blocking the test.
    let reader = Box::new(Cursor::new(Vec::<u8>::new()));
    let writer = Box::new(Vec::<u8>::new());
    let sampling = McpSamplingClient::new(writer, reader);

    // Ollama mock returns a fixed response.
    let ollama_mock = MockModelClient::new("mock local response");

    let hybrid = HybridModelClient::new(
        Some(Box::new(ollama_mock) as Box<dyn ModelClient>),
        Box::new(sampling) as Box<dyn ModelClient>,
    );

    let backend = EmbeddedBackend::with_stub_backend("stub", &[("stub__echo", "Echo stub tool")])
        .with_model_client(Arc::new(hybrid) as Arc<dyn ModelClient>);

    // EmbeddedBackend::initialize must succeed and return the correct protocol version.
    let init = backend
        .initialize(serde_json::json!({}))
        .await
        .expect("initialize should succeed");
    assert_eq!(
        init.protocol_version, "2024-11-05",
        "expected MCP protocol version"
    );

    // EmbeddedBackend::list_tools must return at least the stub tool.
    let list = backend
        .list_tools(serde_json::json!({}))
        .await
        .expect("list_tools should succeed");
    let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("echo")),
        "expected echo tool in tool list; got: {names:?}"
    );
}
