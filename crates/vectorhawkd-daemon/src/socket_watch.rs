//! Socket-identity watch: detects when something other than this process
//! has unlinked and rebound the daemon's Unix socket path out from under it.
//!
//! See `run_daemon` in `lib.rs` for how the periodic task built from this
//! module's pieces is wired into the accept loop, and `daemon_takeover` /
//! `daemon_takeover_signal` for how a verified-stale thief is taken over.
//!
//! # Why this exists
//!
//! A daemon built before `instance_lock` (`3d45ddb`) holds neither the
//! lock file nor any self-awareness of its own staleness — it just binds
//! the socket and runs forever. `daemon_takeover` already covers the case
//! where such a binary is already sitting on the socket when a new daemon
//! *starts up* (`attempt_daemon_takeover`, called once at the top of
//! `run_daemon`). It does not cover the case where the old binary is
//! started by hand *after* a healthy, modern daemon is already up and
//! running: it `unlink()`s the socket path and `bind()`s its own, which
//! silently cuts every shim and CLI connection off from the running daemon
//! — the running daemon's own listener fd is still open and healthy, but
//! nothing will ever connect to it again, because new connection attempts
//! resolve the path to the thief's inode, not ours.
//!
//! A running daemon has no way to notice this on its own without
//! periodically re-checking that the path still resolves to the inode it
//! originally bound — exactly what [`stat_identity`] and [`socket_changed`]
//! exist to do, mirroring `binary_watch`'s identical approach to detecting
//! an in-place binary replacement.
//!
//! # Which path to watch
//!
//! The daemon's own resolved socket path (`AppState::socket_path()` /
//! `opts.socket_path_override`) — the literal path a thief would have to
//! unlink and rebind to intercept new connections.

use camino::Utf8Path;
use std::os::unix::fs::MetadataExt;

/// How often (in seconds) the socket-watch task re-stats the socket path.
/// Deliberately short relative to `binary_watch`'s interval: a stolen
/// socket cuts off *every* client immediately (unlike a stale binary, which
/// keeps serving, just an old version), so the detection-to-takeover window
/// matters far more here. A `stat()` is still blocking I/O and must go
/// through `spawn_blocking` on the daemon's single-threaded executor — see
/// the spawn_blocking discipline note at the top of `lib.rs`.
pub const SOCKET_WATCH_INTERVAL_SECS: u64 = 5;

/// `(device, inode)` identity of the socket special file at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Stat the socket path. Returns `None` on any failure (path missing,
/// permission error, mid-rebind gap) — callers must treat that as
/// "unknown, try again later," never as "changed": see [`socket_changed`]'s
/// `None` case.
pub fn stat_identity(path: &Utf8Path) -> Option<SocketIdentity> {
    let meta = std::fs::metadata(path.as_std_path()).ok()?;
    Some(SocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Pure decision: does the socket path no longer resolve to the inode this
/// daemon bound?
///
/// - `current == Some(id)` and `id != recorded` → `true` — something else
///   now owns the path.
/// - `current == None` → **`false`**, unconditionally. A thief's `bind()`
///   briefly unlinks the old path before creating its replacement (the same
///   shape of race `binary_watch::binary_changed` already treats as "not
///   evidence of anything" for package-manager upgrades); a stat landing in
///   that gap must not be treated as a change, and a daemon mid-rebind of
///   its *own* socket (after a prior, successful takeover) would otherwise
///   false-trigger against itself here too.
/// - `current == Some(id)` and `id == recorded` → `false` — still ours.
pub fn socket_changed(recorded: SocketIdentity, current: Option<SocketIdentity>) -> bool {
    match current {
        Some(id) => id != recorded,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(dev: u64, ino: u64) -> SocketIdentity {
        SocketIdentity { dev, ino }
    }

    #[test]
    fn unchanged_is_not_a_change() {
        assert!(!socket_changed(id(1, 100), Some(id(1, 100))));
    }

    #[test]
    fn same_dev_different_ino_is_a_change() {
        assert!(socket_changed(id(1, 100), Some(id(1, 200))));
    }

    #[test]
    fn different_dev_same_ino_is_a_change() {
        // Inode numbers are only unique within a single filesystem; two
        // different devices can each have an inode 100 that are completely
        // unrelated files.
        assert!(socket_changed(id(1, 100), Some(id(2, 100))));
    }

    #[test]
    fn missing_current_is_never_a_change() {
        // The transient unlink/rebind window must not, on its own, read as
        // a takeover event — see `socket_changed`'s doc comment.
        assert!(!socket_changed(id(1, 100), None));
    }

    #[test]
    fn stat_identity_returns_none_for_missing_path() {
        assert_eq!(
            stat_identity(Utf8Path::new("/nonexistent/path/agent.sock")),
            None
        );
    }

    // Deliberately no "bind, unlink, rebind, assert the inode changed"
    // test against a real socket here: a freed inode is not guaranteed to
    // stay unallocated, and on at least one real filesystem (tmpfs on a
    // GitHub Actions Linux runner) the very next bind at the same path
    // reused the exact same inode number, making `socket_changed` compare
    // equal and the test flake — the opposite of what it was meant to show.
    // `stat_identity` itself is a thin, well-understood wrapper over
    // `std::fs::metadata`'s `dev()`/`ino()`; the behaviour that matters —
    // "is this a change" given recorded vs. current identities — is fully
    // covered by the pure tests above, deterministically, on every
    // platform.
}
