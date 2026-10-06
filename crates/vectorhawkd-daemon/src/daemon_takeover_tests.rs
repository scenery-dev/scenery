//! Tests for `daemon_takeover`'s pure decision logic. Entirely fake-driven
//! — nothing here ever touches a real process.

use super::*;
use crate::process_probe::ProcessInspector;
use std::collections::HashMap;

/// A fully scripted, in-memory `ProcessInspector` — no real syscalls.
#[derive(Default)]
struct FakeInspector {
    argv: HashMap<u32, Vec<u8>>,
    same_uid_alive: HashMap<u32, Option<bool>>,
    exe_replaced_or_deleted: HashMap<u32, Option<bool>>,
    all_pids: Vec<u32>,
}

impl FakeInspector {
    fn daemon_run(mut self, pid: u32) -> Self {
        self.argv.insert(pid, b"vectorhawk\0daemon\0run".to_vec());
        self.same_uid_alive.insert(pid, Some(true));
        self.exe_replaced_or_deleted.insert(pid, Some(false));
        self
    }

    fn with_uid(mut self, pid: u32, same_uid: Option<bool>) -> Self {
        self.same_uid_alive.insert(pid, same_uid);
        self
    }

    fn with_argv(mut self, pid: u32, argv: &[u8]) -> Self {
        self.argv.insert(pid, argv.to_vec());
        self
    }

    fn with_exe_stale(mut self, pid: u32, stale: Option<bool>) -> Self {
        self.exe_replaced_or_deleted.insert(pid, stale);
        self
    }

    fn with_pids(mut self, pids: &[u32]) -> Self {
        self.all_pids = pids.to_vec();
        self
    }
}

impl ProcessInspector for FakeInspector {
    fn argv(&self, pid: u32) -> Option<Vec<u8>> {
        self.argv.get(&pid).cloned()
    }

    fn same_uid_alive(&self, pid: u32) -> Option<bool> {
        self.same_uid_alive.get(&pid).copied().flatten()
    }

    fn exe_replaced_or_deleted(&self, pid: u32) -> Option<bool> {
        self.exe_replaced_or_deleted.get(&pid).copied().flatten()
    }

    fn all_pids(&self) -> Vec<u32> {
        self.all_pids.clone()
    }
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

// ── cmdline_is_daemon_run (moved here from the CLI; re-pinning the
//    regression that motivated positional matching) ────────────────────────

#[test]
fn matches_bare_daemon_run() {
    assert!(cmdline_is_daemon_run(b"vectorhawk\0daemon\0run"));
}

#[test]
fn matches_absolute_path_with_trailing_flag() {
    assert!(cmdline_is_daemon_run(
        b"/home/linuxbrew/.linuxbrew/bin/vectorhawk\0daemon\0run\0--foreground"
    ));
}

#[test]
fn rejects_install_subcommand() {
    assert!(!cmdline_is_daemon_run(b"vectorhawk\0daemon\0install"));
}

#[test]
fn rejects_substring_only_match_the_live_incident() {
    // The exact shape of the 0a862ca incident: tokens present, but not in
    // the right argv positions.
    assert!(!cmdline_is_daemon_run(
        b"bash\0-c\0vectorhawk status; daemon stuff; /run/user/1000/bus"
    ));
}

// ── verify_pid: older version → take over ───────────────────────────────

#[test]
fn older_version_takes_over() {
    let inspector = FakeInspector::default().daemon_run(100);
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("1.0.0")));
    assert_eq!(
        verdict,
        Verdict::TakeOver {
            pid: 100,
            reason: StalenessReason::OlderVersion(v("1.0.0"))
        }
    );
}

// ── verify_pid: equal/newer version → refuse ────────────────────────────

#[test]
fn equal_version_refuses() {
    let inspector = FakeInspector::default().daemon_run(100);
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("2.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

#[test]
fn newer_version_refuses() {
    let inspector = FakeInspector::default().daemon_run(100);
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("3.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

// ── verify_pid: argv mismatch → refuse ──────────────────────────────────

#[test]
fn argv_mismatch_refuses_even_with_older_version() {
    let inspector = FakeInspector::default()
        .with_uid(100, Some(true))
        .with_argv(100, b"bash\0-c\0something vectorhawk daemon run adjacent");
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("1.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

#[test]
fn missing_argv_refuses() {
    let inspector = FakeInspector::default().with_uid(100, Some(true));
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("1.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

// ── verify_pid: different uid → refuse ──────────────────────────────────

#[test]
fn different_uid_refuses() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_uid(100, Some(false));
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("1.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

#[test]
fn unknown_uid_refuses() {
    let inspector = FakeInspector::default().daemon_run(100).with_uid(100, None);
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), Some(v("1.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

// ── verify_pid: it's us → refuse ────────────────────────────────────────

#[test]
fn self_pid_refuses() {
    let inspector = FakeInspector::default().daemon_run(42);
    let verdict = verify_pid(&inspector, 42, 42, &v("2.0.0"), Some(v("1.0.0")));
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

// ── verify_pid: exe replaced/deleted → take over, even with no version ──

#[test]
fn exe_replaced_takes_over_without_a_version() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_exe_stale(100, Some(true));
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), None);
    assert_eq!(
        verdict,
        Verdict::TakeOver {
            pid: 100,
            reason: StalenessReason::ExeReplacedOrDeleted
        }
    );
}

#[test]
fn exe_replaced_takes_priority_over_a_newer_reported_version() {
    // Defensive: exe-staleness is itself sufficient grounds, and must not
    // be overridden by a version signal that looks newer (e.g. a spoofed
    // or buggy version string on a build that is, in fact, abandoned).
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_exe_stale(100, Some(true));
    let verdict = verify_pid(&inspector, 100, 1, &v("1.0.0"), Some(v("99.0.0")));
    assert_eq!(
        verdict,
        Verdict::TakeOver {
            pid: 100,
            reason: StalenessReason::ExeReplacedOrDeleted
        }
    );
}

// ── verify_pid: no version, no exe-staleness signal → refuse (ambiguous) ─

#[test]
fn unknown_version_and_unknown_staleness_refuses() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_exe_stale(100, None);
    let verdict = verify_pid(&inspector, 100, 1, &v("2.0.0"), None);
    assert!(matches!(verdict, Verdict::Refuse(_)));
}

// ── resolve_unique_daemon_run_pid ───────────────────────────────────────

#[test]
fn resolve_unique_finds_the_single_candidate() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_pids(&[1, 50, 100]);
    assert_eq!(resolve_unique_daemon_run_pid(&inspector, 1), Some(100));
}

#[test]
fn resolve_unique_excludes_self() {
    let inspector = FakeInspector::default().daemon_run(1).with_pids(&[1]);
    assert_eq!(resolve_unique_daemon_run_pid(&inspector, 1), None);
}

#[test]
fn resolve_unique_refuses_when_ambiguous_multiple_candidates() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .daemon_run(200)
        .with_pids(&[1, 100, 200]);
    assert_eq!(resolve_unique_daemon_run_pid(&inspector, 1), None);
}

#[test]
fn resolve_unique_returns_none_when_no_candidates() {
    let inspector = FakeInspector::default().with_pids(&[1, 50]);
    assert_eq!(resolve_unique_daemon_run_pid(&inspector, 1), None);
}

#[test]
fn resolve_unique_skips_candidates_with_wrong_uid() {
    let inspector = FakeInspector::default()
        .daemon_run(100)
        .with_uid(100, Some(false))
        .with_pids(&[1, 100]);
    assert_eq!(resolve_unique_daemon_run_pid(&inspector, 1), None);
}
