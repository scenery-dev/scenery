//! Platform-appropriate daemon install/uninstall handlers.
//!
//! Provides four public entry points used by the CLI handlers:
//! - [`install`] — writes the auto-start unit and starts the daemon now.
//! - [`uninstall`] — stops the daemon and removes the auto-start unit.
//! - [`ensure_installed`] — idempotent; installs only when not already installed.
//! - [`status`] — returns [`InstallStatus`] for `doctor` reporting.
//!
//! Platform dispatch happens inside each function via `#[cfg]`; callers see a
//! single cross-platform API. Windows is deferred — the seam is clean because
//! the cfg gates are per-function, not wrapping the whole module.

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

/// Escape hatch for the `HOME`-override gate in [`install`]: set to any
/// non-empty value to skip the check.
///
/// The gate exists to catch Homebrew's `post_install` sandbox (see
/// `home_is_overridden`'s docs), but a handful of setups deliberately run
/// with `HOME` pointed away from the invoking user's passwd entry —
/// containers that set `HOME` themselves, or a deliberate `sudo -u other`
/// install on behalf of a service account. Those are legitimate; this var
/// lets them opt out of the gate explicitly rather than silently disabling
/// it.
pub const ALLOW_HOME_MISMATCH_ENV: &str = "VH_ALLOW_HOME_MISMATCH";

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of [`status`], consumed by the `doctor` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStatus {
    /// No auto-start unit file found.
    NotInstalled,
    /// Unit file is present but the daemon socket is not reachable.
    InstalledNotRunning {
        /// Absolute path to the unit file (plist or .service).
        unit_path: String,
    },
    /// Unit file is present and the daemon socket is reachable.
    InstalledAndRunning {
        /// Absolute path to the unit file (plist or .service).
        unit_path: String,
    },
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Install the platform auto-start unit and start the daemon immediately.
///
/// Idempotent: if the unit is already installed and running this is a no-op
/// (prints a notice and returns `Ok`). If it is installed but not running, the
/// daemon is restarted.
pub fn install() -> Result<()> {
    // Refuse to "install" into an overridden HOME (Homebrew `post_install`
    // sandbox, most commonly). Both platform installers below resolve their
    // unit path from HOME — macOS via `dirs::home_dir()`, Linux via
    // `dirs::config_dir()` — so writing the unit into a HOME that isn't this
    // user's real home writes it somewhere the service manager never reads,
    // then reports success on a unit that's about to vanish. Only `install`
    // is gated: `uninstall`/`status`/`restart` don't write anything into
    // HOME that matters here, and `ensure_installed` calls `install` so it
    // inherits this for free.
    check_home_not_overridden()?;

    #[cfg(target_os = "macos")]
    return macos::install();

    #[cfg(target_os = "linux")]
    return linux::install();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!(
        "vectorhawk daemon install is not yet supported on this platform \
         (macOS and Linux only)"
    )
}

/// Remove the platform auto-start unit and stop the daemon.
///
/// If nothing is installed, prints a notice and returns `Ok(())`.
pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::uninstall();

    #[cfg(target_os = "linux")]
    return linux::uninstall();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!(
        "vectorhawk daemon uninstall is not yet supported on this platform \
         (macOS and Linux only)"
    )
}

/// Restart the daemon in place: kills the running process and starts a fresh
/// one without rewriting the unit file. Useful for picking up a new auth
/// token, env vars, or to recover from a stuck state.
pub fn restart() -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::restart();

    #[cfg(target_os = "linux")]
    return linux::restart();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!(
        "vectorhawk daemon restart is not yet supported on this platform \
         (macOS and Linux only)"
    )
}

/// Idempotent install: install only when the unit is not already present.
///
/// Used by `mcp setup` to provision the daemon transparently. Never errors if
/// the daemon is already running.
pub fn ensure_installed() -> Result<()> {
    match status()? {
        InstallStatus::NotInstalled => {
            install()?;
        }
        InstallStatus::InstalledNotRunning { .. } => {
            // Unit exists but daemon is not up — attempt a start.
            install()?;
        }
        InstallStatus::InstalledAndRunning { .. } => {
            // Already good. Nothing to do.
        }
    }
    Ok(())
}

