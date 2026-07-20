//! F2 managed-paths pusher — writes VectorHawk-managed items into clients'
//! native directories on install and removes them on deactivate.
//!
//! # What it writes
//!
//! | Item   | Destination                                           |
//! |--------|-------------------------------------------------------|
//! | Skill  | `~/.agents/skills/<slug>/` + `.vectorhawk-managed.json` marker, linked at `~/.claude/skills/<slug>` for Claude Code |
//! | MCP    | Entry in `~/.claude.json` `mcpServers` block           |
//! | Plugin | `~/.claude/plugins/<slug>/.claude-plugin/plugin.json`  |
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
//! `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` is set in the environment.
//! Callers do not need to guard this themselves.
//!
//! # Error policy
//!
//! `push_*` failures are non-fatal to the install flow: callers log WARN and
//! continue.  `remove_*` failures are also non-fatal.

use super::marker::{
    insert_db_marker, is_already_marked, update_db_marker_installation_id, write_file_marker,
    ManagedPathMarker,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{debug, info, warn};
use vectorhawkd_core::restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal};
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::ownership;

// ── Env-var gate ──────────────────────────────────────────────────────────────

/// Return `true` when the filesystem reconciler is disabled.
pub(crate) fn reconciler_disabled() -> bool {
    std::env::var_os("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_some()
}

// ── ManagedPathsPusher ────────────────────────────────────────────────────────

/// Pushes VectorHawk-managed items into Claude Code's native directories.
///
/// Cheaply cloneable — the only fields are two paths.
pub struct ManagedPathsPusher {
    db_path: camino::Utf8PathBuf,
    root_dir: camino::Utf8PathBuf,
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
    /// artifact only ever worked because VectorHawk put it there
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
    /// 3. Writes a `.vectorhawk-managed.json` marker into the directory.
    /// 4. Upserts the `managed_path_markers` SQLite row.
    pub fn push_skill(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        skill_md_bytes: &[u8],
        referenced_files: &[(String, Vec<u8>)],
    ) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let skills_dir = resolve_skills_dir()?;
        let skill_dir = skills_dir.join(slug);

        // If a legacy symlink exists at the target path (left over from the
        // pre-v1.0.51 installer that owned this path), unlink it so we can
        // create a real directory we exclusively own. Writing files THROUGH
        // a symlink would leak modifications back into the installer's
        // versioned directory and let the legacy state machine resurrect
        // itself on the next reconcile.
        if skill_dir.is_symlink() {
            fs::remove_file(&skill_dir).with_context(|| {
                format!(
                    "pusher: failed to remove legacy symlink at {}",
                    skill_dir.display()
                )
            })?;
            debug!(
                slug,
                "pusher: replaced legacy installer symlink with managed dir"
            );
        }

        fs::create_dir_all(&skill_dir).with_context(|| {
            format!(
                "pusher: failed to create skill dir: {}",
                skill_dir.display()
            )
        })?;

        // Compute source hash from SKILL.md content.
        let sha256 = hex_sha256(skill_md_bytes);

        // Write SKILL.md atomically.
        atomic_write(&skill_dir.join("SKILL.md"), skill_md_bytes)
            .context("pusher: failed to write SKILL.md")?;

        // Write referenced files.
        for (rel_path, content) in referenced_files {
            let dest = skill_dir.join(rel_path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "pusher: failed to create dir for referenced file: {}",
                        parent.display()
                    )
                })?;
            }
            atomic_write(&dest, content).with_context(|| {
                format!(
                    "pusher: failed to write referenced file: {}",
                    dest.display()
                )
            })?;
        }

        // Write file marker into the skill directory.
        let now_ts = chrono::Utc::now().to_rfc3339();
        write_file_marker(&skill_dir, installation_id, &sha256, &now_ts)
            .context("pusher: failed to write .vectorhawk-managed.json for skill")?;

        // Upsert SQLite marker.
        let path_key = skill_dir.to_string_lossy().to_string();
        self.upsert_db_marker(&path_key, "skill", slug, installation_id, &sha256, &now_ts)
            .context("pusher: failed to upsert managed_path_markers for skill")?;

        // Restore journal: this directory only exists because VectorHawk put
        // it there (source=managed) — uninstall removes it outright, no
        // backup needed.
        self.journal_push(
            JournalEntry::new(JournalOp::ArtifactPush, JournalSource::Managed, path_key)
                .with_slug(slug)
                .with_client("all")
                .with_detail(serde_json::json!({"installation_id": installation_id})),
        );

        // Surface the skill to Claude Code, which does not read
        // `~/.agents/skills`. Cursor, Codex, and Gemini CLI scan the canonical
        // path natively and need nothing here.
        //
        // Guard decision (see pusher_tests.rs / task-4-report.md for the full
        // writeup): `link_dir` will replace a real directory at `link_path`
        // when it alone carries a `.vectorhawk-managed.json` marker — an
        // existence-only check, not a SQLite cross-check. We deliberately do
        // NOT add an extra `managed_path_markers` lookup here: a marked
        // directory is, by this codebase's own ownership invariant
        // (`ownership::is_vectorhawk_managed`), never supposed to hold
        // anything VectorHawk didn't put there itself, so its content is
        // reproducible from the very `skill_md_bytes`/`referenced_files` this
        // call just wrote to the canonical directory. A DB cross-check would
        // also actively misfire on a legitimate, common case: if a prior
        // `remove_skill` deleted the SQLite row but crashed before deleting
        // the directory, the leftover marked directory would have NO
        // matching row, and a cross-check would refuse to relink — leaving
        // Claude Code pointed at stale content instead of finishing the
        // cleanup. Pre-Task-5-migration, a legitimate `~/.claude/skills/<slug>`
        // real directory from before this pivot carries the same marker
        // invariant, so it is safe for this call to replace it with a link to
        // the freshly-written canonical copy.
        let link_path = resolve_claude_link_dir()?.join(slug);
        match super::links::link_dir(&skill_dir, &link_path) {
            Ok(mode) => debug!(slug, ?mode, "pusher: linked skill for Claude Code"),
            Err(e) => warn!(
                slug,
                error = %e,
                "pusher: failed to link skill for Claude Code — canonical copy is still live"
            ),
        }

        info!(slug, dest = %skill_dir.display(), "pusher: skill pushed to ~/.agents/skills/");
        Ok(())
    }

    /// Remove a skill from `~/.agents/skills/<slug>/`, dropping its
    /// `~/.claude/skills/<slug>` link.
    ///
    /// Deletes the marker from SQLite first to avoid a race where F3's drift
    /// detector sees a manageless directory mid-cleanup, then removes the
    /// directory.  Idempotent — returns `Ok(())` if the directory is absent.
    pub fn remove_skill(&self, slug: &str) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let skills_dir = resolve_skills_dir()?;
        let skill_dir = skills_dir.join(slug);
        let path_key = skill_dir.to_string_lossy().to_string();

        // Delete SQLite marker first.
        self.delete_db_marker(&path_key)
            .context("pusher: failed to delete managed_path_markers for skill")?;

        // Drop Claude Code's link before removing the canonical directory, so
        // a crash between the two leaves a dangling link rather than a live
        // link to deleted content.
        let link_path = resolve_claude_link_dir()?.join(slug);
        if let Err(e) = super::links::unlink_dir(&link_path) {
            warn!(slug, error = %e, "pusher: failed to unlink skill for Claude Code");
        }

        // Remove the canonical directory.
        if skill_dir.exists() {
            // Defense-in-depth: never delete Anthropic-native content.
            ownership::ensure_not_native(&skill_dir)?;
            fs::remove_dir_all(&skill_dir).with_context(|| {
                format!(
                    "pusher: failed to remove skill dir: {}",
                    skill_dir.display()
                )
            })?;
            info!(slug, "pusher: skill removed from ~/.agents/skills/");
        } else {
            debug!(
                slug,
                "pusher: skill dir absent — nothing to remove (idempotent)"
            );
        }

        Ok(())
    }

    // ── MCP ───────────────────────────────────────────────────────────────────

    /// Add or update a `mcpServers.<slug>` entry in `~/.claude.json`.
    ///
    /// The entry shape written is:
    /// ```json
    /// { "command": "vectorhawk", "args": ["mcp", "serve", "--server", "<slug>"] }
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
    /// pushed key only ever worked through VectorHawk, so uninstall removes
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

        let entry_value = serde_json::json!({
            "command": "vectorhawk",
            "args": ["mcp", "serve", "--server", slug],
        });

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
        let now_ts = chrono::Utc::now().to_rfc3339();
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
    /// Idempotent — returns `Ok(())` if the file or the entry is absent.
    pub fn remove_mcp(&self, slug: &str) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let claude_json = resolve_claude_json()?;
        let path_key = format!("{}:{}", claude_json.display(), slug);

        // Delete SQLite marker first.
        self.delete_db_marker(&path_key)
            .context("pusher: failed to delete managed_path_markers for mcp")?;

        if !claude_json.exists() {
            debug!(
                slug,
                "pusher: ~/.claude.json absent — nothing to remove (idempotent)"
            );
            return Ok(());
        }

        modify_claude_json(&claude_json, |root| {
            if let Some(Value::Object(ref mut map)) = root.get_mut("mcpServers") {
                map.remove(slug);
            }
        })
        .with_context(|| {
            format!("pusher: failed to modify ~/.claude.json for mcp removal slug={slug}")
        })?;

        info!(slug, "pusher: MCP entry removed from ~/.claude.json");
        Ok(())
    }

    // ── Plugin ────────────────────────────────────────────────────────────────

    /// Write a plugin manifest to `~/.claude/plugins/<slug>/.claude-plugin/plugin.json`.
    ///
    /// TODO(F2): Plugin install fans out into skill+mcp installs which already
    /// get pushed via `push_skill` / `push_mcp`.  This stub is a no-op for now
    /// to satisfy the interface.  Full plugin push will be implemented when
    /// there is a daemon-side plugin install handler.
    pub fn push_plugin(
        &self,
        slug: &str,
        installation_id: Option<&str>,
        manifest_json: &Value,
    ) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let plugins_dir = resolve_plugins_dir()?;
        let plugin_dir = plugins_dir.join(slug).join(".claude-plugin");

        fs::create_dir_all(&plugin_dir).with_context(|| {
            format!(
                "pusher: failed to create plugin dir: {}",
                plugin_dir.display()
            )
        })?;

        let manifest_bytes = serde_json::to_vec_pretty(manifest_json)
            .context("pusher: failed to serialise plugin manifest")?;

        let dest = plugin_dir.join("plugin.json");
        atomic_write(&dest, &manifest_bytes).context("pusher: failed to write plugin.json")?;

        // Marker lives in the parent slug directory.
        let sha256 = hex_sha256(&manifest_bytes);
        let now_ts = chrono::Utc::now().to_rfc3339();
        let marker_dir = plugins_dir.join(slug);
        write_file_marker(&marker_dir, installation_id, &sha256, &now_ts)
            .context("pusher: failed to write .vectorhawk-managed.json for plugin")?;

        let path_key = marker_dir.to_string_lossy().to_string();
        self.upsert_db_marker(&path_key, "plugin", slug, installation_id, &sha256, &now_ts)
            .context("pusher: failed to upsert managed_path_markers for plugin")?;

        self.journal_push(
            JournalEntry::new(JournalOp::ArtifactPush, JournalSource::Managed, path_key)
                .with_slug(slug)
                .with_client("Claude Code")
                .with_detail(serde_json::json!({"installation_id": installation_id})),
        );

        info!(slug, dest = %dest.display(), "pusher: plugin manifest pushed to ~/.claude/plugins/");
        Ok(())
    }

    /// Remove a plugin from `~/.claude/plugins/<slug>/`.
    ///
    /// Idempotent.
    pub fn remove_plugin(&self, slug: &str) -> Result<()> {
        if reconciler_disabled() {
            return Ok(());
        }

        let plugins_dir = resolve_plugins_dir()?;
        let plugin_dir = plugins_dir.join(slug);
        let path_key = plugin_dir.to_string_lossy().to_string();

        // Delete SQLite marker first.
        self.delete_db_marker(&path_key)
            .context("pusher: failed to delete managed_path_markers for plugin")?;

        if plugin_dir.exists() {
            // Defense-in-depth: never delete Anthropic-native content.
            ownership::ensure_not_native(&plugin_dir)?;
            fs::remove_dir_all(&plugin_dir).with_context(|| {
                format!(
                    "pusher: failed to remove plugin dir: {}",
                    plugin_dir.display()
                )
            })?;
            info!(slug, "pusher: plugin removed from ~/.claude/plugins/");
        } else {
            debug!(
                slug,
                "pusher: plugin dir absent — nothing to remove (idempotent)"
            );
        }

        Ok(())
    }

    // ── SQLite helpers ────────────────────────────────────────────────────────

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

    fn delete_db_marker(&self, path: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("pusher: failed to open state DB for marker deletion")?;
        conn.execute(
            "DELETE FROM managed_path_markers WHERE path = ?1",
            rusqlite::params![path],
        )
        .context("pusher: failed to delete managed_path_markers row")?;
        Ok(())
    }
}

