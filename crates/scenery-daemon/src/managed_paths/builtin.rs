//! The built-in skill `authoring-with-scenery` (plan 4.7).
//!
//! A `SKILL.md` embedded in the binary, written to
//! `~/.agents/skills/authoring-with-scenery/` at daemon start and rewritten
//! when its content differs (a runner upgrade, or the user edited the copy),
//! and linked for Claude Code like any skill. It tells the AI client to use
//! `scenery_create_skill` / `scenery_check_skill` / `scenery_share_skill` for
//! skills and the catalog/request tools for MCP servers.
//!
//! It is **Scenery-owned**: its `managed_path_markers` row has
//! `origin = 'builtin'` (ownership is the DB row, never the file marker, F13).
//! Discovery, the classifier, auto-manage, drift (a mismatch re-writes, never
//! branches or alerts), upload and Share all skip it, and it never gets a
//! registry row. A whole-product `scenery uninstall` removes it like any
//! marked skill; the daemon only re-creates it at the next start.

use super::{
    marker::{marker_provenance, set_marker_provenance, ORIGIN_BUILTIN},
    pusher::{reconciler_disabled, ManagedPathsPusher},
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use scenery_core::state::AppState;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Reserved slug of the built-in skill. The backend refuses it for private
/// registration and for catalog names.
pub const BUILTIN_SLUG: &str = scenery_core::authoring::BUILTIN_SLUG;

/// The embedded `SKILL.md`.
pub const BUILTIN_SKILL_MD: &str = include_str!("../../assets/authoring-with-scenery/SKILL.md");

/// What [`ensure_builtin_skill`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinOutcome {
    /// Nothing to do: content, marker row and link were already right.
    Unchanged,
    /// Written (first start, an upgrade, or a modified copy).
    Written,
    /// Not written: the reconciler is disabled.
    Disabled,
    /// Not written: an unmarked directory (the user's own) holds the slug.
    Conflict(PathBuf),
}

fn canonical_dir() -> Option<PathBuf> {
    super::paths::agents_skills_dir().map(|d| d.join(BUILTIN_SLUG))
}

fn embedded_hash() -> String {
    hex::encode(Sha256::digest(BUILTIN_SKILL_MD.as_bytes()))
}

/// Does the on-disk copy exactly match the embedded one, as a Scenery-owned
/// row, with an intact Claude link?
fn is_current(state: &AppState, dir: &Path) -> bool {
    let Ok(conn) = Connection::open(&state.db_path) else {
        return false;
    };
    let key = dir.to_string_lossy();
    if !marker_provenance(&conn, &key).is_builtin() {
        return false;
    }
    match std::fs::read(dir.join("SKILL.md")) {
        Ok(b) if hex::encode(Sha256::digest(&b)) == embedded_hash() => {}
        _ => return false,
    }
    // Exactly the skill file and Scenery's marker: anything else was planted.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name != "SKILL.md" && name != scenery_mcp::ownership::MANAGED_MARKER_FILENAME {
            return false;
        }
    }
    matches!(
        super::pusher::projection_state(BUILTIN_SLUG),
        Ok(super::pusher::ProjectionState::Intact)
    )
}

/// Write the built-in skill if it is missing or differs. Idempotent; safe at
/// every daemon start.
pub fn ensure_builtin_skill(state: &AppState) -> Result<BuiltinOutcome> {
    if reconciler_disabled() {
        return Ok(BuiltinOutcome::Disabled);
    }
    let dir = canonical_dir().context("builtin skill: home directory not resolvable")?;
    if is_current(state, &dir) {
        return Ok(BuiltinOutcome::Unchanged);
    }
    // A real directory with no Scenery marker is the user's own. Never touch it.
    if (dir.is_dir() && !dir.is_symlink() || dir.is_symlink())
        && !scenery_mcp::ownership::is_scenery_managed(&dir)
    {
        warn!(path = %dir.display(), "builtin skill: an unmanaged directory holds the reserved slug; not writing");
        return Ok(BuiltinOutcome::Conflict(dir));
    }
    clear_for_rewrite(state, &dir)?;
    ManagedPathsPusher::new(state)
        .push_skill(BUILTIN_SLUG, None, BUILTIN_SKILL_MD.as_bytes(), &[])
        .context("builtin skill: push failed")?;
    stamp_builtin(state, &dir)?;
    info!(path = %dir.display(), "builtin skill: written");
    Ok(BuiltinOutcome::Written)
}

