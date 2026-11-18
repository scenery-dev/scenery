//! Managed-path marker: read/write `.scenery-managed.json` for skills and
//! plugins, and read/write `managed_path_markers` SQLite rows for MCP entries.
//!
//! For skills and plugins the marker lives next to the skill/plugin directory
//! as a sidecar file.  For MCP servers (which are entries inside a JSON file)
//! we can't write a sidecar — the marker lives exclusively in SQLite.

use anyhow::{Context, Result};
use rusqlite::Connection;
use scenery_mcp::ownership::MANAGED_MARKER_FILENAME;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::warn;

/// Current schema version for `.scenery-managed.json`.
pub const MARKER_FILE_VERSION: u32 = 1;

/// Contents of `.scenery-managed.json` written next to each managed
/// skill/plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedMarkerFile {
    pub marker_version: u32,
    pub installation_id: Option<String>,
    pub source_sha256: String,
    pub migrated_at: String,
    /// `"auto_managed"` for a skill Scenery converted from the user's own
    /// copy; absent for catalog installs and markers older than 2.3.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Registry text id (`<owner_namespace>~<slug>` for a private skill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// Stable registry identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_uuid: Option<String>,
    /// Whole-tree hash of what Scenery wrote (see `scenery_core::skill_tree`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_version: Option<u32>,
}

/// Origin value for a skill Scenery converted from the user's own copy.
pub const ORIGIN_AUTO_MANAGED: &str = "auto_managed";

/// Provenance carried beside a marker: where the skill came from and what
/// Scenery last wrote. All-`None` is a legacy/catalog marker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub origin: Option<String>,
    pub skill_id: Option<String>,
    pub skill_uuid: Option<String>,
    pub tree_hash: Option<String>,
    pub hash_version: Option<u32>,
}

