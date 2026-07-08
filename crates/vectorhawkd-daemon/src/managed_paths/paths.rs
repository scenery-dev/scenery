//! Platform-appropriate path resolution for managed paths.
//!
//! All paths are derived from `dirs::home_dir()` so they work on macOS, Linux,
//! and any future platform Claude Code supports.  Path separators are handled
//! transparently by `std::path::PathBuf`.

use std::path::PathBuf;

// The canonical definitions of the exclusion list and the plumbing-dir predicate
// live in `vectorhawkd_mcp::ownership` (single source of truth, shared with the
// shim). Re-exported here so existing `paths::…` callers keep working.
pub use vectorhawkd_mcp::ownership::{is_excluded_plugin_dir, EXCLUDED_PLUGIN_DIRS};

/// Resolve `~/.claude/skills/`.
pub fn skills_dir() -> Option<PathBuf> {
    vectorhawkd_mcp::ownership::claude_skills_dir()
}

/// Resolve `~/.claude/plugins/`.
pub fn plugins_dir() -> Option<PathBuf> {
    vectorhawkd_mcp::ownership::claude_plugins_dir()
}

/// Resolve `~/.claude.json`.
pub fn claude_json_path() -> Option<PathBuf> {
    vectorhawkd_mcp::ownership::claude_json_path()
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
