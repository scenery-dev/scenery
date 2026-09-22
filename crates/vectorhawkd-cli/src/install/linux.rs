//! Linux systemd-user unit install/uninstall for the VectorHawk daemon.
//!
//! Primary path: systemd user unit at
//! `~/.config/systemd/user/vectorhawk-agent.service`.
//!
//! Durable log: `$XDG_DATA_HOME/VectorHawk/logs/vectorhawkd.log` (falls back
//! to `~/.local/share/VectorHawk/logs/`) — INFO-level tracing output written
//! by an in-process size-bounded rotating appender (see
//! `crates/vectorhawkd-cli/src/logging.rs`), ~50 MB total cap, identical to
//! the macOS appender. The unit sets no `StandardOutput=`/`StandardError=`,
//! so crash/early-startup output (the only thing still written to stderr)
//! goes to journald, which already vacuum-bounds total size by default
//! (`journald.conf` `SystemMaxUse`, ~10%/4GB cap) — `LogRateLimitIntervalSec`/
//! `LogRateLimitBurst` below add a per-unit backstop against a crash-loop
//! flooding that shared cap.
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

/// The env line's key, as it appears (unquoted) inside the systemd
/// `Environment="KEY=value"` assignment.
const REGISTRY_URL_ENV_KEY: &str = "VECTORHAWK_REGISTRY_URL";

/// Resolve the `VECTORHAWK_REGISTRY_URL` value to carry into a
/// (re)rendered unit, so that installing/upgrading never silently re-homes
/// a daemon that was pointed at a private registry.
///
/// Resolution order:
/// 1. `VECTORHAWK_REGISTRY_URL` from the process environment at install
///    time, if set and non-empty — an explicit override always wins.
/// 2. Otherwise, whatever value is already present in the unit file at
///    `existing_unit_path` — carried forward rather than dropped.
/// 3. Otherwise `None` (today's behavior: no such line is emitted).
///
/// Step 2 is tolerant by design: a missing file, an unreadable file, or a
/// unit with no such line all mean "nothing to preserve" — never an error
/// that would fail the install.
fn resolve_registry_url_env(existing_unit_path: &std::path::Path) -> Option<String> {
    if let Ok(v) = std::env::var("VECTORHAWK_REGISTRY_URL") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    parse_registry_url_from_unit(existing_unit_path)
}

