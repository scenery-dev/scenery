//! Tests for `process_probe`.
//!
//! Per the hard safety rule governing this whole feature: these tests only
//! ever signal a process they spawned themselves (a `sleep` child), never
//! an ambient process on the host. That also means the real platform argv
//! parser gets exercised end-to-end against a real process — and the
//! positional matcher (`daemon_takeover::cmdline_is_daemon_run`) must keep
//! rejecting it, proving the production matcher cannot be fooled by an
//! unrelated process that happens to run under the same uid.

use super::*;
use std::process::{Child, Command};

/// Spawn our own short-lived `sleep` child so tests can exercise the real
/// inspector/killer against a real pid without ever touching a process we
/// didn't start. Always reaped (`wait()`) before the test ends so it never
/// leaks a zombie.
fn spawn_dummy_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn dummy `sleep` child for test")
}

/// What the platform actually reported when `argv` came back `None`, so a
/// failure on a CI box we can't reproduce on explains itself.
fn argv_diagnostics(pid: u32, child: &mut Child) -> String {
    let exited = format!("try_wait={:?}", child.try_wait());
    #[cfg(target_os = "linux")]
    {
        let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(b) => format!("cmdline read ok, {} bytes", b.len()),
            Err(e) => format!("cmdline read err: {e}"),
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_else(|e| format!("stat read err: {e}"));
        format!("{exited}; {cmdline}; stat={}", stat.trim())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        exited
    }
}

#[test]
fn same_uid_alive_true_for_our_own_child() {
    let mut child = spawn_dummy_child();
    let pid = child.id();

    assert_eq!(SystemProcessOps.same_uid_alive(pid), Some(true));

    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait();
}

#[test]
fn same_uid_alive_none_after_process_exits_and_is_reaped() {
    let mut child = spawn_dummy_child();
    let pid = child.id();

    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait(); // reap — otherwise it's a zombie and still "alive" to kill(pid, 0)

    assert_eq!(
        SystemProcessOps.same_uid_alive(pid),
        None,
        "a reaped, exited pid must not read back as alive"
    );
}

#[test]
fn real_argv_of_dummy_child_is_rejected_by_the_production_matcher() {
    let mut child = spawn_dummy_child();
    let pid = child.id();

    // `spawn` (posix_spawn / CLONE_VFORK) resumes us as soon as the kernel
    // swaps in the child's new mm — before exec has finished, so on a slow
    // box /proc/<pid>/cmdline can still read back empty. Poll (bounded) for
    // exec to complete rather than reading once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut argv = SystemProcessOps.argv(pid);
    while argv.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        argv = SystemProcessOps.argv(pid);
    }
    assert!(
        argv.is_some(),
        "expected to read back argv for our own child; {}",
        argv_diagnostics(pid, &mut child)
    );
    let argv = argv.unwrap();
    assert!(
        !crate::daemon_takeover::cmdline_is_daemon_run(&argv),
        "a `sleep` child must never match the daemon-run matcher: {argv:?}"
    );

    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait();
}

#[test]
fn all_pids_includes_our_own_child() {
    let mut child = spawn_dummy_child();
    let pid = child.id();

    let pids = SystemProcessOps.all_pids();
    assert!(
        pids.contains(&pid),
        "all_pids() should have found our freshly spawned child {pid}"
    );

    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait();
}

#[test]
fn exe_replaced_or_deleted_is_false_for_our_own_still_running_binary() {
    // `exe_replaced_or_deleted` compares a pid's backing executable against
    // `binary_watch::resolve_watch_path()` — the currently-installed
    // `vectorhawk` binary. That comparison is only meaningful for pids that
    // already passed the `vectorhawk daemon run` argv check (an unrelated
    // binary like `sleep` will always "mismatch" our own exe, which is not
    // a bug — it is simply not this function's intended input domain). The
    // one case this test can check without spawning a fake `vectorhawk`
    // process: our own still-running pid must never read back as stale,
    // since it is by definition running from the path it resolves to.
    let our_pid = std::process::id();
    assert_ne!(
        SystemProcessOps.exe_replaced_or_deleted(our_pid),
        Some(true),
        "a process must never appear stale relative to its own running binary"
    );
}

#[test]
fn exe_replaced_or_deleted_does_not_panic_on_an_unrelated_process() {
    // Contract check only: calling this on a pid outside its intended
    // domain (not a verified `vectorhawk daemon run`) must still return
    // cleanly, never panic — callers are protected from acting on it by
    // `verify_pid`'s prior argv gate, not by this function refusing to run.
    let mut child = spawn_dummy_child();
    let pid = child.id();
    let _ = SystemProcessOps.exe_replaced_or_deleted(pid);
    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait();
}

#[test]
fn kill_on_already_reaped_pid_returns_an_error_not_a_panic() {
    let mut child = spawn_dummy_child();
    let pid = child.id();
    SystemProcessOps.terminate(pid).expect("terminate");
    let _ = child.wait();

    // The pid is gone; signaling it again must be a clean `Err`, never a
    // panic — library code in this crate must not panic (see crate-level
    // policy).
    assert!(SystemProcessOps.kill(pid).is_err());
}

#[cfg(target_os = "macos")]
mod macos_argv_parsing {
    use super::super::platform::parse_kern_procargs2;

    /// Build a synthetic `KERN_PROCARGS2` buffer: `argc`, an exec path,
    /// NUL padding, then `argc` NUL-terminated argv strings.
    fn build_buffer(exec_path: &str, args: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(args.len() as i32).to_ne_bytes());
        buf.extend_from_slice(exec_path.as_bytes());
        buf.push(0);
        // A couple of extra NUL padding bytes, as the kernel actually emits.
        buf.extend_from_slice(&[0, 0, 0]);
        for arg in args {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn parses_argv_matching_daemon_run() {
        let buf = build_buffer(
            "/opt/homebrew/bin/vectorhawk",
            &["vectorhawk", "daemon", "run"],
        );
        let argv = parse_kern_procargs2(&buf).expect("should parse");
        assert_eq!(argv, b"vectorhawk\0daemon\0run");
    }

    #[test]
    fn parses_argv_with_trailing_flag() {
        let buf = build_buffer(
            "/opt/homebrew/bin/vectorhawk",
            &["vectorhawk", "daemon", "run", "--foreground"],
        );
        let argv = parse_kern_procargs2(&buf).expect("should parse");
        assert_eq!(argv, b"vectorhawk\0daemon\0run\0--foreground");
        assert!(crate::daemon_takeover::cmdline_is_daemon_run(&argv));
    }

    #[test]
    fn parses_unrelated_process_argv() {
        let buf = build_buffer("/bin/sleep", &["sleep", "30"]);
        let argv = parse_kern_procargs2(&buf).expect("should parse");
        assert_eq!(argv, b"sleep\x0030".to_vec());
        assert!(!crate::daemon_takeover::cmdline_is_daemon_run(&argv));
    }

    #[test]
    fn truncated_buffer_does_not_panic() {
        let mut buf = build_buffer("/bin/sleep", &["sleep", "30"]);
        buf.truncate(buf.len() - 1); // cut off mid-last-arg
        let _ = parse_kern_procargs2(&buf); // must not panic
    }

    #[test]
    fn empty_buffer_returns_none() {
        assert_eq!(parse_kern_procargs2(&[]), None);
    }

    #[test]
    fn negative_argc_returns_none() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(parse_kern_procargs2(&buf), None);
    }
}
