//! Ownership & origin classification — the single source of truth for deciding
//! whether a piece of on-disk Claude Code content is **Anthropic-native**
//! (strictly out of scope for VectorHawk), **VectorHawk-managed** (we own it),
//! or **custom** (user/third-party content eligible for adoption & governance).
//!
//! # Why this module exists
//!
//! VectorHawk must *never* enumerate, modify, reconcile, or delete Claude Code's
//! own first-party skills/plugins/tools, while it *must* auto-adopt and govern
//! everything else. Historically the "is this ours / is this native?" rule was
//! implemented ad-hoc in several places (the daemon reconciler, the shim
//! aggregator filter, the plugin scanner) with slightly different logic. This
//! module centralises those rules so there is exactly one implementation.
//!
//! # Placement
//!
//! This lives in `vectorhawkd-mcp` (not `vectorhawkd-core`) on purpose: the shim
//! links `vectorhawkd-mcp` *without* the `daemon` feature and cannot reach
//! `vectorhawkd-core`. Both the daemon and the shim can reach this module, and
//! it depends only on `std`, `serde_json`, `dirs`, `anyhow`, and `tracing` — no
//! `rusqlite`, no `vectorhawkd-core` — so it never bloats the shim binary.
//!
//! # Detection signals (verified on disk)
//!
//! A plugin is **Anthropic-native** iff ALL of:
//! 1. Its marketplace resolves — via `~/.claude/plugins/known_marketplaces.json`
//!    — to `source.repo == "anthropics/claude-plugins-official"`, AND
//! 2. Its entry in that marketplace's `.claude-plugin/marketplace.json` has a
//!    **relative** `source` (a `"./plugins/{name}"` string, not an object with a
//!    `url`), AND
//! 3. Its author is Anthropic (`author.name == "Anthropic"`).
//!
//! Everything else under `~/.claude/` (user-authored skills, user-added
//! `mcpServers`, third-party / `external_plugins`, non-official marketplaces) is
//! **custom** and eligible for adoption.
//!
//! MCP servers live as JSON keys in `~/.claude.json`; they cannot carry a
//! sidecar marker, so for them the canonical VectorHawk key *is* the marker: the
//! only VectorHawk-owned key is `vectorhawk`; every other key is custom.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;

// ── Canonical constants (single source of truth) ────────────────────────────────

/// Sidecar file written next to every VectorHawk-managed skill/plugin directory.
pub const MANAGED_MARKER_FILENAME: &str = ".vectorhawk-managed.json";

/// Subdirectory names under `~/.claude/plugins/` that are Anthropic plumbing and
/// must never be touched by the reconciler.
pub const EXCLUDED_PLUGIN_DIRS: &[&str] = &["marketplaces", "cache", "data"];

/// The only `~/.claude.json` `mcpServers` key that VectorHawk owns.
pub const VECTORHAWK_MCP_KEY: &str = "vectorhawk";

/// The GitHub repo slug that uniquely identifies Anthropic's official plugin
/// marketplace in `known_marketplaces.json`.
pub const ANTHROPIC_OFFICIAL_REPO: &str = "anthropics/claude-plugins-official";

/// Author name that marks a plugin as Anthropic first-party.
pub const ANTHROPIC_AUTHOR_NAME: &str = "Anthropic";

// ── Origin ──────────────────────────────────────────────────────────────────────

/// The provenance of a piece of Claude Code content, as far as VectorHawk cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Anthropic first-party content. Strictly out of scope — never touch.
    AnthropicNative,
    /// User / third-party content. Eligible for adoption & governance.
    Custom,
    /// Already adopted and owned by VectorHawk (has a marker).
    VectorHawkManaged,
}

// ── Path helpers ────────────────────────────────────────────────────────────────

/// Resolve `~/.claude/`.
pub fn claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Resolve `~/.claude/skills/`.
pub fn claude_skills_dir() -> Option<PathBuf> {
    claude_dir().map(|c| c.join("skills"))
}

/// Resolve `~/.claude/plugins/`.
pub fn claude_plugins_dir() -> Option<PathBuf> {
    claude_dir().map(|c| c.join("plugins"))
}

/// Resolve `~/.agents/` — the cross-agent config root shared by Cursor,
/// Codex, and Gemini CLI.
pub fn agents_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agents"))
}

/// Resolve `~/.agents/skills/` — the canonical location for VectorHawk-managed
/// skills. Cursor, Codex, and Gemini CLI all scan this natively; Claude Code
/// does not, and receives a symlink under `~/.claude/skills/` instead.
pub fn agents_skills_dir() -> Option<PathBuf> {
    agents_dir().map(|a| a.join("skills"))
}

