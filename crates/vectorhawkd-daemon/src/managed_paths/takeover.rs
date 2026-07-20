//! Adopt-publish takeover — removes a skill's original discovered
//! `source_path` (e.g. `~/.agents/skills/<slug>`) once a real VectorHawk
//! F2-managed copy of that skill is confirmed present on disk.
//!
//! # Why this exists
//!
//! Adopting a locally-discovered skill uploads it through the org's normal
//! scan/approval gate ([`crate::managed_paths::adopt_publish`]). Two
//! outcomes are possible:
//!
//! - `published`: a real registry artifact exists immediately, so the origin
//!   device installs it via the normal install path.
//! - `pending_review`: no real artifact yet — the original `source_path` and
//!   whatever local copy already exists must be left alone until IT approves.
//!
//! Both cases converge on the same rule: **only remove `source_path` after
//! the managed replacement is verified on disk.** [`perform_if_pending`] is
//! the single choke point that enforces this — it is called both right after
//! the immediate `published` install and, later, from the reconciler's
//! normal `Install` handling when the deferred-approval version finally
//! installs (see `sync::reconciler::push_skill_to_claude` and the
//! phantom-artifact branch in `sync::reconciler::do_install`).
//!
//! # Data-loss guard (backup before delete)
//!
//! The user's `source_path` is the only copy of whatever they authored —
//! adopting it must never be able to destroy it. Before
//! [`remove_source_path`] deletes anything it copies the original byte-for-
//! byte into the restore journal's backup area
//! (`<root_dir>/restore-backups/<ts>/adopted/<slug>/...`) via
//! [`vectorhawkd_core::restore_journal::RestoreJournal`] and records a
//! `source = adopted` journal entry pointing `backup_path` at that copy.
//! **The delete only happens if the backup + journal append both succeed.**
//! If either fails, the takeover aborts with an error, the original is left
//! completely untouched, and the pending-takeover record stays set so a
//! later call (retried adopt, next install) can try again.
//!
//! # Idempotency
//!
//! [`perform_if_pending`] is a no-op unless [`vectorhawkd_core::state::AppState::pending_adopt_takeover_source`]
//! has a row for the slug, so calling it speculatively on every completed
//! install is safe and cheap. Removal of `source_path` itself tolerates the
//! path already being gone (SSE redelivery, retried adopt) — no backup is
//! attempted in that case since there is nothing left to lose.
//!
//! # Source == destination
//!
//! Since the `~/.agents/skills` pivot, VectorHawk's canonical write location
//! is *also* one of the discoveries scan roots. A discovery found there has a
//! `source_path` equal to `push_skill`'s destination, and removing it would
//! destroy the managed copy instead of the thing it replaced. [`perform_if_pending`]
//! detects that case up front and no-ops, retiring the pending record — see
//! [`source_is_canonical_dest`].
//!
//! # Killswitch
//!
//! Honors `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` like the rest of the F2
//! pusher — set, this becomes a no-op.

use anyhow::{Context, Result};
use std::{fs, path::Path};
use tracing::{debug, info};
use vectorhawkd_core::{
    restore_journal::{new_backup_ts, JournalEntry, JournalOp, JournalSource, RestoreJournal},
    state::AppState,
};

use super::pusher::managed_skill_present;

const ENV_DISABLE: &str = "VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER";

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

/// If `slug` has a pending adopt-takeover recorded, and its managed copy is
/// now confirmed present on disk, remove the original discovered
/// `source_path` (and clear the pending record).
///
/// No-ops (returns `Ok(())`) in all of the following cases:
/// - the killswitch is set,
/// - there is no pending-takeover record for `slug`,
/// - the managed copy is not yet present on disk (a later call — e.g. the
///   next install attempt — will re-check).
///
/// Conservative by construction: the only path ever removed is the exact
/// `source_path` recorded for this slug, and only after the managed
/// replacement is verified present.
pub fn perform_if_pending(state: &AppState, slug: &str) -> Result<()> {
    if reconciler_disabled() {
        return Ok(());
    }

    let source_path = match state
        .pending_adopt_takeover_source(slug)
        .context("takeover: failed to read pending-takeover record")?
    {
        Some(p) => p,
        None => return Ok(()),
    };

    // Identity guard — MUST precede the presence check, which is trivially
    // true when source and destination are the same path.
    //
    // Since the pivot, `~/.agents/skills` is both the discoveries scan root
    // (`discoveries::extra_roots`) and `push_skill`'s destination
    // (`pusher::resolve_skills_dir`). For a discovery found there,
    // `source_path` IS the directory the managed copy gets written to, so
    // "the managed copy landed, remove the original" would delete the skill
    // that was just installed. Takeover has nothing to take over when source
    // is destination — retire the record so it cannot fire again on the next
    // install, and stop.
    if source_is_canonical_dest(slug, Path::new(&source_path)) {
        state
            .clear_pending_adopt_takeover(slug)
            .context("takeover: failed to clear no-op pending-takeover record")?;
        info!(
            slug,
            source = %source_path,
            "adopt takeover: source_path is the canonical managed destination — \
             nothing to remove (adoption completed in place)"
        );
        return Ok(());
    }

    if !managed_skill_present(slug).context("takeover: failed to check managed copy presence")? {
        debug!(
            slug,
            "takeover: managed copy not yet present on disk — deferring removal"
        );
        return Ok(());
    }

    remove_source_path(state, slug, Path::new(&source_path)).with_context(|| {
        format!("takeover: failed to remove original source_path: {source_path}")
    })?;

    state
        .clear_pending_adopt_takeover(slug)
        .context("takeover: failed to clear pending-takeover record")?;

    info!(
        slug,
        removed = %source_path,
        "adopt takeover: managed copy confirmed present — original discovered path removed"
    );
    Ok(())
}

