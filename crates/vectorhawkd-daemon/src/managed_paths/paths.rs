//! Platform-appropriate path resolution for managed paths.
//!
//! All paths are derived from `dirs::home_dir()` so they work on macOS, Linux,
//! and any future platform Claude Code supports.  Path separators are handled
//! transparently by `std::path::PathBuf`.

use std::path::PathBuf;

/// Subdirectory names under `~/.claude/plugins/` that are Anthropic plumbing
/// and must never be touched by the reconciler.
pub const EXCLUDED_PLUGIN_DIRS: &[&str] = &["marketplaces", "cache", "data"];

/// Resolve `~/.claude/skills/`.
pub fn skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("skills"))
}

/// Resolve `~/.claude/plugins/`.
pub fn plugins_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("plugins"))
}

/// Resolve `~/.claude.json`.
pub fn claude_json_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.json"))
}

/// Return true if `dir_name` is an excluded Anthropic plumbing directory.
pub fn is_excluded_plugin_dir(dir_name: &str) -> bool {
    EXCLUDED_PLUGIN_DIRS.contains(&dir_name)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_dirs_contains_expected() {
        assert!(is_excluded_plugin_dir("marketplaces"));
        assert!(is_excluded_plugin_dir("cache"));
        assert!(is_excluded_plugin_dir("data"));
        assert!(!is_excluded_plugin_dir("my-plugin"));
        assert!(!is_excluded_plugin_dir("vectorhawk-plugin"));
    }

    #[test]
    fn path_helpers_return_some() {
        // On any platform with a home dir these should resolve.
        assert!(skills_dir().is_some());
        assert!(plugins_dir().is_some());
        assert!(claude_json_path().is_some());
    }
}