/// Return the current install and running state of the daemon agent.
pub fn status() -> Result<InstallStatus> {
    #[cfg(target_os = "macos")]
    return macos::status();

    #[cfg(target_os = "linux")]
    return linux::status();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Ok(InstallStatus::NotInstalled)
}

// ── HOME-override guard ──────────────────────────────────────────────────────
//
// Homebrew's `post_install` runs with HOME pointed at a throwaway sandbox
// directory it deletes seconds later. Both platform installers resolve their
// unit path from HOME (`dirs::config_dir()` on Linux, `dirs::home_dir()` on
// macOS), so a real `daemon install` under that sandbox wrote the systemd
// unit into a tree that no longer existed by the time anything went looking
// for it — while printing "Wrote systemd user unit: …" and "Systemd user
// unit enabled and started" right after. The second line wasn't a lie:
// `systemctl --user` keys off XDG_RUNTIME_DIR, not HOME, so it genuinely
// started whatever unit already happened to be registered. The first line
// was: the file it just wrote was never in a place systemd (or Launchd) will
// ever read again. Net effect, live on a real `brew upgrade`: Homebrew never
// installed a working auto-start unit, while reporting that it did. This
// guard turns that into a hard error before any file gets written, rather
// than a print-and-continue.

/// Pure decision: has `HOME` been overridden away from the current user's
/// real home directory?
///
/// `env_home` is the raw `HOME` environment variable; `passwd_home` is the
/// `pw_dir` field from the password database entry for the current uid (see
/// [`passwd_home_dir`]) — a lookup that does not go through `HOME` at all,
/// so it reflects the *real* home directory regardless of what the process
/// environment claims.
///
/// - Both `Some` and they differ (after trimming a single trailing `/` from
///   each, so `/home/x` and `/home/x/` count as equal) → `true`.
/// - Either side is `None`, or they're equal → `false`. This is deliberately
///   conservative: when we can't determine the real home (odd NSS setup, no
///   `HOME` set at all), we must not block the install on a guess.
///
/// Compares the raw strings only — does not canonicalize. A symlinked home
/// directory must not be treated as an override just because one path is a
/// symlink and the other its target; the caller may canonicalize both sides
/// first (see `check_home_not_overridden`) when it can do so without
/// failing, and fall back to these raw values otherwise.
pub(crate) fn home_is_overridden(env_home: Option<&str>, passwd_home: Option<&str>) -> bool {
    match (env_home, passwd_home) {
        (Some(env), Some(passwd)) => {
            env.strip_suffix('/').unwrap_or(env) != passwd.strip_suffix('/').unwrap_or(passwd)
        }
        _ => false,
    }
}