/// True iff `source_path` denotes the same location as
/// `agents_skills_dir()/<slug>` — i.e. removing it would delete the managed
/// copy rather than the thing it replaced.
///
/// Compared literally first, so a *symlink* sitting at the canonical path is
/// recognised without being followed (`canonicalize` would resolve it to its
/// target and miss the identity). The canonicalized comparison then covers
/// the equivalent-but-differently-spelled cases: `/var` vs `/private/var` on
/// macOS, a symlinked `$HOME`, `..` segments.
///
/// Fails **closed**: if the canonical root cannot be resolved, or neither
/// path can be canonicalized, this returns `false` and the normal
/// presence-gated path runs — which is the pre-existing behaviour, still
/// guarded by its own backup-before-delete.
fn source_is_canonical_dest(slug: &str, source_path: &Path) -> bool {
    let Some(dest) = super::paths::agents_skills_dir().map(|d| d.join(slug)) else {
        return false;
    };
    if source_path == dest {
        return true;
    }
    match (fs::canonicalize(source_path), fs::canonicalize(&dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Remove `path`, which may be a real directory, a regular file, or (per the
/// shared contract) a symlink a pre-VectorHawk installer created at the
/// discovered location.
///
/// A symlink is unlinked directly rather than followed, so this never
/// recurses into — or deletes — whatever the symlink points at. Absence is
/// treated as success (idempotent: already-removed dir, retried takeover).
///
/// **Never deletes without backing up first.** Unless `path` is already
/// absent or is a dangling symlink (nothing behind it to lose), the original
/// is copied byte-for-byte into the restore journal's backup area and a
/// journal entry is appended *before* anything is removed. If that backup
/// step fails for any reason, this returns `Err` and `path` is left
/// completely untouched — the caller's pending-takeover record is not
/// cleared, so a later retry can try again.
fn remove_source_path(state: &AppState, slug: &str, path: &Path) -> Result<()> {
    let link_meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "takeover: source_path already absent (idempotent)");
            return Ok(());
        }
        Err(e) => return Err(e).context("failed to stat source_path"),
    };

    if link_meta.file_type().is_symlink() && fs::metadata(path).is_err() {
        // Dangling symlink: whatever it once pointed at is already gone, so
        // there is no data behind it to lose. Unlink directly — no backup
        // needed or possible.
        fs::remove_file(path).context("failed to remove dangling symlink")?;
        debug!(
            path = %path.display(),
            "takeover: removed dangling symlink source_path (nothing to back up)"
        );
        return Ok(());
    }

    back_up_before_delete(state, slug, path).with_context(|| {
        format!(
            "refusing to delete un-backed-up source_path: {}",
            path.display()
        )
    })?;

    if link_meta.file_type().is_symlink() {
        fs::remove_file(path).context("failed to remove symlink")?;
    } else if link_meta.is_dir() {
        fs::remove_dir_all(path).context("failed to remove directory")?;
    } else {
        fs::remove_file(path).context("failed to remove file")?;
    }
    Ok(())
}

/// Copy `path` byte-for-byte into
/// `<root_dir>/restore-backups/<ts>/adopted/<slug>/` and append a
/// `source = adopted` restore-journal entry pointing at that copy — all
/// *before* the caller deletes anything.
///
/// Returns `Err` (and leaves no partial journal entry) if either the copy or
/// the journal append fails, so the caller can treat backup failure as a
/// hard stop rather than proceeding to delete un-backed-up data.
fn back_up_before_delete(state: &AppState, slug: &str, path: &Path) -> Result<()> {
    let journal = RestoreJournal::for_state(state);
    let ts = new_backup_ts();
    let dest = journal.backup_dir_for(&ts).join("adopted").join(slug);

    if path.is_dir() {
        copy_tree_recursive(path, dest.as_std_path())
            .with_context(|| format!("failed to back up directory {} -> {dest}", path.display()))?;
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent.as_std_path())
                .with_context(|| format!("failed to create backup dir {parent}"))?;
        }
        fs::copy(path, dest.as_std_path())
            .with_context(|| format!("failed to back up file {} -> {dest}", path.display()))?;
    }

    journal
        .append(
            JournalEntry::new(
                JournalOp::FileDelete,
                JournalSource::Adopted,
                path.to_string_lossy().to_string(),
            )
            .with_slug(slug)
            .with_backup_path(dest.to_string())
            .with_detail(serde_json::json!({"reason": "adopt_takeover"})),
        )
        .context("failed to append restore-journal entry for adopt takeover backup")?;

    Ok(())
}

/// Recursively copy a directory tree, creating destination directories as
/// needed. Mirrors the equivalent private helper in
/// `vectorhawkd_core::restore_journal` / `managed_paths::migrator` — kept
/// local rather than shared since none of those are exported across the
/// crate boundary.
fn copy_tree_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create backup dest: {}", dest.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read dir for backup: {}", src.display()))?
    {
        let entry = entry.context("failed to read dir entry during backup")?;
        let entry_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat: {}", entry_path.display()))?;

        if meta.is_dir() {
            copy_tree_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "takeover_tests.rs"]
mod tests;
