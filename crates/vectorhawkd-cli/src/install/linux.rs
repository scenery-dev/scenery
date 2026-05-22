//! Linux systemd-user unit install/uninstall for the VectorHawk daemon.
//!
//! Primary path: systemd user unit at
//! `~/.config/systemd/user/vectorhawk-agent.service`.
//!
//! Fallback (no systemctl): XDG autostart desktop entry at
//! `~/.config/autostart/vectorhawk.desktop` with a printed notice.
//!
//! Install sequence (systemd path):
//! 1. Write the .service unit.
//! 2. `systemctl --user daemon-reload`
//! 3. `systemctl --user enable --now vectorhawk-agent.service`
//!
//! Uninstall sequence (systemd path):
//! 1. `systemctl --user disable --now vectorhawk-agent.service`
//! 2. Remove the unit file.
//! 3. `systemctl --user daemon-reload`
//!
//! Note on lingering: on headless/server boxes the user session may not start
//! without a graphical login. To run without an active session, the user can
//! run `sudo loginctl enable-linger $USER` manually. This is intentionally NOT
//! done automatically because it requires elevated privileges.

use anyhow::{Context, Result};
use std::{fs, process::Command};

use super::{daemon_socket_path, resolve_daemon_bin_path, socket_is_reachable, InstallStatus};

const SERVICE_NAME: &str = "vectorhawk-agent.service";
const DESKTOP_FILENAME: &str = "vectorhawk.desktop";

/// Return `~/.config/systemd/user/vectorhawk-agent.service`.
fn unit_path() -> Result<std::path::PathBuf> {
    let config = dirs::config_dir().context("failed to resolve XDG config directory")?;
    Ok(config.join("systemd").join("user").join(SERVICE_NAME))
}

/// Return `~/.config/autostart/vectorhawk.desktop`.
fn desktop_path() -> Result<std::path::PathBuf> {
    let config = dirs::config_dir().context("failed to resolve XDG config directory")?;
    Ok(config.join("autostart").join(DESKTOP_FILENAME))
}

/// Generate the systemd user unit content.
fn render_unit(bin_path: &std::path::Path) -> Result<String> {
    let bin_str = bin_path
        .to_str()
        .context("binary path is not valid UTF-8")?;

    Ok(format!(
        r#"[Unit]
Description=VectorHawk daemon — governed AI platform agent
After=network.target

[Service]
Type=simple
Environment="PATH=/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:/bin"
ExecStart={bin_str} daemon run --foreground
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
"#
    ))
}

/// Generate the XDG autostart .desktop entry (fallback when systemd is absent).
fn render_desktop(bin_path: &std::path::Path) -> Result<String> {
    let bin_str = bin_path
        .to_str()
        .context("binary path is not valid UTF-8")?;

    Ok(format!(
        r#"[Desktop Entry]
Type=Application
Name=VectorHawk Agent
Comment=VectorHawk daemon — governed AI platform agent
Exec={bin_str} daemon run --foreground
Hidden=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
"#
    ))
}

/// Return the XDG_RUNTIME_DIR to use for this user, preferring the env var
/// but falling back to the canonical `/run/user/<uid>` path.
fn xdg_runtime_dir() -> String {
    if let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
        if !v.is_empty() {
            return v;
        }
    }
    let uid = unsafe { libc::getuid() };
    format!("/run/user/{uid}")
}

/// Run a `systemctl --user` command with an explicit XDG_RUNTIME_DIR so it
/// works from Homebrew post_install and other contexts where the env var may
/// be absent.
fn systemctl_user(args: &[&str]) -> Result<()> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);

    let xdg = xdg_runtime_dir();
    let bus = format!("unix:path={xdg}/bus");

    let output = Command::new("systemctl")
        .args(&full_args)
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus)
        .output()
        .context("failed to spawn systemctl")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);
    anyhow::bail!(
        "systemctl --user {} failed (exit {code}): {stderr}",
        args.join(" ")
    )
}

/// Returns `true` if `systemctl --user` is usable on this system.
fn systemctl_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns `true` if the systemd unit is currently enabled.
fn unit_is_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", SERVICE_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Public install / uninstall ────────────────────────────────────────────────

/// Install and start the daemon via systemd user unit (or XDG autostart fallback).
pub fn install() -> Result<()> {
    let bin_path = resolve_daemon_bin_path().context("failed to resolve daemon binary path")?;

    if systemctl_available() {
        install_systemd(&bin_path)
    } else {
        install_desktop_fallback(&bin_path)
    }
}

