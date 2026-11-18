//! Uninstall of the user's OWN auto-managed skills (owner decision 2026-11-18,
//! reverses "uninstall restores the original" of 2.3.x).
//!
//! Reachable from every user-driven uninstall: `scenery skill uninstall`, the
//! MCP `scenery_uninstall` tool, and a portal uninstall of the owner's own
//! private skill (the backend marks that `deactivate` event
//! `reason: "user_uninstall"`). NOT used by revoke / the kill switch, deactivate
//! catch-up or the registry-deleted path: those keep the plain
//! [`ManagedPathsPusher::remove_skill`].
//!
//! # Post-uninstall state
//!
//! - The skill is REMOVED: the `~/.agents/skills/<slug>` directory, every client
//!   link (`~/.claude/skills`, Antigravity), the marker file and
//!   `managed_path_markers` row. The user's original is NOT put back; the
//!   registry holds the uploaded bytes and the backend archives the private
//!   copy, so the portal's Reinstall brings it back.
//! - Journal entries of the conversion (`Adopted` backup / `Managed` push) are
//!   dropped, so a later whole-product `scenery uninstall` cannot replay a stale
//!   restore over a folder the user recreated. The backup files themselves stay
//!   on disk, and so does a `removed-own` backup entry.
//! - Edits not yet uploaded (local edits do not sync until a later phase) are
//!   never lost: [`ManagedPathsPusher::remove_skill`] copies a tree that differs
//!   from what Scenery wrote into `restore-backups/<ts>/removed-own/<slug>`
//!   first and refuses to delete if that copy fails. Unknown (no recorded hash)
//!   counts as edited.
//! - Nothing sticks: if the user later recreates a folder with the same name it
//!   is auto-managed again like any new skill (the backend unarchives the record).
//! - Tracked (git/dev/foreign-installer) skills are not converted and have no
//!   marker: this never touches their files, links or tracking row; the report
//!   says nothing was removed.

use super::{marker::marker_provenance, pusher::ManagedPathsPusher};
use anyhow::{Context, Result};
use scenery_core::{
    installer::OwnSkillUninstall,
    restore_journal::{JournalOp, JournalSource, RestoreJournal},
    state::AppState,
};
use tracing::info;

/// User-driven uninstall of the native projection of `id` (a registry id like
/// `adam~pdf`, or the folder name). See the module docs.
pub fn uninstall_projection(state: &AppState, id: &str) -> Result<bool> {
    uninstall_projection_detailed(state, id).map(|(removed, _)| removed)
}

/// [`uninstall_projection`], also reporting the own auto-managed skill that was
/// torn down (for the backend archive call). `Err` leaves the skill as it was.
pub fn uninstall_projection_detailed(
    state: &AppState,
    id: &str,
) -> Result<(bool, Option<OwnSkillUninstall>)> {
    let slug = state.skill_local_name(id);
    scenery_core::safe_path::validate_single_component("skill slug", &slug)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let pusher = ManagedPathsPusher::new(state);
    let canonical = super::paths::agents_skills_dir().map(|d| d.join(&slug));
    let key = canonical
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let prov = {
        let conn =
            rusqlite::Connection::open(&state.db_path).context("own-uninstall: open state DB")?;
        marker_provenance(&conn, &key)
    };
    if !prov.is_auto_managed() {
        // Not an auto-managed own skill (catalog install, tracked, user
        // content): the ordinary teardown, which leaves anything not ours.
        return Ok((pusher.remove_skill(&slug)?, None));
    }

    // Decide before anything is deleted; `remove_skill` takes the backup.
    let local_edits = canonical
        .as_deref()
        .is_some_and(|dir| super::pusher::own_tree_has_unuploaded_edits(dir, &prov));
    let removed = pusher.remove_skill(&slug)?;
    finish(state, &slug);
    info!(slug, local_edits, "own-uninstall: own skill removed");
    let skill_id = prov
        .skill_id
        .clone()
        .or_else(|| prov.skill_uuid.clone())
        .unwrap_or_else(|| id.to_string());
    Ok((
        removed,
        Some(OwnSkillUninstall {
            skill_id,
            local_edits,
        }),
    ))
}

/// Bookkeeping once the files are gone: forget the pending takeover and the
/// auto-manage attempt rows, drop the conversion's journal entries (a
/// `removed-own` backup entry is kept), so nothing replays later.
fn finish(state: &AppState, slug: &str) {
    let _ = state.clear_pending_adopt_takeover(slug);
    if let Ok(conn) = rusqlite::Connection::open(&state.db_path) {
        let _ = conn.execute(
            "DELETE FROM auto_manage_attempts WHERE slug = ?1",
            rusqlite::params![slug],
        );
    }
    let journal = RestoreJournal::for_state(state);
    let _ = journal.remove_where(|e| {
        // The `removed-own` backup `remove_skill` just journaled stays.
        let keeps_edit_backup =
            e.detail.get("reason").and_then(|v| v.as_str()) == Some("teardown_of_own_skill");
        e.slug.as_deref() == Some(slug)
            && matches!(e.source, JournalSource::Adopted | JournalSource::Managed)
            && e.op != JournalOp::ConfigEdit
            && !keeps_edit_backup
    });
}
