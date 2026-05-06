//! GAP-03 — `notifications/tools/list_changed` broadcast.
//!
//! # What this tests
//!
//! After any tool call that mutates the tool set (`vectorhawk_install`,
//! `vectorhawk_uninstall`, `vectorhawk_update`, `vectorhawk_import`,
//! `vectorhawk_mcp_install`, `vectorhawk_mcp_uninstall`), the daemon must:
//!
//! 1. Send a `notifications/tools/list_changed` framed frame to the calling
//!    shim connection.
//! 2. Broadcast the same notification to **all** other currently-connected shim
//!    connections so their AI clients refresh their tool caches.
//!
//! # How it works
//!
//! Two raw Unix-socket "shim" connections are established directly (no shim
//! binary needed — we drive the framed JSON-RPC protocol ourselves). Shim A
//! issues a `vectorhawk_install` call. Both shim A and shim B must receive a
//! `notifications/tools/list_changed` frame within 1 s.
//!
//! Because the daemon uses length-prefixed framing (4-byte big-endian length +
//! JSON body) for the socket channel, we use the same helpers for reading.
//!
//! # Running
//!
//! Marked `#[ignore]` — requires pre-built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m1_list_changed_broadcast \
//!     -- --include-ignored --nocapture
//! ```

#![allow(clippy::unwrap_used)] // integration tests may unwrap for clarity

use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
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
        std::thread::sleep(Duration::from_millis(50));
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
    let _ = Command::new("pkill").args(["-x", "vectorhawkd"]).status();
    std::thread::sleep(Duration::from_millis(300));
}

// ── Low-level framed socket helpers (std, blocking) ───────────────────────────

/// Write a 4-byte big-endian length-prefixed JSON-RPC frame (blocking).
fn write_framed(stream: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).expect("serialization must succeed");
    let len = body.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Read one 4-byte length-prefixed frame (blocking). Returns `None` on EOF.
fn read_framed(stream: &mut UnixStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Send an `initialize` handshake and wait for the response.
fn handshake(stream: &mut UnixStream) {
    write_framed(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "gap03-test", "version": "0.0.1" }
            }
        }),
    )
    .expect("write initialize");

    let raw = read_framed(stream)
        .expect("read initialize response")
        .expect("EOF on initialize");
    let resp: Value = serde_json::from_slice(&raw).expect("parse initialize response");
    assert_eq!(resp["jsonrpc"], "2.0", "initialize: bad jsonrpc");
    assert!(
        resp.get("result").is_some() && resp.get("error").is_none(),
        "initialize must succeed; got: {resp}"
    );
}

/// Send a `vectorhawk_install` call and read the response frame.
///
/// The tool will error (no skill found at "nonexistent-skill") but that's fine —
/// what matters is that the dispatch path fires and emits list_changed regardless
/// of the inner tool result.
fn call_install(stream: &mut UnixStream, call_id: u64) -> Value {
    write_framed(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": call_id,
            "method": "tools/call",
            "params": {
                "name": "vectorhawk_install",
                "arguments": { "skill_id": "nonexistent-skill-gap03-test" }
            }
        }),
    )
    .expect("write install call");

    let raw = read_framed(stream)
        .expect("read install response")
        .expect("EOF on install response");
    serde_json::from_slice(&raw).expect("parse install response")
}

/// Block until a `notifications/tools/list_changed` frame arrives, or the
/// deadline passes. Skips any non-notification frames (responses with an `id`).
///
/// Returns `true` if the notification was received before the deadline.
fn wait_for_list_changed(stream: &mut UnixStream, deadline: Instant) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }

        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
            .unwrap();

        match read_framed(stream) {
            Ok(Some(raw)) => {
                let msg: Value = match serde_json::from_slice(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if msg.get("method").and_then(|m| m.as_str())
                    == Some("notifications/tools/list_changed")
                    && msg.get("id").is_none()
                {
                    return true;
                }
                // It might be the install response arriving before the
                // notification — keep reading.
            }
            Ok(None) => return false, // EOF
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Timeout slice expired; check deadline and retry.
                if Instant::now() >= deadline {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

// ── Main test ─────────────────────────────────────────────────────────────────

/// GAP-03: after `vectorhawk_install`, both the calling shim (A) and a
/// bystander shim (B) must receive `notifications/tools/list_changed` within 1 s.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn gap03_list_changed_broadcast_to_all_shims() {
    let daemon_bin = release_bin("vectorhawkd");
    assert!(
        daemon_bin.exists(),
        "daemon binary not found at {daemon_bin:?} — run cargo build --workspace --release"
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

    // Connect two raw socket "shims" (A and B).
    let mut shim_a = UnixStream::connect(&socket_path).expect("shim A: connect");
    let mut shim_b = UnixStream::connect(&socket_path).expect("shim B: connect");

    // Both shims complete the MCP initialize handshake.
    handshake(&mut shim_a);
    handshake(&mut shim_b);

    // Shim B will listen for notifications on a background thread.
    // We clone the stream for the reader thread.
    let shim_b_reader = shim_b.try_clone().expect("clone shim_b");
    let b_received: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let b_flag = Arc::clone(&b_received);

    std::thread::spawn(move || {
        let mut reader = shim_b_reader;
        let deadline = Instant::now() + Duration::from_secs(2);
        if wait_for_list_changed(&mut reader, deadline) {
            *b_flag.lock().unwrap() = true;
        }
    });

    // Shim A calls vectorhawk_install (will error with "not found", that's fine).
    let install_resp = call_install(&mut shim_a, 2);
    eprintln!("install response: {install_resp}");
    // We don't assert on the tool-level result (it may be isError=true).
    // What matters is that list_changed fires.

    // Shim A must receive the list_changed notification for its own call.
    let deadline_a = Instant::now() + Duration::from_secs(1);
    let a_got = wait_for_list_changed(&mut shim_a, deadline_a);
    assert!(
        a_got,
        "shim A: did not receive notifications/tools/list_changed within 1 s after vectorhawk_install"
    );

    // Give shim B thread up to 1 s total to receive its notification.
    let b_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if *b_received.lock().unwrap() {
            break;
        }
        if Instant::now() >= b_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        *b_received.lock().unwrap(),
        "shim B: did not receive notifications/tools/list_changed within 1 s after shim A's vectorhawk_install"
    );

    // Cleanup
    let _ = shim_a.shutdown(Shutdown::Both);
    let _ = shim_b.shutdown(Shutdown::Both);
    kill_child(&mut daemon);
}
