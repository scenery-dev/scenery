//! F2 managed-paths pusher — writes Scenery-managed items into clients'
//! native directories on install and removes them on deactivate.
//!
//! # What it writes
//!
//! | Item   | Destination                                           |
//! |--------|-------------------------------------------------------|
//! | Skill  | `~/.agents/skills/<slug>/` + `.scenery-managed.json` marker, linked at `~/.claude/skills/<slug>` for Claude Code |
//! | MCP    | Entry in `~/.claude.json` `mcpServers` block           |
//!
//! Plugin pushes are handled by `managed_paths::plugin_marketplace`
//! (`install_plugin_bundle` / `uninstall_plugin_bundle`), not by this module.
//!
//! # Atomicity
//!
//! All writes use temp-file + atomic rename so Claude Code never sees a partial
//! write.  `~/.claude.json` additionally uses an exclusive file lock via `fs2`
//! so concurrent daemon pushes (e.g. 5 MCP installs arriving simultaneously)
//! cannot corrupt the JSON.
//!
//! # Env-var gate
//!
//! All public methods return `Ok(())` immediately when
//! `SCENERY_DISABLE_FILESYSTEM_RECONCILER` is set in the environment.
//! Callers do not need to guard this themselves.
//!
//! # Error policy
//!
//! `push_*` failures are non-fatal to the install flow: callers log WARN and
//! continue.  `remove_*` failures are also non-fatal.

use super::marker::{
    insert_db_marker, is_already_marked, marker_provenance, set_marker_provenance,
    update_db_marker_installation_id, write_file_marker_with, ManagedPathMarker, Provenance,
    ORIGIN_AUTO_MANAGED,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::Connection;
use scenery_core::atomic_file;
use scenery_core::pack::TreeFile;
use scenery_core::restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal};
use scenery_core::state::AppState;
use scenery_mcp::ownership;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{debug, info, warn};

// ── Env-var gate ──────────────────────────────────────────────────────────────

/// Return `true` when the filesystem reconciler is disabled.
pub(crate) fn reconciler_disabled() -> bool {
    std::env::var_os("SCENERY_DISABLE_FILESYSTEM_RECONCILER").is_some()
}

// ── Push outcome / refusal types ──────────────────────────────────────────────

/// What a successful `push_skill` did beyond writing the canonical dir.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// The `~/.claude/skills/<slug>` link could not be created (typically a
    /// real, unmarked directory sits there). The canonical copy is live.
    pub link_refused: Option<PathBuf>,
}

/// `push_skill` refused to overwrite content at `path` that Scenery does not
/// own. Typed so callers can downcast and report the colliding path.
#[derive(Debug)]
pub struct CollisionRefused {
    pub path: PathBuf,
    pub what: &'static str,
    pub slug: String,
}

impl std::fmt::Display for CollisionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pusher: refusing to replace the {} at {} — not Scenery-managed. \
             Slug '{}' collides with user content in the shared ~/.agents/skills root.",
            self.what,
            self.path.display(),
            self.slug
        )
    }
}

impl std::error::Error for CollisionRefused {}

/// `push_skill` refused to overwrite a managed dir whose live tree differs from
/// its marker baseline: the user's edit is not uploaded yet (plan 4.5.1).
#[derive(Debug)]
pub struct DirtyLocalEdit {
    pub path: PathBuf,
}

impl std::fmt::Display for DirtyLocalEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pusher: not overwriting {}: it has local edits that are not uploaded yet",
            self.path.display()
        )
    }
}

impl std::error::Error for DirtyLocalEdit {}

// ── ManagedPathsPusher ────────────────────────────────────────────────────────

/// Pushes Scenery-managed items into Claude Code's native directories.
///
/// Cheaply cloneable — the only fields are two paths.
#[derive(Clone)]
pub struct ManagedPathsPusher {
    db_path: camino::Utf8PathBuf,
    root_dir: camino::Utf8PathBuf,
}

/// Does the live tree of an auto-managed own skill differ from what Scenery
/// last wrote (the registry-installed copy of the version the user uploaded)?
/// Fail-safe: an unreadable tree or a missing conversion-time hash counts as
/// edited, so the caller backs up rather than deleting.
pub(crate) fn own_tree_has_unuploaded_edits(skill_dir: &Path, prov: &Provenance) -> bool {
    match (
        scenery_core::skill_tree::survey(skill_dir).map(|s| s.tree_hash),
        &prov.tree_hash,
    ) {
        (Ok(cur), Some(written)) => &cur != written,
        _ => true,
    }
}

impl ManagedPathsPusher {
    /// Construct a pusher from the daemon's `AppState`.
    pub fn new(state: &AppState) -> Self {
        Self {
            db_path: state.db_path.clone(),
            root_dir: state.root_dir.clone(),
        }
    }

    /// Restore-journal handle rooted at this pusher's data dir.
    ///
    /// F2 pushes always append with `backup_path = None`: the pushed
    /// artifact only ever worked because Scenery put it there
    /// (`source = managed` / `brokered`), so uninstall removes it outright
    /// rather than restoring from a backup — see `JournalSource`.
    fn journal(&self) -> RestoreJournal {
        RestoreJournal::new(self.root_dir.clone())
    }

    /// Append a restore-journal entry for a completed push, logging (not
    /// propagating) any failure — the push itself already succeeded and
    /// must not be rolled back just because the ledger write failed.
    fn journal_push(&self, entry: JournalEntry) {
        if let Err(e) = self.journal().append(entry) {
            tracing::warn!(error = %e, "pusher: failed to append restore-journal entry (non-fatal)");
        }
    }

    // ── Skill ─────────────────────────────────────────────────────────────────

    /// Write a skill into `~/.agents/skills/<slug>/` (the canonical, real
    /// directory) and link it at `~/.claude/skills/<slug>` for Claude Code,
    /// which does not read `~/.agents/skills` natively.
    ///
    /// `skill_md_bytes` is the raw SKILL.md content.
    /// `referenced_files` is a slice of `(relative_path, bytes)` pairs for any
    /// additional files that belong alongside SKILL.md (prompts/, etc.).
    ///
    /// The function:
    /// 1. Creates the target directory.
    /// 2. Writes SKILL.md + referenced files atomically (temp → rename per file).
    /// 3. Writes a `.scenery-managed.json` marker into the directory.
    /// 4. Upserts the `managed_path_markers` SQLite row.
    pub fn push_skill(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        skill_md_bytes: &[u8],
        referenced_files: &[(String, Vec<u8>)],
    ) -> Result<PushOutcome> {
        self.push_skill_inner(
            slug,
            installation_id,
            skill_md_bytes,
            &as_plain_files(referenced_files),
            false,
        )
    }