// ── Legacy reclaim ────────────────────────────────────────────────────────────

/// One-shot startup pass: for every active skill in `installed_skills`, if
/// `resolve_skills_dir()/<slug>` (the canonical managed-skill root) is still a
/// legacy symlink left behind by the pre-v1.0.51 installer, materialize it as
/// a real F2-managed directory.
///
/// Reads `<install_root>/active/SKILL.md` to source the canonical content
/// (referenced files are not migrated — they'll be re-pushed on the next
/// install of that skill). Sets the F2 marker so future drift scans treat
/// the path as managed. Skills that are already F2-marked are skipped.
///
/// Note: since the pivot to `~/.agents/skills` as the canonical root, the
/// pre-v1.0.51 legacy symlink this pass targets can only ever have existed at
/// the old canonical location, `~/.claude/skills/<slug>` — never at
/// `~/.agents/skills/<slug>`, which did not exist before this pivot. This
/// pass is therefore effectively a no-op going forward; a legacy symlink at
/// `~/.claude/skills/<slug>` (the Claude *link* path now) is instead healed
/// the moment `push_skill` next runs for that slug, since its link step
/// replaces whatever sits at the Claude path — see `push_missing_active_skills`
/// below, which still finds and re-pushes any active skill missing from the
/// canonical root and so subsumes this pass's job.
///
/// Failures for one skill are logged at WARN and do not abort the pass.
///
/// Returns the number of skills reclaimed.
pub fn reclaim_active_skills(state: &AppState, pusher: &ManagedPathsPusher) -> Result<usize> {
    if reconciler_disabled() {
        return Ok(0);
    }
    let active = state
        .list_active_installed_skills()
        .context("reclaim: failed to list active installed skills")?;
    if active.is_empty() {
        return Ok(0);
    }

    let skills_dir = resolve_skills_dir()?;
    let mut reclaimed: usize = 0;

    for (skill_id, install_root, _active_version) in active {
        let target = skills_dir.join(&skill_id);
        // Only act on legacy symlinks. Existing real dirs are already F2's
        // (either pushed by an earlier install or migrated by F1).
        if !target.is_symlink() {
            continue;
        }
        let skill_md_path = Path::new(&install_root).join("active").join("SKILL.md");
        let bytes = match fs::read(&skill_md_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    skill_id = %skill_id,
                    path = %skill_md_path.display(),
                    error = %e,
                    "reclaim: cannot read SKILL.md from legacy install — skipping"
                );
                continue;
            }
        };
        match pusher.push_skill(&skill_id, None, &bytes, &[]) {
            Ok(()) => {
                reclaimed += 1;
                info!(
                    skill_id,
                    "reclaim: legacy installer symlink replaced with F2-managed dir"
                );
            }
            Err(e) => {
                tracing::warn!(
                    skill_id = %skill_id,
                    error = %e,
                    "reclaim: F2 push_skill failed — leaving legacy symlink in place"
                );
            }
        }
    }

    Ok(reclaimed)
}

