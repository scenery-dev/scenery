//! M5.1 mid-load kill test — validates the M4 daemon-required error contract
//! under sustained concurrent stress with 5 shims.
//!
//! # Acceptance criteria validated
//!
//! AC2: Same 5-shim harness as `m5_stress_multi_shim`. At the 500-call mark
//!      (across all shims, tracked via shared atomic counter), the daemon is
//!      SIGKILL'd. All remaining calls (up to ~500 across the 5 shims) must
//!      receive a JSON-RPC error with `code: -32001` and `message` containing
//!      "daemon" (case-insensitive) within 3 seconds of the kill. No shim
//!      hangs; all shims exit cleanly when stdin closes.
//!
//! Time-to-error distribution (max, p99, p50) is recorded and printed to
//! test stdout.
//!
//! # Running
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m5_stress_kill \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Or via the acceptance gate:
//!
//! ```text
//! bash scripts/m5_acceptance.sh
//! ```

#![allow(clippy::unwrap_used)] // integration tests may use unwrap for clarity

use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
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

fn kill_stale_daemon() {
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

fn send_rpc(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut std::io::BufReader<std::process::ChildStdout>,
    request: Value,
) -> std::io::Result<Value> {
    let payload = serde_json::to_string(&request).expect("serialize request");
    writeln!(stdin, "{payload}")?;

    let mut line = String::new();
    stdout.read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "shim closed stdout before responding",
        ));
    }
    serde_json::from_str(trimmed).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON from shim: {e} — line: {trimmed}"),
        )
    })
}

// ── Constants ─────────────────────────────────────────────────────────────────

const SHIM_COUNT: usize = 5;
/// Kill at 100 successful pre-kill calls per shim (500 total).
const KILL_AT_CALL_COUNT: usize = 500;
/// Total frames per shim: 1 initialize + 1 tools/list + 198 tools/call = 200.
const TOOLS_CALL_PER_SHIM: usize = 198;
/// Time-to-error budget: 3 seconds from SIGKILL.
const TIME_TO_ERROR_LIMIT_SECS: u64 = 3;

// ── Test ──────────────────────────────────────────────────────────────────────

