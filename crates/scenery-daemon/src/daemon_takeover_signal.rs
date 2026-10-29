//! Execute a verified [`crate::daemon_takeover::Verdict::TakeOver`]:
//! SIGTERM, wait with a bounded timeout, SIGKILL only if the holder is
//! still verifiably the same process after that timeout.
//!
//! Pure stepper over an injected [`TakeoverIo`] — see this crate's
//! `process_probe` module for why: tests must never signal a process they
//! didn't spawn themselves, so every scenario here (holder exits cleanly,
//! holder ignores SIGTERM, a different process reuses the pid in between)
//! is driven by a fake, with the real OS-backed implementation
//! ([`SystemTakeoverIo`]) exercised only against self-spawned children in
//! `process_probe`'s tests.
//!
//! # The pid-reuse guard
//!
//! Between sending SIGTERM and the timeout expiring, the original process
//! can exit and the kernel can hand its pid number to something completely
//! unrelated. Sending SIGKILL at that point would signal the wrong
//! process. [`TakeoverIo::still_same_process`] exists specifically to catch
//! this: implementations must re-verify the pid's identity (argv +
//! same-uid) immediately before the KILL decision, not trust the
//! fingerprint captured at the start. If that re-check fails,
//! [`execute_takeover`] refuses to send SIGKILL at all — see
//! `TakeoverResult::RefusedPidReused`.

use crate::process_probe::{ProcessInspector, ProcessKiller};
use std::time::Duration;

/// One of the two signals a takeover ever sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// The injected seam between this module's pure stepping logic and the
/// real world (or a test double). One instance is scoped to exactly one
/// target pid — see [`crate::SystemTakeoverIo`] for the real implementation.
pub trait TakeoverIo {
    /// Best-effort; a failure here does not stop the sequence (the holder
    /// may already be gone).
    fn send_signal(&mut self, signal: Signal) -> std::io::Result<()>;

    /// Has the contested resource (the socket, the instance lock) become
    /// free? The real implementation treats "the target pid is no longer
    /// alive" as sufficient — see `SystemTakeoverIo`'s doc comment for why
    /// that is a safe (if occasionally conservative) proxy, given
    /// `instance_lock`'s documented guarantee that the kernel releases the
    /// flock unconditionally the moment every fd referencing it closes.
    fn resource_free(&mut self) -> bool;

    /// Immediately before sending SIGKILL: is the pid still verifiably the
    /// *same* process we targeted (not a different process that reused the
    /// pid after the original exited)? See this module's doc comment.
    fn still_same_process(&mut self) -> bool;

    /// Block for `d`. The real implementation sleeps; fakes in tests must
    /// not actually sleep — see `daemon_takeover_signal_tests`.
    fn sleep(&mut self, d: Duration);
}

/// Outcome of [`execute_takeover`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverResult {
    /// The resource freed up after SIGTERM alone; no SIGKILL was sent.
    AcquiredAfterTerm,
    /// SIGTERM's timeout expired, SIGKILL was sent, and the resource then
    /// freed up within the post-kill grace window.
    AcquiredAfterKill,
    /// SIGTERM's timeout expired, but the pid-reuse guard
    /// (`still_same_process`) failed immediately before the KILL decision
    /// — no SIGKILL was sent. See this module's doc comment.
    RefusedPidReused,
    /// SIGKILL was sent, but the resource never freed within the grace
    /// window that followed. The caller should fall through to the normal
    /// (unchanged) "refuse, name the pid" error path.
    GaveUpAfterKill,
}

/// Run the bounded SIGTERM → wait → (guarded) SIGKILL → wait sequence
/// described in this module's doc comment.
pub fn execute_takeover<IO: TakeoverIo>(
    io: &mut IO,
    term_timeout: Duration,
    kill_grace: Duration,
    poll_interval: Duration,
) -> TakeoverResult {
    let _ = io.send_signal(Signal::Term);
    if wait_until_free(io, term_timeout, poll_interval) {
        return TakeoverResult::AcquiredAfterTerm;
    }

    if !io.still_same_process() {
        return TakeoverResult::RefusedPidReused;
    }

    let _ = io.send_signal(Signal::Kill);
    if wait_until_free(io, kill_grace, poll_interval) {
        TakeoverResult::AcquiredAfterKill
    } else {
        TakeoverResult::GaveUpAfterKill
    }
}

/// Poll `resource_free`, sleeping `poll_interval` between checks, until
/// either it returns `true` or `timeout` has elapsed. Checks once before
/// the first sleep, so a holder that was already gone by the time we got
/// here (or exits essentially immediately) never waits a full interval.
fn wait_until_free<IO: TakeoverIo>(
    io: &mut IO,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    if io.resource_free() {
        return true;
    }
    let mut waited = Duration::ZERO;
    while waited < timeout {
        io.sleep(poll_interval);
        waited += poll_interval;
        if io.resource_free() {
            return true;
        }
    }
    false
}

/// Real, OS-backed [`TakeoverIo`]: signals via a [`ProcessKiller`], and
/// treats "the target pid is no longer alive" as the resource-free signal
/// — see `instance_lock`'s doc comment for why that is sound: the kernel
/// releases the flock unconditionally the instant every fd referencing it
/// closes (clean exit, panic, `kill -9`, or `std::process::exit`), and the
/// same process exiting also closes its listening socket fd. `sleep` is a
/// real blocking sleep — callers only ever run this at daemon startup
/// (before the accept loop exists) or from the synchronous CLI installer,
/// neither of which has other work competing for the thread at that point.
pub struct SystemTakeoverIo<'a, I: ProcessInspector, K: ProcessKiller> {
    pid: u32,
    /// argv captured at verification time, re-checked against the pid's
    /// *current* argv immediately before SIGKILL — the pid-reuse guard.
    fingerprint_argv: Vec<u8>,
    inspector: &'a I,
    killer: &'a K,
}

impl<'a, I: ProcessInspector, K: ProcessKiller> SystemTakeoverIo<'a, I, K> {
    pub fn new(pid: u32, fingerprint_argv: Vec<u8>, inspector: &'a I, killer: &'a K) -> Self {
        Self {
            pid,
            fingerprint_argv,
            inspector,
            killer,
        }
    }
}

impl<'a, I: ProcessInspector, K: ProcessKiller> TakeoverIo for SystemTakeoverIo<'a, I, K> {
    fn send_signal(&mut self, signal: Signal) -> std::io::Result<()> {
        match signal {
            Signal::Term => self.killer.terminate(self.pid),
            Signal::Kill => self.killer.kill(self.pid),
        }
    }

    fn resource_free(&mut self) -> bool {
        self.inspector.same_uid_alive(self.pid).is_none()
    }

    fn still_same_process(&mut self) -> bool {
        if self.inspector.same_uid_alive(self.pid) != Some(true) {
            return false;
        }
        self.inspector.argv(self.pid).as_deref() == Some(self.fingerprint_argv.as_slice())
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
#[path = "daemon_takeover_signal_tests.rs"]
mod tests;
