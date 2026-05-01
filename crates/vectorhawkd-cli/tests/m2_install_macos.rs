//! M2 macOS install integration test.
//!
//! # Overview
//!
//! Validates M2 acceptance criteria 1, 3, and 4 on macOS:
//! - AC1: `vectorhawk daemon install` writes the plist and daemon is reachable
//!        on the socket within 5 s.
//! - AC3: `vectorhawk daemon uninstall` stops the agent and removes the plist.
//! - AC4: Running `daemon install` twice is idempotent (exits 0 both times).
//!
//! # Running
//!
//! ```text
//! cargo build --workspace --release
//! cargo test --release -p vectorhawkd-cli --test m2_install_macos \
//!     -- --include-ignored --nocapture
//! ```
//!
//! Marked `#[ignore]` — requires pre-built release binaries and must run on
//! a macOS host with `launchctl` available. Do NOT run in CI without a real
//! macOS runner.

#![cfg(target_os = "macos")]
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

fn plist_path() -> PathBuf {
    let home = dirs::home_dir().expect("HOME directory must be resolvable");
    home.join("Library")
        .join("LaunchAgents")
        .join("com.vectorhawk.agent.plist")
}

fn socket_path() -> PathBuf {
    // Mirrors install::mod.rs daemon_socket_path() for macOS.
    let data_dir = dirs::data_dir().expect("dirs::data_dir() must succeed on macOS");
    data_dir.join("VectorHawk").join("agent.sock")
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

/// Run `vectorhawk <args>` and return the exit status code.
fn run_cli(cli_bin: &PathBuf, args: &[&str]) -> std::process::ExitStatus {
    Command::new(cli_bin)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn vectorhawk {args:?}: {e}"))
}

// ── Main test ─────────────────────────────────────────────────────────────────

/// macOS end-to-end install / uninstall / idempotency test.
///
/// Marked `#[ignore]` — requires pre-built release binaries.
/// See module-level documentation for running instructions.
#[test]
#[ignore = "requires pre-built release binaries + macOS with launchctl — \
            run cargo build --workspace --release first"]
fn m2_install_uninstall_macos_end_to_end() {
    let cli_bin = release_bin("vectorhawk");
    assert!(
        cli_bin.exists(),
        "vectorhawk binary not found at {cli_bin:?} — run cargo build --workspace --release"
    );

    let plist = plist_path();
    let sock = socket_path();

    // ── Step 1: pre-test cleanup ──────────────────────────────────────────────
    // Ensure clean state. Ignore failure if daemon wasn't installed to begin
    // with — uninstall exits 0 with a "not installed" notice in that case.
    let _ = run_cli(&cli_bin, &["daemon", "uninstall"]);
    // Brief settle time for launchctl to finish.
    std::thread::sleep(Duration::from_millis(500));

    // ── Step 2: install ───────────────────────────────────────────────────────
    let status = run_cli(&cli_bin, &["daemon", "install"]);
    assert!(
        status.success(),
        "vectorhawk daemon install exited non-zero: {status}"
    );

    // ── Step 3: plist exists ──────────────────────────────────────────────────
    assert!(
        plist.exists(),
        "plist should exist after install at {plist:?}"
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

    // ── Step 8: plist is gone ─────────────────────────────────────────────────
    assert!(!plist.exists(), "plist should be removed after uninstall");

    // ── Step 9: socket disappears within 3 s ─────────────────────────────────
    let gone = wait_for_path(&sock, false, Duration::from_secs(3));
    assert!(
        gone,
        "daemon socket should disappear within 3 s after uninstall"
    );

    // ── Step 10: no stale plist or socket ────────────────────────────────────
    assert!(!plist.exists(), "no stale plist after test");
    assert!(!sock.exists(), "no stale socket after test");
}
