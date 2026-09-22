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
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
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
    // Resolve VECTORHAWK_REGISTRY_URL from the env or, failing that, from
    // whatever the plist already had — before we overwrite it below.
    let registry_url = resolve_registry_url_env(&plist);
    let plist_content = render_plist(&bin_path, &log_dir, registry_url.as_deref())
        .context("failed to render LaunchAgent plist")?;
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

#[cfg(test)]
mod tests {
    use super::{render_plist, resolve_registry_url_env};
    use std::path::Path;

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
