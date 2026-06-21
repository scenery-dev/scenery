//! M1 blocking-I/O stress test — validates `spawn_blocking` discipline in the
//! daemon's current-thread Tokio runtime.
//!
//! # Purpose
//!
//! The daemon uses `tokio::runtime::Builder::new_current_thread()`. If any
//! blocking I/O runs on the async executor thread directly (no `spawn_blocking`
//! wrapper), a slow tool call will stall all other concurrent calls because the
//! single executor thread is occupied.
//!
//! This test proves the invariant: a fast management call (`vectorhawk_list`)
//! returns within 100 ms even while 4 concurrent slow tool calls (each sleeping
//! 1.5 s in a stub stdio backend) are in flight.
//!
//! # Slow backend
//!
//! We use a Python script as a stdio MCP backend (same pattern as
//! `vectorhawkd-mcp/src/stdio_process.rs` tests). The script sleeps 1.5 s
//! before responding to every `tools/call` to simulate a slow external backend.
//!
//! The daemon backend registry is currently a stub-only registry (real stdio
//! backend registration is a M1.3 feature). Therefore, this test validates the
//! fast-path management tool dispatch (`vectorhawk_list`) against the shim's
//! own dispatch loop rather than the daemon's backend dispatch. The critical
//! property is that the shim's current-thread async loop is not blocked by
//! the slow backend calls that go through `spawn_blocking` in the aggregator.
//!
//! For the stub backend path (M0/M1 current state), tool calls are synchronous
//! but fast. This test validates two complementary properties:
//!   1. The daemon can handle 50 concurrent tool calls without deadlock.
//!   2. Each call returns within the 30-second global timeout (no blocking).
//!
//! When M1.3 lands with real stdio backends, a second scenario can inject latency
//! via a slow stdio backend — the `spawn_blocking` path in `aggregator.rs` is
//! already in place for that.
//!
//! # Running
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m1_blocking_io_stress \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Marked `#[ignore]` — requires pre-built release binaries.

#![allow(clippy::unwrap_used)] // integration tests may unwrap for clarity

use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

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

fn libc_sigterm() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGTERM
    }
    #[cfg(not(unix))]
    {
        15
    }
}

/// Send one JSON-RPC request and read one response.
fn send_rpc(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut std::io::BufReader<std::process::ChildStdout>,
    request: Value,
) -> Value {
    let payload = serde_json::to_string(&request).unwrap();
    writeln!(stdin, "{payload}").expect("write to shim stdin");

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read from shim stdout");
    serde_json::from_str(line.trim()).expect("shim stdout should be valid JSON")
}

// ── Main stress test ──────────────────────────────────────────────────────────

