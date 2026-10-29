//! Unit tests for `acquire_socket` — the daemon-singleton fix.
//!
//! A second daemon must refuse to start when another daemon already owns
//! the socket, rather than unlinking the live daemon's socket file and
//! stealing it out from under it. See the module doc on `acquire_socket`
//! in `lib.rs` for the full rationale.

#![allow(clippy::unwrap_used)]

use camino::Utf8PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::net::UnixListener;

use crate::{acquire_socket, connect_error_is_definitively_stale};

/// `sockaddr_un.sun_path` is capped at ~104 bytes on macOS/BSD, so unlike
/// other temp-file helpers in this crate we can't afford a descriptive
/// nanosecond-timestamped directory name here — it blows the limit
/// (`InvalidInput: path must be shorter than SUN_LEN`). Keep it short:
/// pid + a per-process counter is unique enough for these tests.
fn temp_socket_path(label: &str) -> Utf8PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("vh{pid:x}{n:x}{label}.sock"));
    Utf8PathBuf::from_path_buf(path).expect("temp path should be utf-8")
}

/// A live listener already on the socket path: `acquire_socket` must return
/// an error naming "another daemon", and — this is the assertion that
/// actually catches a regression to the old unlink-then-bind behavior — the
/// existing socket file must still be present afterwards.
#[tokio::test]
async fn refuses_to_steal_a_live_socket() {
    let path = temp_socket_path("live");
    let _live = UnixListener::bind(&path).expect("bind live listener");
    assert!(path.exists(), "precondition: socket file exists");

    let err = acquire_socket(&path)
        .await
        .expect_err("must refuse to start alongside a live daemon");
    let msg = err.to_string();
    assert!(
        msg.contains("another") && msg.to_lowercase().contains("daemon"),
        "error should say another daemon is already running, got: {msg}"
    );

    assert!(
        path.exists(),
        "the live daemon's socket file must not be unlinked"
    );

    // The original listener is still alive and still accepting on the path —
    // further proof the probe didn't disturb it.
    drop(_live);
}

/// A stale socket FILE with nothing listening on it: `acquire_socket` must
/// remove it and bind successfully.
#[tokio::test]
async fn replaces_a_stale_socket_file() {
    let path = temp_socket_path("stale");

    // Create a stale socket file by binding and dropping the listener
    // without unlinking it first (simulates a crashed daemon).
    {
        let listener = UnixListener::bind(&path).expect("bind throwaway listener");
        drop(listener);
    }
    assert!(path.exists(), "precondition: stale socket file exists");

    let listener = acquire_socket(&path)
        .await
        .expect("must bind over a genuinely stale socket file");

    // The new listener actually works.
    let path_for_client = path.clone();
    let accept = tokio::spawn(async move { listener.accept().await });
    let _client = tokio::net::UnixStream::connect(&path_for_client)
        .await
        .expect("connect to freshly bound socket");
    let accepted = accept.await.expect("accept task").expect("accept result");
    drop(accepted);
}

/// No socket file at all: unchanged behaviour — bind succeeds, no warning
/// about removing anything.
#[tokio::test]
async fn binds_cleanly_when_no_socket_file_exists() {
    let path = temp_socket_path("absent");
    assert!(!path.exists(), "precondition: no socket file");

    let _listener = acquire_socket(&path)
        .await
        .expect("must bind when nothing exists yet");
    assert!(path.exists(), "bind should create the socket file");
}

/// A `PermissionDenied` connect error must NOT be treated as "stale" — it
/// disproves nothing about whether a daemon is listening (root bypasses
/// permission checks entirely, so this is skipped when running as root).
///
/// Constructed portably (no root needed): bind a listener so the socket
/// file exists and looks exactly like a normal live-daemon socket, then
/// chmod the socket file itself to 000. `path.exists()` only needs search
/// permission on the containing directory (unaffected), but the kernel
/// requires write access to the socket special file for `connect()`, so the
/// probe now fails with EACCES/EPERM — verified empirically on this
/// toolchain to map to `ErrorKind::PermissionDenied` regardless of whether
/// the listener behind it is still alive or already gone; this test drops
/// the listener first so a bug that "reclaims" would also actually succeed
/// at binding, making the assertion meaningful.
///
/// This test fails against the pre-fix code (which had no `PermissionDenied`
/// special case: every non-timeout connect error, including this one, was
/// treated as proof of staleness and the socket was removed + rebound).
#[tokio::test]
async fn refuses_when_existing_socket_is_permission_denied() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, permission checks are bypassed");
        return;
    }

    let path = temp_socket_path("eacces");
    {
        let listener = UnixListener::bind(&path).expect("bind throwaway listener");
        drop(listener);
    }
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000))
        .expect("chmod socket file to 000");
    assert!(
        path.exists(),
        "precondition: exists() must still succeed (only the file's own \
         mode changed, not the directory's)"
    );

    let err = acquire_socket(&path)
        .await
        .expect_err("a permission-denied probe must refuse, not reclaim");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("permission"),
        "error should mention the permission problem, got: {msg}"
    );

    assert!(
        path.exists(),
        "a permission-denied probe must not remove the socket file"
    );

    // Clean up: restore permissions so tempdir cleanup can remove the file.
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    let _ = std::fs::remove_file(&path);
}

/// Direct coverage of the classification rule, independent of the
/// filesystem plumbing above: only `PermissionDenied` is ambiguous
/// (refuse); every other connect-failure kind is a definitive "nobody is
/// listening" signal (reclaim).
#[test]
fn classifier_treats_only_permission_denied_as_ambiguous() {
    assert!(!connect_error_is_definitively_stale(&std::io::Error::from(
        std::io::ErrorKind::PermissionDenied
    )));
    assert!(connect_error_is_definitively_stale(&std::io::Error::from(
        std::io::ErrorKind::ConnectionRefused
    )));
    assert!(connect_error_is_definitively_stale(&std::io::Error::from(
        std::io::ErrorKind::NotFound
    )));
    assert!(connect_error_is_definitively_stale(&std::io::Error::from(
        std::io::ErrorKind::Other
    )));
}
