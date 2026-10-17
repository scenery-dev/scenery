//! macOS LaunchAgent install/uninstall for the VectorHawk daemon.
//!
//! Plist path:   `~/Library/LaunchAgents/com.vectorhawk.agent.plist`
//! Crash log:    `~/Library/Logs/VectorHawk/stderr.log` (StandardErrorPath —
//!               launchd does NOT rotate this, so it carries panic/early-
//!               startup output only, never the durable daemon log)
//! Durable log:  `~/Library/Application Support/VectorHawk/logs/vectorhawkd.log`
//!               — INFO-level tracing output, written by an in-process
//!               size-bounded rotating appender (see
//!               `crates/vectorhawkd-cli/src/logging.rs`), ~50 MB total cap.
//!               `StandardOutPath` is intentionally NOT set: tracing no
//!               longer writes to stdout, and launchd would otherwise leave
//!               an unbounded, always-empty file behind.
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

use super::{
    daemon_socket_path, reap_stray_daemon_via_socket, socket_is_reachable, wait_for_socket,
    InstallStatus,
};

const LABEL: &str = "com.vectorhawk.agent";
const PLIST_FILENAME: &str = "com.vectorhawk.agent.plist";

/// `brew services`' launchd label for this formula — `homebrew.mxcl.<name>`
/// is Homebrew's fixed naming convention, not something we control.
const BREW_SERVICE_LABEL: &str = "homebrew.mxcl.vectorhawk";
/// `brew services`' plist filename, written to the same
/// `~/Library/LaunchAgents/` directory as our own agent.
const BREW_PLIST_FILENAME: &str = "homebrew.mxcl.vectorhawk.plist";

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

/// The `VECTORHAWK_REGISTRY_URL` env var name, as it appears (unquoted)
/// inside the plist's `EnvironmentVariables` dict.
const REGISTRY_URL_ENV_KEY: &str = "VECTORHAWK_REGISTRY_URL";

/// Escape a string for safe interpolation into plist XML text content (and,
/// defensively, into attribute position — this plist has none today, but a
/// future edit that adds one should not silently reintroduce this bug).
///
/// `&` is escaped FIRST: every other substitution below introduces new `&`
/// characters (`&lt;`, `&quot;`, ...), and escaping `&` afterward would
/// double-escape those.
///
/// Without this, a registry URL containing `&` — ordinary for a
/// self-hosted registry carrying a query string, e.g.
/// `https://host/api?token=abc&user=x` — produces malformed XML that
/// launchd fails to load or parses wrongly, silently.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Inverse of [`xml_escape`], for reading a value back out of a plist we
/// previously wrote. `&amp;` is decoded LAST — the mirror image of encoding
/// `&` first — so an `&amp;` introduced by escaping some other character
/// (e.g. the `&` in `&lt;`) is not itself misinterpreted as a second-order
/// entity while its sibling entities are still being decoded.
///
/// Without this, `resolve_registry_url_env` would carry the RAW escaped
/// text forward (e.g. `&amp;`) instead of the original value, and every
/// subsequent install/upgrade would re-escape it again, corrupting the
/// value a little further each time.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Resolve the `VECTORHAWK_REGISTRY_URL` value to carry into a
/// (re)rendered plist, so that installing/upgrading never silently re-homes
/// a daemon that was pointed at a private registry. Mirrors
/// `install::linux::resolve_registry_url_env` — see that doc comment for
/// the shared rationale (the Homebrew formula's `post_install` calls
/// `daemon install` on every upgrade, not just first install).
///
/// Resolution order:
/// 1. `VECTORHAWK_REGISTRY_URL` from the process environment at install
///    time, if set and non-empty.
/// 2. Otherwise, whatever value is already present in the plist at
///    `existing_plist_path` — carried forward rather than dropped.
/// 3. Otherwise `None` (today's behavior: no such key is emitted).
///
/// Step 2 is tolerant: a missing file, an unreadable file, or a plist with
/// no such key all mean "nothing to preserve", never an error that fails
/// the install.
fn resolve_registry_url_env(existing_plist_path: &std::path::Path) -> Option<String> {
    if let Ok(v) = std::env::var("VECTORHAWK_REGISTRY_URL") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    parse_registry_url_from_plist(existing_plist_path)
}

