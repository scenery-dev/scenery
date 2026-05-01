//! M2 Linux install integration test.
//!
//! # Overview
//!
//! Validates M2 acceptance criteria 1, 3, and 4 on Linux:
//! - AC2: `vectorhawk daemon install` writes the systemd user unit and daemon
//!        is reachable on the socket.
//! - AC3: `vectorhawk daemon uninstall` stops the agent and removes the unit.
//! - AC4: Running `daemon install` twice is idempotent (exits 0 both times).
//!
//! # Running
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-cli --test m2_install_linux \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Marked `#[ignore]` — requires pre-built release binaries and must run on
//! a Linux host where `systemctl --user` is available. The test gracefully
//! skips when `systemctl --user` is not usable (e.g. inside a Docker container
//! without a user session bus), printing a notice and returning without panic.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)] // integration tests may unwrap for clarity

use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn release_bin(name: &str) -> PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo test");
    // CARGO_MANIFEST_DIR = crates/vectorhawkd-cli  →  ../../target/release/
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .expect("vectorhawkd-cli crate should have a parent")
        .parent()
        .expect("crates/ should have a parent (workspace root)")
        .to_path_buf();
    workspace_root.join("target").join("release").join(name)
}

fn unit_path() -> PathBuf {
    let config = dirs::config_dir().expect("XDG config dir must be resolvable on Linux");
    config
        .join("systemd")
        .join("user")
        .join("vectorhawk-agent.service")
}

fn socket_path() -> PathBuf {
    // Mirrors install::mod.rs daemon_socket_path() for Linux.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("vectorhawk").join("agent.sock");
    }
    let data_dir = dirs::data_dir().expect("dirs::data_dir() must succeed");
    data_dir.join("VectorHawk").join("agent.sock")
}

/// Returns true if `systemctl --user show-environment` exits 0.
fn systemctl_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Poll until the path exists (or doesn't, when `expect_present` is false).
/// Returns true if the condition was met before the timeout.
fn wait_for_path(path: &PathBuf, expect_present: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() == expect_present {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Run `vectorhawk <args>` and return the exit status.
fn run_cli(cli_bin: &PathBuf, args: &[&str]) -> std::process::ExitStatus {
    Command::new(cli_bin)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn vectorhawk {args:?}: {e}"))
}

// ── Main test ─────────────────────────────────────────────────────────────────

/// Linux end-to-end install / uninstall / idempotency test.
///
/// Gracefully skips if `systemctl --user` is not available (e.g. inside a
/// container without a D-Bus session). In that case the test prints a notice
/// and returns without panicking.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
/// See module-level documentation for running instructions.
#[test]
#[ignore = "requires pre-built release binaries + Linux with systemctl --user — \
            run cargo build --workspace --release first"]
fn m2_install_uninstall_linux_end_to_end() {
    let cli_bin = release_bin("vectorhawk");
    assert!(
        cli_bin.exists(),
        "vectorhawk binary not found at {cli_bin:?} — run cargo build --workspace --release"
    );

    // ── Graceful skip if systemctl --user is unavailable ──────────────────────
    if !systemctl_user_available() {
        eprintln!(
            "systemctl --user not available on this system \
             (container without user session?) — skipping m2_install_linux test"
        );
        return;
    }

    let unit = unit_path();
    let sock = socket_path();

    // ── Step 1: pre-test cleanup ──────────────────────────────────────────────
    // Ignore failure — uninstall exits 0 with "not installed" when clean.
    let _ = run_cli(&cli_bin, &["daemon", "uninstall"]);
    std::thread::sleep(Duration::from_millis(500));

    // ── Step 2: install ───────────────────────────────────────────────────────
    let status = run_cli(&cli_bin, &["daemon", "install"]);
    assert!(
        status.success(),
        "vectorhawk daemon install exited non-zero: {status}"
    );

    // ── Step 3: unit file exists ──────────────────────────────────────────────
    assert!(
        unit.exists(),
        "systemd user unit should exist after install at {unit:?}"
    );

    // ── Step 4: socket appears within 5 s ────────────────────────────────────
    let appeared = wait_for_path(&sock, true, Duration::from_secs(5));
    assert!(
        appeared,
        "daemon socket did not appear within 5 s at {sock:?}"
    );

    // ── Step 5: socket is connectable ────────────────────────────────────────
    UnixStream::connect(&sock)
        .unwrap_or_else(|e| panic!("UnixStream::connect to daemon socket failed: {e}"));

    // ── Step 6: idempotent second install ─────────────────────────────────────
    let status2 = run_cli(&cli_bin, &["daemon", "install"]);
    assert!(
        status2.success(),
        "vectorhawk daemon install (second run) exited non-zero: {status2}"
    );

    // Socket must still be reachable after the idempotent install.
    UnixStream::connect(&sock)
        .unwrap_or_else(|e| panic!("socket not connectable after idempotent install: {e}"));

    // ── Step 7: uninstall ─────────────────────────────────────────────────────
    let status3 = run_cli(&cli_bin, &["daemon", "uninstall"]);
    assert!(
        status3.success(),
        "vectorhawk daemon uninstall exited non-zero: {status3}"
    );

    // ── Step 8: unit file is gone ─────────────────────────────────────────────
    assert!(
        !unit.exists(),
        "unit file should be removed after uninstall"
    );

    // ── Step 9: socket disappears within 3 s ─────────────────────────────────
    let gone = wait_for_path(&sock, false, Duration::from_secs(3));
    assert!(
        gone,
        "daemon socket should disappear within 3 s after uninstall"
    );

    // ── Step 10: no stale files ────────────────────────────────────────────────
    assert!(!unit.exists(), "no stale unit file after test");
    assert!(!sock.exists(), "no stale socket after test");
}