/// Look up the current user's home directory from the password database
/// (`getpwuid(getuid())`), independent of the `HOME` environment variable —
/// this is the "ground truth" [`home_is_overridden`] compares `HOME`
/// against.
///
/// Returns `None` when the uid has no passwd entry or its `pw_dir` is null
/// (both effectively "unknown," not "overridden" — see
/// `home_is_overridden`'s conservative-fallback rule).
#[cfg(unix)]
pub(crate) fn passwd_home_dir() -> Option<String> {
    // SAFETY: getuid() cannot fail. getpwuid() returns either a null
    // pointer or a pointer to a struct passwd owned by libc (static/TLS
    // storage) that stays valid until the next passwd-database call on this
    // thread; nothing else in this block calls into libc's passwd/group
    // APIs, and the CStr is copied into an owned String before the block
    // ends, so nothing borrowed from it escapes.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        let pw_dir = (*pw).pw_dir;
        if pw_dir.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Gate for [`install`]: refuse to proceed when `HOME` has been overridden
/// away from the real home directory, unless [`ALLOW_HOME_MISMATCH_ENV`] is
/// set.
#[cfg(unix)]
fn check_home_not_overridden() -> Result<()> {
    if std::env::var_os(ALLOW_HOME_MISMATCH_ENV).is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }

    let env_home = std::env::var("HOME").ok();
    let passwd_home = passwd_home_dir();

    // Prefer comparing canonical forms so a symlinked home (e.g. macOS's
    // /Users -> /System/Volumes/Data/Users) isn't mistaken for an override.
    // Only use the canonical forms when *both* sides resolve — a HOME that
    // doesn't exist (the Homebrew sandbox case, or just a typo) must still
    // be compared, not silently ignored because canonicalize() errored.
    let canonical = match (&env_home, &passwd_home) {
        (Some(e), Some(p)) => match (std::fs::canonicalize(e), std::fs::canonicalize(p)) {
            (Ok(e), Ok(p)) => Some((
                e.to_string_lossy().into_owned(),
                p.to_string_lossy().into_owned(),
            )),
            _ => None,
        },
        _ => None,
    };
    let (cmp_env, cmp_passwd): (Option<&str>, Option<&str>) = match &canonical {
        Some((e, p)) => (Some(e.as_str()), Some(p.as_str())),
        None => (env_home.as_deref(), passwd_home.as_deref()),
    };

    if home_is_overridden(cmp_env, cmp_passwd) {
        anyhow::bail!(
            "refusing to install: HOME is {} but this user's home is {}. \
             The auto-start unit would be written somewhere the service \
             manager never reads, so the install would silently do \
             nothing. Re-run with the correct HOME, or set \
             {ALLOW_HOME_MISMATCH_ENV}=1 to override.",
            env_home.unwrap_or_default(),
            passwd_home.unwrap_or_default(),
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_home_not_overridden() -> Result<()> {
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Return the binary path that should be written into the auto-start unit.
///
/// When the current executable lives inside a Homebrew Cellar directory (e.g.
/// `/opt/homebrew/Cellar/vectorhawk/1.0.45/bin/vectorhawk` or
/// `/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.45/bin/vectorhawk`),
/// rewrite to the unversioned symlink directory so that `brew upgrade`
/// automatically picks up the new binary without re-running `daemon install`.
///
/// For any other install location (manual, cargo install, etc.) the
/// `std::env::current_exe()` result is returned unchanged.
pub(crate) fn resolve_daemon_bin_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve current binary path")?;
    Ok(vectorhawkd_core::binary_path::rewrite_homebrew_cellar_to_symlink(&exe))
}

/// Probe, **instantaneously**, whether the daemon Unix socket at
/// `socket_path` is currently accepting connections.
///
/// This is a *single-shot* `UnixStream::connect` — it does not wait or
/// retry. `timeout_ms` is **not** a wait budget: it only sets a read timeout
/// on the stream *after* a successful connect (so a later read on that
/// stream doesn't block forever). On the failure path — nothing listening
/// yet, which is the common case right after a service start — `timeout_ms`
/// has no effect whatsoever and this returns `false` immediately.
///
/// Callers that need to wait for the socket to come up (e.g. right after
/// starting the daemon) must poll this in a loop — see [`wait_for_socket`].
/// This raw probe is what `status()` on both platforms wants: an honest
/// instantaneous read of "is anything there right now."
#[cfg(unix)]
pub(crate) fn socket_is_reachable(socket_path: &str, timeout_ms: u64) -> bool {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    match UnixStream::connect(socket_path) {
        Ok(stream) => {
            // We connected — try setting a brief read timeout to confirm the
            // socket is live (not just accepting and immediately closing).
            let _ = stream.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
            true
        }
        Err(_) => false,
    }
}

/// Poll [`socket_is_reachable`] on a short interval until `socket_path`
/// accepts a connection or `timeout` elapses.
///
/// Unlike `socket_is_reachable`'s `timeout_ms` parameter (a post-connect
/// read timeout, not a wait — see its doc comment), this is a real bounded
/// wait: it is what install paths need right after starting the daemon,
/// where the socket is not expected to exist yet for at least a few hundred
/// milliseconds (tokio init + `state.db` open + bind).
// Only `linux.rs` (`#[cfg(target_os = "linux")]`) calls these outside of the
// `#[cfg(test)]` module below, so on a macOS build they are otherwise dead
// code — genuinely unused there, not a bug.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn wait_for_socket(socket_path: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if socket_is_reachable(socket_path, 200) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ── Linux systemd decision logic (pure; lives here so `cargo test` can reach ──
// ── it — `linux.rs` is `#[cfg(target_os = "linux")]` and does not compile on ──
// ── the macOS host this is developed on). ──────────────────────────────────

/// Coarse state of the `vectorhawk-agent.service` systemd user unit, as read
/// from `systemctl --user show <unit> -p ActiveState -p SubState`.
///
/// `AutoRestart` is its own variant (not folded into `Activating`) because it
/// is the state that matters most here: `SubState=auto-restart` is reported
/// with `ActiveState=activating`, but it means systemd is mid-crash-loop on
/// the unit, not freshly starting it — a materially different situation for
/// [`unit_is_healthy`] and for the diagnostic message `install_systemd`
/// prints when the unit never comes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum SystemdState {
    /// `ActiveState=active` (and not auto-restarting).
    Active,
    /// `ActiveState=activating`, genuinely starting up (not auto-restart).
    Activating,
    /// `SubState=auto-restart` — systemd is between crash-loop restarts.
    AutoRestart,
    /// `ActiveState=failed`.
    Failed,
    /// `ActiveState=inactive` (or `dead`) — unit is not running at all.
    Inactive,
    /// Anything else, or the state could not be determined (systemctl
    /// missing, unit absent, unparseable output).
    Unknown,
}

/// Given the raw contents of `/proc/<pid>/cgroup`, report whether that
/// process belongs to the named systemd unit (e.g.
/// `"vectorhawk-agent.service"`).
///
/// Handles the cgroup v2 single-line shape:
/// `0::/user.slice/user-1000.slice/user@1000.service/app.slice/vectorhawk-agent.service`
/// as well as cgroup v1's multi-line `N:controller:/path` form. A unit name
/// only counts as a match when it is a **complete path segment** (bounded by
/// `/` or end-of-line) — a plain substring search would be fooled by, e.g.,
/// a sibling unit or a user-chosen path component that happens to contain
/// the service name as infix text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn cgroup_is_service(cgroup_file_contents: &str, service_name: &str) -> bool {
    cgroup_file_contents
        .lines()
        .any(|line| line.split('/').any(|segment| segment == service_name))
}

/// Given the raw, NUL-separated `/proc/<pid>/cmdline` bytes of a process,
/// report whether it is a `vectorhawk daemon run` invocation (with or
/// without trailing args such as `--foreground`).
///
/// Matches **positionally on argv**, not by substring: `argv[0]`'s basename
/// must be exactly `vectorhawk` (argv[0] may be a bare name or an absolute
/// path, e.g. `/home/linuxbrew/.linuxbrew/bin/vectorhawk`), `argv[1]` must
/// be exactly `daemon`, and `argv[2]` must be exactly `run`. Trailing args
/// are ignored.
///
/// This is a hard requirement, not a style preference: an earlier version
/// of both `reap_stray_daemons` and `kill_daemon_process` matched by
/// substring against the whole cmdline blob (`contains("vectorhawk") &&
/// contains("daemon") && contains("run"/"foreground")`), and that killed an
/// innocent process during live verification — a `bash -c …` shell whose
/// command line happened to mention `vectorhawk`, `daemon`, and
/// `/run/user/1000/bus` nowhere near each other. Positional argv matching
/// cannot be fooled by unrelated tokens elsewhere on the line, and it also
/// naturally excludes `vectorhawk daemon install`/`restart` (the CLI
/// invocation doing the reaping/killing itself), since `argv[2]` there is
/// `install`/`restart`, not `run`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn cmdline_is_daemon_run(raw: &[u8]) -> bool {
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

/// Pure decision: is a running daemon process executing a stale binary?
///
/// This is the `brew upgrade` follow-on to the `auto-restart` bug above: on
/// Linux a running process keeps executing its binary's inode after the file
/// backing it is replaced or removed, so `systemctl --user show` can report
/// `MainPID` unchanged and `NRestarts=0` — genuinely `Active`, genuinely
/// healthy by every check that came before this one — while the process is
/// still serving last release's code. Verified live going 1.0.91 → 1.0.92:
/// `MainPID` never changed, but `readlink /proc/<pid>/exe` still showed the
/// 1.0.91 Cellar path marked `(deleted)`.
///
/// `exe_target` is the raw `readlink /proc/<pid>/exe` result (may carry the
/// kernel's `" (deleted)"` suffix). `canonical_expected` is the canonical
/// form of the binary path the unit *should* be running, or `None` when the
/// caller could not determine it.
///
/// - `exe_target` ends with `" (deleted)"` → stale, unconditionally. This is
///   the common case: Homebrew has already removed the old Cellar directory,
///   so the kernel appends this marker to the magic-symlink target.
/// - Otherwise, stale when `canonical_expected` is `Some` and differs from
///   `exe_target`. This catches the case where Homebrew hasn't cleaned up
///   the old Cellar directory yet — the link carries no `(deleted)` marker,
///   but still resolves to the *previous* version's still-present path.
/// - `canonical_expected` is `None` → **never** stale. Be conservative: when
///   we can't determine what "current" should be, we must not force a
///   restart on a guess.
///
/// **Caller trap, pinned by test below:** the rendered unit's `ExecStart`
/// holds the *unversioned* Homebrew symlink (`resolve_daemon_bin_path`'s
/// rewrite target, e.g. `/home/linuxbrew/.linuxbrew/bin/vectorhawk`), while
/// `/proc/<pid>/exe` is a magic symlink the kernel always resolves through
/// to the *real* target (e.g.
/// `…/Cellar/vectorhawk/1.0.92/bin/vectorhawk`). Comparing `ExecStart`'s raw
/// text against `exe_target` would therefore mismatch on **every** healthy
/// install — the unversioned symlink text is never equal to what
/// `/proc/.../exe` reports, healthy or not — and turn this into a restart
/// loop on every single `daemon install`. The caller MUST
/// `std::fs::canonicalize()` the resolved binary path first and pass that
/// canonical form as `canonical_expected`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn exe_is_stale(exe_target: &str, canonical_expected: Option<&str>) -> bool {
    if exe_target.ends_with(" (deleted)") {
        return true;
    }
    match canonical_expected {
        Some(expected) => exe_target != expected,
        None => false,
    }
}