/// Tolerantly parse a `<key>VECTORHAWK_REGISTRY_URL</key><string>...</string>`
/// pair out of an existing plist file. Returns `None` on any failure to
/// read the file, or if no such key (with a non-empty value) is present.
///
/// This is a targeted string scan rather than a full plist/XML parse — the
/// crate has no XML dependency, and the only input this ever has to handle
/// is a plist this same function previously wrote.
fn parse_registry_url_from_plist(path: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let key_tag = format!("<key>{REGISTRY_URL_ENV_KEY}</key>");
    let after_key = &content[content.find(&key_tag)? + key_tag.len()..];
    let string_open = "<string>";
    let string_start = after_key.find(string_open)? + string_open.len();
    let string_end = after_key[string_start..].find("</string>")?;
    let value = xml_unescape(after_key[string_start..string_start + string_end].trim());
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Generate the LaunchAgent plist XML from the given binary path and log dir.
///
/// `RUST_LOG=info` turns on INFO-level daemon logging by default (D1 board
/// card — override-able by re-running with a different `RUST_LOG` in the
/// environment before `daemon install`, or by editing the plist). The
/// durable log destination is NOT `StandardOutPath`/`StandardErrorPath`:
/// `vectorhawk daemon run` initializes its own size-bounded rotating file
/// appender under the state directory (see `logging.rs`), so `StandardOutPath`
/// is omitted entirely (nothing writes to stdout) and `StandardErrorPath`
/// carries only panic/early-startup crash output — launchd doesn't rotate
/// it, but crashes are rare relative to INFO-volume logging.
///
/// `registry_url`, when `Some`, is emitted as an extra
/// `VECTORHAWK_REGISTRY_URL` key/string pair in `EnvironmentVariables` —
/// see [`resolve_registry_url_env`] for how the caller resolves it.
///
/// `KeepAlive` is the plain boolean `true`, not the
/// `{"SuccessfulExit": false}` dictionary form this plist used to carry.
/// Per `launchd.plist(5)`, `SuccessfulExit: false` restarts the job only
/// "in the inverse condition" of a successful exit — i.e. only on a
/// non-zero exit, the macOS analog of systemd's `Restart=on-failure`. That
/// silently defeated the binary-replacement watch (see
/// `vectorhawkd_daemon::binary_watch`), which deliberately exits 0 when it
/// detects an upgrade: launchd would see the clean exit and just leave the
/// daemon stopped. Plain `KeepAlive: true` restarts unconditionally on any
/// exit, so both a crash and the watch's deliberate clean exit bring the
/// daemon back — matching the Linux unit's `Restart=always`.
fn render_plist(
    bin_path: &std::path::Path,
    log_dir: &std::path::Path,
    registry_url: Option<&str>,
) -> Result<String> {
    let bin_str = bin_path
        .to_str()
        .context("binary path is not valid UTF-8")?;
    let bin_str = xml_escape(bin_str);
    let stderr_log = log_dir
        .join("stderr.log")
        .to_str()
        .context("log dir path is not valid UTF-8")?
        .to_string();
    let stderr_log = xml_escape(&stderr_log);

    let registry_env_entry = match registry_url {
        Some(url) => {
            let url = xml_escape(url);
            format!("        <key>{REGISTRY_URL_ENV_KEY}</key>\n        <string>{url}</string>\n")
        }
        None => String::new(),
    };

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
        <key>RUST_LOG</key>
        <string>info</string>
{registry_env_entry}    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
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

// ── Competing-service-manager detection (brew services) ─────────────────────
//
// `brew services` loads its own LaunchAgent for this formula
// (`homebrew.mxcl.vectorhawk`, plist at
// `~/Library/LaunchAgents/homebrew.mxcl.vectorhawk.plist`) — a completely
// separate launchd job from ours (`com.vectorhawk.agent`). Both carry
// `KeepAlive: true` against the same single-instance socket lock
// (`daemon.lock`): whichever loses the race can never acquire it, exits
// non-zero, and launchd restarts it right back — forever. `daemon install`
// (and therefore `mcp setup`, which calls it transparently) must detect a
// healthy brew-managed daemon and skip installing/enabling our own agent
// entirely, rather than relying on bootout/bootstrap races to sort it out.

/// Return `~/Library/LaunchAgents/homebrew.mxcl.vectorhawk.plist`.
fn brew_plist_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("failed to resolve HOME directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(BREW_PLIST_FILENAME))
}

/// Check whether `brew services`' launchd job is currently loaded, via
/// `launchctl print gui/<uid>/homebrew.mxcl.vectorhawk`.
fn brew_service_is_loaded(uid: u32) -> bool {
    Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{BREW_SERVICE_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Pure decision: is the daemon already managed by `brew services`?
///
/// Plist presence alone is the signal, deliberately not combined with
/// "currently loaded" the way an earlier version of this function required.
/// `brew services start` is what writes
/// `~/Library/LaunchAgents/homebrew.mxcl.vectorhawk.plist`, and `brew
/// services stop` is what removes it — Homebrew does not leave this file
/// behind for any other reason, so its mere presence already means brew
/// owns this daemon, independent of whether the job happens to be loaded or
/// running at this exact instant.
///
/// Requiring "loaded" too had a live gap: during a `brew upgrade`, brew's
/// job can be briefly unloaded (or loaded-but-not-yet-running) while it
/// cycles onto the new binary — the plist is untouched the whole time, but
/// `launchctl print` can fail in that window. Gating ownership on "loaded"
/// meant `daemon install` saw neither signal and proceeded to install our
/// own competing `com.vectorhawk.agent`, exactly the bug this guard exists
/// to prevent. The caller (`install`) still checks loaded/running state
/// separately — see [`brew_recovery_action`] — to decide whether brew's job
/// needs a bootstrap or kickstart, but that is a *recovery* decision, not
/// an *ownership* one.
fn brew_service_is_managing(plist_exists: bool) -> bool {
    plist_exists
}

/// What this installer should do to help Homebrew's launchd job back up,
/// once [`brew_service_is_managing`] has already determined brew owns the
/// daemon and our own agent has been self-healed away. Never touches brew's
/// plist file — only ever loads/starts the job it already describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrewRecoveryAction {
    /// The job isn't loaded into the launchd domain at all (e.g. brew's
    /// `KeepAlive` job exited mid-upgrade and hasn't been re-bootstrapped
    /// yet) — load it with `launchctl bootstrap`.
    Bootstrap,
    /// The job is loaded, but the daemon socket isn't reachable — force an
    /// immediate (re)start with `launchctl kickstart`.
    Kickstart,
    /// The job is loaded and the socket is already reachable — nothing to
    /// do.
    None,
}

/// Pure decision: given whether brew's job is currently loaded and whether
/// the daemon socket is currently reachable, which [`BrewRecoveryAction`]
/// (if any) should run?
pub(crate) fn brew_recovery_action(loaded: bool, socket_reachable: bool) -> BrewRecoveryAction {
    if !loaded {
        BrewRecoveryAction::Bootstrap
    } else if !socket_reachable {
        BrewRecoveryAction::Kickstart
    } else {
        BrewRecoveryAction::None
    }
}

/// Render [`BrewRecoveryAction`] into the exact `launchctl` argv the real
/// implementation passes to [`launchctl`] — pulled out as its own pure
/// function so tests can assert on the argv a real run would execute
/// without shelling out to `launchctl` at all. `brew_plist_str` is only
/// consulted for `Bootstrap` (`launchctl bootstrap` needs the plist path;
/// `kickstart` addresses the job purely by label).
pub(crate) fn brew_recovery_argv(
    action: BrewRecoveryAction,
    uid: u32,
    brew_plist_str: &str,
) -> Option<Vec<String>> {
    match action {
        BrewRecoveryAction::Bootstrap => Some(vec![
            "bootstrap".to_string(),
            domain_target(uid),
            brew_plist_str.to_string(),
        ]),
        BrewRecoveryAction::Kickstart => Some(vec![
            "kickstart".to_string(),
            format!("gui/{uid}/{BREW_SERVICE_LABEL}"),
        ]),
        BrewRecoveryAction::None => None,
    }
}

// ── Public install / uninstall ────────────────────────────────────────────────

/// Install the LaunchAgent and start the daemon.
pub fn install() -> Result<()> {
    let plist = plist_path().context("failed to resolve plist path")?;
    let log_dir = log_dir().context("failed to resolve log dir path")?;
    let bin_path =
        super::resolve_daemon_bin_path().context("failed to resolve daemon binary path")?;
    let uid = current_uid();

    // ── Competing-manager guard — must run before anything else below ────────
    // If `brew services` owns the daemon (plist present — see
    // `brew_service_is_managing`'s doc comment for why presence alone is the
    // right signal, not "loaded"), writing and loading our own agent on top
    // of it is exactly the bug this guard exists to prevent — detect it
    // first, skip our own install/enable entirely, self-heal any damage from
    // a previous run that didn't have this guard, and help brew's job back
    // up if it isn't currently loaded/running (never touching its plist).
    let brew_plist = brew_plist_path().context("failed to resolve brew plist path")?;
    if brew_service_is_managing(brew_plist.exists()) {
        println!(
            "VectorHawk daemon is managed by Homebrew services ({BREW_SERVICE_LABEL}); \
             not installing {LABEL}."
        );
        self_heal_remove_our_agent(&plist, uid)?;

        let loaded = brew_service_is_loaded(uid);
        let socket_reachable = socket_is_reachable(&daemon_socket_path(), 200);
        let action = brew_recovery_action(loaded, socket_reachable);
        let brew_plist_str = brew_plist
            .to_str()
            .context("brew plist path is not valid UTF-8")?;
        if let Some(argv) = brew_recovery_argv(action, uid, brew_plist_str) {
            let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            match action {
                BrewRecoveryAction::Bootstrap => {
                    println!("{BREW_SERVICE_LABEL} is not loaded — bootstrapping it.");
                }
                BrewRecoveryAction::Kickstart => {
                    println!(
                        "{BREW_SERVICE_LABEL} is loaded but the daemon socket is not reachable \
                         — kickstarting it."
                    );
                }
                BrewRecoveryAction::None => {}
            }
            if let Err(e) = launchctl(&argv_refs) {
                println!("warning: failed to recover {BREW_SERVICE_LABEL}: {e:#}");
            } else if wait_for_socket(&daemon_socket_path(), std::time::Duration::from_secs(10)) {
                println!("{BREW_SERVICE_LABEL} is now running and the daemon socket is reachable.");
            } else {
                println!(
                    "Recovered {BREW_SERVICE_LABEL}, but the daemon socket did not become \
                     reachable within 10s."
                );
            }
        }
        return Ok(());
    }

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
    // Resolve VECTORHAWK_REGISTRY_URL from the env or, failing that, from
    // whatever the plist already had — before we overwrite it below.
    let registry_url = resolve_registry_url_env(&plist);
    let plist_content = render_plist(&bin_path, &log_dir, registry_url.as_deref())
        .context("failed to render LaunchAgent plist")?;
    fs::write(&plist, &plist_content)
        .with_context(|| format!("failed to write plist: {}", plist.display()))?;

    println!("Wrote LaunchAgent plist: {}", plist.display());

    // ── 3b. Reap a verified-stale daemon holding the socket outside launchd ──
    //
    // Mirrors Linux's `reap_stray_daemons` repair step (see that module's
    // doc comment for the historical bug): an install predating LaunchAgent
    // packaging — or a daemon run by hand — can be sitting on the socket
    // without launchd ever knowing about it. `bootout`/`kickstart -k` below
    // only ever touch a process launchd itself started under this label;
    // they cannot reach an unmanaged process. Verify-and-take-over first so
    // the bootstrap below isn't shut out of the socket it needs. Best
    // effort: if nothing is listening, or the holder can't be verified
    // stale, this changes nothing and bootstrap below proceeds (and simply
    // fails to bind the socket if something still legitimately owns it).
    reap_stray_daemon_via_socket(&daemon_socket_path());

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

/// Disable, stop, and remove our own LaunchAgent (`com.vectorhawk.agent`),
/// because the daemon is confirmed managed elsewhere (`brew services`).
///
/// Only touches our own plist/label — never `brew services`' job. A no-op
/// when there is nothing of ours to clean up (fresh box that never had our
/// agent installed in the first place).
fn self_heal_remove_our_agent(plist: &std::path::Path, uid: u32) -> Result<()> {
    let loaded = service_is_loaded(uid);
    let exists = plist.exists();
    if !super::should_remove_our_unit(true, exists, loaded) {
        return Ok(());
    }

    if loaded {
        let _ = launchctl(&["bootout", &service_target(uid)]);
    }
    if exists {
        fs::remove_file(plist)
            .with_context(|| format!("failed to remove stale plist: {}", plist.display()))?;
    }

    println!(
        "Removed our own LaunchAgent ({LABEL}) — the daemon is managed by Homebrew services \
         instead."
    );
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

#[cfg(test)]
mod tests {
    use super::{
        brew_recovery_action, brew_recovery_argv, brew_service_is_managing, render_plist,
        resolve_registry_url_env, BrewRecoveryAction,
    };
    use std::path::Path;

    // ── brew_service_is_managing ─────────────────────────────────────────────
    //
    // Original regression coverage: spaceghost, 2026-10-13. `brew services`
    // already owned a healthy daemon under `homebrew.mxcl.vectorhawk`;
    // `mcp setup`'s `daemon install` step must recognize that and refuse to
    // install/enable a second, competing `com.vectorhawk.agent` LaunchAgent
    // that can never win the single-instance socket lock.
    //
    // Follow-on fix (also covered here): ownership now follows from plist
    // *presence* alone, not "presence AND currently loaded" — see
    // `brew_service_is_managing`'s doc comment for the upgrade-window gap
    // that combining both signals left open.

    #[test]
    fn managed_when_plist_exists_even_if_not_loaded() {
        // `brew services start` writes this plist and `brew services stop`
        // removes it — presence alone means brew owns the daemon,
        // independent of whether the job happens to be loaded right this
        // instant (e.g. mid `brew upgrade`, where the old job has already
        // exited but the plist hasn't been touched).
        assert!(brew_service_is_managing(true));
    }

    #[test]
    fn not_managed_when_plist_absent() {
        assert!(!brew_service_is_managing(false));
    }

    // ── brew_recovery_action / brew_recovery_argv ────────────────────────────
    //
    // The recovery half of the same fix: once ownership is established from
    // plist presence alone, brew's job may genuinely need help coming back
    // up (not loaded, or loaded but not yet serving the socket). These pure
    // functions are what `install` consults to decide — and to compute the
    // exact `launchctl` argv for — that recovery, asserted here without
    // shelling out to `launchctl` at all.

    #[test]
    fn not_loaded_bootstraps() {
        assert_eq!(
            brew_recovery_action(false, false),
            BrewRecoveryAction::Bootstrap
        );
        // Socket reachability is irrelevant when the job isn't even
        // loaded — there is nothing to serve it.
        assert_eq!(
            brew_recovery_action(false, true),
            BrewRecoveryAction::Bootstrap
        );
    }

    #[test]
    fn loaded_but_socket_unreachable_kickstarts() {
        assert_eq!(
            brew_recovery_action(true, false),
            BrewRecoveryAction::Kickstart
        );
    }

    #[test]
    fn loaded_and_socket_reachable_does_nothing() {
        assert_eq!(brew_recovery_action(true, true), BrewRecoveryAction::None);
    }

    #[test]
    fn bootstrap_argv_targets_the_brew_plist_in_the_domain() {
        let argv = brew_recovery_argv(
            BrewRecoveryAction::Bootstrap,
            501,
            "/Users/spaceghost/Library/LaunchAgents/homebrew.mxcl.vectorhawk.plist",
        );
        assert_eq!(
            argv,
            Some(vec![
                "bootstrap".to_string(),
                "gui/501".to_string(),
                "/Users/spaceghost/Library/LaunchAgents/homebrew.mxcl.vectorhawk.plist".to_string(),
            ])
        );
    }

    #[test]
    fn kickstart_argv_targets_the_brew_service_label_not_the_plist() {
        let argv = brew_recovery_argv(BrewRecoveryAction::Kickstart, 501, "/ignored");
        assert_eq!(
            argv,
            Some(vec![
                "kickstart".to_string(),
                "gui/501/homebrew.mxcl.vectorhawk".to_string(),
            ])
        );
    }

    #[test]
    fn none_action_produces_no_argv() {
        assert_eq!(
            brew_recovery_argv(BrewRecoveryAction::None, 501, "/ignored"),
            None
        );
    }

    // D1 board card: INFO logging on by default, bounded log footprint on
    // disk. `render_plist` must set RUST_LOG=info, must no longer point
    // StandardOutPath at the unbounded launchd redirect (the durable log is
    // now the in-process rotating file appender — see logging.rs), and must
    // keep StandardErrorPath for crash-only output.
    #[test]
    fn plist_sets_rust_log_info() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            None,
        )
        .unwrap();
        assert!(
            plist.contains("<key>RUST_LOG</key>\n        <string>info</string>"),
            "expected RUST_LOG=info in EnvironmentVariables, got:\n{plist}"
        );
    }

    #[test]
    fn plist_does_not_set_standard_out_path() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            None,
        )
        .unwrap();
        assert!(
            !plist.contains("StandardOutPath"),
            "StandardOutPath should be omitted — launchd does not rotate it \
             and nothing writes to stdout anymore, got:\n{plist}"
        );
    }

    #[test]
    fn plist_still_sets_standard_error_path_for_crash_output() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            None,
        )
        .unwrap();
        assert!(
            plist.contains("<key>StandardErrorPath</key>\n    <string>/Users/test/Library/Logs/VectorHawk/stderr.log</string>"),
            "expected StandardErrorPath pointing at stderr.log for crash-only output, got:\n{plist}"
        );
    }

    // ── Registry URL preservation (regression coverage) ───────────────────────
    //
    // `render_plist` used to emit a fixed EnvironmentVariables dict carrying
    // only PATH and RUST_LOG, so regenerating the plist on every
    // install/upgrade silently dropped any VECTORHAWK_REGISTRY_URL the
    // plist previously had.

    #[test]
    fn render_plist_emits_registry_url_when_given() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            Some("https://dev.vectorhawk.ai"),
        )
        .unwrap();
        assert!(
            plist.contains(
                "<key>VECTORHAWK_REGISTRY_URL</key>\n        <string>https://dev.vectorhawk.ai</string>"
            ),
            "expected registry URL key/string pair in the plist, got:\n{plist}"
        );
    }

    #[test]
    fn render_plist_omits_registry_url_when_none() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            None,
        )
        .unwrap();
        assert!(
            !plist.contains("VECTORHAWK_REGISTRY_URL"),
            "expected no registry URL key, got:\n{plist}"
        );
    }

    // ── XML escaping (fix-round-1 blocker) ─────────────────────────────────────
    //
    // `render_plist` used to interpolate the registry URL (and the binary /
    // log paths) straight into plist XML text content. A URL containing
    // `&` — ordinary for a self-hosted registry carrying a query string —
    // produced malformed XML that launchd would fail to load or parse
    // wrongly, silently. Asserted textually (substring checks) rather than
    // by parsing: this crate has no XML/plist parser dev-dependency, and
    // adding one for two tests wasn't worth it.
    //
    // These tests fail against the pre-fix code (no `xml_escape` call at
    // all — the raw `&`/`<` would appear verbatim in the output).

    #[test]
    fn render_plist_escapes_ampersand_in_registry_url() {
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            Some("https://registry.example.com/api?token=abc&user=x"),
        )
        .unwrap();
        assert!(
            plist
                .contains("<string>https://registry.example.com/api?token=abc&amp;user=x</string>"),
            "expected the '&' to be escaped as '&amp;', got:\n{plist}"
        );
        assert!(
            !plist.contains("token=abc&user=x"),
            "the raw, unescaped '&' must not appear anywhere in the plist, got:\n{plist}"
        );
    }

    #[test]
    fn render_plist_escapes_angle_brackets_in_registry_url() {
        // Contrived but exercises the same code path as `&` — a value that
        // would otherwise inject a bogus XML tag.
        let plist = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            Some("https://example.com/<bogus>"),
        )
        .unwrap();
        assert!(
            plist.contains("<string>https://example.com/&lt;bogus&gt;</string>"),
            "expected '<'/'>' to be escaped, got:\n{plist}"
        );
        assert!(
            !plist.contains("<bogus>"),
            "the raw, unescaped angle brackets must not appear in the plist, got:\n{plist}"
        );
    }

    /// A value containing `&` must round-trip: written escaped, then read
    /// back (and carried forward on the next install) as the ORIGINAL raw
    /// value — not the escaped text, which would otherwise get re-escaped a
    /// little further on every subsequent install/upgrade.
    #[test]
    fn resolve_round_trips_a_value_containing_special_characters() {
        let _env = RegistryUrlEnv::unset();
        let original = "https://registry.example.com/api?token=abc&user=x";
        let rendered = render_plist(
            Path::new("/opt/homebrew/bin/vectorhawk"),
            Path::new("/Users/test/Library/Logs/VectorHawk"),
            Some(original),
        )
        .unwrap();

        let plist_path = temp_plist_path("roundtrip");
        std::fs::write(&plist_path, &rendered).unwrap();

        let resolved = resolve_registry_url_env(&plist_path);
        assert_eq!(
            resolved.as_deref(),
            Some(original),
            "expected the raw original value back, not the escaped XML text"
        );

        let _ = std::fs::remove_file(&plist_path);
    }

    /// Serializes tests below that mutate `VECTORHAWK_REGISTRY_URL` in the
    /// process environment — `cargo test` runs tests in the same process by
    /// default, so unguarded env mutation here would race other tests in
    /// this module (mirrors the `KeychainOff` pattern used elsewhere in
    /// this workspace, and `install::linux`'s identical guard, for the
    /// same reason).
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

    fn temp_plist_path(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vh-install-macos-test-{label}-{nanos}.plist"))
    }

    #[test]
    fn resolve_prefers_env_var_over_existing_plist() {
        let _env = RegistryUrlEnv::set("https://env.example.com");
        let plist_path = temp_plist_path("env-wins");
        std::fs::write(
            &plist_path,
            "<key>VECTORHAWK_REGISTRY_URL</key>\n<string>https://old.example.com</string>\n",
        )
        .unwrap();

        let resolved = resolve_registry_url_env(&plist_path);
        assert_eq!(resolved.as_deref(), Some("https://env.example.com"));

        let _ = std::fs::remove_file(&plist_path);
    }

    /// The actual regression case: env var unset at install time, but the
    /// existing plist on disk has one — it must be carried forward, not
    /// dropped.
    #[test]
    fn resolve_falls_back_to_existing_plist_when_env_unset() {
        let _env = RegistryUrlEnv::unset();
        let plist_path = temp_plist_path("fallback");
        std::fs::write(
            &plist_path,
            "<dict>\n<key>PATH</key>\n<string>/bin</string>\n<key>VECTORHAWK_REGISTRY_URL</key>\n<string>https://dev.vectorhawk.ai</string>\n</dict>\n",
        )
        .unwrap();

        let resolved = resolve_registry_url_env(&plist_path);
        assert_eq!(resolved.as_deref(), Some("https://dev.vectorhawk.ai"));

        let _ = std::fs::remove_file(&plist_path);
    }

    #[test]
    fn resolve_returns_none_when_env_unset_and_no_existing_plist() {
        let _env = RegistryUrlEnv::unset();
        let plist_path = temp_plist_path("absent");
        assert!(!plist_path.exists(), "precondition: no existing plist");

        let resolved = resolve_registry_url_env(&plist_path);
        assert_eq!(resolved, None);
    }

    /// Existing plist unreadable (here: a directory instead of a file) must
    /// not error — it means "nothing to preserve", same as absent.
    #[test]
    fn resolve_is_tolerant_of_unreadable_existing_plist() {
        let _env = RegistryUrlEnv::unset();
        let dir_path = temp_plist_path("unreadable-dir");
        std::fs::create_dir_all(&dir_path).unwrap();

        let resolved = resolve_registry_url_env(&dir_path);
        assert_eq!(resolved, None);

        let _ = std::fs::remove_dir_all(&dir_path);
    }
}
