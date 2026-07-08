//! Filesystem scanner: enumerate skills, plugins, and MCP servers.
//!
//! Each scanner function returns a `Vec<MigrationItem>` describing what exists
//! on disk.  Malformed entries are logged at WARN and skipped so a single bad
//! directory does not abort the whole scan.

use super::ItemKind;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::warn;
use vectorhawkd_mcp::ownership::{self, is_excluded_plugin_dir, is_vectorhawk_mcp_key};

/// One discovered item ready for migration.
#[derive(Debug, Clone)]
pub struct MigrationItem {
    pub kind: ItemKind,
    /// Human-friendly identifier (dir name for skill/plugin, key for MCP).
    pub slug: String,
    /// Absolute path of the item's root directory (skill/plugin) or the
    /// virtual key path used for MCP entries.
    pub source_path: PathBuf,
    /// All files that contribute to `canonical_hash` (non-empty).
    pub files: Vec<PathBuf>,
    /// SHA-256 of the canonical content, hex-encoded.
    pub canonical_hash: String,
    /// Serialised payload sent to the backend's migrate endpoint.
    pub payload: serde_json::Value,
}

// ── Skills ────────────────────────────────────────────────────────────────────

/// Scan `~/.claude/skills/` and return one `MigrationItem` per well-formed dir.
///
/// A skill is "well-formed" if its immediate child is a directory containing
/// `SKILL.md`. Missing `SKILL.md` → logged at WARN, skipped.
pub fn scan_skills_dir(skills_dir: &Path) -> Result<Vec<MigrationItem>> {
    if !skills_dir.exists() {
        return Ok(vec![]);
    }

    let entries = fs::read_dir(skills_dir)
        .with_context(|| format!("failed to read skills dir: {}", skills_dir.display()))?;

    let mut items = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "managed_paths/scanner: error reading skills dir entry");
                continue;
            }
        };

        let path = entry.path();

        // Only process directories.
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "managed_paths/scanner: cannot stat skills entry");
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => {
                warn!(path = %path.display(), "managed_paths/scanner: non-UTF-8 skill dir name — skipping");
                continue;
            }
        };

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            warn!(
                slug = %slug,
                "managed_paths/scanner: skill dir missing SKILL.md — skipping"
            );
            continue;
        }

        let skill_md_content = match fs::read(&skill_md) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(slug = %slug, error = %e, "managed_paths/scanner: cannot read SKILL.md — skipping");
                continue;
            }
        };

        let canonical_hash = sha256_bytes(&skill_md_content);

        let payload = serde_json::json!({
            "skill_md": String::from_utf8_lossy(&skill_md_content),
        });

        items.push(MigrationItem {
            kind: ItemKind::Skill,
            slug,
            source_path: path,
            files: vec![skill_md],
            canonical_hash,
            payload,
        });
    }

    Ok(items)
}

// ── Plugins ───────────────────────────────────────────────────────────────────

/// Scan `~/.claude/plugins/` and return one `MigrationItem` per user-installed plugin.
///
/// Excluded dirs (`marketplaces`, `cache`, `data`) are skipped silently.
/// Plugin dirs missing `.claude-plugin/plugin.json` are logged at WARN and skipped.
pub fn scan_plugins_dir(plugins_dir: &Path) -> Result<Vec<MigrationItem>> {
    if !plugins_dir.exists() {
        return Ok(vec![]);
    }

    let entries = fs::read_dir(plugins_dir)
        .with_context(|| format!("failed to read plugins dir: {}", plugins_dir.display()))?;

    let mut items = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "managed_paths/scanner: error reading plugins dir entry");
                continue;
            }
        };

        let path = entry.path();

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "managed_paths/scanner: cannot stat plugins entry");
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => {
                warn!(path = %path.display(), "managed_paths/scanner: non-UTF-8 plugin dir name — skipping");
                continue;
            }
        };

        // Skip Anthropic plumbing dirs.
        if is_excluded_plugin_dir(&slug) {
            continue;
        }

        // Claude Code stores the plugin manifest at `.claude-plugin/plugin.json`
        // inside each plugin's top-level directory (confirmed on disk).
        let manifest_path = path.join(".claude-plugin").join("plugin.json");
        if !manifest_path.exists() {
            warn!(
                slug = %slug,
                "managed_paths/scanner: plugin dir missing .claude-plugin/plugin.json — skipping"
            );
            continue;
        }

        let manifest_bytes = match fs::read(&manifest_path) {
            Ok(b) => b,
            Err(e) => {
                warn!(slug = %slug, error = %e, "managed_paths/scanner: cannot read plugin manifest — skipping");
                continue;
            }
        };

        let canonical_hash = sha256_bytes(&manifest_bytes);

        let manifest_value: serde_json::Value = match serde_json::from_slice(&manifest_bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(slug = %slug, error = %e, "managed_paths/scanner: plugin manifest not valid JSON — skipping");
                continue;
            }
        };

        let payload = serde_json::json!({ "manifest_json": manifest_value });

        items.push(MigrationItem {
            kind: ItemKind::Plugin,
            slug,
            source_path: path,
            files: vec![manifest_path],
            canonical_hash,
            payload,
        });
    }

    Ok(items)
}

// ── Plugin marketplaces (nested layout) ─────────────────────────────────────────

