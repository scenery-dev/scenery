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

use super::{
    cgroup_is_service, cmdline_is_daemon_run, daemon_socket_path, exe_is_stale,
    resolve_daemon_bin_path, socket_is_reachable, unit_is_healthy, wait_for_socket, InstallStatus,
    SystemdState,
};

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
///
/// `Restart=always` (not `on-failure`): a daemon should always be running,
/// and the binary-replacement watch (see `vectorhawkd_daemon::binary_watch`)
/// deliberately exits 0 when it detects its own binary was replaced by an
/// upgrade, trusting systemd to bring it straight back up on the new build.
/// `on-failure` only restarts on a non-zero exit, so it would never restart
/// after that clean exit — silently turning "the daemon notices the
/// upgrade" back into "the daemon stops and stays stopped," the exact
/// failure mode this whole mechanism exists to fix.
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
Restart=always
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

/// Read the unit's real `ActiveState`/`SubState` via `systemctl --user show`
/// and classify it into [`SystemdState`].
///
/// Returns `None` whenever the state genuinely can't be determined — no
/// systemd user session at all, `systemctl` missing, or unparseable output.
///
/// Homebrew's `post_install` does NOT lack a D-Bus session bus — a real log
/// from a live `brew upgrade` shows `systemctl --user daemon-reload` and
/// `systemctl --user enable --now` both succeeding there (they print
/// "Systemd user unit enabled and started," which cannot happen without a
/// session bus), verified on both macOS and Linux with 1.0.96. What
/// `post_install` doesn't reliably inherit is the login session's
/// `XDG_RUNTIME_DIR`/`DBUS_SESSION_BUS_ADDRESS` *environment variables* —
/// which is the actual reason `systemctl_user` (and this function) set them
/// explicitly rather than relying on the process environment.
fn unit_state() -> Option<SystemdState> {
    let xdg = xdg_runtime_dir();
    let bus = format!("unix:path={xdg}/bus");

    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            SERVICE_NAME,
            "-p",
            "ActiveState",
            "-p",
            "SubState",
        ])
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_active_sub_state(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `systemctl --user show -p ActiveState -p SubState` output (two
/// `Key=value` lines, order not guaranteed) into a [`SystemdState`].
///
/// `SubState=auto-restart` is checked first and wins over `ActiveState`:
/// systemd reports `ActiveState=activating` while a unit is between
/// restarts, which reads as "starting up" but actually means `Restart=`
/// (`always`, as of this unit — previously `on-failure`) is cycling it — a
/// materially different situation for the diagnostic message
/// `install_systemd` prints when the unit never comes up.
fn parse_active_sub_state(output: &str) -> Option<SystemdState> {
    let mut active_state = None;
    let mut sub_state = None;
    for line in output.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active_state = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("SubState=") {
            sub_state = Some(v.trim());
        }
    }

    if sub_state == Some("auto-restart") {
        return Some(SystemdState::AutoRestart);
    }

    match active_state {
        Some("active") => Some(SystemdState::Active),
        Some("activating") => Some(SystemdState::Activating),
        Some("failed") => Some(SystemdState::Failed),
        Some("inactive") | Some("dead") => Some(SystemdState::Inactive),
        Some(_) => Some(SystemdState::Unknown),
        None => None,
    }
}

/// Read the unit's `MainPID` via `systemctl --user show <unit> -p MainPID
/// --value`, using the same explicit `XDG_RUNTIME_DIR`/
/// `DBUS_SESSION_BUS_ADDRESS` plumbing as [`unit_state`] and for the same
/// reason — the bus is present in the Homebrew `post_install` context, but
/// the env vars pointing at it aren't reliably inherited there; see
/// [`unit_state`]'s doc comment for the live evidence.
///
/// Returns `None` whenever a running process can't be pinned down:
/// `systemctl` failed or is unreachable, the value didn't parse, or
/// `MainPID=0` — which systemd reports when the unit has no running process
/// (not active, or between crash-loop restarts). Callers feed this into the
/// stale-exe check, where `None` here means "nothing running to check" and
/// falls through to the existing state/socket checks rather than being
/// treated as stale.
fn unit_main_pid() -> Option<u32> {
    let xdg = xdg_runtime_dir();
    let bus = format!("unix:path={xdg}/bus");

    let output = Command::new("systemctl")
        .args(["--user", "show", SERVICE_NAME, "-p", "MainPID", "--value"])
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if pid == 0 {
        return None;
    }
    Some(pid)
}

/// Is the unit's currently-running process (per `MainPID`) executing a
/// stale binary? This is the `brew upgrade` bug this module fixes: on Linux
/// a running process keeps executing its binary's inode after the file
/// backing it is replaced or removed by `brew upgrade`, so the unit can be
/// genuinely `Active` with a live socket while still serving last release's
/// code — see [`exe_is_stale`] in `mod.rs` for the live incident and the
/// decision logic.
///
/// Every failure path here — no `MainPID`, an unreadable
/// `/proc/<pid>/exe`, a `bin_path` that doesn't canonicalize — returns
/// `false`. "Cannot determine" must never force a restart; see
/// `exe_is_stale`'s conservative-fallback policy, which this mirrors on the
/// I/O side.
fn running_exe_is_stale(bin_path: &std::path::Path) -> bool {
    let Some(pid) = unit_main_pid() else {
        return false;
    };

    let Ok(exe_link) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let Some(exe_target) = exe_link.to_str() else {
        return false;
    };

    // `bin_path` is `resolve_daemon_bin_path()`'s result — the *unversioned*
    // Homebrew symlink when Homebrew-installed (same value written into
    // `ExecStart`) — while `/proc/<pid>/exe` is a magic symlink the kernel
    // always resolves through to the *real* Cellar target. Canonicalizing
    // here is what makes the comparison apples-to-apples: comparing the raw
    // symlink text against `exe_target` would mismatch on every healthy
    // install and turn this into a restart loop on every `daemon install`
    // (see `exe_is_stale`'s doc comment for the full trap). A failed
    // canonicalize (e.g. the symlink target vanished entirely) yields
    // `None`, which `exe_is_stale` also treats conservatively as not stale.
    let canonical_expected = fs::canonicalize(bin_path)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string));

    exe_is_stale(exe_target, canonical_expected.as_deref())
}

