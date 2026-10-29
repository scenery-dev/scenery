//! Tests for `execute_takeover`'s pure stepping logic. `FakeTakeoverIo`
//! never sleeps for real and never touches a process — every scenario is
//! driven purely by call counts.

use super::*;

#[derive(Default)]
struct FakeTakeoverIo {
    signals_sent: Vec<Signal>,
    /// `resource_free` returns `true` starting from this call index
    /// (0-based across the whole run, including the pre-loop check).
    free_from_call: Option<usize>,
    resource_free_calls: usize,
    still_same_process: bool,
    sleep_calls: usize,
}

impl FakeTakeoverIo {
    fn never_frees() -> Self {
        Self {
            still_same_process: true,
            ..Default::default()
        }
    }

    fn frees_on_call(n: usize) -> Self {
        Self {
            free_from_call: Some(n),
            still_same_process: true,
            ..Default::default()
        }
    }
}

impl TakeoverIo for FakeTakeoverIo {
    fn send_signal(&mut self, signal: Signal) -> std::io::Result<()> {
        self.signals_sent.push(signal);
        Ok(())
    }

    fn resource_free(&mut self) -> bool {
        let idx = self.resource_free_calls;
        self.resource_free_calls += 1;
        match self.free_from_call {
            Some(n) => idx >= n,
            None => false,
        }
    }

    fn still_same_process(&mut self) -> bool {
        self.still_same_process
    }

    fn sleep(&mut self, _d: Duration) {
        self.sleep_calls += 1;
    }
}

const TERM_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);

// ── holder exits on TERM → acquire succeeds, no KILL ever sent ─────────

#[test]
fn holder_exits_immediately_on_term_acquires_without_kill() {
    // resource_free() is called once before any sleep; make that first
    // call already report free.
    let mut io = FakeTakeoverIo::frees_on_call(0);
    let result = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    assert_eq!(result, TakeoverResult::AcquiredAfterTerm);
    assert_eq!(io.signals_sent, vec![Signal::Term]);
}

#[test]
fn holder_exits_after_a_couple_of_polls_on_term_acquires_without_kill() {
    let mut io = FakeTakeoverIo::frees_on_call(3);
    let result = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    assert_eq!(result, TakeoverResult::AcquiredAfterTerm);
    assert_eq!(io.signals_sent, vec![Signal::Term]);
    assert!(io.sleep_calls > 0, "should have polled/slept at least once");
}

// ── holder ignores TERM, dies on KILL → acquire succeeds ───────────────

#[test]
fn holder_survives_term_but_dies_on_kill_acquires() {
    // Never frees during the TERM phase; frees shortly after KILL is sent.
    // TERM_TIMEOUT / POLL = 50 polls in the term phase, plus the initial
    // check = 51 `resource_free` calls before KILL is sent.
    let mut io = FakeTakeoverIo::frees_on_call(53);
    let result = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    assert_eq!(result, TakeoverResult::AcquiredAfterKill);
    assert_eq!(io.signals_sent, vec![Signal::Term, Signal::Kill]);
}

// ── holder never dies, even after KILL → give up, named in the result ──

#[test]
fn holder_never_dies_gives_up_after_kill() {
    let mut io = FakeTakeoverIo::never_frees();
    let result = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    assert_eq!(result, TakeoverResult::GaveUpAfterKill);
    assert_eq!(io.signals_sent, vec![Signal::Term, Signal::Kill]);
}

// ── PID reuse between TERM and KILL → no KILL is ever sent ─────────────

#[test]
fn pid_reused_between_term_and_kill_never_sends_kill() {
    let mut io = FakeTakeoverIo::never_frees();
    io.still_same_process = false; // simulates a different process now at this pid
    let result = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    assert_eq!(result, TakeoverResult::RefusedPidReused);
    assert_eq!(
        io.signals_sent,
        vec![Signal::Term],
        "SIGKILL must never be sent once the pid-reuse guard fails"
    );
}

// ── bounds: term timeout and kill grace are each actually bounded ──────

#[test]
fn term_phase_polls_bounded_number_of_times() {
    let mut io = FakeTakeoverIo::never_frees();
    io.still_same_process = false;
    let _ = execute_takeover(&mut io, TERM_TIMEOUT, KILL_GRACE, POLL);
    // One initial check + one per poll interval within term_timeout.
    let max_term_polls = (TERM_TIMEOUT.as_millis() / POLL.as_millis()) as usize + 1;
    assert!(
        io.resource_free_calls <= max_term_polls,
        "term phase must not poll past its bounded timeout: {} calls, bound {}",
        io.resource_free_calls,
        max_term_polls
    );
}