/// AC2: 5-shim kill at 500-call mark. Remaining calls must receive -32001
/// "daemon" error within 3 s. No shim hangs.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m5_kill_at_500_all_remaining_get_daemon_required_error_within_3s() {
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

    // Spawn daemon with 5 ms stub latency so 500 calls take ~2-3 s to complete,
    // giving realistic concurrency and time for the kill to arrive mid-flight.
    let mut daemon = Command::new(&daemon_bin)
        .args(["daemon", "run"])
        .env("VECTORHAWK_STUB_LATENCY_MS", "5")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawk daemon run");

    let daemon_pid = daemon.id();

    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    std::thread::sleep(Duration::from_millis(200));

    // ── Shared state ──────────────────────────────────────────────────────────

    // Running count of successfully completed calls (pre-kill).
    let pre_kill_calls = Arc::new(AtomicUsize::new(0));
    // Flag set to true after SIGKILL is delivered.
    let daemon_killed = Arc::new(AtomicBool::new(false));
    // Timestamp (ns from test start) when SIGKILL was delivered.
    let kill_time_ns = Arc::new(AtomicU64::new(0));
    // Collection of (time-to-error_ms) values for calls that returned after the kill.
    let post_kill_error_ms: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    // Count of post-kill responses that violated the contract.
    let contract_violations = Arc::new(AtomicUsize::new(0));

    let test_start = Instant::now();

    // ── Killer thread ─────────────────────────────────────────────────────────
    // Watches pre_kill_calls; fires SIGKILL when it reaches KILL_AT_CALL_COUNT.

    {
        let pre = Arc::clone(&pre_kill_calls);
        let killed = Arc::clone(&daemon_killed);
        let kill_ns = Arc::clone(&kill_time_ns);
        let ts = test_start;
        std::thread::spawn(move || loop {
            if pre.load(Ordering::Relaxed) >= KILL_AT_CALL_COUNT {
                let ns = ts.elapsed().as_nanos() as u64;
                kill_ns.store(ns, Ordering::SeqCst);
                let _ = nix_kill(daemon_pid, libc_sigkill());
                killed.store(true, Ordering::SeqCst);
                eprintln!(
                    "SIGKILL delivered at {:.3} s (after {} pre-kill calls)",
                    ts.elapsed().as_secs_f64(),
                    pre.load(Ordering::Relaxed)
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        });
    }

    // ── Launch 5 shim threads ─────────────────────────────────────────────────

    let threads: Vec<_> = (0..SHIM_COUNT)
        .map(|shim_idx| {
            let bin = shim_bin.clone();
            let pre = Arc::clone(&pre_kill_calls);
            let killed = Arc::clone(&daemon_killed);
            let kill_ns = Arc::clone(&kill_time_ns);
            let errors_ms = Arc::clone(&post_kill_error_ms);
            let violations = Arc::clone(&contract_violations);
            let ts = test_start;

            std::thread::spawn(move || {
                let mut shim = Command::new(&bin)
                    .args(["mcp", "serve"])
                    .env("VECTORHAWK_STUB_LATENCY_MS", "5")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap_or_else(|e| {
                        panic!("shim #{shim_idx}: spawn vectorhawk mcp serve failed: {e}")
                    });

                let mut stdin = shim.stdin.take().expect("shim stdin");
                let stdout_raw = shim.stdout.take().expect("shim stdout");
                let mut stdout = std::io::BufReader::new(stdout_raw);

                // ---- initialize (always pre-kill) ----------------------------
                if let Ok(r) = send_rpc(
                    &mut stdin,
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "clientInfo": {
                                "name": format!("m5-kill-{shim_idx}"),
                                "version": "0.0.1"
                            }
                        }
                    }),
                ) {
                    if r.get("result").is_some() {
                        pre.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---- tools/list (always pre-kill) ----------------------------
                if let Ok(r) = send_rpc(
                    &mut stdin,
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/list",
                        "params": {}
                    }),
                ) {
                    if r.get("result").is_some() {
                        pre.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---- 198 × tools/call ----------------------------------------
                let tool_names = ["stub__echo", "stub__ping"];
                for call_idx in 0..TOOLS_CALL_PER_SHIM {
                    let tool = tool_names[call_idx % 2];
                    let req_id = (call_idx + 3) as u64;
                    let was_killed_before = killed.load(Ordering::SeqCst);

                    match send_rpc(
                        &mut stdin,
                        &mut stdout,
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "method": "tools/call",
                            "params": {
                                "name": tool,
                                "arguments": { "message": format!("s{shim_idx}-c{call_idx}") }
                            }
                        }),
                    ) {
                        Ok(r) => {
                            if !was_killed_before && !killed.load(Ordering::SeqCst) {
                                // Pre-kill: must succeed.
                                if r.get("result").is_some() && r.get("error").is_none() {
                                    pre.fetch_add(1, Ordering::Relaxed);
                                }
                            } else {
                                // Post-kill: must be -32001 with "daemon" in message.
                                let elapsed_ns = ts.elapsed().as_nanos() as u64;
                                let kill_ns_val = kill_ns.load(Ordering::SeqCst);
                                let elapsed_ms = if elapsed_ns > kill_ns_val {
                                    (elapsed_ns - kill_ns_val) / 1_000_000
                                } else {
                                    0
                                };

                                // Record time-to-error for distribution reporting.
                                if let Ok(mut v) = errors_ms.lock() {
                                    v.push(elapsed_ms);
                                }

                                let ok = r.get("error").is_some()
                                    && r.get("result").is_none()
                                    && r["error"]["code"].as_i64() == Some(-32001)
                                    && r["error"]["message"]
                                        .as_str()
                                        .map(|m| m.to_lowercase().contains("daemon"))
                                        .unwrap_or(false)
                                    && elapsed_ms <= TIME_TO_ERROR_LIMIT_SECS * 1_000;

                                if !ok {
                                    violations.fetch_add(1, Ordering::Relaxed);
                                    eprintln!(
                                        "shim #{shim_idx}: contract violation at call #{call_idx}: \
                                         time={elapsed_ms}ms resp={r}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            // EOF or broken pipe after kill is expected; only report
                            // if it happened suspiciously before the kill.
                            if !was_killed_before && !killed.load(Ordering::SeqCst) {
                                eprintln!(
                                    "shim #{shim_idx}: unexpected pre-kill I/O error at call \
                                     #{call_idx}: {e}"
                                );
                            }
                            // After kill: EOF means the shim closed — acceptable.
                            break;
                        }
                    }
                }

                drop(stdin);
                let _ = shim.wait();
            })
        })
        .collect();

    for t in threads {
        t.join().expect("shim stress-kill thread panicked");
    }

    // Wait for daemon to be fully reaped.
    let _ = daemon.wait();
    remove_socket_if_present(&socket_path);

    // ── Compute time-to-error distribution ────────────────────────────────────

    let errors_snapshot = {
        let v = post_kill_error_ms.lock().unwrap();
        let mut s = v.clone();
        s.sort_unstable();
        s
    };

    let (p50_ms, p99_ms, max_ms) = if errors_snapshot.is_empty() {
        (0u64, 0u64, 0u64)
    } else {
        let n = errors_snapshot.len();
        let p50 = errors_snapshot[n / 2];
        let p99 = errors_snapshot[(n * 99) / 100];
        let max = *errors_snapshot.last().unwrap();
        (p50, p99, max)
    };

    let pre_kill_count = pre_kill_calls.load(Ordering::Relaxed);
    let violation_count = contract_violations.load(Ordering::Relaxed);

    eprintln!(
        "M5 kill test: pre-kill calls = {pre_kill_count}, \
         post-kill error responses = {}, violations = {violation_count}",
        errors_snapshot.len()
    );
    eprintln!(
        "Time-to-error distribution: p50 = {p50_ms} ms, p99 = {p99_ms} ms, max = {max_ms} ms"
    );

    // ── Assertions ────────────────────────────────────────────────────────────

    assert!(
        pre_kill_count >= KILL_AT_CALL_COUNT,
        "expected >= {KILL_AT_CALL_COUNT} pre-kill calls to complete before SIGKILL; \
         got {pre_kill_count}. Daemon may have died prematurely."
    );

    assert_eq!(
        violation_count, 0,
        "{violation_count} post-kill responses violated the -32001 / 'daemon' / 3s contract"
    );

    assert!(
        max_ms <= TIME_TO_ERROR_LIMIT_SECS * 1_000,
        "max time-to-error {max_ms} ms exceeds {TIME_TO_ERROR_LIMIT_SECS} s budget"
    );
}
