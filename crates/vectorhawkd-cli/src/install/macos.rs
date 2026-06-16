//! macOS LaunchAgent install/uninstall for the VectorHawk daemon.
//!
//! Plist path: `~/Library/LaunchAgents/com.vectorhawk.agent.plist`
//! Log dir:    `~/Library/Logs/VectorHawk/`
//!
//! Install sequence:
//! 1. Write plist with the current binary path.
//! 2. Create `~/Library/Logs/VectorHawk/` (LaunchAgent will fail to start if
//!    the log dir does not exist).
//! 3. `launchctl bootstrap gui/<uid> <plist>` — loads and starts the agent.
//! 4. `launchctl enable gui/<uid>/com.vectorhawk.agent` — persists across
//!    reboots even if the agent crashes during first run.
//!
//! Uninstall sequence:
//! 1. `launchctl bootout gui/<uid>/com.vectorhawk.agent` — stops + unloads.
//! 2. Remove the plist file.

use anyhow::{Context, Result};
use std::{fs, process::Command};

use super::{daemon_socket_path, socket_is_reachable, InstallStatus};

const LABEL: &str = "com.vectorhawk.agent";
const PLIST_FILENAME: &str = "com.vectorhawk.agent.plist";

/// Return `~/Library/LaunchAgents/com.vectorhawk.agent.plist`.
fn plist_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("failed to resolve HOME directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILENAME))
}

/// Return `~/Library/Logs/VectorHawk/`.
fn log_dir() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("failed to resolve HOME directory")?;
    Ok(home.join("Library").join("Logs").join("VectorHawk"))
}

/// Return the numeric UNIX user-ID of the current process.
fn current_uid() -> u32 {
    // SAFETY: getuid() is always safe to call.
    unsafe { libc::getuid() }
}

/// Build the service target string: `gui/<uid>/com.vectorhawk.agent`.
fn service_target(uid: u32) -> String {
    format!("gui/{uid}/{LABEL}")
}

/// Build the domain target string: `gui/<uid>`.
fn domain_target(uid: u32) -> String {
    format!("gui/{uid}")
}

/// Generate the LaunchAgent plist XML from the given binary path and log dir.
fn render_plist(bin_path: &std::path::Path, log_dir: &std::path::Path) -> Result<String> {
    let bin_str = bin_path
        .to_str()
        .context("binary path is not valid UTF-8")?;
    let stdout_log = log_dir
        .join("stdout.log")
        .to_str()
        .context("log dir path is not valid UTF-8")?
        .to_string();
    let stderr_log = log_dir
        .join("stderr.log")
        .to_str()
        .context("log dir path is not valid UTF-8")?
        .to_string();

    // The daemon subcommand that keeps the daemon running in the foreground.
    // `vectorhawk daemon run --foreground` delegates to `run_daemon()`.
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin_str}</string>
        <string>daemon</string>
        <string>run</string>
        <string>--foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    ))
}

/// Run a `launchctl` command, capturing stderr, and return a descriptive error
/// if the exit code is non-zero.
fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to spawn launchctl")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);
    anyhow::bail!(
        "launchctl {} failed (exit {code}): {stderr}",
        args.join(" ")
    )
}

