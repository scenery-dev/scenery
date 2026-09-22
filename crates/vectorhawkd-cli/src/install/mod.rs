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
    Ok(rewrite_homebrew_cellar_to_symlink(&exe))
}

/// Rewrite a Homebrew Cellar path (`<prefix>/Cellar/<formula>/<version>/bin/<name>`)
/// to the unversioned symlink (`<prefix>/bin/<name>`). Any non-Cellar path is
/// returned unchanged. Extracted so unit tests can hit it without spawning a
/// real exe.
pub(crate) fn rewrite_homebrew_cellar_to_symlink(exe: &std::path::Path) -> std::path::PathBuf {
    let Some(bin_name) = exe.file_name() else {
        return exe.to_path_buf();
    };

    let components: Vec<_> = exe.components().collect();
    for (idx, component) in components.iter().enumerate() {
        // OsStr literal comparison via `==` works because OsStr implements
        // PartialEq<str>.
        if component.as_os_str() == std::ffi::OsStr::new("Cellar") && idx >= 1 {
            let prefix: std::path::PathBuf = components[..idx].iter().collect();
            return prefix.join("bin").join(bin_name);
        }
    }

    exe.to_path_buf()
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
/// the unit, not freshly starting it. Direct-spawning a competitor while the
/// unit is in `auto-restart` is exactly what turns a transient failure into a
/// permanent one (see `should_direct_spawn`), so callers need to be able to
/// tell the two apart.
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

/// Pure decision: should the installer direct-spawn a daemon process outside
/// systemd?
///
/// `socket_up` — is the daemon socket already accepting connections (from
/// [`wait_for_socket`])? `systemd_state` — the unit's state, or `None` when
/// there is no systemd user session to ask (the genuine Homebrew
/// `post_install`, no-D-Bus case) or unit-tracking simply isn't in play.
///
/// The rule is deliberately conservative: direct-spawn is for the case where
/// systemd is not managing the daemon at all, never a way to "help" systemd
/// along.
///
/// - Socket already up → never spawn; there is nothing to fix.
/// - Systemd reports `Active`, `Activating`, or `AutoRestart` → never spawn.
///   `AutoRestart` matters most: spawning a competitor while the unit is
///   crash-looping is exactly how a transient failure becomes permanent —
///   the stray wins `acquire_socket`'s race, the managed unit's restart
///   attempts keep losing it, and `Restart=on-failure` leaves the unit stuck
///   in `auto-restart` forever with no managed daemon ever coming up.
/// - Otherwise (`Failed`, `Inactive`, `Unknown`, or no systemd session at
///   all) → direct spawn is the correct fallback.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn should_direct_spawn(socket_up: bool, systemd_state: Option<SystemdState>) -> bool {
    if socket_up {
        return false;
    }
    match systemd_state {
        Some(SystemdState::Active)
        | Some(SystemdState::Activating)
        | Some(SystemdState::AutoRestart) => false,
        Some(SystemdState::Failed)
        | Some(SystemdState::Inactive)
        | Some(SystemdState::Unknown)
        | None => true,
    }
}

