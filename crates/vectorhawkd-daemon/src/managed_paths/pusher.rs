//! F2 managed-paths pusher — writes VectorHawk-managed items into Claude Code's
//! native directories on install and removes them on deactivate.
//!
//! # What it writes
//!
//! | Item   | Destination                                           |
//! |--------|-------------------------------------------------------|
//! | Skill  | `~/.claude/skills/<slug>/` + `.vectorhawk-managed.json` marker |
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
};
use tracing::{debug, info};
use vectorhawkd_core::state::AppState;

// ── Env-var gate ──────────────────────────────────────────────────────────────

/// Return `true` when the filesystem reconciler is disabled.
fn reconciler_disabled() -> bool {
    std::env::var_os("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_some()
}

// ── ManagedPathsPusher ────────────────────────────────────────────────────────

/// Pushes VectorHawk-managed items into Claude Code's native directories.
///
/// Cheaply cloneable — the only field is an `Arc<AppState>`.
pub struct ManagedPathsPusher {
    db_path: camino::Utf8PathBuf,
}

impl ManagedPathsPusher {
    /// Construct a pusher from the daemon's `AppState`.
    pub fn new(state: &AppState) -> Self {
        Self {
            db_path: state.db_path.clone(),
        }
    }

    // ── Skill ─────────────────────────────────────────────────────────────────

    /// Write a skill into `~/.claude/skills/<slug>/`.
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

        info!(slug, dest = %skill_dir.display(), "pusher: skill pushed to ~/.claude/skills/");
        Ok(())
    }

    /// Remove a skill from `~/.claude/skills/<slug>/`.
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

        // Remove the directory.
        if skill_dir.exists() {
            fs::remove_dir_all(&skill_dir).with_context(|| {
                format!(
                    "pusher: failed to remove skill dir: {}",
                    skill_dir.display()
                )
            })?;
            info!(slug, "pusher: skill removed from ~/.claude/skills/");
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
    pub fn push_mcp(&self, slug: &str, installation_id: Option<&str>) -> Result<()> {
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

// ── Path resolution ───────────────────────────────────────────────────────────

fn resolve_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("pusher: HOME directory not resolvable"))
}

fn resolve_skills_dir() -> Result<PathBuf> {
    Ok(resolve_home()?.join(".claude").join("skills"))
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
