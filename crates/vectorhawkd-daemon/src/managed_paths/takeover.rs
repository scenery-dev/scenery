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
//! # Idempotency
//!
//! [`perform_if_pending`] is a no-op unless [`vectorhawkd_core::state::AppState::pending_adopt_takeover_source`]
//! has a row for the slug, so calling it speculatively on every completed
//! install is safe and cheap. Removal of `source_path` itself tolerates the
//! path already being gone (SSE redelivery, retried adopt).
//!
//! # Killswitch
//!
//! Honors `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` like the rest of the F2
//! pusher — set, this becomes a no-op.

use anyhow::{Context, Result};
use std::{fs, path::Path};
use tracing::{debug, info};
use vectorhawkd_core::state::AppState;

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

    if !managed_skill_present(slug).context("takeover: failed to check managed copy presence")? {
        debug!(
            slug,
            "takeover: managed copy not yet present on disk — deferring removal"
        );
        return Ok(());
    }

    remove_source_path(Path::new(&source_path)).with_context(|| {
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

/// Remove `path`, which may be a real directory or (per the shared contract)
/// a symlink a pre-VectorHawk installer created at the discovered location.
///
/// A symlink is unlinked directly rather than followed, so this never
/// recurses into — or deletes — whatever the symlink points at. Absence is
/// treated as success (idempotent: already-removed dir, retried takeover).
fn remove_source_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(path).context("failed to remove symlink")?;
        }
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).context("failed to remove directory")?;
        }
        Ok(_) => {
            fs::remove_file(path).context("failed to remove file")?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "takeover: source_path already absent (idempotent)");
        }
        Err(e) => return Err(e).context("failed to stat source_path"),
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "takeover_tests.rs"]
mod tests;