/// Systemd user unit install path.
fn install_systemd(bin_path: &std::path::Path) -> Result<()> {
    let unit = unit_path().context("failed to resolve unit file path")?;

    // ── Idempotency guard — but allow upgrade rewrites ────────────────────────
    // Skip only when the unit exists, is enabled, AND the ExecStart path in the
    // unit file matches the current binary. After a brew upgrade the binary path
    // changes (new Cellar directory), so we must rewrite the unit and restart.
    let is_upgrade = if unit.exists() && unit_is_enabled() {
        let unit_has_current_binary = fs::read_to_string(&unit)
            .ok()
            .map(|s| bin_path.to_str().map(|b| s.contains(b)).unwrap_or(false))
            .unwrap_or(false);
        if unit_has_current_binary {
            println!("VectorHawk daemon is already installed and up to date — no changes made.");
            return Ok(());
        }
        // Binary path changed (upgrade): fall through to rewrite + restart.
        println!("VectorHawk daemon binary path changed — updating unit and restarting.");
        true
    } else {
        false
    };

    // ── 1. Ensure unit dir exists ─────────────────────────────────────────────
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create systemd user dir: {}", parent.display()))?;
    }

    // ── 2. Write unit file ────────────────────────────────────────────────────
    let content = render_unit(bin_path).context("failed to render systemd unit")?;
    fs::write(&unit, &content)
        .with_context(|| format!("failed to write unit file: {}", unit.display()))?;

    println!("Wrote systemd user unit: {}", unit.display());

    // ── 3. daemon-reload ──────────────────────────────────────────────────────
    systemctl_user(&["daemon-reload"]).context("systemctl daemon-reload failed")?;

    // ── 4. Start or restart the daemon ────────────────────────────────────────
    let xdg = xdg_runtime_dir();
    let xdg_sock = format!("{xdg}/vectorhawk/agent.sock");

    let started_via_systemd = if is_upgrade {
        // `restart` atomically stops the old process and starts the new one.
        // Works when a D-Bus user session is present (interactive login).
        // In Homebrew post_install the D-Bus session is absent so restart will
        // fail — we handle that below by killing the old PID directly.
        systemctl_user(&["restart", SERVICE_NAME]).is_ok()
    } else {
        // Fresh install: enable the unit for auto-start and start it now.
        // Same D-Bus caveat applies; fall back to direct spawn below.
        systemctl_user(&["enable", "--now", SERVICE_NAME]).is_ok()
    };

    // ── 5. Verify socket reachable; spawn directly if systemd didn't work ─────
    // Use the canonical XDG path so the socket check agrees with where the
    // daemon will bind regardless of whether XDG_RUNTIME_DIR is in the env.

    // On an upgrade where systemctl restart failed (no D-Bus), the old process
    // is still running from a deleted inode. Kill it so the socket goes away,
    // then let the direct-spawn path start the new binary.
    if is_upgrade && !started_via_systemd {
        kill_daemon_process();
        // Brief pause for the socket to close after SIGTERM.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    if !socket_is_reachable(&xdg_sock, 1500) {
        // systemctl didn't start the daemon (no D-Bus session or systemd user
        // session not yet created). Spawn the daemon directly, detached from
        // the current session (setsid) so it survives post_install exit.
        use std::os::unix::process::CommandExt;
        let xdg_clone = xdg.clone();
        let _ = unsafe {
            std::process::Command::new(&bin_path)
                .args(["daemon", "run", "--foreground"])
                .env("XDG_RUNTIME_DIR", &xdg_clone)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .pre_exec(|| {
                    // Create a new session so SIGHUP on parent exit doesn't
                    // reach the daemon.
                    libc::setsid();
                    Ok(())
                })
                .spawn()
        };

        // Give it up to 2 s to bind the socket.
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if socket_is_reachable(&xdg_sock, 500) {
                break;
            }
        }

        if socket_is_reachable(&xdg_sock, 500) {
            println!("VectorHawk daemon started (direct spawn fallback).");
            if !is_upgrade {
                println!(
                    "Note: the daemon is managed by systemd on next login. For \
                     permanent auto-start without a graphical session, run:\n  \
                     sudo loginctl enable-linger $USER"
                );
            }
        } else {
            println!(
                "VectorHawk daemon unit installed. Start it now with:\n  \
                 XDG_RUNTIME_DIR={xdg} systemctl --user start {SERVICE_NAME}\n  \
                 or: {bin_str} daemon run --foreground &",
                bin_str = bin_path.display(),
            );
        }
    } else if started_via_systemd {
        println!("Systemd user unit enabled and started ({SERVICE_NAME}).");
    } else {
        println!("VectorHawk daemon is running.");
    }
    Ok(())
}

