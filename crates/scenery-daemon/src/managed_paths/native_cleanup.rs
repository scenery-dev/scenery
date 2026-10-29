//! Startup cleanup of bookkeeping for Anthropic-native content.
//!
//! Product rule: Scenery never enumerates, adopts, blocks, modifies,
//! quarantines or removes anything inside Anthropic's official marketplace.
//! Older builds only protected `<installLocation>/plugins/` and adopted the
//! third-party `external_plugins/*` entries. This module undoes that
//! bookkeeping, idempotently:
//!
//! - removes the `.scenery-managed.json` sidecar we wrote (only if its content
//!   proves it is ours — it must parse and match a marker row / journal entry);
//! - deletes the `managed_path_markers` row;
//! - drops the restore-journal entry (nothing to restore: the plugin files
//!   were never changed).
//!
//! No other file in the marketplace is touched.

use anyhow::{Context, Result};
use rusqlite::Connection;
use scenery_core::{restore_journal::RestoreJournal, state::AppState};
use scenery_mcp::ownership::{self, MANAGED_MARKER_FILENAME};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use tracing::info;

use super::marker::read_file_marker;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub sidecars_removed: usize,
    pub marker_rows_removed: usize,
    pub journal_entries_removed: usize,
}

impl CleanupReport {
    pub fn is_noop(&self) -> bool {
        *self == CleanupReport::default()
    }
}

/// Run the cleanup against the real `~/.claude/plugins` tree.
pub fn cleanup_native_markers(state: &AppState) -> Result<CleanupReport> {
    let Some(plugins_dir) = ownership::claude_plugins_dir() else {
        return Ok(CleanupReport::default());
    };
    let marketplaces =
        ownership::read_known_marketplaces_at(&plugins_dir.join("known_marketplaces.json"));
    cleanup_native_markers_in(state, &plugins_dir, marketplaces.as_ref())
}

/// Testable core. If `marketplaces` is `None` (unreadable) nothing is done: the
/// guard would fail safe and call everything native, which is not evidence that
/// a given row is ours to remove.
pub fn cleanup_native_markers_in(
    state: &AppState,
    plugins_dir: &Path,
    marketplaces: Option<&Map<String, Value>>,
) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let Some(marketplaces) = marketplaces else {
        return Ok(report);
    };
    let is_native =
        |p: &Path| ownership::is_anthropic_native_in(p, plugins_dir, Some(marketplaces));

    // Hashes that identify a sidecar as ours, keyed by directory.
    let mut ours: Vec<(PathBuf, Vec<String>)> = Vec::new();

    // ── marker rows ───────────────────────────────────────────────────────────
    let conn = Connection::open(state.db_path.as_std_path())
        .context("native cleanup: failed to open state DB")?;
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT path, kind, slug, source_sha256 FROM managed_path_markers \
             WHERE kind IN ('skill','plugin')",
        )?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    for (path, kind, slug, sha) in rows {
        let dir = PathBuf::from(&path);
        if !is_native(&dir) {
            continue;
        }
        ours.push((dir, vec![sha]));
        conn.execute(
            "DELETE FROM managed_path_markers WHERE path = ?1",
            rusqlite::params![path],
        )?;
        report.marker_rows_removed += 1;
        info!(%path, %kind, %slug, "native cleanup: removed marker row for Anthropic-native path");
    }

    // ── journal entries ───────────────────────────────────────────────────────
    let journal = RestoreJournal::for_state(state);
    let removed = journal.remove_where(|e| is_native(Path::new(&e.target_path)))?;
    for e in &removed {
        info!(
            target_path = %e.target_path,
            slug = e.slug.as_deref().unwrap_or(""),
            "native cleanup: dropped restore-journal entry for Anthropic-native path"
        );
        let hash = e
            .detail
            .get("canonical_hash")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        ours.push((PathBuf::from(&e.target_path), hash.into_iter().collect()));
    }
    report.journal_entries_removed = removed.len();

    // ── sidecars ──────────────────────────────────────────────────────────────
    let mut merged: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for (dir, hashes) in ours {
        match merged.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, h)) => h.extend(hashes),
            None => merged.push((dir, hashes)),
        }
    }
    for (dir, hashes) in merged {
        let sidecar = dir.join(MANAGED_MARKER_FILENAME);
        let is_plain_file = std::fs::symlink_metadata(&sidecar)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_plain_file {
            continue;
        }
        // Ours only if it parses as a Scenery marker AND carries a hash we
        // recorded for this directory (a plugin could ship its own sidecar).
        let matches_ours = match read_file_marker(&dir) {
            Ok(Some(m)) => hashes.contains(&m.source_sha256),
            _ => false,
        };
        if !matches_ours {
            info!(path = %sidecar.display(), "native cleanup: sidecar is not ours — left in place");
            continue;
        }
        std::fs::remove_file(&sidecar)
            .with_context(|| format!("native cleanup: failed to remove {}", sidecar.display()))?;
        report.sidecars_removed += 1;
        info!(path = %sidecar.display(), "native cleanup: removed our sidecar from Anthropic-native path");
    }

    Ok(report)
}

#[cfg(test)]
#[path = "native_cleanup_tests.rs"]
mod tests;