    /// Like [`push_skill`](Self::push_skill), but an unmarked real directory
    /// at the canonical path is converted to managed in place instead of
    /// refused. ONLY for the adopt flow, when the adopted `source_path` is
    /// that canonical directory (the user's own copy, which they just asked
    /// Scenery to take over). The original is backed up through the restore
    /// journal before being replaced; if the backup fails nothing is touched.
    /// A foreign symlink at the path is still refused.
    pub fn push_skill_adopting_in_place(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        skill_md_bytes: &[u8],
        referenced_files: &[(String, Vec<u8>)],
    ) -> Result<PushOutcome> {
        self.push_skill_inner(
            slug,
            installation_id,
            skill_md_bytes,
            &as_plain_files(referenced_files),
            true,
        )
    }

    /// [`push_skill`](Self::push_skill) / [`push_skill_adopting_in_place`]
    /// (selected by `adopt_in_place`) for a tree read with
    /// [`scenery_core::pack::collect_tree_files`], which keeps each file's
    /// executable bit. Every install path that has a real skill tree on disk
    /// (registry install, adopt, heal) goes through here so `scripts/` stay
    /// runnable.
    pub fn push_skill_files(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        skill_md_bytes: &[u8],
        files: &[TreeFile],
        adopt_in_place: bool,
    ) -> Result<PushOutcome> {
        let borrowed: Vec<(&str, &[u8], bool)> = files
            .iter()
            .map(|f| (f.rel.as_str(), f.bytes.as_slice(), f.executable))
            .collect();
        self.push_skill_inner(
            slug,
            installation_id,
            skill_md_bytes,
            &borrowed,
            adopt_in_place,
        )
    }