/// Human-readable label for a [`SystemdState`], for status/diagnostic
/// messages printed to the user.
fn describe_state(state: Option<SystemdState>) -> &'static str {
    match state {
        Some(SystemdState::Active) => "active",
        Some(SystemdState::Activating) => "activating",
        Some(SystemdState::AutoRestart) => "auto-restart (crash-looping)",
        Some(SystemdState::Failed) => "failed",
        Some(SystemdState::Inactive) => "inactive",
        Some(SystemdState::Unknown) | None => "unknown",
    }
}

/// Scan `/proc` for VectorHawk daemon processes running **outside** the
/// `vectorhawk-agent.service` cgroup, and SIGTERM them.
///
/// This is the repair step for boxes already stuck in the bug this module
/// fixes: a previous buggy install direct-spawned a daemon that `setsid()`
/// detached from the invoking *session* but did **not** move into the
/// unit's *cgroup* (`setsid()` only changes the POSIX session/process-group;
/// cgroup membership is a separate, unrelated kernel mechanism) — so the
/// stray keeps running indefinitely in the shell's `session-NNNN.scope`,
/// indistinguishable from a legitimate process by session alone. Since
/// 1.0.91's `acquire_socket` singleton guard, whichever of {stray, managed
/// unit} binds the socket first wins and the other refuses to start — so a
/// stray that won the race once keeps permanently starving the managed
/// unit on every subsequent `daemon-reload`/`start` unless something reaps
/// it first. This is that something.
///
/// Called only from the systemctl path, before (re)starting the unit — the
/// XDG-autostart fallback has no cgroups to compare against and no stray
/// problem (it never direct-spawns a competitor).
///
/// Every step here is best-effort and non-fatal: a `/proc` we can't fully
/// enumerate, a `cmdline`/`cgroup` file we can't read, or a `kill` that
/// fails (already exited, no permission) must never fail the install.
fn reap_stray_daemons() {
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return;
    };
    let our_pid = std::process::id();

    for entry in proc_entries.flatten() {
        let pid_str = entry.file_name().to_string_lossy().into_owned();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == our_pid {
            continue;
        }

        let Ok(raw_cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        // Positional argv match (see `cmdline_is_daemon_run`'s doc comment)
        // — not a substring test. A prior substring-based version of this
        // check killed an innocent shell whose command line happened to
        // mention "vectorhawk", "daemon" and "/run/user/1000/bus" as
        // unrelated tokens.
        if !cmdline_is_daemon_run(&raw_cmdline) {
            continue;
        }

        // If we can't read the cgroup, we can't prove it's a stray — leave
        // it alone rather than guess and kill something we shouldn't.
        let Ok(cgroup_contents) = fs::read_to_string(entry.path().join("cgroup")) else {
            continue;
        };
        if cgroup_is_service(&cgroup_contents, SERVICE_NAME) {
            // Already owned by the systemd unit — this is the process we're
            // about to (re)start, not a stray.
            continue;
        }

        println!(
            "Stopping a VectorHawk daemon running outside systemd (pid {pid}) so \
             the service can take ownership."
        );
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
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

    // ── Idempotency guard — but only when genuinely healthy ───────────────────
    // Skip (no-op) only when the unit exists, is enabled, the ExecStart path
    // matches the current binary, AND the unit is actually `Active` with a
    // reachable socket. "Exists, enabled, right binary" is not enough: a unit
    // can sit there fully installed while stuck in `auto-restart` because a
    // stray direct-spawned daemon (the bug this module fixes) is holding the
    // socket out from under it — `NRestarts` climbing forever with
    // `MainPID=0`. Early-returning "already installed and up to date" in that
    // state is a lie, and worse, it would return *before* the stray-reaping
    // and restart logic below ever runs — on exactly the boxes that need it.
    // So the health check happens here, up front, with its own state/socket
    // reads (the socket path is needed now, before it's otherwise computed
    // below for the post-restart wait).
    let xdg = xdg_runtime_dir();
    let xdg_sock = format!("{xdg}/vectorhawk/agent.sock");

    let unit_exists_and_enabled = unit.exists() && unit_is_enabled();
    let unit_has_current_binary = unit_exists_and_enabled
        && fs::read_to_string(&unit)
            .ok()
            .map(|s| bin_path.to_str().map(|b| s.contains(b)).unwrap_or(false))
            .unwrap_or(false);

    // `needs_restart` covers two distinct cases that both mean "the unit is
    // already on disk, but we must not just write-and-walk-away": (a) the
    // binary path changed (a Homebrew upgrade rewrote the Cellar path), or
    // (b) the binary is unchanged but the service isn't actually healthy.
    // Both take the same `restart` (rather than `enable --now`) path below,
    // since the unit is already enabled in both cases.
    let needs_restart = if unit_exists_and_enabled {
        if unit_has_current_binary {
            let pre_check_state = unit_state();
            let pre_check_socket_up = socket_is_reachable(&xdg_sock, 200);
            // Catches the case `state`/`socket_up` alone can't: systemd
            // reports Active, MainPID unchanged, NRestarts=0 — genuinely
            // healthy by every check above — but `brew upgrade` replaced the
            // binary on disk out from under the still-running process, which
            // keeps executing the old (possibly now-deleted) inode
            // indefinitely on Linux. See `exe_is_stale` in `mod.rs` for the
            // live incident this fixes.
            let exe_stale = running_exe_is_stale(bin_path);
            let healthy = unit_is_healthy(pre_check_state, pre_check_socket_up, exe_stale);
            if healthy {
                println!(
                    "VectorHawk daemon is already installed and up to date — no changes made."
                );
                return Ok(());
            }
            if exe_stale && pre_check_state == Some(SystemdState::Active) && pre_check_socket_up {
                // Distinct from the generic "not running" message below:
                // this box's unit is genuinely up and healthy by
                // state/socket — the *only* thing wrong is that the running
                // process predates the on-disk binary. Reusing the
                // auto-restart wording here would mislead a user checking
                // why an install that used to be a no-op now restarts.
                println!(
                    "VectorHawk daemon is running an outdated binary (upgraded on disk) — restarting."
                );
            } else {
                println!(
                    "VectorHawk daemon unit is installed but not running ({}) — repairing.",
                    describe_state(pre_check_state)
                );
            }
            true
        } else {
            // Binary path changed (upgrade): fall through to rewrite + restart.
            println!("VectorHawk daemon binary path changed — updating unit and restarting.");
            true
        }
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
    // If this fails, the likely cause is no systemd user session at all (no
    // D-Bus session bus for this user) — most commonly a headless/server box
    // that hasn't logged in graphically since boot. Say so explicitly rather
    // than surfacing only the bare systemctl stderr.
    systemctl_user(&["daemon-reload"]).context(
        "systemctl --user daemon-reload failed — most likely there is no \
         systemd user session for this user (no D-Bus session bus). On a \
         headless/server box, run `sudo loginctl enable-linger $USER` once \
         to start one at boot without a graphical login, then re-run \
         `vectorhawk daemon install`",
    )?;

    // `xdg` / `xdg_sock` were already computed above, before the idempotency
    // health check.

    // ── 3b. Reap strays before starting the unit ──────────────────────────────
    // Only on this (systemctl) path — the XDG-autostart fallback never
    // direct-spawns a competitor, so it has no strays to clean up. This must
    // run *before* start/restart below: it is what repairs a box already
    // stuck in the bug this module fixes (a stray from a previous install
    // holding the socket, so the managed unit can never win it).
    reap_stray_daemons();
    // Bounded wait for the socket to clear after the SIGTERMs above, so the
    // unit we're about to start isn't immediately shut out by a stray that's
    // merely slow to exit. Non-fatal either way — if it doesn't clear in
    // time, the start attempt below will simply fail and we fall through to
    // the diagnostics branch rather than spawning another competitor.
    let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while socket_is_reachable(&xdg_sock, 100) && std::time::Instant::now() < reap_deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // ── 4. Start or restart the daemon ────────────────────────────────────────
    // `needs_restart` is true both for a binary-path upgrade and for the
    // "installed but unhealthy" repair case established above — in both, the
    // unit is already enabled, so `restart` (not `enable --now`) is correct.
    let started_via_systemd = if needs_restart {
        // `restart` atomically stops the old process and starts the new one.
        // `systemctl_user` supplies its own explicit XDG_RUNTIME_DIR/
        // DBUS_SESSION_BUS_ADDRESS (see its doc comment), so this works fine
        // inside Homebrew's `post_install` sandbox too — a real D-Bus
        // session bus is present there on both platforms (see `unit_state`'s
        // doc comment for the live evidence). A failure here is a genuine
        // problem, not an absent bus, and is surfaced by the diagnostics
        // branch below.
        systemctl_user(&["restart", SERVICE_NAME]).is_ok()
    } else {
        // Fresh install: enable the unit for auto-start and start it now.
        systemctl_user(&["enable", "--now", SERVICE_NAME]).is_ok()
    };

    // ── 5. Verify the daemon actually came up, and decide what (if anything)
    //      to do about it ───────────────────────────────────────────────────
    //
    // The unit is `Type=simple`, so `systemctl --user enable --now` (or
    // `restart`) returns as soon as the process is *forked* — well before
    // tokio has initialised, `state.db` has been opened, and the socket has
    // been bound. A single instantaneous probe right after that call fires
    // into a guaranteed-empty window on essentially every install, which is
    // the root cause this whole restructure exists to fix. `wait_for_socket`
    // polls for up to 5 s, which comfortably covers that startup window.
    let socket_up = wait_for_socket(&xdg_sock, std::time::Duration::from_secs(5));

    // Ask systemd what it actually thinks the unit's state is, so a failure
    // message below is grounded in ground truth rather than just "the socket
    // probe failed once."
    let state = unit_state();

    if socket_up {
        if started_via_systemd {
            println!("Systemd user unit enabled and started ({SERVICE_NAME}).");
        } else {
            println!("VectorHawk daemon is running.");
        }
    } else {
        // systemd did not bring the daemon up within the wait budget — surface
        // the real unit state and the tools to diagnose it. This module used
        // to fall back to spawning a daemon directly, detached from systemd,
        // on the theory that Homebrew's `post_install` sandbox has no D-Bus
        // session bus. That premise is false (see `unit_state`'s doc comment
        // for the live evidence), and the fallback was actively harmful: a
        // process spawned this way is `setsid()`-detached from the invoking
        // session but never joins the unit's systemd cgroup, so it becomes
        // exactly the kind of stray `reap_stray_daemons()` above exists to
        // clean up — and if spawned while the unit is crash-looping
        // (`AutoRestart`), it wins `acquire_socket`'s race and makes the
        // crash loop permanent. Never spawn a competitor; just report.
        println!(
            "VectorHawk daemon unit is installed and systemd reports it as \
             {}, but the daemon socket did not come up within 5s.\n\
             Diagnose with:\n  \
             XDG_RUNTIME_DIR={xdg} systemctl --user status {SERVICE_NAME}\n  \
             XDG_RUNTIME_DIR={xdg} journalctl --user -u {SERVICE_NAME}\n\
             Start it manually with:\n  \
             XDG_RUNTIME_DIR={xdg} systemctl --user start {SERVICE_NAME}",
            describe_state(state)
        );
    }
    Ok(())
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