/// Walk the Claude Code plugin **marketplace** tree and return one
/// `MigrationItem` per *custom* plugin found on disk.
///
/// Claude Code does not store plugins as immediate children of
/// `~/.claude/plugins/`; they live nested under
/// `~/.claude/plugins/marketplaces/<marketplace>/{plugins,external_plugins}/<plugin>/`.
/// `scan_plugins_dir` only sees the top level (and skips the plumbing dirs), so
/// this function is what actually discovers third-party plugins.
///
/// Skipped:
/// - **Anthropic-native** plugins — the internal `plugins/` subtree of the
///   official marketplace (see [`ownership::is_anthropic_native_in`]).
/// - **Already-managed** plugins — those carrying a `.vectorhawk-managed.json`.
///
/// Classification reads `known_marketplaces.json` from the *passed* `plugins_dir`
/// so it stays self-consistent (and unit-testable) regardless of `$HOME`.
pub fn scan_plugin_marketplaces(plugins_dir: &Path) -> Result<Vec<MigrationItem>> {
    let marketplaces_dir = plugins_dir.join("marketplaces");
    if !marketplaces_dir.exists() {
        return Ok(vec![]);
    }

    let marketplaces =
        ownership::read_known_marketplaces_at(&plugins_dir.join("known_marketplaces.json"));

    let mut items = Vec::new();

    let mkt_entries = fs::read_dir(&marketplaces_dir).with_context(|| {
        format!(
            "failed to read marketplaces dir: {}",
            marketplaces_dir.display()
        )
    })?;

    for mkt in mkt_entries {
        let mkt = match mkt {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "managed_paths/scanner: error reading marketplace entry");
                continue;
            }
        };
        let mkt_path = mkt.path();
        if !mkt_path.is_dir() {
            continue;
        }

        // Plugins live under `plugins/` (bundled) and `external_plugins/` (third-party).
        for sub in ["plugins", "external_plugins"] {
            let sub_dir = mkt_path.join(sub);
            if !sub_dir.exists() {
                continue;
            }
            let plugin_entries = match fs::read_dir(&sub_dir) {
                Ok(e) => e,
                Err(e) => {
                    warn!(path = %sub_dir.display(), error = %e, "managed_paths/scanner: cannot read plugin subdir");
                    continue;
                }
            };

            for pe in plugin_entries {
                let pe = match pe {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let pdir = pe.path();
                if !pdir.is_dir() {
                    continue;
                }

                let slug = match pdir.file_name().and_then(|n| n.to_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        warn!(path = %pdir.display(), "managed_paths/scanner: non-UTF-8 plugin dir name — skipping");
                        continue;
                    }
                };

                let manifest_path = pdir.join(".claude-plugin").join("plugin.json");
                if !manifest_path.exists() {
                    continue;
                }

                // Already ours — nothing to adopt.
                if ownership::is_vectorhawk_managed(&pdir) {
                    continue;
                }
                // Anthropic first-party — strictly out of scope.
                if ownership::is_anthropic_native_in(&pdir, plugins_dir, marketplaces.as_ref()) {
                    continue;
                }

                let manifest_bytes = match fs::read(&manifest_path) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(slug = %slug, error = %e, "managed_paths/scanner: cannot read plugin manifest — skipping");
                        continue;
                    }
                };

                let manifest_value: serde_json::Value = match serde_json::from_slice(
                    &manifest_bytes,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(slug = %slug, error = %e, "managed_paths/scanner: plugin manifest not valid JSON — skipping");
                        continue;
                    }
                };

                let canonical_hash = sha256_bytes(&manifest_bytes);
                let payload = serde_json::json!({ "manifest_json": manifest_value });

                items.push(MigrationItem {
                    kind: ItemKind::Plugin,
                    slug,
                    source_path: pdir,
                    files: vec![manifest_path],
                    canonical_hash,
                    payload,
                });
            }
        }
    }

    Ok(items)
}

// ── MCP servers ───────────────────────────────────────────────────────────────

/// Parse `~/.claude.json` and return one `MigrationItem` per `mcpServers` entry
/// that is not named exactly `"vectorhawk"`.
pub fn scan_claude_json(claude_json: &Path) -> Result<Vec<MigrationItem>> {
    if !claude_json.exists() {
        return Ok(vec![]);
    }

    let content = fs::read(claude_json)
        .with_context(|| format!("failed to read {}", claude_json.display()))?;

    let root: serde_json::Value = serde_json::from_slice(&content)
        .with_context(|| format!("failed to parse JSON from {}", claude_json.display()))?;

    let mcp_servers = match root.get("mcpServers") {
        Some(serde_json::Value::Object(map)) => map.clone(),
        Some(_) => {
            warn!(
                "managed_paths/scanner: mcpServers in ~/.claude.json is not an object — skipping"
            );
            return Ok(vec![]);
        }
        None => return Ok(vec![]),
    };

    let mut items = Vec::new();

    for (key, value) in &mcp_servers {
        // Always skip the VectorHawk aggregator entry.
        if is_vectorhawk_mcp_key(key) {
            continue;
        }

        let payload_bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(e) => {
                warn!(key = %key, error = %e, "managed_paths/scanner: cannot serialise mcpServer entry — skipping");
                continue;
            }
        };

        let canonical_hash = sha256_bytes(&payload_bytes);

        // Construct a virtual path key that the marker table can use as a
        // primary key.  We store the absolute claude.json path + ":<key>".
        let virtual_path = claude_json
            .to_str()
            .map(|s| format!("{s}:{key}"))
            .unwrap_or_else(|| format!("~/.claude.json:{key}"));

        items.push(MigrationItem {
            kind: ItemKind::Mcp,
            slug: key.clone(),
            source_path: PathBuf::from(virtual_path),
            files: vec![claude_json.to_path_buf()],
            canonical_hash,
            payload: serde_json::json!({ "mcp_config": value }),
        });
    }

    Ok(items)
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "scanner_tests.rs"]
mod scanner_tests;