    fn push_skill_inner(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        skill_md_bytes: &[u8],
        referenced_files: &[(&str, &[u8], bool)],
        adopt_in_place: bool,
    ) -> Result<PushOutcome> {
        if reconciler_disabled() {
            return Ok(PushOutcome::default());
        }
        scenery_core::safe_path::validate_single_component("skill slug", slug)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let skills_dir = resolve_skills_dir()?;
        let skill_dir = skills_dir.join(slug);
        let mut conversion: Option<super::conversion::Conversion> = None;

        // A symlink at the canonical path must be unlinked before we create a
        // real directory here: writing files THROUGH a symlink would leak
        // modifications back into whatever it points at (the original bug —
        // the pre-v1.0.51 installer's versioned dir resurrecting itself on
        // the next reconcile).
        //
        // But `~/.agents/skills` is a SHARED root, not Scenery's private
        // one: `npx skills`, Cursor and Codex users all write here, and
        // `discoveries.rs` scans it precisely because it is user-populated. So
        // ownership must be established before unlinking, exactly as
        // `links::link_dir` does for a real directory at the Claude path.
        //
        // Scenery never puts a symlink at the canonical path itself — it
        // writes a real directory — so a link here is presumed foreign unless
        // it resolves to content carrying our `.scenery-managed.json`
        // marker. Refuse rather than clobber; the slug collision is the
        // caller's problem to report, and the user's link survives.
        //
        // No backup is taken: removing a link destroys no data (its target is
        // untouched), matching `link_dir`'s own stale-link handling.
        if skill_dir.is_symlink() {
            if !ownership::is_scenery_managed(&skill_dir) {
                return Err(anyhow::Error::new(CollisionRefused {
                    path: skill_dir.clone(),
                    what: "symlink",
                    slug: slug.to_string(),
                }));
            }
            fs::remove_file(&skill_dir).with_context(|| {
                format!(
                    "pusher: failed to remove managed symlink at {}",
                    skill_dir.display()
                )
            })?;
            debug!(
                slug,
                "pusher: replaced a Scenery-managed symlink with a real managed dir"
            );
        } else if skill_dir.is_dir() && !ownership::is_scenery_managed(&skill_dir) {
            if adopt_in_place {
                // Adopt of the canonical directory itself. The new tree is
                // built in a sibling staging dir, the user's copy is backed up,
                // then the two are swapped with renames (see
                // `managed_paths::conversion`): a crash or a full disk midway
                // leaves either the complete original or the complete new
                // tree at the canonical path, never a half-written one.
                conversion = Some(
                    super::conversion::Conversion::begin(
                        self.root_dir.as_std_path(),
                        slug,
                        &skill_dir,
                    )
                    .context("pusher: adopt in place: could not start the conversion")?,
                );
                info!(slug, "pusher: adopting user directory in place");
            } else {
                // The more common shape of the same collision: `npx skills`,
                // Cursor and Codex all create real directories at the canonical
                // path, and `discoveries.rs` deliberately reports them without
                // adopting them, so they legitimately sit here unmanaged forever.
                // Annexing one here would overwrite its SKILL.md, stamp it with
                // our marker, and prime it for a later remove_skill to
                // `remove_dir_all` content Scenery never wrote — refuse rather
                // than clobber, exactly as the symlink branch above does.
                return Err(anyhow::Error::new(CollisionRefused {
                    path: skill_dir.clone(),
                    what: "directory",
                    slug: slug.to_string(),
                }));
            }
        }

        // Plan 4.5.1: a managed dir whose tree differs from its baseline holds an
        // edit that may not be uploaded yet; never write over it.
        if conversion.is_none() && skill_dir.is_dir() && ownership::is_scenery_managed(&skill_dir) {
            if let Ok(conn) = Connection::open(&self.db_path) {
                let key = skill_dir.to_string_lossy().to_string();
                if super::local_edits::tree_is_dirty(&conn, &key, &skill_dir) {
                    return Err(anyhow::Error::new(DirtyLocalEdit { path: skill_dir }));
                }
            }
        }

        // Where the new tree is written: the staging dir during an in-place
        // conversion, the canonical dir otherwise.
        let write_root: PathBuf = match &conversion {
            Some(c) => c.staging().to_path_buf(),
            None => {
                fs::create_dir_all(&skill_dir).with_context(|| {
                    format!(
                        "pusher: failed to create skill dir: {}",
                        skill_dir.display()
                    )
                })?;
                skill_dir.clone()
            }
        };

        // Compute source hash from SKILL.md content.
        let sha256 = hex_sha256(skill_md_bytes);
        let now_ts = scenery_core::timefmt::now_rfc3339();

        // Provenance: a pending auto-manage takeover for this slug stamps the
        // marker (`origin = auto_managed`, registry ids); otherwise whatever
        // the existing row already says carries forward, so an upgrade of an
        // auto-managed skill stays auto-managed.
        let mut prov = self.provenance_for(slug, &skill_dir.to_string_lossy());

        let written = (|| -> Result<()> {
            // Write SKILL.md atomically.
            atomic_write(&write_root.join("SKILL.md"), skill_md_bytes)
                .context("pusher: failed to write SKILL.md")?;

            // Write referenced files.
            for &(rel_path, content, executable) in referenced_files {
                let dest = write_root.join(rel_path);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "pusher: failed to create dir for referenced file: {}",
                            parent.display()
                        )
                    })?;
                }
                atomic_write_with_mode(&dest, content, executable).with_context(|| {
                    format!(
                        "pusher: failed to write referenced file: {}",
                        dest.display()
                    )
                })?;
            }

            // In-place conversion: back up the original and swap the fully
            // written staging tree in. Only now does the canonical path change.
            if let Some(c) = conversion.as_mut() {
                c.swap(&self.journal())?;
            }

            // Write file marker into the skill directory (after the swap, so
            // a marked directory is always complete). The tree hash is of what
            // Scenery just wrote (marker excluded), the baseline uninstall and
            // drift compare later edits against.
            prov.tree_hash = scenery_core::skill_tree::survey(&skill_dir)
                .map(|s| s.tree_hash)
                .map_err(|e| debug!(slug, error = %e, "pusher: could not hash the installed tree"))
                .ok();
            prov.hash_version = prov.tree_hash.as_ref().map(|_| 2);
            write_file_marker_with(&skill_dir, installation_id, &sha256, &now_ts, &prov)
                .context("pusher: failed to write .scenery-managed.json for skill")
        })();
        if let Err(e) = written {
            if let Some(c) = conversion.take() {
                c.rollback();
            }
            return Err(e);
        }
        // Marked: the conversion (if any) is complete; drop the old dir. The
        // backup of THIS conversion is recorded in the push entry so a later
        // uninstall restores it, not an older cycle's.
        let adopted_backup = conversion
            .as_ref()
            .and_then(|c| c.backup_path().map(str::to_string));
        if let Some(c) = conversion.take() {
            c.commit();
        }

        // Upsert SQLite marker.
        let path_key = skill_dir.to_string_lossy().to_string();
        self.upsert_db_marker(&path_key, "skill", slug, installation_id, &sha256, &now_ts)
            .context("pusher: failed to upsert managed_path_markers for skill")?;
        match Connection::open(&self.db_path) {
            Ok(conn) => {
                if let Err(e) = set_marker_provenance(&conn, &path_key, &prov) {
                    warn!(slug, error = %e, "pusher: could not record marker provenance");
                }
            }
            Err(e) => warn!(slug, error = %e, "pusher: could not open state DB for provenance"),
        }

        // Restore journal: this directory only exists because Scenery put
        // it there (source=managed) — uninstall removes it outright, no
        // backup needed.
        //
        // `tree_sha256` fingerprints exactly what Scenery installed (marker
        // excluded). Uninstall compares the live tree against it to tell
        // "still what Scenery wrote" from "the user edited it since", and
        // preserves the latter instead of deleting it.
        let tree_sha256 = scenery_core::safe_copy::tree_fingerprint(
            &skill_dir,
            &[ownership::MANAGED_MARKER_FILENAME],
        )
        .map_err(|e| debug!(slug, error = %e, "pusher: could not fingerprint installed tree"))
        .ok();
        self.journal_push(
            JournalEntry::new(JournalOp::ArtifactPush, JournalSource::Managed, path_key)
                .with_slug(slug)
                .with_client("all")
                .with_detail(serde_json::json!({
                    "installation_id": installation_id,
                    "tree_sha256": tree_sha256,
                    "origin": prov.origin,
                    "skill_id": prov.skill_id,
                    "adopted_backup": adopted_backup,
                })),
        );

        // Surface the skill to Claude Code, which does not read
        // `~/.agents/skills`. Cursor, Codex, and Gemini CLI scan the canonical
        // path natively and need nothing here.
        //
        // Ownership at the link path is decided by the marker file alone —
        // `link_dir` replaces a real directory there only when it carries
        // `.scenery-managed.json`, and refuses otherwise. That is an
        // existence check on disk, deliberately NOT cross-checked against
        // `managed_path_markers`.
        //
        // Why no DB cross-check: it would misfire on a legitimate, common
        // case. If a prior `remove_skill` deleted the SQLite row but crashed
        // before removing the directory, the leftover marked directory has no
        // matching row — a cross-check would refuse to relink and leave Claude
        // Code pointed at stale content instead of finishing the cleanup. The
        // filesystem marker is the source of truth here (see
        // `ownership::is_scenery_managed`); SQLite is an index, and an
        // index that can disagree with the thing it indexes must not be the
        // one holding the safety property.
        //
        // Note the marker proves only that Scenery *wrote* the directory,
        // not that its current content is reproducible — which is why
        // `link_dir` backs up before replacing, rather than relying on this
        // call's `skill_md_bytes` being able to recreate it.
        let link_path = resolve_claude_link_dir()?.join(slug);
        let mut outcome = PushOutcome::default();
        match super::links::link_dir(&skill_dir, &link_path) {
            Ok(mode) => debug!(slug, ?mode, "pusher: linked skill for Claude Code"),
            Err(e) => {
                if self.is_pending_takeover_source(slug, &link_path) {
                    // Expected: takeover removes this real dir and relinks
                    // right after the push (takeover::relink_if_claude_source).
                    debug!(
                        slug,
                        error = %e,
                        "pusher: link deferred — takeover will replace the source dir"
                    );
                } else {
                    warn!(
                        slug,
                        error = %e,
                        "pusher: failed to link skill for Claude Code — canonical copy is still live"
                    );
                }
                outcome.link_refused = Some(link_path);
            }
        }

        // Antigravity does not scan `~/.agents/skills`; its global skills dir is
        // `~/.gemini/config/skills`. Alias there too when Antigravity is
        // installed. Best-effort: a failure never fails the push.
        if let Some(dir) = antigravity_skills_link_dir() {
            match super::links::link_dir(&skill_dir, &dir.join(slug)) {
                Ok(mode) => debug!(slug, ?mode, "pusher: linked skill for Antigravity"),
                Err(e) => warn!(slug, error = %e, "pusher: failed to link skill for Antigravity"),
            }
        }

        info!(slug, dest = %skill_dir.display(), "pusher: skill pushed to ~/.agents/skills/");
        Ok(outcome)
    }

    /// True when `path` is the recorded source of a pending adopt takeover
    /// for `slug`. Any DB error reads as "no" so a real conflict still WARNs.
    fn is_pending_takeover_source(&self, slug: &str, path: &Path) -> bool {
        let Ok(conn) = Connection::open(&self.db_path) else {
            return false;
        };
        let src: rusqlite::Result<String> = conn.query_row(
            "SELECT source_path FROM adopt_pending_takeovers WHERE slug = ?1",
            [slug],
            |r| r.get(0),
        );
        matches!(src, Ok(s) if Path::new(&s) == path)
    }

    /// Remove a skill from `~/.agents/skills/<slug>/`, dropping its
    /// `~/.claude/skills/<slug>` link.
    ///
    /// Order matters (F13): the link and the directory go first, the
    /// `managed_path_markers` row last. The row is the only trusted proof that
    /// a directory is ours — discovery and F1 adoption ignore the
    /// `.scenery-managed.json` file — so a crash mid-removal must never
    /// leave our directory on disk without its row, or the next scan would
    /// report (and F1 would re-adopt) Scenery's own leftover. With this
    /// order a crash leaves either the row plus (part of) the directory —
    /// retried here, or classified by drift — or the row alone, which drift
    /// retires as DELETED.
    ///
    /// Idempotent — returns `Ok(false)` if there was nothing to remove (and
    /// stays silent), `Ok(true)` if a link, directory or marker row was
    /// actually removed.
    pub fn remove_skill(&self, slug: &str) -> Result<bool> {
        if reconciler_disabled() {
            return Ok(false);
        }
        scenery_core::safe_path::validate_single_component("skill slug", slug)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // Held to the end: drift must not see the directory gone while the
        // row is still present (it would report a false "deleted").
        let _removal_lock = super::marker::lock_skill_removal(self.db_path.as_std_path())?;
        let mut removed = false;

        let skills_dir = resolve_skills_dir()?;
        let skill_dir = skills_dir.join(slug);
        let path_key = skill_dir.to_string_lossy().to_string();

        // Drop Claude Code's link before removing the canonical directory, so
        // a crash between the two leaves a dangling link rather than a live
        // link to deleted content.
        let link_path = resolve_claude_link_dir()?.join(slug);
        let link_present = link_path.symlink_metadata().is_ok();
        match super::links::unlink_dir(&link_path) {
            Ok(()) => removed |= link_present,
            Err(e) => warn!(slug, error = %e, "pusher: failed to unlink skill for Claude Code"),
        }

        if let Some(dir) = antigravity_skills_link_dir() {
            let alias = dir.join(slug);
            let present = alias.symlink_metadata().is_ok();
            match super::links::unlink_dir(&alias) {
                Ok(()) => removed |= present,
                Err(e) => warn!(slug, error = %e, "pusher: failed to unlink skill for Antigravity"),
            }
        }

        // Remove the canonical directory.
        if skill_dir.exists() {
            // Defense-in-depth: never delete Anthropic-native content.
            ownership::ensure_not_native(&skill_dir)?;
            // Same ownership rule as push_skill's guard and `links::unlink_dir`:
            // a real directory here is removed only when it is ours — it
            // carries the `.scenery-managed.json` marker, or our own DB row
            // sits at this exact path (a retry after a crash may find the
            // marker file already deleted). Without this, a slug collision
            // that push_skill refused to annex would still be destroyed here.
            // Refuse rather than delete; the caller already treats
            // `remove_skill` failures as non-fatal (warn-and-continue).
            let row_present = {
                let conn = Connection::open(&self.db_path)
                    .context("pusher: failed to open state DB for ownership check")?;
                is_already_marked(&conn, &path_key)
                    .context("pusher: failed to check managed_path_markers")?
            };
            if !skill_dir.is_symlink() && !row_present && !ownership::is_scenery_managed(&skill_dir)
            {
                // User content that is not ours is not something a teardown
                // is supposed to remove, so this is not a failure: leave it
                // and report what we did remove.
                warn!(
                    slug,
                    path = %skill_dir.display(),
                    "pusher: refusing to remove the directory — not Scenery-managed; left in place"
                );
                return Ok(removed);
            }
            if !skill_dir.is_symlink() {
                // The user's OWN skill may hold edits nothing else has: back
                // them up first, and refuse to delete if that fails.
                self.back_up_own_skill_before_removal(slug, &skill_dir, &path_key)?;
            }
            if skill_dir.is_symlink() {
                fs::remove_file(&skill_dir)
            } else {
                fs::remove_dir_all(&skill_dir)
            }
            .with_context(|| {
                format!(
                    "pusher: failed to remove skill dir: {}",
                    skill_dir.display()
                )
            })?;
            removed = true;
            info!(slug, "pusher: skill removed from ~/.agents/skills/");
        } else {
            debug!(
                slug,
                "pusher: skill dir absent — nothing to remove (idempotent)"
            );
        }

        // Only now that nothing of ours is left on disk, drop the row.
        removed |= self
            .delete_db_marker(&path_key)
            .context("pusher: failed to delete managed_path_markers for skill")?;

        Ok(removed)
    }

    /// Before a teardown (revoke, purge, deactivate) deletes an auto-managed
    /// skill whose tree differs from what Scenery last wrote, copy the current
    /// tree into `restore-backups/<unique ts>/removed-own/<slug>` and journal
    /// it (`source = managed`, so uninstall does not put it back over newer
    /// work; the copy just stays on disk). An unedited own skill needs no
    /// backup. If the backup fails the delete is refused.
    fn back_up_own_skill_before_removal(
        &self,
        slug: &str,
        skill_dir: &Path,
        path_key: &str,
    ) -> Result<()> {
        let prov = {
            let conn = Connection::open(&self.db_path)
                .context("pusher: failed to open state DB for provenance")?;
            marker_provenance(&conn, path_key)
        };
        if !prov.is_auto_managed() {
            return Ok(());
        }
        if !own_tree_has_unuploaded_edits(skill_dir, &prov) {
            return Ok(()); // exactly what Scenery wrote
        }
        let journal = self.journal();
        let src = camino::Utf8Path::from_path(skill_dir).ok_or_else(|| {
            anyhow::anyhow!("pusher: path is not valid UTF-8: {}", skill_dir.display())
        })?;
        let backup = journal
            .backup_tree_unique("removed-own", slug, src)
            .with_context(|| {
                format!(
                    "pusher: refusing to remove {}: could not back up the user's own skill first",
                    skill_dir.display()
                )
            })?;
        journal
            .append(
                JournalEntry::new(
                    JournalOp::FileDelete,
                    JournalSource::Managed,
                    skill_dir.to_string_lossy().to_string(),
                )
                .with_slug(slug)
                .with_backup_path(backup.to_string())
                .with_detail(serde_json::json!({"reason": "teardown_of_own_skill"})),
            )
            .context("pusher: failed to journal the own-skill backup")?;
        info!(slug, backup = %backup, "pusher: backed up own skill before removal");
        Ok(())
    }

    // ── MCP ───────────────────────────────────────────────────────────────────

    /// Add or update a `mcpServers.<slug>` entry in `~/.claude.json`.
    ///
    /// The entry shape written is:
    /// ```json
    /// { "command": "scenery", "args": ["mcp", "serve", "--server", "<slug>"] }
    /// ```
    ///
    /// The function holds an exclusive file lock on `~/.claude.json` for the
    /// entire read-modify-write cycle so concurrent pushes cannot corrupt the
    /// file.  If `~/.claude.json` does not exist, it is created with 0600
    /// permissions.
    ///
    /// Other entries in `mcpServers` (e.g. Anthropic's built-ins) are
    /// preserved.
    ///
    /// `brokered` classifies the restore-journal entry: `true` for
    /// credential-brokered servers routed through the gateway (has a
    /// `gateway_url`), `false` for directly-configured (`server_config`)
    /// servers. Either way the entry is `source=brokered`/`managed` — this
    /// pushed key only ever worked through Scenery, so uninstall removes
    /// it outright rather than restoring it.
    pub fn push_mcp(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        brokered: bool,
    ) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let claude_json = resolve_claude_json()?;

        let entry_value = scenery_mcp_entry(slug);

        modify_claude_json(&claude_json, |root| {
            // Ensure root is an object.
            if !root.is_object() {
                *root = Value::Object(serde_json::Map::new());
            }
            if let Value::Object(ref mut top_map) = root {
                let servers = top_map
                    .entry("mcpServers".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Value::Object(ref mut srv_map) = servers {
                    srv_map.insert(slug.to_string(), entry_value.clone());
                }
            }
        })
        .with_context(|| format!("pusher: failed to modify ~/.claude.json for mcp slug={slug}"))?;

        // Upsert SQLite marker (virtual key: path:<slug>).
        let path_key = format!("{}:{}", claude_json.display(), slug);
        let sha256 = hex_sha256(entry_value.to_string().as_bytes());
        let now_ts = scenery_core::timefmt::now_rfc3339();
        self.upsert_db_marker(&path_key, "mcp", slug, installation_id, &sha256, &now_ts)
            .context("pusher: failed to upsert managed_path_markers for mcp")?;

        let source = if brokered {
            JournalSource::Brokered
        } else {
            JournalSource::Managed
        };
        self.journal_push(
            JournalEntry::new(
                JournalOp::ArtifactPush,
                source,
                claude_json.to_string_lossy().to_string(),
            )
            .with_slug(slug)
            .with_client("Claude Code")
            .with_detail(serde_json::json!({
                "server_key": slug,
                "mcp_key": "mcpServers",
                "installation_id": installation_id,
            })),
        );

        info!(slug, "pusher: MCP entry written to ~/.claude.json");
        Ok(())
    }

    /// Remove the `mcpServers.<slug>` entry from `~/.claude.json`.
    ///
    /// Idempotent — returns `Ok(false)` (silently) if the file or the entry
    /// is absent, `Ok(true)` if an entry or marker row was removed.
    pub fn remove_mcp(&self, slug: &str) -> Result<bool> {
        if reconciler_disabled() {
            return Ok(false);
        }

        let claude_json = resolve_claude_json()?;
        let path_key = format!("{}:{}", claude_json.display(), slug);

        // Delete SQLite marker first.
        let mut removed = self
            .delete_db_marker(&path_key)
            .context("pusher: failed to delete managed_path_markers for mcp")?;

        if !claude_json.exists() {
            debug!(
                slug,
                "pusher: ~/.claude.json absent — nothing to remove (idempotent)"
            );
            return Ok(removed);
        }

        let mut entry_present = false;
        modify_claude_json(&claude_json, |root| {
            if let Some(Value::Object(ref mut map)) = root.get_mut("mcpServers") {
                entry_present = map.remove(slug).is_some();
            }
        })
        .with_context(|| {
            format!("pusher: failed to modify ~/.claude.json for mcp removal slug={slug}")
        })?;

        removed |= entry_present;
        if entry_present {
            info!(slug, "pusher: MCP entry removed from ~/.claude.json");
        }
        Ok(removed)
    }

    /// Remove Scenery-owned `~/.claude.json` MCP entries that no live
    /// `mcp_installations` row backs any more, returning the slugs removed.
    ///
    /// For a revoke whose `mcp_installations` row is already gone (deleted by
    /// an older runner, or by a failed teardown) nothing local maps the
    /// server UUID to its slug, so the entry could never be removed by key.
    /// An entry counts as Scenery's only when BOTH hold: a `kind = 'mcp'`
    /// marker row exists for exactly `<claude.json>:<slug>`, and the live
    /// entry is still byte-for-byte what [`Self::push_mcp`] writes. A marker
    /// whose entry the user has since replaced or removed is dropped without
    /// touching the entry. Anything without a marker row is never touched.
    ///
    /// Not serialized against another server's install (events lock per
    /// artifact key; taking every lock is not feasible), so the race is
    /// closed by re-checking, per slug, for a live row at the point of each
    /// destructive step: inside `modify_claude_json`'s file lock right before
    /// the entry is removed, and in one `BEGIN IMMEDIATE` transaction with the
    /// marker delete. An install upserts its row BEFORE it writes the entry
    /// or marker, so a row that appears after the initial read is seen by
    /// the re-check and the slug is skipped (never removed, never re-pushed
    /// by anyone else). A row that has not been upserted yet cannot have
    /// pushed anything, so there is nothing fresh to lose.
    ///
    /// Markers are read BEFORE the live rows, which covers an install that
    /// finished entirely between the two reads.
    pub fn remove_orphaned_mcp_entries(&self) -> Result<Vec<String>> {
        self.sweep_orphaned_mcp_entries(&|| {})
    }

    /// [`Self::remove_orphaned_mcp_entries`] with a hook that runs right
    /// after the initial live-row read (test seam for the reinstall race).
    pub(crate) fn sweep_orphaned_mcp_entries(
        &self,
        after_live_read: &dyn Fn(),
    ) -> Result<Vec<String>> {
        if reconciler_disabled() {
            return Ok(Vec::new());
        }
        let claude_json = resolve_claude_json()?;
        let prefix = format!("{}:", claude_json.display());

        let conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for orphaned mcp lookup")?;
        let mut stmt = conn
            .prepare("SELECT path, slug FROM managed_path_markers WHERE kind = 'mcp'")
            .context("pusher: failed to prepare mcp marker lookup")?;
        let markers = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("pusher: failed to query mcp markers")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("pusher: failed to read mcp markers")?;
        let live: std::collections::HashSet<String> = {
            let mut stmt = conn
                .prepare("SELECT mcp_server_name FROM mcp_installations")
                .context("pusher: failed to prepare mcp_installations lookup")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .context("pusher: failed to query mcp_installations")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("pusher: failed to read mcp_installations")?;
            names.iter().map(|n| crate::mcp_server_slug(n)).collect()
        };
        after_live_read();

        let mut removed = Vec::new();
        for (path, slug) in markers {
            if path != format!("{prefix}{slug}") || live.contains(&slug) {
                continue;
            }
            let expected = scenery_mcp_entry(&slug);
            let mut entry_removed = false;
            if claude_json.exists() {
                let mut recheck_err: Option<anyhow::Error> = None;
                modify_claude_json(&claude_json, |root| {
                    // Under the file lock: a row now present means a
                    // reinstall landed after our read; leave it alone.
                    match self.slug_has_live_row(&slug) {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(e) => {
                            recheck_err = Some(e);
                            return;
                        }
                    }
                    if let Some(Value::Object(ref mut map)) = root.get_mut("mcpServers") {
                        if map.get(&slug) == Some(&expected) {
                            map.remove(&slug);
                            entry_removed = true;
                        }
                    }
                })
                .with_context(|| {
                    format!("pusher: failed to modify ~/.claude.json for orphaned mcp slug={slug}")
                })?;
                if let Some(e) = recheck_err {
                    return Err(e);
                }
            }
            let marker_removed = self
                .delete_orphan_marker_if_no_row(&path, &slug)
                .context("pusher: failed to delete orphaned managed_path_markers for mcp")?;
            if entry_removed {
                info!(
                    slug,
                    "pusher: orphaned MCP entry removed from ~/.claude.json"
                );
            }
            if entry_removed || marker_removed {
                removed.push(slug);
            }
        }
        Ok(removed)
    }

    /// Does any `mcp_installations` row currently map to `slug`?
    fn slug_has_live_row(&self, slug: &str) -> Result<bool> {
        let conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for live-row recheck")?;
        slug_has_row(&conn, slug)
    }

    /// Delete the marker unless a row for `slug` exists, atomically: the
    /// `IMMEDIATE` transaction blocks a concurrent install's row upsert, so
    /// the check and the delete cannot straddle it.
    fn delete_orphan_marker_if_no_row(&self, path: &str, slug: &str) -> Result<bool> {
        let mut conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for marker deletion")?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("pusher: failed to begin marker deletion transaction")?;
        if slug_has_row(&tx, slug)? {
            return Ok(false);
        }
        let n = tx
            .execute(
                "DELETE FROM managed_path_markers WHERE path = ?1",
                rusqlite::params![path],
            )
            .context("pusher: failed to delete managed_path_markers row")?;
        tx.commit()
            .context("pusher: failed to commit marker deletion")?;
        Ok(n > 0)
    }

    // ── SQLite helpers ────────────────────────────────────────────────────────

    /// Provenance for the marker about to be written at `path` for `slug`:
    /// the pending auto-manage takeover's ids when one exists, else the
    /// existing row's, else legacy (all `None`).
    fn provenance_for(&self, slug: &str, path: &str) -> Provenance {
        let Ok(conn) = Connection::open(&self.db_path) else {
            return Provenance::default();
        };
        let pending = conn
            .query_row(
                "SELECT origin, skill_id, skill_uuid FROM adopt_pending_takeovers \
                 WHERE slug = ?1 AND origin IS NOT NULL",
                [slug],
                |row| {
                    Ok(Provenance {
                        origin: row.get(0)?,
                        skill_id: row.get(1)?,
                        skill_uuid: row.get(2)?,
                        ..Default::default()
                    })
                },
            )
            .ok();
        match pending {
            Some(p) if p.origin.as_deref() == Some(ORIGIN_AUTO_MANAGED) => p,
            _ => {
                let existing = marker_provenance(&conn, path);
                Provenance {
                    tree_hash: None,
                    hash_version: None,
                    ..existing
                }
            }
        }
    }

    fn upsert_db_marker(
        &self,
        path: &str,
        kind: &str,
        slug: &str,
        installation_id: Option<&str>,
        sha256: &str,
        migrated_at: &str,
    ) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for marker upsert")?;

        // Check if a row exists.
        let already = is_already_marked(&conn, path)
            .context("pusher: failed to check managed_path_markers")?;

        if already {
            // Refresh the hash: Scenery just wrote this content, and the
            // path+hash ownership check (F13) must keep recognising it.
            conn.execute(
                "UPDATE managed_path_markers SET source_sha256 = ?1 WHERE path = ?2",
                rusqlite::params![sha256, path],
            )
            .context("pusher: failed to refresh source_sha256 in managed_path_markers")?;
            // Update installation_id if provided.
            if let Some(id) = installation_id {
                update_db_marker_installation_id(&conn, path, id)
                    .context("pusher: failed to update installation_id in managed_path_markers")?;
            }
        } else {
            let marker = ManagedPathMarker {
                path: path.to_string(),
                kind: kind.to_string(),
                slug: slug.to_string(),
                installation_id: installation_id.map(str::to_string),
                source_sha256: sha256.to_string(),
                migrated_at: migrated_at.to_string(),
            };
            insert_db_marker(&conn, &marker)
                .context("pusher: failed to insert managed_path_markers row")?;
        }

        Ok(())
    }

    /// Returns whether a row was actually deleted.
    fn delete_db_marker(&self, path: &str) -> Result<bool> {
        let conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for marker deletion")?;
        let n = conn
            .execute(
                "DELETE FROM managed_path_markers WHERE path = ?1",
                rusqlite::params![path],
            )
            .context("pusher: failed to delete managed_path_markers row")?;
        Ok(n > 0)
    }
}

