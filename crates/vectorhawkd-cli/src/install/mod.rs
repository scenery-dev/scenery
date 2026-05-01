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

use anyhow::Result;

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

/// Return the absolute path of the currently-running binary, resolving any
/// symlinks in the path (e.g. `/private/var/folders/…` cargo target paths on
/// macOS are stable once canonicalized).
pub(crate) fn current_exe_path() -> Result<std::path::PathBuf> {
    let raw = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("failed to resolve current binary path: {e}"))?;
    std::fs::canonicalize(&raw)
        .map_err(|e| anyhow::anyhow!("failed to canonicalize binary path {}: {e}", raw.display()))
}

/// Probe whether the daemon Unix socket at `socket_path` is accepting
/// connections within `timeout_ms` milliseconds.
///
/// Returns `true` if the connect succeeds, `false` on any error or timeout.
/// Uses a blocking connect so this must be called from a blocking context
/// (i.e. not an async Tokio task) or from CLI main before the runtime starts.
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
