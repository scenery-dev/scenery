//! Single-instance lock on the daemon's state directory.
//!
//! # Why this exists
//!
//! `acquire_socket` (in `lib.rs`) already refuses to steal the Unix socket
//! from a live daemon — but it only protects the *socket path*. A daemon
//! that already holds an open listener fd keeps running completely
//! normally — accept loop, sync reconciler, SQLite access, everything —
//! even after its socket *file* is deleted out from under it and a
//! different process binds a fresh one at the same path. Pre-`a115543` code
//! did exactly that unconditionally (unlink-then-bind, no liveness probe),
//! and any future displacement (an operator manually `rm`ing the socket
//! file while debugging, a packaging bug, a TOCTOU race) reproduces the
//! same shape: the displaced process never finds out, and keeps ticking its
//! sync loop against the shared `state.db` forever. That is the mechanism
//! behind the spaceghost incident this module closes: an orphaned daemon
//! kept resurrecting a killed skill because nothing told it to stop
//! touching shared state once it was no longer the one holding the socket.
//!
//! This lock is independent of the socket entirely. It is acquired once, at
//! the very start of [`crate::run_daemon`], before any other startup work,
//! and held for the process's whole lifetime. Its only job is the
//! invariant: at most one `vectorhawkd` process operates on a given
//! `state.db` at a time — full stop, regardless of socket state.
//!
//! # What this does NOT do
//!
//! It does not decide which of two daemons should win, and it does not
//! terminate anything. On collision it fails safe — same philosophy as
//! `acquire_socket`: the losing process refuses to start and reports why.
//! Whether an upgrade path (or a new daemon) should be allowed to verify
//! and SIGTERM an older `vectorhawk daemon run` process to take over
//! automatically is a product decision left open — see the module-level
//! discussion in `run_daemon`'s caller / the task that added this file.
//!
//! # Why `flock`, not a PID file
//!
//! A PID file requires the *next* process to decide whether a recorded PID
//! is stale (process gone, or — worse — a *different* process has since
//! reused that PID) — exactly the kind of guess `acquire_socket`'s doc
//! comment argues against. An OS-level advisory lock (`flock`/`fcntl` via
//! `fs2`) has no such ambiguity: it is held by a file descriptor, and the
//! kernel releases it unconditionally when every fd referencing it closes
//! — on a clean exit, an uncaught panic, `kill -9`, or `std::process::exit`
//! (which skips Rust's `Drop` entirely but not the kernel's fd teardown).
//! There is no stale-lock state to clean up and nothing to go wrong.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt;
use std::fs::{File, OpenOptions};

/// Name of the lock file, sibling to `state.db` inside the daemon's root
/// data directory.
pub const LOCK_FILE_NAME: &str = "daemon.lock";

/// Resolve the lock file's path from the daemon's root data directory.
///
/// Pure — no I/O — so it is directly testable without touching a real
/// filesystem.
pub fn lock_path(root_dir: &Utf8Path) -> Utf8PathBuf {
    root_dir.join(LOCK_FILE_NAME)
}

/// Holds the OS-level exclusive advisory lock for the lifetime of the
/// daemon process.
///
/// Deliberately has no public API beyond its existence — callers hold it in
/// a binding that lives for the process's lifetime and never otherwise
/// touch it. See the module docs for why dropping it (by any means,
/// including ones that skip `Drop`) is always safe.
pub struct InstanceLock {
    _file: File,
}

/// Acquire the single-instance lock for `root_dir`, refusing to steal it
/// from a live process.
///
/// Non-blocking: returns immediately whether or not the lock is held by
/// someone else. On collision, returns a descriptive error; the caller
/// (`run_daemon`) propagates it and the process exits without having
/// touched `state.db`, the socket, or anything else. It never waits, never
/// retries, and never terminates the other holder — see the module docs for
/// why that last part is a decision this function deliberately does not
/// make.
pub fn acquire(root_dir: &Utf8Path) -> Result<InstanceLock> {
    let path = lock_path(root_dir);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.as_std_path())
        .with_context(|| format!("failed to open instance lock file: {path}"))?;

    file.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!(
            "another vectorhawkd process already holds the instance lock at \
             {path} — refusing to run two daemons against the same state \
             directory. If you are certain no other vectorhawkd process is \
             running, look for a stray process first \
             (`pgrep -fl 'vectorhawk daemon run'` or `ps aux | grep vectorhawk`) \
             before removing this file by hand."
        )
    })?;

    Ok(InstanceLock { _file: file })
}

#[cfg(test)]
#[path = "instance_lock_tests.rs"]
mod tests;