// ── Active-skill self-heal ──────────────────────────────────────────────────────
//
// RETIRED (this branch): `reclaim_active_skills`, the pre-v1.0.51 one-shot
// pass that materialised legacy installer symlinks at `resolve_skills_dir()/
// <slug>` into real F2-managed directories.
//
// The pivot moved `resolve_skills_dir()` to `~/.agents/skills`, a path that
// did not exist before the pivot, so the legacy symlink it looked for cannot
// be there — the pass was dormant. What it *could* still do was drive an
// unguarded `remove_file` into a now-shared root that `npx skills`, Cursor
// and Codex users populate. `push_missing_active_skills` below already covers
// the real job (re-push any active skill missing from the canonical root, and
// its link step heals whatever sits at the old Claude path), so the pass was
// removed rather than re-documented.

/// One-shot startup pass: for every active skill in `installed_skills`, if its
/// canonical `~/.agents/skills/<slug>` directory is **entirely absent**,
/// re-push it from the installed copy so the skill reappears for every client
/// (and gets re-linked at `~/.claude/skills/<slug>` for Claude Code).
///
/// This heals machines whose native skill dirs were removed out-of-band — e.g.
/// the v1.0.67 cleanup pass that (incorrectly) deleted installed skills along
/// with Scenery's own command-skills. Going forward it also self-heals any
/// user-installed skill dir that gets deleted while the daemon is down, and —
/// since the pivot to `~/.agents/skills` — any active skill that only has a
/// stale/legacy path at the old Claude location: `push_skill`'s link step
/// resolves that when this pass calls it. This subsumes the retired
/// `reclaim_active_skills` pass (see the note above).
///
/// Only acts on missing paths: existing real dirs are left to F2/drift. Reads
/// SKILL.md plus sibling files from `<install_root>/active/`. Per-skill
/// failures are logged at WARN and do not abort the pass. Returns the number
/// of skills re-pushed.
pub fn push_missing_active_skills(state: &AppState, pusher: &ManagedPathsPusher) -> Result<usize> {
    if reconciler_disabled() {
        return Ok(0);
    }
    let active = state
        .list_active_installed_skills()
        .context("heal: failed to list active installed skills")?;
    if active.is_empty() {
        return Ok(0);
    }

    let skills_dir = resolve_skills_dir()?;
    let mut healed: usize = 0;

    for (skill_id, install_root, _active_version) in active {
        let target = skills_dir.join(&skill_id);
        // Skip anything that already exists on disk (real dir or symlink) —
        // those are owned by F2/reclaim/drift, not this heal pass.
        if target.exists() || target.is_symlink() {
            continue;
        }
        let active_dir = Path::new(&install_root).join("active");
        let skill_md_path = active_dir.join("SKILL.md");
        let bytes = match fs::read(&skill_md_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    skill_id = %skill_id,
                    path = %skill_md_path.display(),
                    error = %e,
                    "heal: cannot read SKILL.md from installed copy — skipping"
                );
                continue;
            }
        };
        let refs = scenery_core::pack::collect_tree_files(&active_dir);

        match pusher.push_skill_files(&skill_id, None, &bytes, &refs, false) {
            Ok(_) => {
                healed += 1;
                info!(skill_id, "heal: re-pushed missing installed skill dir");
            }
            Err(e) => tracing::warn!(
                skill_id = %skill_id,
                error = %e,
                "heal: push_skill failed (non-fatal)"
            ),
        }
    }

    Ok(healed)
}