impl Provenance {
    pub fn is_auto_managed(&self) -> bool {
        self.origin.as_deref() == Some(ORIGIN_AUTO_MANAGED)
    }
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

/// Write `.scenery-managed.json` into `dir_path`.
pub fn write_file_marker(
    dir_path: &Path,
    installation_id: Option<&str>,
    source_sha256: &str,
    migrated_at: &str,
) -> Result<()> {
    write_file_marker_with(
        dir_path,
        installation_id,
        source_sha256,
        migrated_at,
        &Provenance::default(),
    )
}

/// [`write_file_marker`] with provenance (`origin`, ids, `tree_hash`).
pub fn write_file_marker_with(
    dir_path: &Path,
    installation_id: Option<&str>,
    source_sha256: &str,
    migrated_at: &str,
    prov: &Provenance,
) -> Result<()> {
    let marker = ManagedMarkerFile {
        marker_version: MARKER_FILE_VERSION,
        installation_id: installation_id.map(str::to_string),
        source_sha256: source_sha256.to_string(),
        migrated_at: migrated_at.to_string(),
        origin: prov.origin.clone(),
        skill_id: prov.skill_id.clone(),
        skill_uuid: prov.skill_uuid.clone(),
        tree_hash: prov.tree_hash.clone(),
        hash_version: prov.hash_version,
    };

    let json = serde_json::to_string_pretty(&marker)
        .context("failed to serialise .scenery-managed.json")?;

    let marker_path = dir_path.join(MANAGED_MARKER_FILENAME);
    std::fs::write(&marker_path, json)
        .with_context(|| format!("failed to write marker file: {}", marker_path.display()))?;

    Ok(())
}

/// Read `.scenery-managed.json` from `dir_path`.
///
/// Returns `None` if the file does not exist (not yet managed).
/// Returns `Err` only for I/O or JSON parse errors; a version mismatch is
/// tolerated and returned as `Ok(Some(...))` with the raw struct so the caller
/// can decide.
pub fn read_file_marker(dir_path: &Path) -> Result<Option<ManagedMarkerFile>> {
    let marker_path = dir_path.join(MANAGED_MARKER_FILENAME);
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
                "managed_paths/marker: malformed .scenery-managed.json — treating as absent"
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

/// Read the provenance columns of the marker row at exactly `path`.
/// Missing row, or a row written before 2.3.0, yields the default (legacy).
pub fn marker_provenance(conn: &Connection, path: &str) -> Provenance {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT origin, skill_id, skill_uuid, tree_hash, hash_version \
         FROM managed_path_markers WHERE path = ?1",
        [path],
        |row| {
            Ok(Provenance {
                origin: row.get(0)?,
                skill_id: row.get(1)?,
                skill_uuid: row.get(2)?,
                tree_hash: row.get(3)?,
                hash_version: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
            })
        },
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// Write the provenance columns of the marker row at `path` (the row must
/// exist; insert it first).
pub fn set_marker_provenance(conn: &Connection, path: &str, prov: &Provenance) -> Result<()> {
    conn.execute(
        "UPDATE managed_path_markers \
         SET origin = ?1, skill_id = ?2, skill_uuid = ?3, tree_hash = ?4, hash_version = ?5 \
         WHERE path = ?6",
        rusqlite::params![
            prov.origin,
            prov.skill_id,
            prov.skill_uuid,
            prov.tree_hash,
            prov.hash_version.map(i64::from),
            path
        ],
    )
    .context("failed to write managed_path_markers provenance")?;
    Ok(())
}

/// Is the skill directory `dir` an auto-managed one according to the
/// authoritative DB row (never the marker file)? Compares canonicalised
/// paths like [`tracked_marker_for_path`].
pub fn is_auto_managed_path(conn: &Connection, dir: &Path) -> bool {
    match tracked_marker_for_path(conn, "skill", dir) {
        Ok(Some(row)) => marker_provenance(conn, &row.path).is_auto_managed(),
        _ => false,
    }
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

// ── DB-backed ownership (F13) ─────────────────────────────────────────────────

/// Whether a skill/plugin directory is Scenery-managed according to the
/// authoritative `managed_path_markers` table.
///
/// The `.scenery-managed.json` sidecar is deliberately NOT consulted: any
/// skill author can ship one. Only a row Scenery itself wrote, at this
/// exact canonical path, counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbOwnership {
    /// A row exists at this canonical path and the on-disk content hash
    /// matches it.
    Verified,
    /// A row exists at this canonical path but the content no longer matches
    /// its hash (edited, replaced, or half-written).
    HashMismatch,
    /// A row with `origin = 'auto_managed'` whose content differs from what
    /// Scenery wrote: the user kept editing their own skill. A normal edit,
    /// never `managed_hash_mismatch` and never held (plan 4.5.4); drift
    /// reports it.
    EditedOwn,
    /// A row with no provenance at all (`origin`, `skill_id`, `tree_hash` all
    /// empty): written by an older runner's adopt/migrate pass for a skill
    /// another installer owns. It proves neither that Scenery manages the
    /// skill nor that anyone tampered with it, so it is ignored (and cleaned
    /// up by [`drop_legacy_row`]). Scenery's own writes always carry a tree
    /// hash.
    Legacy { row_path: String },
    /// No row at this path — not ours, whatever marker file it carries.
    Untracked,
}

fn is_blank(v: &Option<String>) -> bool {
    v.as_deref().map_or(true, |s| s.trim().is_empty())
}

/// True for a pre-provenance row: no origin, no skill id, no tree hash.
pub fn is_legacy_provenance(prov: &Provenance) -> bool {
    is_blank(&prov.origin) && is_blank(&prov.skill_id) && is_blank(&prov.tree_hash)
}

/// Delete a legacy marker row, and the `.scenery-managed.json` sidecar beside
/// the skill only if it too carries no provenance. Best effort.
pub fn drop_legacy_row(conn: &Connection, row_path: &str, skill_dir: &Path) {
    if !is_legacy_provenance(&marker_provenance(conn, row_path)) {
        return;
    }
    if let Err(e) = conn.execute(
        "DELETE FROM managed_path_markers WHERE path = ?1 AND kind = 'skill'",
        [row_path],
    ) {
        warn!(path = row_path, error = %e, "managed_paths/marker: could not drop legacy row");
        return;
    }
    if let Ok(Some(f)) = read_file_marker(skill_dir) {
        if is_blank(&f.origin) && is_blank(&f.skill_id) && is_blank(&f.tree_hash) {
            let _ = std::fs::remove_file(skill_dir.join(MANAGED_MARKER_FILENAME));
        }
    }
}

/// Return the `kind` row whose path is `dir`, comparing canonicalised paths so
/// a symlink (e.g. `~/.claude/skills/<slug>` -> `~/.agents/skills/<slug>`) or
/// a `/var` vs `/private/var` spelling resolves to the same row. Slug is never
/// used as the key.
pub fn tracked_marker_for_path(
    conn: &Connection,
    kind: &str,
    dir: &Path,
) -> Result<Option<ManagedPathMarker>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, kind, slug, installation_id, source_sha256, migrated_at \
             FROM managed_path_markers WHERE kind = ?1",
        )
        .context("failed to prepare managed_path_markers lookup")?;
    let rows = stmt
        .query_map([kind], |row| {
            Ok(ManagedPathMarker {
                path: row.get(0)?,
                kind: row.get(1)?,
                slug: row.get(2)?,
                installation_id: row.get(3)?,
                source_sha256: row.get(4)?,
                migrated_at: row.get(5)?,
            })
        })
        .context("failed to query managed_path_markers")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read managed_path_markers rows")?;

    let raw = dir.to_string_lossy();
    if let Some(row) = rows.iter().find(|r| r.path == raw) {
        return Ok(Some(row.clone()));
    }
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return Ok(None);
    };
    Ok(rows.into_iter().find(|r| {
        std::fs::canonicalize(&r.path)
            .map(|p| p == canonical)
            .unwrap_or(false)
    }))
}

/// Classify `dir` against `managed_path_markers` — see [`DbOwnership`].
/// A DB error is treated as [`DbOwnership::Untracked`] (fail-open towards
/// reporting/adopting, never towards hiding).
pub fn db_ownership(conn: &Connection, kind: &str, dir: &Path) -> DbOwnership {
    let row = match tracked_marker_for_path(conn, kind, dir) {
        Ok(Some(row)) => row,
        Ok(None) => return DbOwnership::Untracked,
        Err(e) => {
            warn!(path = %dir.display(), error = %e, "managed_paths/marker: ownership lookup failed");
            return DbOwnership::Untracked;
        }
    };
    if is_legacy_provenance(&marker_provenance(conn, &row.path)) {
        return DbOwnership::Legacy { row_path: row.path };
    }
    match super::drift::current_hash_for(&row) {
        Ok(Some(h)) if h == row.source_sha256 => DbOwnership::Verified,
        _ if marker_provenance(conn, &row.path).is_auto_managed() => DbOwnership::EditedOwn,
        _ => DbOwnership::HashMismatch,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "marker_tests.rs"]
mod marker_tests;
