//! M5.1 multi-shim stress test — 5 concurrent shims, 200 frames each (1000 total).
//!
//! # Acceptance criteria validated
//!
//! AC1: 5 concurrent shims each drive `initialize` → `tools/list` → 198 ×
//!      `tools/call` (alternating `stub__echo` / `stub__ping`) = 200 frames/shim,
//!      1000 total. Asserts all 1000 succeed and zero socket I/O errors appear.
//!
//! AC3 (budget): Peak daemon RSS recorded via `ps -o rss=` polling at 1 Hz.
//!      Asserts ≤ 100 MB under sustained load.
//!
//! Wallclock bound: ≤ 30 s for the full 1000-call run.
//!
//! # Running
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m5_stress_multi_shim \
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
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

// ── Shared helpers ────────────────────────────────────────────────────────────
// Each integration test binary is its own compilation unit; helpers are
// duplicated rather than shared.

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

/// Sample daemon RSS (in kB) via `ps -o rss= -p <pid>`.
/// Returns 0 if the process is gone or ps fails.
fn sample_rss_kb(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse::<u64>().unwrap_or(0)
        }
        Err(_) => 0,
    }
}

// ── Constants ─────────────────────────────────────────────────────────────────

const SHIM_COUNT: usize = 5;
/// Total frames per shim: 1 initialize + 1 tools/list + 198 tools/call = 200.
const TOOLS_CALL_PER_SHIM: usize = 198;
const TOTAL_CALLS: usize = SHIM_COUNT * (1 + 1 + TOOLS_CALL_PER_SHIM);
const PEAK_RSS_LIMIT_MB: u64 = 100;
const WALL_CLOCK_LIMIT_SECS: u64 = 30;

// ── Test ──────────────────────────────────────────────────────────────────────