// ── Adopt-discovery push ──────────────────────────────────────────────────────

/// Push a skill from `source_path` (the discovery's on-disk location, e.g.
/// `~/.agents/skills/<slug>`) into `~/.agents/skills/<slug>/` as a real
/// F2-managed directory (linked at `~/.claude/skills/<slug>` for Claude
/// Code). Called from the SSE handler when the user adopts a discovery via
/// the portal.
///
/// For `kind="skill"` we read `<source_path>/SKILL.md` plus any sibling files
/// (prompts/, etc.) and hand them to `push_skill`. For other kinds we no-op
/// for now — plugin/mcp adopt is deferred to the same future release as
/// publish.
pub async fn push_adopted_discovery(
    state: &Arc<AppState>,
    slug: &str,
    kind: &str,
    source_path: &str,
) -> Result<()> {
    if reconciler_disabled() {
        return Ok(());
    }
    if kind != "skill" {
        tracing::debug!(slug, kind, "adopt: kind is not 'skill' — push deferred");
        return Ok(());
    }

    // Source IS the canonical managed directory: pushing it onto itself would
    // be refused as a collision. Conversion to managed happens when the
    // published version installs (reconciler + takeover), not from here, so a
    // pending-review adopt never marks the user's copy managed early.
    if super::takeover::source_is_canonical_dest(slug, Path::new(source_path)) {
        info!(
            slug,
            source = %source_path,
            "adopt: source_path is the canonical managed directory — \
             conversion to managed happens when the published version installs"
        );
        return Ok(());
    }

    let slug = slug.to_string();
    let source_path = source_path.to_string();
    let state_clone = Arc::clone(state);

    tokio::task::spawn_blocking(move || -> Result<()> {
        let source_dir = PathBuf::from(&source_path);
        let skill_md_path = source_dir.join("SKILL.md");
        let skill_md_bytes = fs::read(&skill_md_path).with_context(|| {
            format!("adopt: cannot read SKILL.md at {}", skill_md_path.display())
        })?;

        // Collect sibling files (everything except SKILL.md and our own
        // marker) so prompts/, scripts/ and other adjacent assets travel with
        // the skill into the managed dir, executable bits included.
        let refs = scenery_core::pack::collect_tree_files(&source_dir);

        let pusher = ManagedPathsPusher::new(&state_clone);
        pusher
            .push_skill_files(&slug, None, &skill_md_bytes, &refs, false)
            .context("adopt: push_skill failed")?;
        info!(
            slug,
            source = %source_path,
            "adopt: discovery pushed into ~/.agents/skills/"
        );
        Ok(())
    })
    .await
    .context("adopt: spawn_blocking panicked")?
}