/// Empty the slot before the built-in is written, so the result is exactly the
/// embedded `SKILL.md` and nothing else.
///
/// * A directory the row says is NOT builtin (for example the user's own
///   auto-managed skill that holds the reserved slug) is copied into
///   `restore-backups/<ts>/builtin/<slug>` and journaled first; if the backup
///   fails nothing is removed and the write is abandoned.
/// * A modified builtin is replaced whole, so a planted extra file (a script
///   next to `SKILL.md`) never survives a rewrite.
///
/// A symlink is unlinked, never followed: its target is not ours to delete.
fn clear_for_rewrite(state: &AppState, dir: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Ok(()); // nothing there
    };
    let is_builtin = Connection::open(&state.db_path)
        .map(|c| marker_provenance(&c, &dir.to_string_lossy()).is_builtin())
        .unwrap_or(false);
    if !is_builtin {
        back_up_displaced(state, dir)?;
    }
    if meta.file_type().is_symlink() {
        std::fs::remove_file(dir).context("builtin skill: could not unlink the old link")?;
    } else {
        std::fs::remove_dir_all(dir).context("builtin skill: could not clear the old copy")?;
    }
    Ok(())
}

/// Copy what is about to be displaced into the restore-backups area and journal
/// it (`source = managed`: uninstall does not put it back, but it is on disk).
fn back_up_displaced(state: &AppState, dir: &Path) -> Result<()> {
    use scenery_core::restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal};

    let src = camino::Utf8Path::from_path(dir)
        .ok_or_else(|| anyhow::anyhow!("builtin skill: path is not valid UTF-8"))?;
    let journal = RestoreJournal::for_state(state);
    let backup = journal
        .backup_tree_unique("builtin", BUILTIN_SLUG, src)
        .context("builtin skill: could not back up the skill that holds the reserved slug")?;
    journal
        .append(
            JournalEntry::new(
                JournalOp::FileDelete,
                JournalSource::Managed,
                dir.to_string_lossy().to_string(),
            )
            .with_slug(BUILTIN_SLUG)
            .with_backup_path(backup.to_string())
            .with_detail(serde_json::json!({"reason": "builtin_reserved_slug"})),
        )
        .context("builtin skill: failed to journal the backup")?;
    warn!(path = %dir.display(), backup = %backup, "builtin skill: moved a non-builtin skill aside");
    Ok(())
}

/// Mark the row (and the file marker) `origin = builtin`.
fn stamp_builtin(state: &AppState, dir: &Path) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("builtin skill: open state DB")?;
    let key = dir.to_string_lossy().to_string();
    let mut prov = marker_provenance(&conn, &key);
    prov.origin = Some(ORIGIN_BUILTIN.to_string());
    prov.skill_id = None;
    prov.skill_uuid = None;
    prov.tree_hash = None;
    prov.hash_version = None;
    set_marker_provenance(&conn, &key, &prov)?;
    // The file marker mirrors it for humans; ownership is the row.
    if let Ok(Some(m)) = super::marker::read_file_marker(dir) {
        super::marker::write_file_marker_with(
            dir,
            m.installation_id.as_deref(),
            &m.source_sha256,
            &m.migrated_at,
            &prov,
        )?;
    }
    Ok(())
}

/// Drift pass: when the copy exists but differs from the embedded one (edited
/// or stale), re-write it. A missing copy is left alone (the next daemon start
/// recreates it), so an uninstall in progress is never fought.
pub fn refresh_if_modified(state: &AppState) {
    if reconciler_disabled() {
        return;
    }
    let Some(dir) = canonical_dir() else { return };
    let Ok(conn) = Connection::open(&state.db_path) else {
        return;
    };
    if !marker_provenance(&conn, &dir.to_string_lossy()).is_builtin() {
        return;
    }
    drop(conn);
    let on_disk = std::fs::read(dir.join("SKILL.md")).ok();
    match on_disk {
        Some(b) if hex::encode(Sha256::digest(&b)) != embedded_hash() => {
            debug!("builtin skill: modified copy — re-writing");
            if let Err(e) = ensure_builtin_skill(state) {
                warn!(error = %e, "builtin skill: re-write failed");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "builtin_tests.rs"]
mod tests;
