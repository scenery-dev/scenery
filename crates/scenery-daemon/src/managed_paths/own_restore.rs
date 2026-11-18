//! "Uninstall = restore" for the user's OWN skills that Scenery converted in
//! place (auto-manage). Reachable from every user-driven uninstall: `scenery
//! skill uninstall`, the MCP `scenery_uninstall` tool and a portal-driven
//! uninstall (`removed` installation, i.e. `Purge`). NOT used by revoke (the
//! kill switch must never put a revoked skill back) or deactivate.
//!
//! # Post-uninstall state (decided here, the plan does not spell it out)
//!
//! - Unedited since conversion: the user's original tree is copied back from
//!   the oldest `Adopted` backup (symlink-safe `copy_tree`, file modes kept),
//!   staged beside the target and swapped in by rename. It is an ordinary
//!   unmanaged directory again: no marker file, no `managed_path_markers`
//!   row, no links in `~/.claude/skills` / Antigravity, journal entries for
//!   the path dropped. The backups stay on disk.
//! - Edited since conversion (or no fingerprint to compare): the CURRENT tree
//!   stays, unmanaged (plan 4.5.5: restoring the old original over newer work
//!   would lose data); the original stays in its `adopted/` backup.
//! - Either way a sticky `auto_manage_attempts` row (`user_uninstalled`)
//!   stops auto-manage from converting the skill again; it is forgotten when
//!   the directory disappears.

use super::{auto_manage, links, marker::marker_provenance, pusher::ManagedPathsPusher};
use anyhow::{Context, Result};
use scenery_core::{
    restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal},
    safe_copy::{copy_tree, tree_fingerprint, unique_token},
    state::AppState,
};
use scenery_mcp::ownership::MANAGED_MARKER_FILENAME;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{info, warn};

/// What [`restore_own_skill`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnOutcome {
    /// Not an auto-managed own skill (or nothing to restore from): untouched.
    NotOwn,
    /// The original was put back at this path.
    Restored(PathBuf),
    /// The user edited the managed copy: it stays at `kept`, unmanaged; the
    /// original remains in `original_backup`.
    KeptEdited {
        kept: PathBuf,
        original_backup: PathBuf,
    },
}

/// User-driven uninstall of the native projection of `slug`: restore the
/// user's own original when the skill was auto-managed, otherwise the plain
/// teardown (`remove_skill`). Never touches a directory Scenery does not own.
///
/// `id` may be a registry id (`adam~pdf`); the folder is its local name.
pub fn uninstall_projection(state: &AppState, id: &str) -> Result<bool> {
    let slug = state.skill_local_name(id);
    match restore_own_skill(state, &slug)? {
        OwnOutcome::NotOwn => ManagedPathsPusher::new(state).remove_skill(&slug),
        OwnOutcome::Restored(_) | OwnOutcome::KeptEdited { .. } => Ok(true),
    }
}

/// See the module docs. `Err` leaves the skill as it was (nothing deleted).
pub fn restore_own_skill(state: &AppState, slug: &str) -> Result<OwnOutcome> {
    scenery_core::safe_path::validate_single_component("skill slug", slug)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(canonical) = super::paths::agents_skills_dir().map(|d| d.join(slug)) else {
        return Ok(OwnOutcome::NotOwn);
    };
    let key = canonical.to_string_lossy().to_string();
    let conn = rusqlite::Connection::open(&state.db_path).context("own-restore: open state DB")?;
    let prov = marker_provenance(&conn, &key);
    drop(conn);
    if !prov.is_auto_managed() {
        return Ok(OwnOutcome::NotOwn);
    }
    let journal = RestoreJournal::for_state(state);
    let entries = journal.read_all().context("own-restore: read journal")?;
    let Some(adopted) = oldest_existing_backup(&entries, slug) else {
        warn!(slug, "own-restore: no adopted backup found; plain teardown");
        return Ok(OwnOutcome::NotOwn);
    };
    let backup = PathBuf::from(adopted.backup_path.clone().unwrap_or_default());
    let target = PathBuf::from(&adopted.target_path);

    let pristine = is_pristine(&entries, &canonical);
    let outcome = if target == canonical {
        if pristine {
            swap_in_original(&backup, &canonical)?;
            unlink_clients(slug);
            OwnOutcome::Restored(target.clone())
        } else {
            keep_edited(&canonical, slug, &backup)
        }
    } else {
        restore_elsewhere(state, &backup, &target, &canonical, slug, pristine)?
    };
    finish(state, &journal, slug, &key, &target);
    info!(slug, ?outcome, "own-restore: own skill uninstalled");
    Ok(outcome)
}