// ── Path resolution ───────────────────────────────────────────────────────────

fn resolve_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("pusher: HOME directory not resolvable"))
}

/// The canonical managed-skill root: `~/.agents/skills/`.
fn resolve_skills_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".agents").join("skills"))
}

/// Where Claude Code looks: `~/.claude/skills/`. Scenery places only
/// symlinks here — never real content.
fn resolve_claude_link_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude").join("skills"))
}

/// Antigravity's global skills dir, `~/.gemini/config/skills/`, returned only
/// when Antigravity is present (`~/.gemini/config/` exists) so a machine
/// without it never gets a stray `~/.gemini` tree. Same path on every OS.
pub(crate) fn antigravity_skills_link_dir() -> Option<PathBuf> {
    let config = resolve_home().ok()?.join(".gemini").join("config");
    config.is_dir().then(|| config.join("skills"))
}

/// Return `true` if the F2-managed copy for `slug` is present on disk, i.e.
/// `~/.agents/skills/<slug>/SKILL.md` exists.
///
/// Used by the adopt-publish takeover flow (`managed_paths::takeover`) to
/// confirm the Scenery-managed replacement actually landed before removing
/// the original discovered `source_path` it is replacing.
pub fn managed_skill_present(slug: &str) -> Result<bool> {
    let skills_dir = resolve_skills_dir()?;
    Ok(skills_dir.join(slug).join("SKILL.md").exists())
}

