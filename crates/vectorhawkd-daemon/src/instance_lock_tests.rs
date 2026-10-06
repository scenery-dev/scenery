//! Unit tests for the single-instance `state.db` lock (Bug B, gap 3).
#![allow(clippy::unwrap_used)]

use super::*;

fn temp_root() -> Utf8PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vh-instance-lock-tests-{nanos}"));
    std::fs::create_dir_all(&path).expect("create temp root");
    Utf8PathBuf::from_path_buf(path).expect("temp path must be utf-8")
}

#[test]
fn lock_path_is_sibling_of_root_dir() {
    let root = Utf8PathBuf::from("/fake/root");
    assert_eq!(
        lock_path(&root),
        Utf8PathBuf::from("/fake/root/daemon.lock")
    );
}

/// Two `acquire()` calls against the same root dir must behave exactly like
/// two separate `vectorhawkd` processes contending for the same `state.db`:
/// the second one fails fast while the first guard is alive, and succeeds
/// once it is dropped. `flock` semantics are per *open file description*,
/// not per-process, so two independent `OpenOptions::open` calls inside
/// this one test process correctly model two different processes.
#[test]
fn second_acquire_fails_while_first_guard_is_held() {
    let root = temp_root();

    let first = acquire(&root).expect("first acquire should succeed — nothing else holds it");
    let second = acquire(&root);
    assert!(
        second.is_err(),
        "a second acquire must fail while the first guard is still alive"
    );

    drop(first);

    let third = acquire(&root);
    assert!(
        third.is_ok(),
        "acquire must succeed again once the first guard is dropped"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `std::process::exit` and a crash both skip Rust's `Drop`, so the
/// production guarantee this module makes rests entirely on the kernel
/// releasing the lock when the file descriptor closes — not on `Drop`
/// running. Simulate that directly: close the underlying fd without
/// invoking `InstanceLock`'s destructor logic (there isn't any — the whole
/// point is that there's nothing to run) by dropping the returned guard via
/// `mem::drop`, which for a plain `File` is indistinguishable at the OS
/// level from any other fd-closing event, including process exit.
#[test]
fn lock_releases_on_fd_close_with_no_drop_glue_required() {
    let root = temp_root();

    let guard = acquire(&root).expect("first acquire should succeed");
    assert!(acquire(&root).is_err(), "precondition: lock must be held");

    // `InstanceLock` carries no Drop impl beyond the default — closing its
    // file handle is the entire release mechanism, exactly as it would be
    // on any process exit path.
    std::mem::drop(guard);

    assert!(
        acquire(&root).is_ok(),
        "lock must be free immediately after the holding fd closes"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `acquire` must create the lock file on first use rather than requiring
/// some other startup step to have created it already — `state.db` itself
/// is created lazily by `AppState::bootstrap`, and this lock must not
/// require a stricter precondition than that.
#[test]
fn acquire_creates_the_lock_file_if_missing() {
    let root = temp_root();
    let path = lock_path(&root);
    assert!(!path.exists(), "precondition: lock file does not exist yet");

    let _guard = acquire(&root).expect("acquire should create and lock the file");
    assert!(path.exists(), "acquire must create the lock file");

    let _ = std::fs::remove_dir_all(&root);
}