/// Resolve `~/.claude/plugins/known_marketplaces.json`.
pub fn known_marketplaces_path() -> Option<PathBuf> {
    claude_plugins_dir().map(|p| p.join("known_marketplaces.json"))
}

/// Resolve `~/.claude.json`.
pub fn claude_json_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.json"))
}

// ── Marker / VectorHawk-managed ─────────────────────────────────────────────────

/// True iff `dir` contains a `.vectorhawk-managed.json` marker — the single
/// positive test for "VectorHawk owns this directory". Filesystem is the source
/// of truth; this does not consult SQLite.
pub fn is_vectorhawk_managed(dir: &Path) -> bool {
    dir.join(MANAGED_MARKER_FILENAME).exists()
}

/// True if `dir_name` is an Anthropic plumbing directory under `~/.claude/plugins/`.
pub fn is_excluded_plugin_dir(dir_name: &str) -> bool {
    EXCLUDED_PLUGIN_DIRS.contains(&dir_name)
}

/// True iff `key` is the VectorHawk-owned `mcpServers` key.
pub fn is_vectorhawk_mcp_key(key: &str) -> bool {
    key == VECTORHAWK_MCP_KEY
}

// ── known_marketplaces.json parsing ─────────────────────────────────────────────

/// Read `known_marketplaces.json` and return it as a JSON object, or `None` if
/// missing / unreadable / malformed (fail-closed: callers treat the absence of a
/// confirmed Anthropic repo as "not provably native", except the mutation guard
/// which fails safe — see [`is_anthropic_native_path`]).
pub fn read_known_marketplaces() -> Option<serde_json::Map<String, Value>> {
    read_known_marketplaces_at(&known_marketplaces_path()?)
}

/// Read a specific `known_marketplaces.json` path. Keeps classification
/// self-consistent with a caller-supplied plugins dir (and testable). Returns
/// `None` if missing / unreadable / malformed.
pub fn read_known_marketplaces_at(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let bytes = std::fs::read(path).ok()?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    root.as_object().cloned()
}

/// Return the `source.repo` for a marketplace entry, if present.
pub fn marketplace_repo(entry: &Value) -> Option<&str> {
    entry.get("source")?.get("repo")?.as_str()
}

/// Return the `installLocation` path for a marketplace entry, if present.
pub fn marketplace_install_location(entry: &Value) -> Option<PathBuf> {
    entry
        .get("installLocation")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
}

/// True if the named marketplace, within the given `known_marketplaces` map,
/// resolves to Anthropic's official repo. Pure — no filesystem access.
pub fn is_official_marketplace_in(
    name: &str,
    marketplaces: &serde_json::Map<String, Value>,
) -> bool {
    marketplaces
        .get(name)
        .and_then(marketplace_repo)
        .map(|repo| repo == ANTHROPIC_OFFICIAL_REPO)
        .unwrap_or(false)
}

/// True if the named marketplace resolves to Anthropic's official repo, reading
/// `known_marketplaces.json` from disk. Returns `false` if the file is absent.
pub fn is_official_marketplace(name: &str) -> bool {
    read_known_marketplaces()
        .map(|m| is_official_marketplace_in(name, &m))
        .unwrap_or(false)
}

// ── marketplace.json plugin-entry parsing ───────────────────────────────────────

/// True if a marketplace `plugins[]` entry's `source` is a **relative path**
/// string (e.g. `"./plugins/foo"`) — the shape Anthropic uses for its own
/// bundled plugins. An object `source` (with a `url`) denotes an external /
/// third-party plugin and is never native.
pub fn plugin_source_is_relative(entry: &Value) -> bool {
    match entry.get("source") {
        Some(Value::String(s)) => s.starts_with("./") || s.starts_with("../"),
        _ => false,
    }
}

/// True if the `author.name` of a marketplace entry or a `plugin.json` value is
/// exactly `"Anthropic"`.
pub fn author_is_anthropic(value: &Value) -> bool {
    value
        .get("author")
        .and_then(|a| a.get("name"))
        .and_then(|n| n.as_str())
        .map(|n| n == ANTHROPIC_AUTHOR_NAME)
        .unwrap_or(false)
}

/// Classify a single plugin, given its marketplace name, its entry in that
/// marketplace's `marketplace.json` `plugins[]` array, and (optionally) its
/// on-disk directory for a marker check.
///
/// Order of precedence:
/// 1. A VectorHawk marker in `plugin_dir` → [`Origin::VectorHawkManaged`].
/// 2. Official marketplace **and** relative source **and** Anthropic author →
///    [`Origin::AnthropicNative`].
/// 3. Otherwise → [`Origin::Custom`].
pub fn classify_plugin(marketplace_name: &str, entry: &Value, plugin_dir: Option<&Path>) -> Origin {
    let marker_present = plugin_dir.map(is_vectorhawk_managed).unwrap_or(false);
    classify_plugin_with(
        is_official_marketplace(marketplace_name),
        entry,
        marker_present,
    )
}