/// Is the canonical projection of `slug` on disk — `SKILL.md` plus the
/// `.scenery-managed.json` marker under `~/.agents/skills/<slug>`?
///
/// The daemon flips `installed_skills` to `active` BEFORE it pushes, so that
/// row alone does not mean the files exist. The authoritative signal for
/// `scenery skill install` is the outcome the daemon records
/// (`projection_outcome`); this is only the fallback used when the daemon
/// never reported one. Resolves `~` with the same function the pusher uses.
pub fn canonical_projection_present(slug: &str) -> Result<bool> {
    Ok(canonical_projection_present_in(&resolve_home()?, slug))
}

/// Is the full projection of `slug` intact — canonical dir + marker AND the
/// Claude link? Always `true` when the filesystem reconciler is disabled.
pub fn projection_intact(slug: &str) -> Result<bool> {
    Ok(projection_state(slug)? == ProjectionState::Intact)
}

/// Result of checking a skill's projection directly on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionState {
    /// Canonical dir + marker, and the Claude path is a symlink to it (or a
    /// Scenery-marked copy), or the filesystem reconciler is disabled.
    Intact,
    /// A real directory Scenery does not own sits at the Claude link path.
    LinkBlocked(PathBuf),
    /// Missing or stale pieces; the daemon still has work to do.
    Incomplete,
}