/// Tolerantly parse an `Environment="VECTORHAWK_REGISTRY_URL=<value>"` line
/// out of an existing unit file. Returns `None` on any failure to read the
/// file, or if no such line (with a non-empty value) is present.
///
/// `value` here is the raw quoted-value text as it appears in the unit
/// (possibly containing systemd's `\"`/`\\` escapes — see
/// [`systemd_unescape`]) and is unescaped before being returned, so the
/// caller gets the original value back, not escaped text that would get
/// double-escaped on the next render.
fn parse_registry_url_from_unit(path: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let prefix = format!(r#"Environment="{REGISTRY_URL_ENV_KEY}="#);
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            if let Some(value) = rest.strip_suffix('"') {
                let value = systemd_unescape(value);
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Escape a value for safe interpolation into a double-quoted systemd unit
/// assignment (`Environment="KEY=<value>"`). systemd unit files use
/// C-style backslash escaping inside quoted strings (systemd.syntax(7)): an
/// unescaped `"` inside the value would terminate the quoted string early,
/// silently corrupting the directive (and everything after it on the line)
/// rather than raising an error.
///
/// `\` is escaped FIRST: escaping `"` afterward introduces new `\` chars
/// (`\"`), and escaping `\` after that would double-escape them.
fn systemd_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Inverse of [`systemd_escape`]. Single left-to-right scan (rather than
/// sequential global replaces, which are ambiguous to get right for
/// adjacent `\`/`\"` sequences) — a bare `\` is followed by `"` or `\` to
/// decode one escaped character, or kept as-is if not (defensive; today's
/// encoder never produces a stray trailing backslash).
fn systemd_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Generate the systemd user unit content.
///
/// `RUST_LOG=info` turns on INFO-level daemon logging by default (D1 board
/// card — the daemon's own default is already `info` for `daemon run`, this
/// makes the level explicit and editable in the unit file). The durable log
/// is the daemon's own size-bounded rotating file appender (see
/// `logging.rs`), not systemd's journal — `LogRateLimitIntervalSec`/
/// `LogRateLimitBurst` bound the (crash-only) stderr stream journald
/// receives, as a per-unit backstop on top of journald's own vacuum cap.
///
/// `registry_url`, when `Some`, is emitted as a quoted
/// `Environment="VECTORHAWK_REGISTRY_URL=<value>"` line — see
/// [`resolve_registry_url_env`] for how the caller resolves it. Regenerating
/// the unit without this would silently drop a private-registry pin on every
/// install/upgrade (the Homebrew formula's `post_install` calls `daemon
/// install` on every upgrade too).
fn render_unit(bin_path: &std::path::Path, registry_url: Option<&str>) -> Result<String> {
    let bin_str = bin_path
        .to_str()
        .context("binary path is not valid UTF-8")?;

    let registry_env_line = match registry_url {
        Some(url) => {
            let url = systemd_escape(url);
            format!("Environment=\"{REGISTRY_URL_ENV_KEY}={url}\"\n")
        }
        None => String::new(),
    };

    Ok(format!(
        r#"[Unit]
Description=VectorHawk daemon — governed AI platform agent
After=network.target

[Service]
Type=simple
Environment="PATH=/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:/bin"
Environment="RUST_LOG=info"
{registry_env_line}ExecStart={bin_str} daemon run --foreground
Restart=on-failure
RestartSec=2
LogRateLimitIntervalSec=30
LogRateLimitBurst=1000

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
    // Resolve VECTORHAWK_REGISTRY_URL from the env or, failing that, from
    // whatever the unit already had — before we overwrite it below.
    let registry_url = resolve_registry_url_env(&unit);
    let content =
        render_unit(bin_path, registry_url.as_deref()).context("failed to render systemd unit")?;
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
            std::process::Command::new(bin_path)
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

/// Stop and start the daemon in place via `systemctl --user restart`.
/// Falls back to an error on systems without systemctl (autostart entries
/// can't be programmatically restarted — the user would log out and back in).
pub fn restart() -> Result<()> {
    if !systemctl_available() {
        anyhow::bail!(
            "vectorhawk daemon restart requires systemd. On systems without \
             systemctl, log out and back in (or kill the running process) to \
             restart the daemon."
        );
    }

    let output = Command::new("systemctl")
        .args(["--user", "restart", SERVICE_NAME])
        .output()
        .context("failed to spawn systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("systemctl --user restart failed: {stderr}");
    }

    println!("VectorHawk daemon restarted.");
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

#[cfg(test)]
mod tests {
    use super::{render_unit, resolve_registry_url_env};
    use std::path::Path;

    // D1 board card: INFO logging on by default via the unit file, plus a
    // per-unit rate-limit backstop on the (crash-only) stderr stream that
    // still reaches journald.
    #[test]
    fn unit_sets_rust_log_info() {
        let unit =
            render_unit(Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"), None).unwrap();
        assert!(
            unit.contains(r#"Environment="RUST_LOG=info""#),
            "expected RUST_LOG=info in the unit, got:\n{unit}"
        );
    }

    #[test]
    fn unit_sets_log_rate_limit() {
        let unit =
            render_unit(Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"), None).unwrap();
        assert!(
            unit.contains("LogRateLimitIntervalSec=") && unit.contains("LogRateLimitBurst="),
            "expected a journald rate-limit backstop in the unit, got:\n{unit}"
        );
    }

    // ── Registry URL preservation (regression coverage) ───────────────────────
    //
    // `render_unit` used to emit a fixed template carrying only PATH and
    // RUST_LOG, so regenerating the unit on every install/upgrade silently
    // dropped any VECTORHAWK_REGISTRY_URL the unit previously had.

    #[test]
    fn render_unit_emits_registry_url_when_given() {
        let unit = render_unit(
            Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"),
            Some("https://dev.vectorhawk.ai"),
        )
        .unwrap();
        assert!(
            unit.contains(r#"Environment="VECTORHAWK_REGISTRY_URL=https://dev.vectorhawk.ai""#),
            "expected registry URL env line in the unit, got:\n{unit}"
        );
    }

    #[test]
    fn render_unit_omits_registry_url_when_none() {
        let unit =
            render_unit(Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"), None).unwrap();
        assert!(
            !unit.contains("VECTORHAWK_REGISTRY_URL"),
            "expected no registry URL line, got:\n{unit}"
        );
    }

    // ── systemd Environment= escaping (fix-round-1 should-fix) ─────────────────
    //
    // `render_unit` used to interpolate the registry URL straight into the
    // double-quoted `Environment="…"` assignment. A value containing `"`
    // terminates the quoted string early, silently corrupting the directive
    // (and whatever follows it on the line) instead of erroring.
    //
    // These tests fail against the pre-fix code (no `systemd_escape` call
    // at all — the raw `"` would appear verbatim, closing the string early).

    #[test]
    fn render_unit_escapes_quote_in_registry_url_and_does_not_break_the_next_directive() {
        let unit = render_unit(
            Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"),
            Some(r#"https://registry.example.com/"injected"#),
        )
        .unwrap();
        assert!(
            unit.contains(
                r#"Environment="VECTORHAWK_REGISTRY_URL=https://registry.example.com/\"injected""#
            ),
            "expected the '\"' to be escaped as '\\\"', got:\n{unit}"
        );
        // The line immediately following the (would-be-corrupted) directive
        // must still be intact, on its own line, with ExecStart still
        // pointing at the real binary — i.e. nothing "leaked" out of the
        // quoted value into the rest of the unit.
        assert!(
            unit.contains(
                "ExecStart=/home/linuxbrew/.linuxbrew/bin/vectorhawk daemon run --foreground\n"
            ),
            "ExecStart must be untouched and on its own line, got:\n{unit}"
        );
    }

    /// A value containing `"` must round-trip: written escaped, then read
    /// back (and carried forward on the next install) as the ORIGINAL raw
    /// value — not the escaped text, which would otherwise get re-escaped a
    /// little further on every subsequent install/upgrade.
    #[test]
    fn resolve_round_trips_a_value_containing_special_characters() {
        let _env = RegistryUrlEnv::unset();
        let original = r#"https://registry.example.com/"injected\path"#;
        let rendered = render_unit(
            Path::new("/home/linuxbrew/.linuxbrew/bin/vectorhawk"),
            Some(original),
        )
        .unwrap();

        let unit_path = temp_unit_path("roundtrip");
        std::fs::write(&unit_path, &rendered).unwrap();

        let resolved = resolve_registry_url_env(&unit_path);
        assert_eq!(
            resolved.as_deref(),
            Some(original),
            "expected the raw original value back, not the escaped unit text"
        );

        let _ = std::fs::remove_file(&unit_path);
    }

    /// Serializes tests below that mutate `VECTORHAWK_REGISTRY_URL` in the
    /// process environment — `cargo test` runs tests in the same process by
    /// default, so unguarded env mutation here would race other tests in
    /// this module (mirrors the `KeychainOff` pattern used elsewhere in this
    /// workspace for the same reason).
    static REGISTRY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct RegistryUrlEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl RegistryUrlEnv {
        fn set(value: &str) -> Self {
            let guard = REGISTRY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("VECTORHAWK_REGISTRY_URL", value);
            RegistryUrlEnv { _guard: guard }
        }
        fn unset() -> Self {
            let guard = REGISTRY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("VECTORHAWK_REGISTRY_URL");
            RegistryUrlEnv { _guard: guard }
        }
    }
    impl Drop for RegistryUrlEnv {
        fn drop(&mut self) {
            std::env::remove_var("VECTORHAWK_REGISTRY_URL");
        }
    }

    fn temp_unit_path(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vh-install-linux-test-{label}-{nanos}.service"))
    }

    #[test]
    fn resolve_prefers_env_var_over_existing_unit() {
        let _env = RegistryUrlEnv::set("https://env.example.com");
        let unit_path = temp_unit_path("env-wins");
        std::fs::write(
            &unit_path,
            "Environment=\"VECTORHAWK_REGISTRY_URL=https://old.example.com\"\n",
        )
        .unwrap();

        let resolved = resolve_registry_url_env(&unit_path);
        assert_eq!(resolved.as_deref(), Some("https://env.example.com"));

        let _ = std::fs::remove_file(&unit_path);
    }

    /// The actual regression case: env var unset at install time, but the
    /// existing unit on disk has one — it must be carried forward, not
    /// dropped.
    #[test]
    fn resolve_falls_back_to_existing_unit_when_env_unset() {
        let _env = RegistryUrlEnv::unset();
        let unit_path = temp_unit_path("fallback");
        std::fs::write(
            &unit_path,
            "[Service]\nEnvironment=\"VECTORHAWK_REGISTRY_URL=https://dev.vectorhawk.ai\"\nExecStart=/x daemon run\n",
        )
        .unwrap();

        let resolved = resolve_registry_url_env(&unit_path);
        assert_eq!(resolved.as_deref(), Some("https://dev.vectorhawk.ai"));

        let _ = std::fs::remove_file(&unit_path);
    }

    #[test]
    fn resolve_returns_none_when_env_unset_and_no_existing_unit() {
        let _env = RegistryUrlEnv::unset();
        let unit_path = temp_unit_path("absent");
        assert!(!unit_path.exists(), "precondition: no existing unit file");

        let resolved = resolve_registry_url_env(&unit_path);
        assert_eq!(resolved, None);
    }

    /// Existing unit unreadable (here: a directory instead of a file) must
    /// not error — it means "nothing to preserve", same as absent.
    #[test]
    fn resolve_is_tolerant_of_unreadable_existing_unit() {
        let _env = RegistryUrlEnv::unset();
        let dir_path = temp_unit_path("unreadable-dir");
        std::fs::create_dir_all(&dir_path).unwrap();

        let resolved = resolve_registry_url_env(&dir_path);
        assert_eq!(resolved, None);

        let _ = std::fs::remove_dir_all(&dir_path);
    }
}