/// Oldest `Adopted` journal entry for `slug` whose backup still exists (the
/// oldest is the pre-Scenery state; later ones can back up half-converted trees).
fn oldest_existing_backup<'a>(entries: &'a [JournalEntry], slug: &str) -> Option<&'a JournalEntry> {
    entries.iter().find(|e| {
        e.source == JournalSource::Adopted
            && e.slug.as_deref() == Some(slug)
            && e.backup_path
                .as_deref()
                .is_some_and(|b| fs::symlink_metadata(b).is_ok())
    })
}

/// Live tree still equals what Scenery last wrote (journal fingerprint).
fn is_pristine(entries: &[JournalEntry], canonical: &Path) -> bool {
    let want = entries
        .iter()
        .rev()
        .filter(|e| {
            e.source == JournalSource::Managed
                && e.op == JournalOp::ArtifactPush
                && Path::new(&e.target_path) == canonical
        })
        .find_map(|e| e.detail.get("tree_sha256").and_then(|v| v.as_str()));
    let is_dir = fs::symlink_metadata(canonical).is_ok_and(|m| m.is_dir());
    is_dir
        && want.is_some_and(|h| {
            tree_fingerprint(canonical, &[MANAGED_MARKER_FILENAME]).is_ok_and(|cur| cur == h)
        })
}

/// Stage the backup beside `target`, move the live tree aside, rename the
/// staged copy in, then drop the old tree. A failure before the second rename
/// leaves the live tree where it was.
fn swap_in_original(backup: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .context("own-restore: target has no parent")?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .context("own-restore: target has no name")?;
    let token = unique_token();
    let staging = parent.join(format!(".{name}.scenery-restore-{token}"));
    let old = parent.join(format!(".{name}.scenery-old-{token}"));
    if let Err(e) = copy_tree(backup, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e).context("own-restore: staging the original");
    }
    let had_live = fs::symlink_metadata(target).is_ok();
    if had_live {
        if let Err(e) = fs::rename(target, &old) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e).context("own-restore: moving the managed copy aside");
        }
    }
    if let Err(e) = fs::rename(&staging, target) {
        if had_live {
            let _ = fs::rename(&old, target);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(e).context("own-restore: moving the original into place");
    }
    if had_live {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Edited: leave the tree, make it unmanaged (marker off so nothing sweeps it).
fn keep_edited(canonical: &Path, slug: &str, backup: &Path) -> OwnOutcome {
    let _ = fs::remove_file(canonical.join(MANAGED_MARKER_FILENAME));
    unlink_clients(slug);
    OwnOutcome::KeptEdited {
        kept: canonical.to_path_buf(),
        original_backup: backup.to_path_buf(),
    }
}

/// The original lived at another root (`~/.pi/...`): put it back there if the
/// path is free, then handle the managed canonical copy (edits are kept).
fn restore_elsewhere(
    state: &AppState,
    backup: &Path,
    target: &Path,
    canonical: &Path,
    slug: &str,
    pristine: bool,
) -> Result<OwnOutcome> {
    if fs::symlink_metadata(target).is_ok() {
        // Something is there now: never overwrite it. Keep the managed copy.
        return Ok(keep_edited(canonical, slug, backup));
    }
    copy_tree(backup, target).context("own-restore: restoring the original")?;
    if pristine {
        unlink_clients(slug);
        ManagedPathsPusher::new(state).remove_skill(slug)?;
        Ok(OwnOutcome::Restored(target.to_path_buf()))
    } else {
        Ok(keep_edited(canonical, slug, backup))
    }
}

fn unlink_clients(slug: &str) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(super::paths::claude_skills_link_dir());
    dirs.extend(super::pusher::antigravity_skills_link_dir());
    for d in dirs {
        if let Err(e) = links::unlink_dir(&d.join(slug)) {
            warn!(slug, error = %e, "own-restore: could not unlink client link");
        }
    }
}

/// Bookkeeping once the files are settled: drop the marker row and the
/// journal entries for the path (so a later whole-product uninstall does not
/// replay a stale restore), and make the exclusion sticky.
fn finish(state: &AppState, journal: &RestoreJournal, slug: &str, key: &str, target: &Path) {
    if let Ok(conn) = rusqlite::Connection::open(&state.db_path) {
        let _ = conn.execute(
            "DELETE FROM managed_path_markers WHERE path = ?1",
            rusqlite::params![key],
        );
    }
    let _ = state.clear_pending_adopt_takeover(slug);
    let _ = journal.remove_where(|e| {
        e.slug.as_deref() == Some(slug)
            && matches!(e.source, JournalSource::Adopted | JournalSource::Managed)
            && e.op != JournalOp::ConfigEdit
    });
    let hash = scenery_core::skill_tree::survey(target)
        .map(|s| s.tree_hash)
        .unwrap_or_default();
    auto_manage::record_attempt(
        state,
        slug,
        &target.to_string_lossy(),
        &hash,
        auto_manage::AttemptStatus::Skipped,
        Some(auto_manage::USER_UNINSTALLED),
    );
}