/// Direct, read-only check of `slug`'s projection (see [`ProjectionState`]).
pub fn projection_state(slug: &str) -> Result<ProjectionState> {
    if reconciler_disabled() {
        return Ok(ProjectionState::Intact);
    }
    Ok(projection_state_in(&resolve_home()?, slug))
}

fn projection_state_in(home: &Path, slug: &str) -> ProjectionState {
    if !canonical_projection_present_in(home, slug) {
        return ProjectionState::Incomplete;
    }
    let canonical = home.join(".agents").join("skills").join(slug);
    let link = home.join(".claude").join("skills").join(slug);
    if link.is_symlink() {
        return match super::links::link_is_intact(&canonical, &link) {
            Ok(true) => ProjectionState::Intact,
            _ => ProjectionState::Incomplete,
        };
    }
    if link.exists() {
        return if ownership::is_scenery_managed(&link) {
            ProjectionState::Intact
        } else {
            ProjectionState::LinkBlocked(link)
        };
    }
    ProjectionState::Incomplete
}

fn canonical_projection_present_in(home: &Path, slug: &str) -> bool {
    let dir = home.join(".agents").join("skills").join(slug);
    dir.join("SKILL.md").exists() && dir.join(ownership::MANAGED_MARKER_FILENAME).exists()
}

fn slug_has_row(conn: &Connection, slug: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare("SELECT mcp_server_name FROM mcp_installations")
        .context("pusher: failed to prepare mcp_installations lookup")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("pusher: failed to query mcp_installations")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("pusher: failed to read mcp_installations")?;
    Ok(names.iter().any(|n| crate::mcp_server_slug(n) == slug))
}

/// The `mcpServers.<slug>` entry Scenery writes for a managed MCP server.
fn scenery_mcp_entry(slug: &str) -> Value {
    serde_json::json!({
        "command": "scenery",
        "args": ["mcp", "serve", "--server", slug],
    })
}

fn resolve_claude_json() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude.json"))
}

// ── Atomic write ──────────────────────────────────────────────────────────────

/// Write `content` to `dest` via a sibling temp file + atomic rename.
fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    atomic_write_with_mode(dest, content, false)
}

/// Borrow `(path, bytes)` pairs as non-executable files for the shared writer.
fn as_plain_files(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8], bool)> {
    files
        .iter()
        .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice(), false))
        .collect()
}

/// Write `content` to `dest` through a temp file in the same directory.
/// `executable` gives the file execute bits matching its read bits (the umask
/// still decides who may read it), so a skill's `scripts/` stay runnable.
///
/// The temp name is the full file name plus a suffix, not `with_extension`:
/// that would map both `run.sh` and `run.tmp` to `run.tmp`, and the second
/// write would clobber the first file.
fn atomic_write_with_mode(dest: &Path, content: &[u8], executable: bool) -> Result<()> {
    let mut tmp_name = dest.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".scenery-tmp");
    let tmp = dest.with_file_name(tmp_name);
    fs::write(&tmp, content)
        .with_context(|| format!("pusher: failed to write tmp file: {}", tmp.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&tmp)
            .with_context(|| format!("pusher: failed to stat {}", tmp.display()))?
            .permissions()
            .mode();
        fs::set_permissions(
            &tmp,
            fs::Permissions::from_mode(mode | ((mode & 0o444) >> 2)),
        )
        .with_context(|| format!("pusher: failed to chmod {}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    fs::rename(&tmp, dest).with_context(|| {
        format!(
            "pusher: failed to rename {} → {}",
            tmp.display(),
            dest.display()
        )
    })?;
    Ok(())
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ── ~/.claude.json read-lock-modify-write ─────────────────────────────────────

/// Modify `~/.claude.json` atomically and safely under concurrent access.
///
/// Uses a **separate lock file** (`~/.claude.json.lock`) rather than locking
/// the JSON file itself. This avoids the POSIX rename + flock race where a
/// thread opens the target file before a rename and ends up holding an fd to
/// the orphaned (pre-rename) inode, reading stale content.
///
/// Protocol:
/// 1. Open/create the lock file (`~/.claude.json.lock`) and acquire an exclusive flock.
/// 2. Read current `~/.claude.json` via `fs::read_to_string` (sees the latest rename).
/// 3. Parse, mutate, serialise.
/// 4. Write to a thread-unique temp file in the same directory.
/// 5. Atomic rename: temp → `~/.claude.json`.
/// 6. Release the flock.
///
/// If `~/.claude.json` does not exist it is created with 0600 permissions.
fn modify_claude_json<F>(path: &Path, mutate: F) -> Result<()>
where
    F: FnOnce(&mut Value),
{
    let parent_dir = path.parent().ok_or_else(|| {
        anyhow::anyhow!("pusher: path has no parent directory: {}", path.display())
    })?;

    // Step 1: acquire an exclusive lock on a stable lock file (not the JSON
    // file itself, which gets replaced on every write).
    let lock_path = parent_dir.join(".claude.json.lock");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // JUSTIFICATION: lock file is used only for flock; we never write to it,
        // so we must not truncate any advisory data another process might store.
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("pusher: failed to open lock file: {}", lock_path.display()))?;
    lock_file.lock_exclusive().with_context(|| {
        format!(
            "pusher: failed to acquire exclusive lock: {}",
            lock_path.display()
        )
    })?;

    // Step 2: read current JSON (re-open by path so we always see the latest
    // rename, even if another writer swapped the file since we took the lock).
    let content = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("pusher: failed to read: {}", path.display()))?
    } else {
        String::new()
    };

    // Step 3: parse and mutate.
    let mut root: Value = if content.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&content)
            .context("pusher: ~/.claude.json contains invalid JSON — aborting push")?
    };

    mutate(&mut root);

    let new_content = serde_json::to_string_pretty(&root)
        .context("pusher: failed to serialise ~/.claude.json")?;
    // Round-trip sanity check.
    let _: Value =
        serde_json::from_str(&new_content).context("pusher: serialised JSON did not round-trip")?;

    // Step 4: atomic replace, keeping the file's mode (0600 on first create).
    atomic_file::write_atomic(path, new_content.as_bytes(), Some(0o600))
        .with_context(|| format!("pusher: failed to replace {}", path.display()))?;

    // Step 5: release the lock.  `fs2` unlocks when the file handle drops.
    // Explicitly drop to make the release point clear.
    drop(lock_file);

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "pusher_tests.rs"]
mod tests;