/// Pure decision: is the systemd-managed daemon actually healthy, i.e. is it
/// safe to treat `daemon install` as a no-op ("already installed and up to
/// date")?
///
/// This is deliberately strict — `Active` and a reachable socket, nothing
/// looser. An earlier version of the installer's idempotency guard only
/// checked "unit exists, is enabled, ExecStart matches the current binary"
/// and returned early on that alone, without ever asking whether the unit
/// was actually running. That let a box stuck in `SubState=auto-restart`
/// (e.g. because a stray direct-spawned daemon — see `should_direct_spawn`'s
/// docs — is holding the socket) report "no changes made" on every
/// subsequent `daemon install`, forever, because the early return fired
/// *before* the stray-reaping and restart logic ever ran. `Activating` is
/// deliberately excluded too: an in-progress start is not yet a settled
/// "healthy," and treating it as good enough would risk the same silent
/// no-op if it never actually finishes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn unit_is_healthy(state: Option<SystemdState>, socket_up: bool) -> bool {
    state == Some(SystemdState::Active) && socket_up
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
        cgroup_is_service, rewrite_homebrew_cellar_to_symlink, should_direct_spawn,
        unit_is_healthy, SystemdState,
    };
    use std::path::Path;

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

    // ── should_direct_spawn ────────────────────────────────────────────────

    #[test]
    fn never_spawns_when_socket_already_up() {
        for state in [
            None,
            Some(SystemdState::Active),
            Some(SystemdState::Activating),
            Some(SystemdState::AutoRestart),
            Some(SystemdState::Failed),
            Some(SystemdState::Inactive),
            Some(SystemdState::Unknown),
        ] {
            assert!(
                !should_direct_spawn(true, state),
                "must never spawn when socket_up=true, state={state:?}"
            );
        }
    }

    #[test]
    fn never_spawns_while_systemd_is_active() {
        assert!(!should_direct_spawn(false, Some(SystemdState::Active)));
    }

    #[test]
    fn never_spawns_while_systemd_is_activating() {
        assert!(!should_direct_spawn(false, Some(SystemdState::Activating)));
    }

    #[test]
    fn never_spawns_while_systemd_is_auto_restarting() {
        // This is the case that made the fail loop permanent: spawning a
        // stray while the unit is crash-looping steals the socket out from
        // under every future restart attempt.
        assert!(!should_direct_spawn(false, Some(SystemdState::AutoRestart)));
    }

    #[test]
    fn spawns_when_systemd_failed() {
        assert!(should_direct_spawn(false, Some(SystemdState::Failed)));
    }

    #[test]
    fn spawns_when_systemd_inactive() {
        assert!(should_direct_spawn(false, Some(SystemdState::Inactive)));
    }

    #[test]
    fn spawns_when_systemd_state_unknown() {
        assert!(should_direct_spawn(false, Some(SystemdState::Unknown)));
    }

    #[test]
    fn spawns_when_no_systemd_session_at_all() {
        // Genuine Homebrew post_install / no-D-Bus case.
        assert!(should_direct_spawn(false, None));
    }

    // ── unit_is_healthy ────────────────────────────────────────────────────
    //
    // Regression coverage for the idempotency-guard bug: a unit stuck in
    // `auto-restart` with the socket down must never be reported healthy,
    // even though it "exists, is enabled, and has the right ExecStart."

    #[test]
    fn healthy_only_when_active_and_socket_up() {
        assert!(unit_is_healthy(Some(SystemdState::Active), true));
    }

    #[test]
    fn not_healthy_when_active_but_socket_down() {
        assert!(!unit_is_healthy(Some(SystemdState::Active), false));
    }

    #[test]
    fn not_healthy_when_auto_restarting_even_if_socket_momentarily_up() {
        // This is the exact bad state from the live repro: MainPID=0,
        // SubState=auto-restart. A momentarily-reachable socket (e.g. a
        // stray about to be reaped) must not paper over it.
        assert!(!unit_is_healthy(Some(SystemdState::AutoRestart), true));
    }

    #[test]
    fn not_healthy_when_activating() {
        // In-progress start is not yet a settled "healthy."
        assert!(!unit_is_healthy(Some(SystemdState::Activating), true));
    }

    #[test]
    fn not_healthy_when_failed_or_inactive_or_unknown_or_none() {
        for state in [
            Some(SystemdState::Failed),
            Some(SystemdState::Inactive),
            Some(SystemdState::Unknown),
            None,
        ] {
            assert!(!unit_is_healthy(state, true), "state={state:?}");
            assert!(!unit_is_healthy(state, false), "state={state:?}");
        }
    }

    #[test]
    fn rewrites_arm_homebrew_cellar_to_symlink() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/opt/homebrew/Cellar/vectorhawk/1.0.45/bin/vectorhawk",
        ));
        assert_eq!(got, Path::new("/opt/homebrew/bin/vectorhawk"));
    }

    #[test]
    fn rewrites_linuxbrew_cellar_to_symlink() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.45/bin/vectorhawk",
        ));
        assert_eq!(got, Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"));
    }

    #[test]
    fn leaves_non_cellar_paths_alone() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/Users/dev/code/vectorhawk/target/release/vectorhawk",
        ));
        assert_eq!(
            got,
            Path::new("/Users/dev/code/vectorhawk/target/release/vectorhawk")
        );
    }
}