/// Pure decision: is the systemd-managed daemon actually healthy, i.e. is it
/// safe to treat `daemon install` as a no-op ("already installed and up to
/// date")?
///
/// This is deliberately strict — `Active`, a reachable socket, and *not*
/// running a stale binary (see [`exe_is_stale`]), nothing looser. An earlier
/// version of the installer's idempotency guard only checked "unit exists,
/// is enabled, ExecStart matches the current binary" and returned early on
/// that alone, without ever asking whether the unit was actually running.
/// That let a box stuck in `SubState=auto-restart` (e.g. because a stray
/// daemon left over from an older, buggy install — see
/// `reap_stray_daemons`'s docs — is holding the socket) report "no changes
/// made" on every subsequent `daemon install`,
/// forever, because the early return fired *before* the stray-reaping and
/// restart logic ever ran. `Activating` is deliberately excluded too: an
/// in-progress start is not yet a settled "healthy," and treating it as good
/// enough would risk the same silent no-op if it never actually finishes.
///
/// `exe_stale` folds in the `brew upgrade` case: a unit can be genuinely
/// `Active` with a live socket while the running process is still executing
/// last release's binary from a deleted inode (see [`exe_is_stale`]'s doc
/// for the live incident). That is a form of unhealthy too — it must fall
/// through to the same repair path as `auto-restart`, not early-return "no
/// changes made," or every release's fixes silently fail to take effect on
/// upgrade until something else restarts the service.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn unit_is_healthy(
    state: Option<SystemdState>,
    socket_up: bool,
    exe_stale: bool,
) -> bool {
    state == Some(SystemdState::Active) && socket_up && !exe_stale
}