// ── Active-skill self-heal ──────────────────────────────────────────────────────

/// One-shot startup pass: for every active skill in `installed_skills`, if its
/// canonical `~/.agents/skills/<slug>` directory is **entirely absent**,
/// re-push it from the installed copy so the skill reappears for every client
/// (and gets re-linked at `~/.claude/skills/<slug>` for Claude Code).
///
/// This heals machines whose native skill dirs were removed out-of-band — e.g.
/// the v1.0.67 cleanup pass that (incorrectly) deleted installed skills along
/// with VectorHawk's own command-skills. Going forward it also self-heals any
/// user-installed skill dir that gets deleted while the daemon is down, and —
/// since the pivot to `~/.agents/skills` — any active skill that only has a
/// stale/legacy path at the old Claude location: `push_skill`'s link step
/// resolves that when this pass calls it, subsuming [`reclaim_active_skills`]'s
/// now-effectively-dormant job (see its doc comment).
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
        let mut refs: Vec<(String, Vec<u8>)> = Vec::new();
        collect_extras(&active_dir, &active_dir, &mut refs);

        match pusher.push_skill(&skill_id, None, &bytes, &refs) {
            Ok(()) => {
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
        // marker) so prompts/ and other adjacent assets travel with the
        // skill into the managed dir.
        let mut refs: Vec<(String, Vec<u8>)> = Vec::new();
        collect_extras(&source_dir, &source_dir, &mut refs);

        let pusher = ManagedPathsPusher::new(&state_clone);
        pusher
            .push_skill(&slug, None, &skill_md_bytes, &refs)
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

/// Recursively walk `dir`, appending `(relative_path, bytes)` for every file
/// except `SKILL.md` and `.vectorhawk-managed.json`. Errors on individual
/// reads are skipped silently — adoption is best-effort.
fn collect_extras(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if path.is_file() {
            if name == "SKILL.md" || name == ".vectorhawk-managed.json" {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                if let Some(rel_str) = rel.to_str() {
                    if let Ok(bytes) = fs::read(&path) {
                        out.push((rel_str.to_string(), bytes));
                    }
                }
            }
        } else if path.is_dir() {
            collect_extras(root, &path, out);
        }
    }
}

// ── Path resolution ───────────────────────────────────────────────────────────

fn resolve_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("pusher: HOME directory not resolvable"))
}