/// Check whether the service is currently loaded via `launchctl print`.
/// Returns `true` if exit code 0 (service domain exists), `false` otherwise.
fn service_is_loaded(uid: u32) -> bool {
    Command::new("launchctl")
        .args(["print", &service_target(uid)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Public install / uninstall ────────────────────────────────────────────────

/// Install the LaunchAgent and start the daemon.
pub fn install() -> Result<()> {
    let plist = plist_path().context("failed to resolve plist path")?;
    let log_dir = log_dir().context("failed to resolve log dir path")?;
    let bin_path =
        super::resolve_daemon_bin_path().context("failed to resolve daemon binary path")?;
    let uid = current_uid();

    // ── Idempotency guard — but allow upgrade rewrites ────────────────────────
    // Skip only when the plist exists, the service is loaded, AND the plist
    // ExecStart path matches the current binary. After a brew upgrade the binary
    // path changes (new Cellar directory), so we must rewrite the plist and
    // restart. Otherwise `daemon install` during post_install is a no-op and
    // the old binary keeps running.
    if plist.exists() && service_is_loaded(uid) {
        let plist_has_current_binary = fs::read_to_string(&plist)
            .ok()
            .map(|s| bin_path.to_str().map(|b| s.contains(b)).unwrap_or(false))
            .unwrap_or(false);
        if plist_has_current_binary {
            println!("VectorHawk daemon is already installed and up to date — no changes made.");
            return Ok(());
        }
        // Binary path changed (upgrade): fall through to rewrite + restart.
        println!("VectorHawk daemon binary path changed — updating plist and restarting.");
    }

    // ── 1. Ensure log directory exists ────────────────────────────────────────
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;

    // ── 2. Ensure LaunchAgents directory exists ───────────────────────────────
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create LaunchAgents dir: {}", parent.display()))?;
    }

    // ── 3. Write plist ────────────────────────────────────────────────────────
    let plist_content =
        render_plist(&bin_path, &log_dir).context("failed to render LaunchAgent plist")?;
    fs::write(&plist, &plist_content)
        .with_context(|| format!("failed to write plist: {}", plist.display()))?;

    println!("Wrote LaunchAgent plist: {}", plist.display());

    // ── 4. If currently loaded (stale state), boot it out first ───────────────
    if service_is_loaded(uid) {
        let _ = launchctl(&["bootout", &service_target(uid)]);
        // Give launchd a moment to settle after bootout; on Sequoia a
        // bootstrap immediately after bootout can fail with exit 5 (EBUSY).
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // ── 5. Enable before bootstrap ────────────────────────────────────────────
    // `enable` must come before `bootstrap` on Sequoia; if the service was ever
    // disabled via `launchctl disable`, bootstrap fails with exit 5 until
    // the service is re-enabled.
    let _ = launchctl(&["enable", &service_target(uid)]);

    // ── 6. Bootstrap the service (loads into the domain) ─────────────────────
    let plist_str = plist.to_str().context("plist path is not valid UTF-8")?;
    launchctl(&["bootstrap", &domain_target(uid), plist_str])
        .context("failed to bootstrap LaunchAgent")?;

    // ── 7. Enable for persistence (idempotent, already done above) ───────────
    // Call again after bootstrap to ensure the label is persisted in the
    // enabled-services database even on first install.
    let _ = launchctl(&["enable", &service_target(uid)]);

    // ── 8. Kickstart for immediate launch ─────────────────────────────────────
    // On macOS 15+ (Sequoia) `bootstrap` may defer the initial start as
    // "speculative" even with RunAtLoad=true. `kickstart -k` forces an
    // immediate start. We use `-k` (kill existing) so that if a stale process
    // somehow survived the earlier bootout it is replaced.
    //
    // Non-fatal: if kickstart fails (e.g. process already started by the time
    // we get here) we log a warning but do not fail the overall install.
    if let Err(e) = launchctl(&["kickstart", "-k", &service_target(uid)]) {
        eprintln!("warning: kickstart returned an error (daemon may still start): {e:#}");
    }

    println!("LaunchAgent loaded and started (label: {LABEL}).");
    println!("The daemon will start automatically at login.");
    Ok(())
}

/// Stop the daemon and remove the LaunchAgent plist.
pub fn uninstall() -> Result<()> {
    let plist = plist_path().context("failed to resolve plist path")?;
    let uid = current_uid();

    let loaded = service_is_loaded(uid);
    let plist_exists = plist.exists();

    if !loaded && !plist_exists {
        println!("VectorHawk daemon is not installed — nothing to remove.");
        return Ok(());
    }

    // ── 1. Stop + unload the service ──────────────────────────────────────────
    if loaded {
        launchctl(&["bootout", &service_target(uid)])
            .context("failed to bootout LaunchAgent (daemon may still be running)")?;
        println!("LaunchAgent stopped and unloaded.");
    }

    // ── 2. Remove the plist file ──────────────────────────────────────────────
    if plist_exists {
        fs::remove_file(&plist)
            .with_context(|| format!("failed to remove plist: {}", plist.display()))?;
        println!("Removed plist: {}", plist.display());
    }

    println!("VectorHawk daemon uninstalled.");
    Ok(())
}

/// Stop and start the LaunchAgent in place.
///
/// Uses `launchctl kickstart -k gui/<uid>/com.vectorhawk.agent`, which kills
/// the running process (if any) and starts a fresh one — equivalent to a
/// restart without re-writing the plist. Returns an error if the service is
/// not installed.
pub fn restart() -> Result<()> {
    let uid = current_uid();

    if !service_is_loaded(uid) {
        anyhow::bail!(
            "VectorHawk daemon is not installed — run `vectorhawk daemon install` first."
        );
    }

    launchctl(&["kickstart", "-k", &service_target(uid)])
        .context("failed to restart LaunchAgent")?;

    println!("VectorHawk daemon restarted.");
    Ok(())
}

/// Return the current install/running status of the LaunchAgent.
pub fn status() -> Result<InstallStatus> {
    let plist = plist_path().context("failed to resolve plist path")?;

    if !plist.exists() {
        return Ok(InstallStatus::NotInstalled);
    }

    let unit_path = plist.to_str().unwrap_or("(non-UTF-8 path)").to_string();

    let socket_path = daemon_socket_path();
    if socket_is_reachable(&socket_path, 500) {
        Ok(InstallStatus::InstalledAndRunning { unit_path })
    } else {
        Ok(InstallStatus::InstalledNotRunning { unit_path })
    }
}
