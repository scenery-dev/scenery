//! M3.4 concurrency test — two simultaneous `auth login` flows against one daemon.
//!
//! # What this tests
//!
//! AC6 from the M3 spec:
//!   Two concurrent OAuth login flows (distinct `state` values) driven by two
//!   parallel threads against the same daemon instance.  Each flow:
//!   1. Subscribes to `auth/wait_for_callback` with its own unique `state`.
//!   2. Receives the callback code from a simulated browser hit.
//!   3. Asserts the received code matches what was sent — no cross-contamination.
//!
//! Additionally verifies AC6's "state collision" path: subscribing the *same*
//! `state` twice returns a "duplicate login in progress" error on the second
//! subscriber.
//!
//! # Running
//!
//! Marked `#[ignore]` — requires pre-built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-cli --test m3_concurrency \
//!     -- --include-ignored --nocapture
//! ```

#![allow(clippy::unwrap_used)]

use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn release_bin(name: &str) -> PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo test");
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .expect("cli crate should have a parent")
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
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signum);
        Ok(())
    }
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

/// Minimal synchronous framed JSON-RPC socket client.
///
/// Each instance owns its own `UnixStream` connection, so concurrent flows
/// can each have their own independent socket connection.
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

    fn call(&mut self, request: serde_json::Value) -> serde_json::Value {
        let body = serde_json::to_vec(&request).unwrap();
        let len_bytes = (body.len() as u32).to_be_bytes();
        self.stream.write_all(&len_bytes).unwrap();
        self.stream.write_all(&body).unwrap();
        self.stream.flush().unwrap();
        self.read_frame()
    }

    fn read_frame(&mut self) -> serde_json::Value {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

/// Send a minimal HTTP/1.1 GET and return the status code.
fn http_get_status(host: &str, port: u16, path: &str) -> u16 {
    let stream = match TcpStream::connect((host, port)) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    writer.write_all(request.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut buf = [0u8; 64];
    let n = std::io::Read::read(&mut stream.try_clone().unwrap(), &mut buf).unwrap_or(0);
    let response = String::from_utf8_lossy(&buf[..n]);
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC6 — two concurrent login flows, distinct states, no token cross-contamination.
///
/// Flow A uses `state-conc-A` and expects `code-conc-A`.
/// Flow B uses `state-conc-B` and expects `code-conc-B`.
/// Both subscribe simultaneously; callbacks are fired in reverse order to
/// confirm there is no pairing-by-arrival-order bug.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m3_concurrent_login_flows_no_cross_contamination() {
    let daemon_bin = release_bin("vectorhawkd");
    assert!(
        daemon_bin.exists(),
        "vectorhawkd binary not found — run cargo build --workspace --release"
    );

    let socket_path = daemon_socket_path();
    kill_stale_daemon();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    // Spawn daemon.
    let mut daemon = Command::new(&daemon_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd");

    let socket_appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !socket_appeared {
        let _ = nix_kill(daemon.id(), libc_sigterm());
        panic!("daemon socket did not appear within 5 s at {socket_path:?}");
    }

    // Allow the HTTP listener time to bind.
    std::thread::sleep(Duration::from_millis(400));

    // Get the OAuth listener port.
    let mut probe = FramedSocket::connect(&socket_path);
    let port_resp = probe.call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "auth/get_oauth_listener_port",
        "params": {}
    }));

    assert!(
        port_resp.get("error").is_none(),
        "auth/get_oauth_listener_port must succeed: {port_resp}"
    );

    let port = port_resp["result"]["port"]
        .as_u64()
        .expect("result.port must be a u64") as u16;

    assert!(
        (39127..=39136).contains(&port),
        "port {port} must be in the range [39127, 39136]"
    );

    println!("OAuth listener on port {port}");

    // Spawn two waiter threads, each with its own socket connection and state.
    let sp_a = socket_path.clone();
    let sp_b = socket_path.clone();

    let waiter_a = std::thread::spawn(move || {
        let mut sock = FramedSocket::connect(&sp_a);
        sock.call(json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "auth/wait_for_callback",
            "params": {
                "state": "state-conc-A",
                "timeout_secs": 10
            }
        }))
    });

    let waiter_b = std::thread::spawn(move || {
        let mut sock = FramedSocket::connect(&sp_b);
        sock.call(json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "auth/wait_for_callback",
            "params": {
                "state": "state-conc-B",
                "timeout_secs": 10
            }
        }))
    });

    // Allow both waiters time to subscribe before firing callbacks.
    std::thread::sleep(Duration::from_millis(300));

    // Fire callbacks in reverse order (B first, then A) to confirm no
    // positional coupling — flow A must still receive code-conc-A.
    let status_b = http_get_status(
        "127.0.0.1",
        port,
        "/oauth/cli/callback?code=code-conc-B&state=state-conc-B",
    );
    let status_a = http_get_status(
        "127.0.0.1",
        port,
        "/oauth/cli/callback?code=code-conc-A&state=state-conc-A",
    );

    assert_eq!(status_b, 200, "callback B must return 200");
    assert_eq!(status_a, 200, "callback A must return 200");

    let resp_a = waiter_a.join().expect("waiter-A thread panicked");
    let resp_b = waiter_b.join().expect("waiter-B thread panicked");

    // No error on either side.
    assert!(
        resp_a.get("error").is_none(),
        "flow A must not error: {resp_a}"
    );
    assert!(
        resp_b.get("error").is_none(),
        "flow B must not error: {resp_b}"
    );

    // Each flow receives its own code — no cross-contamination.
    let code_a = resp_a["result"]["code"]
        .as_str()
        .expect("flow A code must be a string");
    let code_b = resp_b["result"]["code"]
        .as_str()
        .expect("flow B code must be a string");

    assert_eq!(code_a, "code-conc-A", "flow A got wrong code: {code_a}");
    assert_eq!(code_b, "code-conc-B", "flow B got wrong code: {code_b}");

    println!("flow A received: {code_a}");
    println!("flow B received: {code_b}");

    // Cleanup.
    let _ = nix_kill(daemon.id(), libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = daemon.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// AC6 state collision: subscribing the same state value twice returns a
/// "duplicate login in progress" error on the second subscription.
///
/// This verifies the `OAuthState::DuplicateSubscriber` path end-to-end over
/// the JSON-RPC socket.
#[test]
#[ignore = "requires pre-built release binaries — run cargo build --workspace --release first"]
fn m3_concurrent_same_state_second_subscriber_errors() {
    let daemon_bin = release_bin("vectorhawkd");
    assert!(
        daemon_bin.exists(),
        "vectorhawkd binary not found — run cargo build --workspace --release"
    );

    let socket_path = daemon_socket_path();
    kill_stale_daemon();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let mut daemon = Command::new(&daemon_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vectorhawkd");

    let appeared = wait_for_socket(&socket_path, Duration::from_secs(5));
    if !appeared {
        let _ = nix_kill(daemon.id(), libc_sigterm());
        panic!("daemon socket did not appear within 5 s");
    }

    std::thread::sleep(Duration::from_millis(400));

    // First subscriber: registers "collision-state" and blocks.
    let sp1 = socket_path.clone();
    let first_waiter = std::thread::spawn(move || {
        let mut sock = FramedSocket::connect(&sp1);
        sock.call(json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "auth/wait_for_callback",
            "params": {
                "state": "collision-state",
                "timeout_secs": 10
            }
        }))
    });

    // Allow first subscriber to register.
    std::thread::sleep(Duration::from_millis(200));

    // Second subscriber: uses the same state — must immediately receive an error.
    let sp2 = socket_path.clone();
    let second_resp = {
        let mut sock = FramedSocket::connect(&sp2);
        sock.call(json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "auth/wait_for_callback",
            "params": {
                "state": "collision-state",
                "timeout_secs": 10
            }
        }))
    };

    // Second subscriber must receive an error response, not a timeout.
    assert!(
        second_resp.get("error").is_some(),
        "second subscriber with duplicate state must receive an error: {second_resp}"
    );

    let err_msg = second_resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        err_msg.contains("already")
            || err_msg.contains("duplicate")
            || err_msg.contains("subscriber"),
        "error message must describe the duplicate subscription: {err_msg}"
    );

    println!("second subscriber correctly received error: {err_msg}");

    // Get listener port and fire callback for the first subscriber so it exits cleanly.
    let mut probe = FramedSocket::connect(&socket_path);
    let port_resp = probe.call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "auth/get_oauth_listener_port",
        "params": {}
    }));
    let port = port_resp["result"]["port"].as_u64().unwrap_or(39127) as u16;

    http_get_status(
        "127.0.0.1",
        port,
        "/oauth/cli/callback?code=cleanup-code&state=collision-state",
    );

    let _ = first_waiter.join();

    // Cleanup.
    let _ = nix_kill(daemon.id(), libc_sigterm());
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = daemon.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
