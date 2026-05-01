//! M3.1 integration test — OAuth callback listener, pub/sub, JSON-RPC round-trip.
//!
//! # What this tests
//!
//! Acceptance criteria from the M3.1 spec:
//!   AC1:  Fixed-port OAuth callback listener binds in daemon.
//!   AC1a: `auth/get_oauth_listener_port` returns the bound port.
//!   AC1b: `auth/wait_for_callback` + browser callback delivers the code over
//!         the JSON-RPC socket.
//!
//! # Running
//!
//! Marked `#[ignore]` — requires pre-built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-daemon --test m3_oauth_listener \
//!     -- --include-ignored --nocapture
//! ```
//!
//! # Architecture
//!
//! The test follows the same binary-subprocess pattern as `m0_acceptance` and
//! `m1_multi_shim`: spawn `vectorhawkd` as a child process, connect a raw
//! Unix socket, speak the 4-byte-length-prefix JSON-RPC framing directly.
//!
//! `auth/wait_for_callback` is an inherently long-running call (blocks until
//! the browser fires the redirect).  We issue it from a background thread and
//! then drive the HTTP callback from the main test thread.
//!
//! All synchronous I/O is fine here because this is a `#[test]` (not `#[tokio::test]`).

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::{
    io::{BufRead, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

// ── Helpers (same pattern as m0_acceptance) ───────────────────────────────────

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
    let _ = Command::new("pkill").args(["-x", "vectorhawkd"]).status();
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

// ── Synchronous framed socket client ─────────────────────────────────────────
//
// The daemon socket uses 4-byte big-endian length prefix + UTF-8 JSON body.
// We implement a minimal sync version here to avoid an async runtime in the test.

struct FramedSocket {
    stream: UnixStream,
}

impl FramedSocket {
    fn connect(path: &PathBuf) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        Self { stream }
    }

    /// Send a JSON-RPC request (framed) and return the response value.
    fn call(&mut self, request: Value) -> Value {
        let body = serde_json::to_vec(&request).unwrap();
        let len_bytes = (body.len() as u32).to_be_bytes();
        self.stream.write_all(&len_bytes).unwrap();
        self.stream.write_all(&body).unwrap();
        self.stream.flush().unwrap();
        self.read_frame()
    }

    fn read_frame(&mut self) -> Value {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

// ── Synchronous HTTP helper ───────────────────────────────────────────────────
//
// We avoid pulling reqwest into the integration test by sending a minimal HTTP
// GET over a raw TcpStream.  The daemon's axum server speaks plain HTTP/1.1.

fn http_get(host: &str, port: u16, path: &str) -> (u16, String) {
    let stream = TcpStream::connect((host, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let reader = std::io::BufReader::new(stream);

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    writer.write_all(request.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut lines = reader.lines();

    // Parse status line.
    let status_line = lines
        .next()
        .expect("should have status line")
        .expect("status line should be readable");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status code");

    // Skip headers until blank line.
    let mut content_length: Option<usize> = None;
    for line in lines.by_ref() {
        let line = line.expect("header line");
        if line.is_empty() {
            break;
        }
        if line.to_lowercase().starts_with("content-length:") {
            if let Some(v) = line.split(':').nth(1) {
                content_length = v.trim().parse().ok();
            }
        }
    }

    // Read body (content-length bytes or until EOF).
    let body = if let Some(len) = content_length {
        // Re-acquire the underlying reader (we consumed headers via Lines).
        // This is a bit awkward — reconstruct from what we can.
        // For simplicity, collect remaining lines and join.
        let remaining: Vec<String> = lines.map(|l| l.unwrap_or_default()).collect();
        let joined = remaining.join("\n");
        joined[..len.min(joined.len())].to_string()
    } else {
        lines
            .map(|l| l.unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    };

    (status_code, body)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC1 + AC1a: daemon binds OAuth listener; `auth/get_oauth_listener_port`
/// returns the bound port in [39127, 39136].
///
/// AC1b: `auth/wait_for_callback` waits on socket; browser hit on the callback
/// URL delivers the code over the JSON-RPC response.
///
/// Requires pre-built release binaries.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m3_ac1_oauth_listener_and_wait_for_callback() {
    let daemon_bin = release_bin("vectorhawkd");
    assert!(
        daemon_bin.exists(),
        "daemon binary not found at {daemon_bin:?} — run cargo build --workspace --release"
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

    // Give the HTTP listener time to bind.
    std::thread::sleep(Duration::from_millis(300));

    // ── AC1a: query the listener port ────────────────────────────────────────

    let mut socket = FramedSocket::connect(&socket_path);

    let port_resp = socket.call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "auth/get_oauth_listener_port",
        "params": {}
    }));

    assert!(
        port_resp.get("error").is_none(),
        "auth/get_oauth_listener_port should not error: {port_resp}"
    );

    let port = port_resp["result"]["port"]
        .as_u64()
        .expect("result.port must be a u64") as u16;

    assert!(
        (39127..=39136).contains(&port),
        "port {port} must be in the range [39127, 39136]"
    );

    println!("OAuth listener bound on port {port}");

    // ── AC1b: wait_for_callback delivers the code ─────────────────────────────
    //
    // Issue auth/wait_for_callback from a background thread (it blocks until
    // the browser fires the redirect).  The main thread then simulates the
    // browser by hitting the callback URL.

    let socket_path_clone = socket_path.clone();
    let waiter = std::thread::spawn(move || {
        let mut wait_sock = FramedSocket::connect(&socket_path_clone);
        wait_sock.call(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "auth/wait_for_callback",
            "params": {
                "state": "m3-test-state",
                "timeout_secs": 10
            }
        }))
    });

    // Allow the wait handler time to subscribe.
    std::thread::sleep(Duration::from_millis(200));

    // Simulate browser callback.
    let path = "/oauth/cli/callback?code=m3-test-code&state=m3-test-state";
    let (status, body) = http_get("127.0.0.1", port, path);

    assert_eq!(status, 200, "callback endpoint should return 200");
    assert!(
        body.contains("VectorHawk login complete") || body.contains("VectorHawk"),
        "body should contain the success page; got: {body}"
    );

    // Collect wait_for_callback result.
    let wait_resp = waiter.join().expect("waiter thread should not panic");

    assert!(
        wait_resp.get("error").is_none(),
        "wait_for_callback should succeed: {wait_resp}"
    );
    assert_eq!(
        wait_resp["result"]["code"], "m3-test-code",
        "code should be the value sent by the simulated browser"
    );

    // ── Missing params return 400 ─────────────────────────────────────────────

    let (bad_status, _) = http_get("127.0.0.1", port, "/oauth/cli/callback?state=only-state");
    assert_eq!(bad_status, 400, "missing code should yield 400");

    // ── Cleanup ───────────────────────────────────────────────────────────────

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

/// Concurrency: two distinct OAuth states subscribed concurrently; both
/// callbacks delivered independently without cross-contamination.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m3_two_concurrent_wait_for_callback_independent() {
    let daemon_bin = release_bin("vectorhawkd");
    assert!(daemon_bin.exists(), "daemon binary not found");

    let socket_path = daemon_socket_path();
    kill_stale_daemon();
    remove_socket_if_present(&socket_path);

    let mut daemon = Command::new(&daemon_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd");

    let appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !appeared {
        kill_child(&mut daemon);
        panic!("daemon socket did not appear within 5 s");
    }

    std::thread::sleep(Duration::from_millis(300));

    // Get listener port.
    let mut probe = FramedSocket::connect(&socket_path);
    let port_resp = probe.call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "auth/get_oauth_listener_port",
        "params": {}
    }));
    let port = port_resp["result"]["port"].as_u64().unwrap() as u16;

    // Spawn two concurrent waiter threads with distinct states.
    let sp1 = socket_path.clone();
    let sp2 = socket_path.clone();

    let waiter_a = std::thread::spawn(move || {
        let mut s = FramedSocket::connect(&sp1);
        s.call(json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "auth/wait_for_callback",
            "params": {"state": "state-A", "timeout_secs": 10}
        }))
    });

    let waiter_b = std::thread::spawn(move || {
        let mut s = FramedSocket::connect(&sp2);
        s.call(json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "auth/wait_for_callback",
            "params": {"state": "state-B", "timeout_secs": 10}
        }))
    });

    // Allow both waiters to subscribe.
    std::thread::sleep(Duration::from_millis(300));

    // Fire callbacks in reverse order to prove no coupling.
    let (s_b, _) = http_get(
        "127.0.0.1",
        port,
        "/oauth/cli/callback?code=code-B&state=state-B",
    );
    let (s_a, _) = http_get(
        "127.0.0.1",
        port,
        "/oauth/cli/callback?code=code-A&state=state-A",
    );

    assert_eq!(s_b, 200);
    assert_eq!(s_a, 200);

    let resp_a = waiter_a.join().expect("waiter-A panicked");
    let resp_b = waiter_b.join().expect("waiter-B panicked");

    assert_eq!(
        resp_a["result"]["code"], "code-A",
        "waiter-A got wrong code"
    );
    assert_eq!(
        resp_b["result"]["code"], "code-B",
        "waiter-B got wrong code"
    );

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