/// Pure classifier core — no filesystem access. See [`classify_plugin`].
pub fn classify_plugin_with(is_official: bool, entry: &Value, marker_present: bool) -> Origin {
    if marker_present {
        return Origin::VectorHawkManaged;
    }
    if is_official && plugin_source_is_relative(entry) && author_is_anthropic(entry) {
        return Origin::AnthropicNative;
    }
    Origin::Custom
}

// ── Anthropic-native path guard ─────────────────────────────────────────────────

/// True if `path` lies within Anthropic's own protected first-party tree:
/// the internal `plugins/` subtree of an official marketplace, or the
/// `cache`/`data` plumbing dirs under `~/.claude/plugins/`.
///
/// **Fail-safe:** if `known_marketplaces.json` cannot be read, any path under
/// `~/.claude/plugins/marketplaces/` is treated as native (we would rather
/// refuse a mutation than risk touching Anthropic content).
pub fn is_anthropic_native_path(path: &Path) -> bool {
    let Some(plugins_dir) = claude_plugins_dir() else {
        return false;
    };
    is_anthropic_native_in(path, &plugins_dir, read_known_marketplaces().as_ref())
}

/// Pure core of [`is_anthropic_native_path`]: decide nativeness given the
/// plugins dir and an optional `known_marketplaces` map (no filesystem access).
pub fn is_anthropic_native_in(
    path: &Path,
    plugins_dir: &Path,
    marketplaces: Option<&serde_json::Map<String, Value>>,
) -> bool {
    // cache/data plumbing under ~/.claude/plugins/ is always native.
    for dir in ["cache", "data"] {
        if path.starts_with(plugins_dir.join(dir)) {
            return true;
        }
    }

    let marketplaces_dir = plugins_dir.join("marketplaces");
    if !path.starts_with(&marketplaces_dir) {
        return false;
    }

    match marketplaces {
        Some(map) => {
            for entry in map.values() {
                let is_official =
                    marketplace_repo(entry).map(|r| r == ANTHROPIC_OFFICIAL_REPO) == Some(true);
                if !is_official {
                    continue;
                }
                if let Some(loc) = marketplace_install_location(entry) {
                    // Protect the internal `plugins/` subtree; `external_plugins/`
                    // within the same marketplace is third-party and adoptable.
                    if path.starts_with(loc.join("plugins")) {
                        return true;
                    }
                }
            }
            false
        }
        None => {
            // Fail-safe: can't prove otherwise, so protect the whole marketplaces tree.
            warn!(
                path = %path.display(),
                "ownership: known_marketplaces.json unreadable — treating marketplace path as native (fail-safe)"
            );
            true
        }
    }
}

/// Guard for mutation sites under `~/.claude/`: returns `Err` if `path` is
/// Anthropic-native content that VectorHawk must never touch.
pub fn ensure_not_native(path: &Path) -> anyhow::Result<()> {
    if is_anthropic_native_path(path) {
        anyhow::bail!(
            "refusing to mutate Anthropic-native path (out of scope): {}",
            path.display()
        );
    }
    Ok(())
}

// ── ~/.claude.json mcpServers ────────────────────────────────────────────────────

/// Read `~/.claude.json` and return the set of **native** (non-`vectorhawk`)
/// `mcpServers` keys.
///
/// These keys represent MCP backends the AI client reaches directly, so the
/// aggregator must not also surface them under its own namespace. Fail-open:
/// returns an empty set if the file is missing or malformed, so the shim is
/// never blocked from serving `tools/list`.
pub fn native_mcp_keys_from_claude_json() -> HashSet<String> {
    match claude_json_path() {
        Some(path) => native_mcp_keys_at(&path),
        None => HashSet::new(),
    }
}

/// Read a specific `claude.json` path and return its native (non-`vectorhawk`)
/// `mcpServers` keys. Fail-open: empty set on missing/malformed. See
/// [`native_mcp_keys_from_claude_json`].
pub fn native_mcp_keys_at(claude_json: &Path) -> HashSet<String> {
    let Ok(bytes) = std::fs::read(claude_json) else {
        return HashSet::new();
    };
    let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    if let Some(map) = root.get("mcpServers").and_then(|v| v.as_object()) {
        for key in map.keys() {
            if !is_vectorhawk_mcp_key(key) {
                out.insert(key.clone());
            }
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod ownership_tests;
