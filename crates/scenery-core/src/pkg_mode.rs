//! macOS `.pkg` install detection and managed registry URL.
//!
//! A pkg-managed install puts the binary under [`PKG_ROOT`] and registers a
//! system LaunchAgent. The registry URL can come from a managed preference
//! (MDM) or, for the dev pkg flavor, an installer-choice file.

use std::path::Path;

pub const PKG_ROOT: &str = "/Library/Application Support/Scenery";
pub const PKG_BIN: &str = "/Library/Application Support/Scenery/bin/scenery";
pub const SYSTEM_AGENT_LABEL: &str = "dev.scenery.agent";
pub const SYSTEM_PLIST_PATH: &str = "/Library/LaunchAgents/dev.scenery.agent.plist";
pub const MANAGED_PREFS_DOMAIN: &str = "/Library/Managed Preferences/dev.scenery.runner";
pub const INSTALLER_CHOICE_FILE: &str = "/Library/Application Support/Scenery/registry-url";

/// Pure detection: running the pkg binary, or the system LaunchAgent exists.
pub fn is_pkg_managed_at(exe: &Path, system_plist_exists: bool) -> bool {
    exe == Path::new(PKG_BIN) || system_plist_exists
}

/// True when this process is part of a macOS pkg install.
pub fn is_pkg_managed() -> bool {
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_default();
        is_pkg_managed_at(&exe, Path::new(SYSTEM_PLIST_PATH).exists())
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Trim and validate a URL read from `defaults` or the choice file.
pub fn parse_defaults_output(stdout: &str) -> Option<String> {
    let s = stdout.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))?;
    if rest.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// The `RegistryURL` managed preference, if set and valid.
///
/// Only the device-level managed preferences path ([`MANAGED_PREFS_DOMAIN`]) is
/// read; user-scoped managed profiles are not. MDM profiles must target the
/// device.
pub fn managed_registry_url() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/defaults")
            .args(["read", MANAGED_PREFS_DOMAIN, "RegistryURL"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_defaults_output(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Managed pref wins, then the installer-choice file; both validated.
pub fn resolve_pkg_registry_url(
    managed: Option<String>,
    choice_file: Option<String>,
) -> Option<String> {
    managed
        .as_deref()
        .and_then(parse_defaults_output)
        .or_else(|| choice_file.as_deref().and_then(parse_defaults_output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_exe_is_managed() {
        assert!(is_pkg_managed_at(Path::new(PKG_BIN), false));
    }

    #[test]
    fn system_plist_alone_is_managed() {
        assert!(is_pkg_managed_at(
            Path::new("/opt/homebrew/bin/scenery"),
            true
        ));
    }

    #[test]
    fn brew_exe_is_not_managed() {
        assert!(!is_pkg_managed_at(
            Path::new("/opt/homebrew/Cellar/scenery/2.7.0/bin/scenery"),
            false
        ));
    }

    #[test]
    fn defaults_output_trims_newline() {
        assert_eq!(
            parse_defaults_output("https://dev.scenery.dev\n"),
            Some("https://dev.scenery.dev".into())
        );
    }

    #[test]
    fn managed_pref_beats_choice_file() {
        assert_eq!(
            resolve_pkg_registry_url(
                Some("https://a.example".into()),
                Some("https://dev.scenery.dev".into())
            ),
            Some("https://a.example".into())
        );
        assert_eq!(
            resolve_pkg_registry_url(None, Some("https://dev.scenery.dev\n".into())),
            Some("https://dev.scenery.dev".into())
        );
        assert_eq!(resolve_pkg_registry_url(None, None), None);
    }

    #[test]
    fn defaults_output_rejects_garbage() {
        assert_eq!(parse_defaults_output("not a url"), None);
        assert_eq!(parse_defaults_output(""), None);
        assert_eq!(parse_defaults_output("file:///etc"), None);
    }
}