/// AC1 + AC3: 5 concurrent shims × 200 frames each (1000 total).
///
/// Each shim alternates `stub__echo` and `stub__ping` for its 198 tool calls.
/// An RSS sampler thread polls at 1 Hz throughout; peak RSS is asserted ≤ 100 MB.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m5_five_shims_1000_calls_all_succeed_rss_within_budget() {
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

    // Spawn daemon with 5 ms stub latency to exercise realistic concurrency.
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

    // ── RSS sampler thread ────────────────────────────────────────────────────
    // Polls at 1 Hz. Signals stop via an atomic flag; records peak in an atomic.

    let stop_sampler = Arc::new(AtomicUsize::new(0));
    let peak_rss_kb = Arc::new(AtomicUsize::new(0));
    let final_rss_kb = Arc::new(AtomicUsize::new(0));

    {
        let stop = Arc::clone(&stop_sampler);
        let peak = Arc::clone(&peak_rss_kb);
        let final_rss = Arc::clone(&final_rss_kb);
        std::thread::spawn(move || loop {
            if stop.load(Ordering::Relaxed) != 0 {
                break;
            }
            let rss = sample_rss_kb(daemon_pid);
            if rss > peak.load(Ordering::Relaxed) as u64 {
                peak.store(rss as usize, Ordering::Relaxed);
            }
            final_rss.store(rss as usize, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(1));
        });
    }

    // ── Launch 5 shim threads ─────────────────────────────────────────────────

    let successful_calls = Arc::new(AtomicUsize::new(0));
    let socket_errors = Arc::new(AtomicUsize::new(0));

    let wall_start = Instant::now();

    let threads: Vec<_> = (0..SHIM_COUNT)
        .map(|shim_idx| {
            let bin = shim_bin.clone();
            let ok = Arc::clone(&successful_calls);
            let errs = Arc::clone(&socket_errors);

            std::thread::spawn(move || {
                let mut shim = Command::new(&bin)
                    .args(["mcp", "serve"])
                    .env("VECTORHAWK_STUB_LATENCY_MS", "5")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap_or_else(|e| {
                        panic!("shim #{shim_idx}: spawn vectorhawk mcp serve failed: {e}")
                    });

                let mut stdin = shim.stdin.take().expect("shim stdin");
                let stdout_raw = shim.stdout.take().expect("shim stdout");
                let stderr_raw = shim.stderr.take().expect("shim stderr");
                let mut stdout = std::io::BufReader::new(stdout_raw);

                // ---- initialize (frame 1) ------------------------------------
                match send_rpc(
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
                                "name": format!("m5-stress-{shim_idx}"),
                                "version": "0.0.1"
                            }
                        }
                    }),
                ) {
                    Ok(r) => {
                        if r.get("result").is_some() && r.get("error").is_none() {
                            ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            errs.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("shim #{shim_idx}: initialize I/O error: {e}");
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---- tools/list (frame 2) ------------------------------------
                match send_rpc(
                    &mut stdin,
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/list",
                        "params": {}
                    }),
                ) {
                    Ok(r) => {
                        if r.get("result").is_some() && r.get("error").is_none() {
                            ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            errs.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("shim #{shim_idx}: tools/list I/O error: {e}");
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---- 198 × tools/call (frames 3–200) -------------------------
                // Alternate between stub__echo and stub__ping.
                let tool_names = ["stub__echo", "stub__ping"];
                for call_idx in 0..TOOLS_CALL_PER_SHIM {
                    let tool = tool_names[call_idx % 2];
                    let req_id = (call_idx + 3) as u64;

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
                            if r.get("result").is_some() && r.get("error").is_none() {
                                ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                // daemon-required errors or other errors count
                                errs.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            errs.fetch_add(1, Ordering::Relaxed);
                            eprintln!("shim #{shim_idx}: tools/call #{call_idx} I/O error: {e}");
                        }
                    }
                }

                // ---- cleanup: drain stderr, close stdin, wait ----------------
                drop(stdin);
                // Check stderr for "daemon socket error" strings.
                let mut stderr_reader = std::io::BufReader::new(stderr_raw);
                let mut stderr_line = String::new();
                let mut stderr_error_count = 0usize;
                loop {
                    stderr_line.clear();
                    match stderr_reader.read_line(&mut stderr_line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if stderr_line.to_lowercase().contains("daemon socket error") {
                                stderr_error_count += 1;
                            }
                        }
                        Err(_) => break,
                    }
                }
                if stderr_error_count > 0 {
                    errs.fetch_add(stderr_error_count, Ordering::Relaxed);
                }

                let _ = shim.wait();
            })
        })
        .collect();

    for t in threads {
        t.join().expect("shim stress thread panicked");
    }

    let wall_elapsed = wall_start.elapsed();

    // Stop RSS sampler.
    stop_sampler.store(1, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));

    let ok_count = successful_calls.load(Ordering::Relaxed);
    let err_count = socket_errors.load(Ordering::Relaxed);
    let peak_mb = peak_rss_kb.load(Ordering::Relaxed) as u64 / 1024;
    let final_mb = final_rss_kb.load(Ordering::Relaxed) as u64 / 1024;

    // Write peak RSS to a file for the acceptance gate to parse.
    let rss_file = {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo test");
        let workspace_root = PathBuf::from(&manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        workspace_root
            .join("target")
            .join("m5-daemon-rss-under-load.txt")
    };
    let _ = std::fs::create_dir_all(rss_file.parent().unwrap());
    let _ = std::fs::write(
        &rss_file,
        format!(
            "peak_mb={peak_mb}\nfinal_mb={final_mb}\nwall_secs={}\n",
            wall_elapsed.as_secs_f64()
        ),
    );

    eprintln!(
        "M5 stress: {ok_count}/{TOTAL_CALLS} succeeded, {err_count} socket errors, \
         peak RSS = {peak_mb} MB, final RSS = {final_mb} MB, wall time = {wall_elapsed:?}"
    );

    // ── Cleanup ───────────────────────────────────────────────────────────────

    let _ = nix_kill(daemon_pid, libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = nix_kill(daemon_pid, 9);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    remove_socket_if_present(&socket_path);

    // ── Assertions ────────────────────────────────────────────────────────────

    assert_eq!(
        ok_count, TOTAL_CALLS,
        "{ok_count}/{TOTAL_CALLS} frames succeeded — {err_count} errors. \
         All 1000 frames must succeed."
    );

    assert_eq!(
        err_count, 0,
        "{err_count} socket I/O errors detected (expected 0)"
    );

    assert!(
        peak_mb <= PEAK_RSS_LIMIT_MB,
        "peak daemon RSS {peak_mb} MB exceeds {PEAK_RSS_LIMIT_MB} MB under-load budget"
    );

    assert!(
        wall_elapsed <= Duration::from_secs(WALL_CLOCK_LIMIT_SECS),
        "total wall time {wall_elapsed:?} exceeds {WALL_CLOCK_LIMIT_SECS} s budget"
    );
}
