//! Managed-path marker: read/write `.vectorhawk-managed.json` for skills and
//! plugins, and read/write `managed_path_markers` SQLite rows for MCP entries.
//!
//! For skills and plugins the marker lives next to the skill/plugin directory
//! as a sidecar file.  For MCP servers (which are entries inside a JSON file)
//! we can't write a sidecar — the marker lives exclusively in SQLite.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::warn;

/// Current schema version for `.vectorhawk-managed.json`.
pub const MARKER_FILE_VERSION: u32 = 1;

/// Contents of `.vectorhawk-managed.json` written next to each managed
/// skill/plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedMarkerFile {
    pub marker_version: u32,
    pub installation_id: Option<String>,
    pub source_sha256: String,
    pub migrated_at: String,
}

/// Row in the `managed_path_markers` SQLite table.
#[derive(Debug, Clone)]
pub struct ManagedPathMarker {
    /// Absolute path of the managed resource (or virtual `<path>:<key>` for MCP).
    pub path: String,
    /// `"skill"`, `"plugin"`, or `"mcp"`.
    pub kind: String,
    pub slug: String,
    /// Backend's row ID — nullable until backend confirms.
    pub installation_id: Option<String>,
    pub source_sha256: String,
    pub migrated_at: String,
}

// ── File marker (skill / plugin) ──────────────────────────────────────────────

/// Write `.vectorhawk-managed.json` into `dir_path`.
pub fn write_file_marker(
    dir_path: &Path,
    installation_id: Option<&str>,
    source_sha256: &str,
    migrated_at: &str,
) -> Result<()> {
    let marker = ManagedMarkerFile {
        marker_version: MARKER_FILE_VERSION,
        installation_id: installation_id.map(str::to_string),
        source_sha256: source_sha256.to_string(),
        migrated_at: migrated_at.to_string(),
    };

    let json = serde_json::to_string_pretty(&marker)
        .context("failed to serialise .vectorhawk-managed.json")?;

    let marker_path = dir_path.join(".vectorhawk-managed.json");
    std::fs::write(&marker_path, json)
        .with_context(|| format!("failed to write marker file: {}", marker_path.display()))?;

    Ok(())
}

/// Read `.vectorhawk-managed.json` from `dir_path`.
///
/// Returns `None` if the file does not exist (not yet managed).
/// Returns `Err` only for I/O or JSON parse errors; a version mismatch is
/// tolerated and returned as `Ok(Some(...))` with the raw struct so the caller
/// can decide.
pub fn read_file_marker(dir_path: &Path) -> Result<Option<ManagedMarkerFile>> {
    let marker_path = dir_path.join(".vectorhawk-managed.json");
    if !marker_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read(&marker_path)
        .with_context(|| format!("failed to read marker file: {}", marker_path.display()))?;

    match serde_json::from_slice::<ManagedMarkerFile>(&content) {
        Ok(m) => Ok(Some(m)),
        Err(e) => {
            warn!(
                path = %marker_path.display(),
                error = %e,
                "managed_paths/marker: malformed .vectorhawk-managed.json — treating as absent"
            );
            Ok(None)
        }
    }
}

// ── SQLite marker ─────────────────────────────────────────────────────────────

/// Insert or ignore a `managed_path_markers` row.
///
/// Uses `INSERT OR IGNORE` so this is idempotent on the primary key (`path`).
pub fn insert_db_marker(conn: &Connection, marker: &ManagedPathMarker) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO managed_path_markers \
         (path, kind, slug, installation_id, source_sha256, migrated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            marker.path,
            marker.kind,
            marker.slug,
            marker.installation_id,
            marker.source_sha256,
            marker.migrated_at,
        ],
    )
    .context("failed to insert managed_path_markers row")?;
    Ok(())
}

/// Update `installation_id` for an existing marker row.
pub fn update_db_marker_installation_id(
    conn: &Connection,
    path: &str,
    installation_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE managed_path_markers SET installation_id = ?1 WHERE path = ?2",
        rusqlite::params![installation_id, path],
    )
    .context("failed to update managed_path_markers installation_id")?;
    Ok(())
}

/// Return `true` if `path` already has a row in `managed_path_markers`.
pub fn is_already_marked(conn: &Connection, path: &str) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let result = conn
        .query_row(
            "SELECT 1 FROM managed_path_markers WHERE path = ?1",
            [path],
            |_row| Ok(()),
        )
        .optional()
        .context("failed to query managed_path_markers")?;
    Ok(result.is_some())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "marker_tests.rs"]
mod marker_tests;