/// The canonical managed-skill root: `~/.agents/skills/`.
fn resolve_skills_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".agents").join("skills"))
}

/// Where Claude Code looks: `~/.claude/skills/`. VectorHawk places only
/// symlinks here — never real content.
fn resolve_claude_link_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude").join("skills"))
}

/// Return `true` if the F2-managed copy for `slug` is present on disk, i.e.
/// `~/.agents/skills/<slug>/SKILL.md` exists.
///
/// Used by the adopt-publish takeover flow (`managed_paths::takeover`) to
/// confirm the VectorHawk-managed replacement actually landed before removing
/// the original discovered `source_path` it is replacing.
pub fn managed_skill_present(slug: &str) -> Result<bool> {
    let skills_dir = resolve_skills_dir()?;
    Ok(skills_dir.join(slug).join("SKILL.md").exists())
}

fn resolve_plugins_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude").join("plugins"))
}

fn resolve_claude_json() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude.json"))
}

// ── Atomic write ──────────────────────────────────────────────────────────────

/// Write `content` to `dest` via a sibling `.tmp` file + atomic rename.
fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, content)
        .with_context(|| format!("pusher: failed to write tmp file: {}", tmp.display()))?;
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
    let (content, file_existed) = if path.exists() {
        let c = fs::read_to_string(path)
            .with_context(|| format!("pusher: failed to read: {}", path.display()))?;
        (c, true)
    } else {
        (String::new(), false)
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

    // Step 4: write to a thread-unique temp file (unique name avoids two
    // concurrent threads clobbering each other's temp file).
    let thread_id = std::thread::current().id();
    let tmp_name = format!(".claude.json.{thread_id:?}.tmp");
    let tmp_path = parent_dir.join(tmp_name);
    fs::write(&tmp_path, new_content.as_bytes())
        .with_context(|| format!("pusher: failed to write tmp: {}", tmp_path.display()))?;

    // Step 5: atomic rename.
    fs::rename(&tmp_path, path).context("pusher: failed to rename tmp → ~/.claude.json")?;

    // Set 0600 permissions on first create.
    if !file_existed {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(path, perms)
                .context("pusher: failed to set 0600 permissions on ~/.claude.json")?;
        }
    }

    // Step 6: release the lock.  `fs2` unlocks when the file handle drops.
    // Explicitly drop to make the release point clear.
    drop(lock_file);

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "pusher_tests.rs"]
mod tests;