/// Send SIGTERM to any running `vectorhawk daemon run --foreground` process.
///
/// Used during upgrades when `systemctl --user restart` fails (no D-Bus
/// session in Homebrew post_install). On Linux a process keeps running after
/// its binary is deleted, so we must explicitly kill it before spawning the
/// replacement. Non-fatal: errors are silently ignored.
fn kill_daemon_process() {
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return;
    };
    for entry in proc_entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmdline_path = entry.path().join("cmdline");
        let Ok(raw) = fs::read(&cmdline_path) else {
            continue;
        };
        // cmdline is NUL-separated; check it contains our marker tokens
        let cmdline = String::from_utf8_lossy(&raw);
        if cmdline.contains("vectorhawk")
            && cmdline.contains("daemon")
            && cmdline.contains("foreground")
        {
            if let Ok(pid) = pid_str.parse::<i32>() {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
    }
}

/// XDG autostart fallback when systemd is not available.
fn install_desktop_fallback(bin_path: &std::path::Path) -> Result<()> {
    let desktop = desktop_path().context("failed to resolve desktop entry path")?;

    if desktop.exists() {
        println!(
            "VectorHawk autostart entry already exists at {} — no changes made.",
            desktop.display()
        );
        return Ok(());
    }

    if let Some(parent) = desktop.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create autostart dir: {}", parent.display()))?;
    }

    let content = render_desktop(bin_path).context("failed to render .desktop entry")?;
    fs::write(&desktop, &content)
        .with_context(|| format!("failed to write desktop entry: {}", desktop.display()))?;

    println!("Notice: systemctl was not found — falling back to XDG autostart.");
    println!("Wrote autostart entry: {}", desktop.display());
    println!(
        "The VectorHawk daemon will start at your next graphical login. \
         To start it now, run: {} daemon run --foreground &",
        bin_path.display()
    );
    Ok(())
}

/// Stop and uninstall the daemon (systemd or autostart fallback).
pub fn uninstall() -> Result<()> {
    if systemctl_available() {
        uninstall_systemd()
    } else {
        uninstall_desktop_fallback()
    }
}

fn uninstall_systemd() -> Result<()> {
    let unit = unit_path().context("failed to resolve unit file path")?;
    let enabled = unit_is_enabled();
    let unit_exists = unit.exists();

    if !enabled && !unit_exists {
        println!("VectorHawk daemon is not installed — nothing to remove.");
        return Ok(());
    }

    // ── 1. Disable + stop ─────────────────────────────────────────────────────
    if enabled {
        systemctl_user(&["disable", "--now", SERVICE_NAME])
            .context("failed to disable and stop systemd unit")?;
        println!("Systemd unit disabled and stopped.");
    }

    // ── 2. Remove unit file ───────────────────────────────────────────────────
    if unit_exists {
        fs::remove_file(&unit)
            .with_context(|| format!("failed to remove unit file: {}", unit.display()))?;
        println!("Removed unit file: {}", unit.display());
    }

    // ── 3. daemon-reload ──────────────────────────────────────────────────────
    let _ = systemctl_user(&["daemon-reload"]);

    println!("VectorHawk daemon uninstalled.");
    Ok(())
}

fn uninstall_desktop_fallback() -> Result<()> {
    let desktop = desktop_path().context("failed to resolve desktop entry path")?;

    if !desktop.exists() {
        println!("VectorHawk daemon is not installed — nothing to remove.");
        return Ok(());
    }

    fs::remove_file(&desktop)
        .with_context(|| format!("failed to remove desktop entry: {}", desktop.display()))?;

    println!("Removed autostart entry: {}", desktop.display());
    println!("VectorHawk daemon uninstalled.");
    Ok(())
}

/// Return the current install/running status.
pub fn status() -> Result<InstallStatus> {
    // Check systemd unit first; fall back to desktop entry.
    let unit = unit_path().context("failed to resolve unit file path")?;
    let desktop = desktop_path().context("failed to resolve desktop entry path")?;

    let unit_path_str = if unit.exists() {
        unit.to_str().unwrap_or("(non-UTF-8 path)").to_string()
    } else if desktop.exists() {
        desktop.to_str().unwrap_or("(non-UTF-8 path)").to_string()
    } else {
        return Ok(InstallStatus::NotInstalled);
    };

    let socket_path = daemon_socket_path();
    if socket_is_reachable(&socket_path, 500) {
        Ok(InstallStatus::InstalledAndRunning {
            unit_path: unit_path_str,
        })
    } else {
        Ok(InstallStatus::InstalledNotRunning {
            unit_path: unit_path_str,
        })
    }
}