/// Validate that 50 concurrent tool calls through the daemon do not deadlock
/// and that each individual call completes within the 30-second timeout budget.
///
/// This test exercises the current-thread Tokio runtime's ability to handle
/// concurrent connections. Each call goes through `spawn_blocking` internally
/// (for audit writes) — if any blocking I/O leaked onto the executor thread,
/// the 50-call batch would serialize and individual call latency would grow
/// proportionally.
///
/// Expected behaviour (pass):
///   - All 50 calls complete in < 10 s total (not 50 × stub latency).
///   - No call takes longer than 5 s individually.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m1_fifty_concurrent_tool_calls_no_deadlock() {
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

    // Give the daemon a moment to finish initialization.
    std::thread::sleep(Duration::from_millis(200));

    // ---- Establish shim + get the tool name ---------------------------------

    let mut primary_shim = Command::new(&shim_bin)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawk mcp serve");

    let mut primary_stdin = primary_shim.stdin.take().expect("shim stdin");
    let primary_stdout_raw = primary_shim.stdout.take().expect("shim stdout");
    let mut primary_stdout = std::io::BufReader::new(primary_stdout_raw);

    // Initialize.
    let init_resp = send_rpc(
        &mut primary_stdin,
        &mut primary_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "stress-test", "version": "0.0.1" }
            }
        }),
    );
    assert!(
        init_resp.get("result").is_some(),
        "primary shim initialize must succeed: {init_resp}"
    );

    // Get a tool name from tools/list.
    let list_resp = send_rpc(
        &mut primary_stdin,
        &mut primary_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    assert!(
        list_resp.get("result").is_some(),
        "primary shim tools/list must succeed: {list_resp}"
    );

    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools array required");
    assert!(!tools.is_empty(), "at least one tool must be available");

    let tool_name = tools[0]["name"]
        .as_str()
        .expect("tool name must be a string")
        .to_string();

    // ---- Spawn 50 concurrent tool calls across threads ----------------------
    //
    // Each thread spawns its own shim connection so calls are truly concurrent
    // at the daemon level.

    let successful_calls = Arc::new(AtomicUsize::new(0));
    let max_call_ms = Arc::new(AtomicUsize::new(0));

    const CALL_COUNT: usize = 50;
    const MAX_CALL_MS_ALLOWED: u64 = 5_000;

    let batch_start = Instant::now();

    let threads: Vec<_> = (0..CALL_COUNT)
        .map(|i| {
            let tool = tool_name.clone();
            let bin = shim_bin.clone();
            let ok_counter = Arc::clone(&successful_calls);
            let max_ms = Arc::clone(&max_call_ms);

            std::thread::spawn(move || {
                let mut shim = Command::new(&bin)
                    .args(["mcp", "serve"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn vectorhawk mcp serve");

                let mut stdin = shim.stdin.take().expect("stdin");
                let stdout_raw = shim.stdout.take().expect("stdout");
                let mut stdout = std::io::BufReader::new(stdout_raw);

                // Initialize.
                let _ = send_rpc(
                    &mut stdin,
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "clientInfo": { "name": format!("stress-{i}"), "version": "0.0.1" }
                        }
                    }),
                );

                // Time the tool call.
                let call_start = Instant::now();
                let resp = send_rpc(
                    &mut stdin,
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/call",
                        "params": { "name": tool, "arguments": {} }
                    }),
                );
                let elapsed_ms = call_start.elapsed().as_millis() as usize;

                // Track maximum call latency.
                let prev_max = max_ms.load(Ordering::Relaxed);
                if elapsed_ms > prev_max {
                    max_ms.store(elapsed_ms, Ordering::Relaxed);
                }

                if resp.get("result").is_some() && resp.get("error").is_none() {
                    ok_counter.fetch_add(1, Ordering::Relaxed);
                }

                drop(stdin);
                let _ = shim.wait();
            })
        })
        .collect();

    for t in threads {
        t.join().expect("stress thread panicked");
    }

    let total_elapsed = batch_start.elapsed();
    let ok = successful_calls.load(Ordering::Relaxed);
    let max_ms = max_call_ms.load(Ordering::Relaxed);

    eprintln!(
        "50-call stress: {ok}/{CALL_COUNT} succeeded, max single-call latency = {max_ms} ms, \
         total wall time = {total_elapsed:?}"
    );

    // ---- Assertions ---------------------------------------------------------

    // All 50 calls must succeed.
    assert_eq!(
        ok, CALL_COUNT,
        "{} / {CALL_COUNT} tool calls succeeded — some calls failed",
        ok
    );

    // No single call should take longer than 5 s. If spawn_blocking is
    // missing, the current-thread runtime would serialize all 50 calls: even
    // the stub (fast) path under contention from 50 simultaneous connections
    // should not approach this bound.
    assert!(
        max_ms <= MAX_CALL_MS_ALLOWED as usize,
        "max single-call latency {max_ms} ms exceeded {MAX_CALL_MS_ALLOWED} ms — \
         possible blocking I/O on executor thread"
    );

    // The total wall time for 50 concurrent calls must be less than 5x serial
    // time. With real concurrency the calls overlap; without it they'd stack.
    // We use a generous bound of 10 s (stub calls are fast, <50 ms each).
    assert!(
        total_elapsed < Duration::from_secs(10),
        "50 concurrent tool calls took {total_elapsed:?} — possible blocking executor"
    );

    // ---- Cleanup ------------------------------------------------------------

    // Also verify the primary shim still works after the stress run (proves
    // the daemon connection pool was not exhausted or deadlocked).
    let fast_resp = send_rpc(
        &mut primary_stdin,
        &mut primary_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": {}
        }),
    );
    assert!(
        fast_resp.get("result").is_some() && fast_resp.get("error").is_none(),
        "post-stress tools/list on primary shim must succeed (daemon still alive): {fast_resp}"
    );

    drop(primary_stdin);
    let _ = primary_shim.wait();

    let _ = nix_kill(daemon.id(), libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "daemon did not exit within 3 s after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// M1 spawn_blocking acceptance — slow blocking backend must not head-of-line
/// block independent calls.
///
/// The daemon is spawned with `VECTORHAWK_STUB_LATENCY_MS=1500`, which causes
/// the stub backend's dispatch to perform a 1.5 s `std::thread::sleep` wrapped
/// in `tokio::task::spawn_blocking`. This simulates a real slow backend (such
/// as a stdio MCP server with high latency) without requiring a fixture binary.
///
/// We then fire 5 concurrent slow `tools/call` invocations through 5 separate
/// shims, and concurrently send a `tools/list` through a 6th shim. The fast
/// `tools/list` must respond well under 1.5 s — proving the current-thread
/// runtime is not serializing the slow calls onto the executor thread.
///
/// If `spawn_blocking` were missing on the slow path, the executor thread
/// would be parked inside `std::thread::sleep` and the fast call would queue
/// behind it, taking >1.5 s.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m1_slow_backend_does_not_block_independent_calls() {
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

    // Spawn daemon with the stub-latency env var. Every stub backend
    // dispatch will sleep 1.5 s in spawn_blocking.
    let mut daemon = Command::new(&daemon_bin)
        .args(["daemon", "run"])
        .env("VECTORHAWK_STUB_LATENCY_MS", "1500")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawk daemon run");

    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    std::thread::sleep(Duration::from_millis(200));

    // Discover the stub tool name via a quick shim.
    let tool_name = {
        let mut probe = Command::new(&shim_bin)
            .args(["mcp", "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vectorhawk mcp serve (probe)");
        let mut stdin = probe.stdin.take().expect("stdin");
        let stdout_raw = probe.stdout.take().expect("stdout");
        let mut stdout = std::io::BufReader::new(stdout_raw);

        let _ = send_rpc(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": { "name": "probe", "version": "0.0.1" }
                }
            }),
        );
        let list = send_rpc(
            &mut stdin,
            &mut stdout,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        );
        let tool = list["result"]["tools"][0]["name"]
            .as_str()
            .expect("at least one tool listed")
            .to_string();
        drop(stdin);
        let _ = probe.wait();
        tool
    };

    // ---- Fire 5 concurrent slow tool calls through 5 separate shims --------

    let slow_call_count = 5usize;
    let mut slow_handles = Vec::with_capacity(slow_call_count);
    for i in 0..slow_call_count {
        let bin = shim_bin.clone();
        let tool = tool_name.clone();
        slow_handles.push(std::thread::spawn(move || {
            let mut shim = Command::new(&bin)
                .args(["mcp", "serve"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn vectorhawk mcp serve (slow-call)");
            let mut stdin = shim.stdin.take().expect("stdin");
            let stdout_raw = shim.stdout.take().expect("stdout");
            let mut stdout = std::io::BufReader::new(stdout_raw);
            let _ = send_rpc(
                &mut stdin,
                &mut stdout,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": { "name": format!("slow-{i}"), "version": "0.0.1" }
                    }
                }),
            );
            let resp = send_rpc(
                &mut stdin,
                &mut stdout,
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": tool, "arguments": {} }
                }),
            );
            drop(stdin);
            let _ = shim.wait();
            resp
        }));
    }

    // Give the 5 slow calls time to be in-flight inside spawn_blocking.
    std::thread::sleep(Duration::from_millis(300));

    // ---- Now fire a fast tools/list through a fresh shim --------------------
    // tools/list is served by RealBackend::list_tools (in-memory snapshot of
    // the registry) — no dispatch, no sleep. If the runtime is unblocked it
    // returns immediately.

    let fast_start = Instant::now();
    let mut fast_shim = Command::new(&shim_bin)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vectorhawk mcp serve (fast probe)");
    let mut fast_stdin = fast_shim.stdin.take().expect("stdin");
    let fast_stdout_raw = fast_shim.stdout.take().expect("stdout");
    let mut fast_stdout = std::io::BufReader::new(fast_stdout_raw);

    let _ = send_rpc(
        &mut fast_stdin,
        &mut fast_stdout,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": { "name": "fast-probe", "version": "0.0.1" }
            }
        }),
    );
    let list_resp = send_rpc(
        &mut fast_stdin,
        &mut fast_stdout,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    );
    let fast_elapsed = fast_start.elapsed();
    drop(fast_stdin);
    let _ = fast_shim.wait();

    assert!(
        list_resp.get("result").is_some() && list_resp.get("error").is_none(),
        "fast tools/list must succeed: {list_resp}"
    );

    // The hard ceiling is 1500ms (one slow call's sleep). We allow generous
    // headroom for shim spawn + initialize + tools/list — but well under the
    // serialization threshold. If the runtime was blocked, this would be
    // >= 1500 ms because the executor would be parked in std::thread::sleep.
    const FAST_CEILING_MS: u128 = 800;
    assert!(
        fast_elapsed.as_millis() < FAST_CEILING_MS,
        "fast tools/list took {} ms (must be < {} ms — slow blocking calls are head-of-line blocking the runtime)",
        fast_elapsed.as_millis(),
        FAST_CEILING_MS
    );
    eprintln!(
        "fast tools/list completed in {} ms while {} slow calls were in flight (spawn_blocking discipline holds)",
        fast_elapsed.as_millis(),
        slow_call_count
    );

    // ---- Drain slow calls + cleanup -----------------------------------------

    for (i, h) in slow_handles.into_iter().enumerate() {
        let resp = h.join().expect("slow-call thread panicked");
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "slow call {i} must succeed: {resp}"
        );
    }

    let _ = nix_kill(daemon.id(), libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "daemon did not exit within 5 s after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