/// Resolve the platform socket path without bootstrapping AppState (avoids
/// creating state dirs just for a status check).
#[cfg(unix)]
pub(crate) fn daemon_socket_path() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let base = std::path::PathBuf::from(runtime).join("vectorhawk");
            if let Some(s) = base.join("agent.sock").to_str() {
                return s.to_string();
            }
        }
    }

    // macOS and Linux fallback: socket lives alongside state.db
    if let Some(data_dir) = dirs::data_dir() {
        if let Some(s) = data_dir.join("VectorHawk").join("agent.sock").to_str() {
            return s.to_string();
        }
    }

    // Last resort (will not work, but is a valid path string).
    "~/.local/share/VectorHawk/agent.sock".to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        cgroup_is_service, cmdline_is_daemon_run, exe_is_stale, home_is_overridden,
        unit_is_healthy, SystemdState,
    };

    // ── home_is_overridden ─────────────────────────────────────────────────

    #[test]
    fn home_equal_paths_is_not_overridden() {
        assert!(!home_is_overridden(
            Some("/home/spaceghost"),
            Some("/home/spaceghost")
        ));
    }

    #[test]
    fn home_trailing_slash_difference_is_not_overridden() {
        // A single trailing slash is not a meaningful difference.
        assert!(!home_is_overridden(
            Some("/home/spaceghost/"),
            Some("/home/spaceghost")
        ));
        assert!(!home_is_overridden(
            Some("/home/spaceghost"),
            Some("/home/spaceghost/")
        ));
    }

    #[test]
    fn home_genuine_mismatch_is_overridden() {
        // The live Homebrew post_install repro.
        assert!(home_is_overridden(
            Some("/var/tmp/s-7l6HKUXr/vectorhawk-postinstall-abc"),
            Some("/home/spaceghost")
        ));
    }

    #[test]
    fn home_env_none_is_conservatively_not_overridden() {
        assert!(!home_is_overridden(None, Some("/home/spaceghost")));
    }

    #[test]
    fn home_passwd_none_is_conservatively_not_overridden() {
        assert!(!home_is_overridden(
            Some("/var/tmp/s-7l6HKUXr/whatever"),
            None
        ));
    }

    #[test]
    fn home_both_none_is_not_overridden() {
        assert!(!home_is_overridden(None, None));
    }

    #[test]
    fn home_both_empty_strings_are_equal_so_not_overridden() {
        assert!(!home_is_overridden(Some(""), Some("")));
    }

    // ── cgroup_is_service ──────────────────────────────────────────────────

    #[test]
    fn cgroup_v2_single_line_matches_our_unit() {
        let contents =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vectorhawk-agent.service\n";
        assert!(cgroup_is_service(contents, "vectorhawk-agent.service"));
    }

    #[test]
    fn cgroup_v1_multi_line_matches_our_unit() {
        let contents = "\
12:pids:/user.slice/user-1000.slice/user@1000.service/app.slice/vectorhawk-agent.service
11:memory:/user.slice/user-1000.slice/user@1000.service/app.slice/vectorhawk-agent.service
1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/vectorhawk-agent.service
";
        assert!(cgroup_is_service(contents, "vectorhawk-agent.service"));
    }

    #[test]
    fn cgroup_of_a_different_unit_does_not_match() {
        let contents =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/some-other.service\n";
        assert!(!cgroup_is_service(contents, "vectorhawk-agent.service"));
    }

    #[test]
    fn cgroup_process_outside_any_service_scope_does_not_match() {
        // e.g. a stray spawned directly in the login session scope, not a
        // systemd unit at all.
        let contents = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-abc.scope\n";
        assert!(!cgroup_is_service(contents, "vectorhawk-agent.service"));
    }

    #[test]
    fn cgroup_substring_inside_an_unrelated_segment_does_not_match() {
        // The unit name appears only as *infix text* inside a longer, unrelated
        // path segment — must not be treated as a match.
        let contents = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/not-vectorhawk-agent.service-backup.scope\n";
        assert!(!cgroup_is_service(contents, "vectorhawk-agent.service"));
    }

    // ── cmdline_is_daemon_run ───────────────────────────────────────────────
    //
    // Regression coverage for the live incident: substring matching against
    // the raw cmdline blob killed an innocent `bash -c …` shell whose
    // command line happened to contain "vectorhawk", "daemon", and
    // "/run/user/1000/bus" as unrelated tokens.

    #[test]
    fn does_not_match_the_shell_that_actually_got_killed() {
        // The real false positive: a bash process whose full command line
        // mentions "vectorhawk", "daemon" and "/run/user/1000/bus" as
        // separate, unrelated substrings — none of them in argv[0..3]
        // position.
        let cmdline = b"bash\0-c\0echo vectorhawk daemon status > /run/user/1000/bus\0".to_vec();
        assert!(!cmdline_is_daemon_run(&cmdline));
    }

    #[test]
    fn matches_absolute_path_daemon_run_with_foreground_flag() {
        let cmdline =
            b"/home/linuxbrew/.linuxbrew/bin/vectorhawk\0daemon\0run\0--foreground\0".to_vec();
        assert!(cmdline_is_daemon_run(&cmdline));
    }

    #[test]
    fn matches_bare_name_daemon_run_with_no_trailing_args() {
        let cmdline = b"vectorhawk\0daemon\0run\0".to_vec();
        assert!(cmdline_is_daemon_run(&cmdline));
    }

    #[test]
    fn does_not_match_daemon_install_the_installing_process_itself() {
        let cmdline = b"vectorhawk\0daemon\0install\0".to_vec();
        assert!(!cmdline_is_daemon_run(&cmdline));
    }

    #[test]
    fn does_not_match_daemon_restart() {
        let cmdline = b"vectorhawk\0daemon\0restart\0".to_vec();
        assert!(!cmdline_is_daemon_run(&cmdline));
    }

    #[test]
    fn does_not_match_similarly_named_binaries() {
        for argv0 in ["not-vectorhawk", "vectorhawkd", "vectorhawk-old"] {
            let cmdline = format!("{argv0}\0daemon\0run\0").into_bytes();
            assert!(
                !cmdline_is_daemon_run(&cmdline),
                "argv0={argv0} must not match"
            );
        }
    }

    #[test]
    fn empty_malformed_or_short_cmdlines_do_not_match_and_do_not_panic() {
        let cases: &[&[u8]] = &[
            b"",
            b"vectorhawk\0",
            b"vectorhawk\0daemon\0",
            b"\0\0\0",
            b"vectorhawk",
        ];
        for raw in cases {
            assert!(!cmdline_is_daemon_run(raw), "raw={raw:?}");
        }
    }

    // ── unit_is_healthy ────────────────────────────────────────────────────
    //
    // Regression coverage for the idempotency-guard bug: a unit stuck in
    // `auto-restart` with the socket down must never be reported healthy,
    // even though it "exists, is enabled, and has the right ExecStart."

    #[test]
    fn healthy_only_when_active_and_socket_up_and_exe_not_stale() {
        assert!(unit_is_healthy(Some(SystemdState::Active), true, false));
    }

    #[test]
    fn not_healthy_when_active_but_socket_down() {
        assert!(!unit_is_healthy(Some(SystemdState::Active), false, false));
    }

    #[test]
    fn not_healthy_when_auto_restarting_even_if_socket_momentarily_up() {
        // This is the exact bad state from the live repro: MainPID=0,
        // SubState=auto-restart. A momentarily-reachable socket (e.g. a
        // stray about to be reaped) must not paper over it.
        assert!(!unit_is_healthy(
            Some(SystemdState::AutoRestart),
            true,
            false
        ));
    }

    #[test]
    fn not_healthy_when_activating() {
        // In-progress start is not yet a settled "healthy."
        assert!(!unit_is_healthy(
            Some(SystemdState::Activating),
            true,
            false
        ));
    }

    #[test]
    fn not_healthy_when_failed_or_inactive_or_unknown_or_none() {
        for state in [
            Some(SystemdState::Failed),
            Some(SystemdState::Inactive),
            Some(SystemdState::Unknown),
            None,
        ] {
            assert!(!unit_is_healthy(state, true, false), "state={state:?}");
            assert!(!unit_is_healthy(state, false, false), "state={state:?}");
        }
    }

    #[test]
    fn not_healthy_when_active_and_socket_up_but_exe_is_stale() {
        // The `brew upgrade` bug this module fixes: Active, socket up,
        // MainPID unchanged, NRestarts=0 — every prior check says "healthy"
        // — but the running process is still executing last release's
        // binary from a deleted inode. Must fall through to repair, not
        // early-return "no changes made."
        assert!(!unit_is_healthy(Some(SystemdState::Active), true, true));
    }

    // ── exe_is_stale ────────────────────────────────────────────────────────
    //
    // Regression coverage for the `brew upgrade` stale-exe bug: verified live
    // on Linux immediately after 1.0.91 -> 1.0.92, where `MainPID` never
    // changed and `NRestarts=0`, but `readlink /proc/<pid>/exe` still showed
    // the 1.0.91 Cellar path marked "(deleted)".

    #[test]
    fn deleted_marker_is_always_stale_regardless_of_canonical_expected() {
        let exe_target =
            "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.91/bin/vectorhawk (deleted)";
        assert!(exe_is_stale(exe_target, None));
        assert!(exe_is_stale(
            exe_target,
            Some("/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.92/bin/vectorhawk")
        ));
    }

    #[test]
    fn healthy_install_canonical_expected_equals_exe_target_is_not_stale() {
        let path = "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.92/bin/vectorhawk";
        assert!(!exe_is_stale(path, Some(path)));
    }

    #[test]
    fn old_cellar_dir_still_present_differing_paths_no_deleted_marker_is_stale() {
        // Homebrew does not always clean up the old Cellar directory
        // immediately, so the link can point at a still-present old-version
        // path with no "(deleted)" marker at all.
        let exe_target = "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.91/bin/vectorhawk";
        let canonical_expected =
            "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.92/bin/vectorhawk";
        assert!(exe_is_stale(exe_target, Some(canonical_expected)));
    }

    #[test]
    fn none_expected_is_never_stale_conservative_fallback() {
        // Cannot determine the expected path (e.g. canonicalize failed) —
        // must not force a restart on a guess, even though exe_target here
        // looks like an old version.
        let exe_target = "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.91/bin/vectorhawk";
        assert!(!exe_is_stale(exe_target, None));
    }

    #[test]
    fn symlink_trap_raw_execstart_would_mismatch_but_canonical_form_matches() {
        // ExecStart in the rendered unit holds the *unversioned* Homebrew
        // symlink; /proc/<pid>/exe (a magic symlink) always resolves through
        // to the *real* Cellar target. Comparing the raw ExecStart text
        // against exe_target — the bug this test pins against — would
        // incorrectly report every healthy install as stale:
        let raw_exec_start = "/home/linuxbrew/.linuxbrew/bin/vectorhawk";
        let exe_target = "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.92/bin/vectorhawk";
        assert!(
            exe_is_stale(exe_target, Some(raw_exec_start)),
            "sanity check: the raw ExecStart symlink text does not equal the \
             resolved exe target, so comparing it raw would (wrongly) read \
             as stale"
        );

        // The fix: canonicalize the resolved binary path first — what
        // `std::fs::canonicalize(bin_path)` produces at the real call site
        // in linux.rs. For a healthy install that canonical form equals
        // exe_target exactly (canonicalizing the symlink resolves to the
        // same real Cellar path the kernel reports), so it must NOT be
        // reported stale.
        let canonical_expected = exe_target;
        assert!(!exe_is_stale(exe_target, Some(canonical_expected)));
    }

    // `rewrite_homebrew_cellar_to_symlink`'s own unit tests moved with it to
    // `vectorhawkd_core::binary_path` — see that module.
}
