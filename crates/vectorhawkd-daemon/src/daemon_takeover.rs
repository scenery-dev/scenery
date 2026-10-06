//! Automatic takeover of an older/stale `vectorhawk daemon run` process.
//!
//! # Why this exists
//!
//! `acquire_socket` (in `lib.rs`) and `instance_lock` (this crate) both
//! refuse to start a second daemon while a first one is live — correct, but
//! incomplete: a daemon built before `instance_lock` (commit `3d45ddb`) and
//! `binary_watch` (v1.0.94–96) holds neither of those, only the socket, and
//! it never notices its own binary has been replaced. Such a daemon sits on
//! the socket forever; every subsequent `vectorhawk daemon run` (a fresh
//! install, an upgrade, a crash-restart) refuses to start, and stale
//! governance code keeps running. This module is the owner-approved fix:
//! verify the holder, then take over automatically.
//!
//! # The one rule this module exists to enforce
//!
//! **Never kill on uncertainty.** Every check below is a gate, not a
//! heuristic nudge: if a single one of them can't be answered with
//! confidence, the verdict is [`Verdict::Refuse`] — never a guess in the
//! direction of killing something. See `process_probe`'s doc comment for
//! the same philosophy applied to how each fact is gathered.
//!
//! # The rules (owner-specified)
//!
//! A pid is only ever taken over when **all** of these hold:
//! 1. it is not this process;
//! 2. it shares this process's effective uid;
//! 3. its argv matches `vectorhawk daemon run` **positionally** — see
//!    [`cmdline_is_daemon_run`]'s doc comment for why this must never be a
//!    substring test (a prior substring-matching version of this exact
//!    check killed an innocent `bash -c ...` shell during live
//!    verification — commit `0a862ca`);
//! 4. it is verifiably **older** than this build (via a version learned
//!    from its own socket `initialize` response — every build since M0
//!    answers this), **or** its on-disk executable has been replaced or
//!    deleted since it started.
//!
//! An equal-or-newer version is always refused, by design: it prevents two
//! daemons that each think the other is stale from killing each other in a
//! loop.
//!
//! This is the single place that decision is made — [`verify_pid`] is
//! called identically by `run_daemon` at startup (`lib.rs`) and by
//! `vectorhawk daemon install` (`vectorhawkd-cli`'s `install/linux.rs` and
//! `install/macos.rs`), so the rules above are enforced exactly once, not
//! reimplemented per call site.

use crate::process_probe::ProcessInspector;
use semver::Version;

/// Why a candidate was judged stale enough to take over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessReason {
    /// Learned via the candidate's own socket `initialize` response.
    OlderVersion(Version),
    /// The on-disk executable backing the candidate's pid is gone or no
    /// longer the one the candidate started from.
    ExeReplacedOrDeleted,
}

/// The outcome of [`verify_pid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Safe to take over, per the rules in this module's doc comment.
    TakeOver { pid: u32, reason: StalenessReason },
    /// Refuse — `pid` either fails a hard gate (uid/argv/self) or could not
    /// be verified as older. `reason` is a human-readable explanation
    /// naming the pid, suitable for a log line or error message.
    Refuse(String),
}

/// Given everything we can learn about `pid`, decide whether it is safe to
/// take over. See this module's doc comment for the exact rules.
///
/// `version_hint` is the candidate's version, learned by the caller from a
/// live socket `initialize` exchange (`peer_handshake::query_version`) —
/// `None` when no socket connection was available (the "flock held, no
/// reachable socket" case) or the exchange failed/timed out. When `None`,
/// only an [`StalenessReason::ExeReplacedOrDeleted`] signal can justify a
/// takeover; an unknown version with a healthy-looking executable always
/// refuses rather than guess.
pub fn verify_pid<I: ProcessInspector>(
    inspector: &I,
    pid: u32,
    our_pid: u32,
    our_version: &Version,
    version_hint: Option<Version>,
) -> Verdict {
    if pid == our_pid {
        return Verdict::Refuse(format!("pid {pid} is this process itself — refusing"));
    }

    match inspector.same_uid_alive(pid) {
        Some(true) => {}
        Some(false) => {
            return Verdict::Refuse(format!(
                "pid {pid} is alive but does not share this process's uid — refusing"
            ));
        }
        None => {
            return Verdict::Refuse(format!(
                "pid {pid} could not be confirmed alive under this process's uid — refusing"
            ));
        }
    }

    let argv_matches = inspector
        .argv(pid)
        .map(|argv| cmdline_is_daemon_run(&argv))
        .unwrap_or(false);
    if !argv_matches {
        return Verdict::Refuse(format!(
            "pid {pid} does not look like a `vectorhawk daemon run` process — refusing"
        ));
    }

    if inspector.exe_replaced_or_deleted(pid) == Some(true) {
        return Verdict::TakeOver {
            pid,
            reason: StalenessReason::ExeReplacedOrDeleted,
        };
    }

    match version_hint {
        Some(v) if v < *our_version => Verdict::TakeOver {
            pid,
            reason: StalenessReason::OlderVersion(v),
        },
        Some(v) => Verdict::Refuse(format!(
            "pid {pid} is running v{v}, which is not older than this build (v{our_version}) \
             — refusing to avoid a takeover loop"
        )),
        None => Verdict::Refuse(format!(
            "pid {pid}'s version could not be determined and its executable is not \
             detectably stale — refusing rather than guess"
        )),
    }
}

/// Given no socket connection to read peer credentials from, find the
/// single `vectorhawk daemon run` process (same uid, not us) that is
/// plausibly holding the instance lock.
///
/// Returns `Some(pid)` only when **exactly one** such candidate exists.
/// Zero candidates means nothing to take over; more than one is itself
/// ambiguous (which one holds the lock is not something this scan can
/// distinguish) and must not pick one at random — see this module's
/// "never kill on uncertainty" rule.
pub fn resolve_unique_daemon_run_pid<I: ProcessInspector>(
    inspector: &I,
    our_pid: u32,
) -> Option<u32> {
    let mut candidates: Vec<u32> = inspector
        .all_pids()
        .into_iter()
        .filter(|&pid| pid != our_pid)
        .filter(|&pid| inspector.same_uid_alive(pid) == Some(true))
        .filter(|&pid| {
            inspector
                .argv(pid)
                .map(|argv| cmdline_is_daemon_run(&argv))
                .unwrap_or(false)
        })
        .collect();

    candidates.dedup();
    match candidates.len() {
        1 => candidates.pop(),
        _ => None,
    }
}

/// Given the raw, NUL-separated argv bytes of a process (Linux
/// `/proc/<pid>/cmdline` shape, or the equivalent reconstructed by
/// `process_probe` on macOS from `KERN_PROCARGS2`), report whether it is a
/// `vectorhawk daemon run` invocation (with or without trailing args such
/// as `--foreground`).
///
/// Matches **positionally on argv**, not by substring: `argv[0]`'s basename
/// must be exactly `vectorhawk` (argv[0] may be a bare name or an absolute
/// path, e.g. `/home/linuxbrew/.linuxbrew/bin/vectorhawk`), `argv[1]` must
/// be exactly `daemon`, and `argv[2]` must be exactly `run`. Trailing args
/// are ignored.
///
/// This is a hard requirement, not a style preference: an earlier version
/// of this exact check (then duplicated across `reap_stray_daemons` and
/// `kill_daemon_process` in the CLI installer) matched by substring against
/// the whole cmdline blob (`contains("vectorhawk") && contains("daemon") &&
/// contains("run"/"foreground")`), and that killed an innocent process
/// during live verification — a `bash -c …` shell whose command line
/// happened to mention `vectorhawk`, `daemon`, and `/run/user/1000/bus`
/// nowhere near each other (commit `0a862ca`). Positional argv matching
/// cannot be fooled by unrelated tokens elsewhere on the line, and it also
/// naturally excludes `vectorhawk daemon install`/`restart` (the CLI
/// invocation doing the reaping/killing itself), since `argv[2]` there is
/// `install`/`restart`, not `run`.
///
/// This is the single implementation, used by both daemon startup
/// ([`verify_pid`]) and the CLI installer (`vectorhawkd-cli`'s
/// `install/mod.rs` re-exports this directly) — see this module's doc
/// comment.
pub fn cmdline_is_daemon_run(raw: &[u8]) -> bool {
    let mut argv = raw.split(|&b| b == 0);
    let Some(argv0) = argv.next() else {
        return false;
    };
    let Some(argv1) = argv.next() else {
        return false;
    };
    let Some(argv2) = argv.next() else {
        return false;
    };

    let basename = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    basename == b"vectorhawk" && argv1 == b"daemon" && argv2 == b"run"
}

#[cfg(test)]
#[path = "daemon_takeover_tests.rs"]
mod tests;
