//! Migration logic: backup, POST to backend, write markers.
//!
//! For each `MigrationItem`:
//! 1. Check the SQLite `managed_path_markers` table — if already present, skip.
//! 2. Back up original files to the run-scoped backup directory.
//! 3. POST to `POST /portal/managed-paths/migrate` (single-item batch).
//! 4. Write the marker (file sidecar or SQLite row).
//! 5. Buffer an audit event via the existing `audit_events` table.
//!
//! If the backend POST fails, the item is NOT marked and will be retried on
//! the next daemon start.  The backup is still preserved.

use super::{
    marker::{insert_db_marker, is_already_marked, write_file_marker, ManagedPathMarker},
    scanner::MigrationItem,
    ItemKind,
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{debug, info, warn};
use vectorhawkd_core::{
    auth::load_all_tokens,
    restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal},
    state::AppState,
};
use vectorhawkd_mcp::ownership;

// ── Backup manifest ───────────────────────────────────────────────────────────

/// One item in the per-run backup `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestItem {
    /// `"skill"`, `"plugin"`, or `"mcp"`.
    pub kind: String,
    /// Human-friendly identifier (dir name or MCP key).
    pub slug: String,
    /// Absolute path where the item lived at migration time.
    pub original_path: String,
    /// Where the backup copy lives.
    pub backup_path: String,
    /// The F2-marker path (equals `original_path` for skills/plugins).
    pub f2_marker_path: String,
    /// Backend catalog skill ID (UUID), if available.
    pub catalog_skill_id: Option<String>,
    /// Backend installation row ID (UUID), if available.
    pub installation_id: Option<String>,
}

/// Full per-run backup manifest written to `<backup_root>/manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub manifest_version: u32,
    /// ISO-8601 timestamp of the migration run (matches the backup directory name).
    pub migration_ts: String,
    pub items: Vec<ManifestItem>,
}

/// Append one `ManifestItem` to `<backup_root>/manifest.json`.
///
/// Reads the existing manifest if present, appends the new entry, then writes
/// back atomically (temp file + rename).  If the backup directory does not
/// exist yet it is created.
pub fn append_manifest_item(backup_root: &Path, ts: &str, item: ManifestItem) -> Result<()> {
    let manifest_path = backup_root.join("manifest.json");
    let tmp_path = backup_root.join("manifest.json.tmp");

    // Ensure the backup dir exists (may not have been created yet for MCP-only runs).
    fs::create_dir_all(backup_root).with_context(|| {
        format!(
            "append_manifest_item: failed to create backup dir: {}",
            backup_root.display()
        )
    })?;

    // Read existing manifest or create a fresh one.
    let mut manifest = if manifest_path.exists() {
        let data = fs::read(&manifest_path).with_context(|| {
            format!(
                "append_manifest_item: failed to read existing manifest: {}",
                manifest_path.display()
            )
        })?;
        serde_json::from_slice::<BackupManifest>(&data).unwrap_or_else(|e| {
            warn!(
                error = %e,
                path = %manifest_path.display(),
                "append_manifest_item: could not parse existing manifest — starting fresh"
            );
            BackupManifest {
                manifest_version: 1,
                migration_ts: ts.to_string(),
                items: vec![],
            }
        })
    } else {
        BackupManifest {
            manifest_version: 1,
            migration_ts: ts.to_string(),
            items: vec![],
        }
    };

    manifest.items.push(item);

    let json = serde_json::to_vec_pretty(&manifest)
        .context("append_manifest_item: failed to serialise manifest")?;

    fs::write(&tmp_path, &json).with_context(|| {
        format!(
            "append_manifest_item: failed to write tmp manifest: {}",
            tmp_path.display()
        )
    })?;

    fs::rename(&tmp_path, &manifest_path).with_context(|| {
        format!(
            "append_manifest_item: failed to rename tmp manifest to: {}",
            manifest_path.display()
        )
    })?;

    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Migrate a single `MigrationItem`.
///
/// Returns:
/// - `Ok(true)` — item was newly migrated.
/// - `Ok(false)` — item was already tracked (idempotent skip).
/// - `Err(_)` — unrecoverable error for this item (caller logs + continues).
pub async fn migrate_item(
    item: &MigrationItem,
    backup_root: &Path,
    state: &AppState,
    registry_url: &str,
    http_client: &reqwest::Client,
) -> Result<bool> {
    let path_key = item.source_path.to_string_lossy().to_string();

    // ── 0. Never adopt Anthropic-native content ───────────────────────────────
    // Defense-in-depth: the scanner already filters native plugins out, but a
    // second, unconditional check here guarantees Anthropic first-party content
    // is never backed up, uploaded, marked, or otherwise touched — even if a
    // future scanner path forgets to classify.
    if ownership::is_anthropic_native_path(&item.source_path) {
        debug!(
            slug = %item.slug,
            path = %path_key,
            "managed_paths: refusing to adopt Anthropic-native item (out of scope)"
        );
        return Ok(false);
    }

    // ── 0.5 Never re-adopt VectorHawk's own managed content ───────────────────
    // Defense-in-depth alongside the scanner's own check, exactly as for
    // Anthropic-native content above.
    //
    // The path-equality idempotency check in step 1 is NOT sufficient here.
    // Since the pivot to `~/.agents/skills`, the `managed_path_markers` row for
    // a pushed skill is keyed on the canonical `.agents` path, while the
    // scanner walks `~/.claude/skills` and would hand us the *symlink* path —
    // a different key, so `is_already_marked` returns false and the item would
    // sail through. Everything downstream then compounds: the org's own skill
    // is POSTed to the backend as a newly discovered native item (a duplicate,
    // governance-visible installation), a second marker row is inserted at the
    // link path, and `write_file_marker` writes *through the symlink* into the
    // canonical directory — which is precisely the write-through-a-link
    // failure mode the `links` module exists to prevent.
    //
    // Ownership is content-based (the `.vectorhawk-managed.json` marker), not
    // link-based, so a user's own symlinked skill still gets adopted normally.
    if ownership::is_vectorhawk_managed(&item.source_path) {
        debug!(
            slug = %item.slug,
            path = %path_key,
            "managed_paths: refusing to adopt VectorHawk-managed item (already ours)"
        );
        return Ok(false);
    }

    // ── 1. Idempotency check ──────────────────────────────────────────────────
    {
        let conn = Connection::open(&state.db_path).context("migrator: failed to open state DB")?;
        if is_already_marked(&conn, &path_key)
            .context("migrator: failed to check managed_path_markers")?
        {
            debug!(slug = %item.slug, "managed_paths: already tracked — skipping");
            return Ok(false);
        }
    }

    // ── 2. Backup ─────────────────────────────────────────────────────────────
    backup_item(item, backup_root)
        .with_context(|| format!("managed_paths: backup failed for {}", item.slug))?;

    // ── 3. POST to backend ────────────────────────────────────────────────────
    let maybe_installation_id = post_migrate(item, registry_url, http_client, state)
        .await
        .map_err(|e| {
            warn!(slug = %item.slug, error = %e, "managed_paths: backend migrate call failed; will retry next start");
            e
        })?;

    // ── 4. Write marker (replace-with-managed) ────────────────────────────────
    // Writing the marker is how VectorHawk "replaces the local copy with a
    // managed copy": the item becomes VectorHawk-owned in place, and the drift
    // reconciler governs it from here (keep-in-sync, quarantine, or kill).
    //
    // NOTE: fully destructive sole-source replacement for MCP servers (rewriting
    // ~/.claude.json to route the server through the gateway) is intentionally
    // NOT done here — the daemon cannot yet serve adopted MCP backends via the
    // aggregator until the approved-servers/gateway pipeline lands
    // (`/runner/approved-servers` is currently a stub). Removing the user's
    // direct entry before then would break the tool. See the board card.
    let now_ts = chrono::Utc::now().to_rfc3339();
    let conn = Connection::open(&state.db_path)
        .context("migrator: failed to open state DB for marker write")?;

    let db_marker = ManagedPathMarker {
        path: path_key.clone(),
        kind: item.kind.to_string(),
        slug: item.slug.clone(),
        installation_id: maybe_installation_id.clone(),
        source_sha256: item.canonical_hash.clone(),
        migrated_at: now_ts.clone(),
    };
    insert_db_marker(&conn, &db_marker)
        .context("migrator: failed to write managed_path_markers row")?;

    // For skills and plugins also write the sidecar file into the item's dir.
    match item.kind {
        ItemKind::Skill | ItemKind::Plugin => {
            if let Err(e) = write_file_marker(
                &item.source_path,
                maybe_installation_id.as_deref(),
                &item.canonical_hash,
                &now_ts,
            ) {
                // Non-fatal: SQLite row is the authoritative idempotency key.
                warn!(slug = %item.slug, error = %e, "managed_paths: failed to write .vectorhawk-managed.json (non-fatal)");
            }
        }
        ItemKind::Mcp => {
            // No sidecar possible for JSON entries.
        }
    }

    // ── 5. Manifest entry ─────────────────────────────────────────────────────
    // Derive the backup path for this item so the rollback module can restore it.
    let backup_path = match item.kind {
        ItemKind::Skill => backup_root
            .join("skills")
            .join(&item.slug)
            .to_string_lossy()
            .to_string(),
        ItemKind::Plugin => backup_root
            .join("plugins")
            .join(&item.slug)
            .to_string_lossy()
            .to_string(),
        ItemKind::Mcp => backup_root
            .join("claude.json")
            .to_string_lossy()
            .to_string(),
    };

    // Extract the run timestamp from the backup_root directory name.
    let run_ts = backup_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // ── 5.5 Restore journal (source=native) ───────────────────────────────────
    // ONE ledger: F1 takeovers get a restore-journal entry too, alongside the
    // legacy `.vectorhawk-backup/<ts>/manifest.json` this function has always
    // written (kept as-is for `migrate rollback` backwards compat). Reuses
    // the SAME `backup_path` computed above — no second copy is made.
    //
    // Skill/Plugin: the native dir gets a `.vectorhawk-managed.json` sidecar
    // written into it in place, so this is a `file_replace` of that dir.
    // Mcp: no live rewrite happens yet (see the NOTE above) but the entry is
    // still recorded now — same `config_edit` vocabulary as `write_mcp_entry`
    // — so the ledger already has an entry once the live rewrite lands.
    let (journal_op, journal_target) = match item.kind {
        ItemKind::Skill | ItemKind::Plugin => (JournalOp::FileReplace, path_key.clone()),
        ItemKind::Mcp => (
            JournalOp::ConfigEdit,
            item.files
                .first()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| path_key.clone()),
        ),
    };
    let mut journal_detail = serde_json::json!({
        "canonical_hash": item.canonical_hash,
        "installation_id": maybe_installation_id,
    });
    if item.kind == ItemKind::Mcp {
        journal_detail["server_key"] = serde_json::Value::String(item.slug.clone());
        journal_detail["mcp_key"] = serde_json::Value::String("mcpServers".to_string());
    }
    let journal_entry = JournalEntry::new(journal_op, JournalSource::Native, journal_target)
        .with_slug(item.slug.clone())
        .with_client("Claude Code")
        .with_backup_path(backup_path.clone())
        .with_detail(journal_detail);
    if let Err(e) = RestoreJournal::for_state(state).append(journal_entry) {
        // Non-fatal: the legacy manifest.json backup is already on disk and
        // remains restorable via `migrate rollback` even if this fails.
        warn!(slug = %item.slug, error = %e, "managed_paths: failed to append restore-journal entry (non-fatal)");
    }

    let manifest_item = ManifestItem {
        kind: item.kind.to_string(),
        slug: item.slug.clone(),
        original_path: item.source_path.to_string_lossy().to_string(),
        backup_path,
        f2_marker_path: item.source_path.to_string_lossy().to_string(),
        catalog_skill_id: None, // backend does not return catalog_skill_id separately today
        installation_id: maybe_installation_id.clone(),
    };

    if let Err(e) = append_manifest_item(backup_root, &run_ts, manifest_item) {
        // Non-fatal: missing manifest does not break migration.
        warn!(slug = %item.slug, error = %e, "managed_paths: failed to append to backup manifest (non-fatal)");
    }

    // ── 6. Audit event ────────────────────────────────────────────────────────
    buffer_audit_event(&conn, item, maybe_installation_id.as_deref())
        .unwrap_or_else(|e| warn!(slug = %item.slug, error = %e, "managed_paths: failed to buffer audit event (non-fatal)"));

    Ok(true)
}

// ── Backup ────────────────────────────────────────────────────────────────────

fn backup_item(item: &MigrationItem, backup_root: &Path) -> Result<()> {
    match item.kind {
        ItemKind::Skill => {
            let dest = backup_root.join("skills").join(&item.slug);
            copy_dir_recursive(&item.source_path, &dest)
                .with_context(|| format!("failed to backup skill dir to {}", dest.display()))?;
        }
        ItemKind::Plugin => {
            let dest = backup_root.join("plugins").join(&item.slug);
            copy_dir_recursive(&item.source_path, &dest)
                .with_context(|| format!("failed to backup plugin dir to {}", dest.display()))?;
        }
        ItemKind::Mcp => {
            // Back up the whole claude.json once (idempotent — same file may be
            // backed up for multiple MCP entries in the same run).
            if let Some(src) = item.files.first() {
                let dest = backup_root.join("claude.json");
                if !dest.exists() {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create backup dir: {}", parent.display())
                        })?;
                    }
                    fs::copy(src, &dest).with_context(|| {
                        format!("failed to copy claude.json to backup: {}", dest.display())
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Recursively copy a directory, creating the destination if needed.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create backup dest: {}", dest.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read dir for backup: {}", src.display()))?
    {
        let entry = entry.context("failed to read dir entry during backup")?;
        let entry_path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dest.join(&file_name);

        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat: {}", entry_path.display()))?;

        if meta.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
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

// ── Backend POST ──────────────────────────────────────────────────────────────

/// Response from `/portal/managed-paths/migrate` for a single item.
#[derive(Debug, Deserialize)]
struct MigratedItem {
    installation_id: String,
    #[allow(dead_code)]
    is_new: bool,
}

#[derive(Debug, Deserialize)]
struct MigrateResponse {
    migrated: Vec<MigratedItem>,
    errors: Vec<serde_json::Value>,
}

/// POST the item to the backend's migrate endpoint and return the
/// `installation_id` if the backend accepted it.
///
/// Returns `Ok(None)` if the POST is skipped because there is no auth token.
async fn post_migrate(
    item: &MigrationItem,
    registry_url: &str,
    http_client: &reqwest::Client,
    state: &AppState,
) -> Result<Option<String>> {
    // Load auth token — skip backend call if not authenticated.
    let token = match load_bearer_token(state, registry_url) {
        Some(t) => t,
        None => {
            debug!(slug = %item.slug, "managed_paths: no auth token — skipping backend migrate call");
            return Ok(None);
        }
    };

    let url = format!(
        "{}/portal/managed-paths/migrate",
        registry_url.trim_end_matches('/')
    );

    let device_id = state.get_sync_state("device_id").ok().flatten();
    let request_body = serde_json::json!({
        "device_id": device_id,
        "items": [{
            "kind": item.kind.to_string(),
            "slug": item.slug,
            "canonical_hash": item.canonical_hash,
            "payload": item.payload,
        }]
    });

    let resp = http_client
        .post(&url)
        .bearer_auth(&token)
        .json(&request_body)
        .send()
        .await
        .with_context(|| format!("managed_paths: HTTP POST to {url} failed"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("managed_paths: backend returned 401 — token expired or invalid");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("managed_paths: backend migrate returned HTTP {status}: {body}");
    }

    let migrate_resp: MigrateResponse = resp
        .json()
        .await
        .context("managed_paths: failed to deserialise migrate response")?;

    if !migrate_resp.errors.is_empty() {
        warn!(
            slug = %item.slug,
            errors = ?migrate_resp.errors,
            "managed_paths: backend reported errors for item"
        );
    }

    let installation_id = migrate_resp
        .migrated
        .into_iter()
        .next()
        .map(|m| m.installation_id);

    Ok(installation_id)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load the access token for `registry_url` from SQLite.
fn load_bearer_token(state: &AppState, registry_url: &str) -> Option<String> {
    match load_all_tokens(state) {
        Ok(rows) => rows
            .into_iter()
            .find(|r| r.registry_url == registry_url)
            .map(|r| r.access_token),
        Err(e) => {
            warn!(error = %e, "managed_paths: failed to load auth tokens");
            None
        }
    }
}

/// Write a `managed_path_migrated` audit event to `audit_events`.
fn buffer_audit_event(
    conn: &Connection,
    item: &MigrationItem,
    installation_id: Option<&str>,
) -> Result<()> {
    let payload = serde_json::json!({
        "kind": item.kind.to_string(),
        "slug": item.slug,
        "canonical_hash": item.canonical_hash,
        "installation_id": installation_id,
        "source_path": item.source_path.to_string_lossy(),
    });
    let payload_str = serde_json::to_string(&payload)
        .context("managed_paths: failed to serialise audit payload")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    conn.execute(
        "INSERT INTO audit_events (event_type, payload, created_at, uploaded) VALUES (?1, ?2, ?3, 0)",
        rusqlite::params!["managed_path_migrated", payload_str, now],
    )
    .context("managed_paths: failed to insert audit event")?;
    Ok(())
}

// ── ~/.claude/skills → ~/.agents/skills one-shot migration ────────────────────

/// One-shot migration: move VectorHawk-managed skills from
/// `~/.claude/skills/<slug>/` to `~/.agents/skills/<slug>/` and leave a
/// symlink behind for Claude Code.
///
/// Only directories carrying a `.vectorhawk-managed.json` marker are touched —
/// user-authored skills in the same directory are left exactly as they are.
/// Idempotent: already-migrated skills (the Claude path is a symlink) are
/// skipped, so this is safe to run on every daemon start.
///
/// Returns the number of skills whose content was actually *moved* this run.
/// Relink-only and re-key-only outcomes are not counted — they are repairs,
/// not migrations.
///
/// # Failure handling
///
/// Every per-entry step is isolated: a skill that cannot be migrated is logged
/// and skipped, and the remaining skills still migrate. Only a failure to read
/// `~/.claude/skills` itself aborts the run, and even then the caller treats a
/// migration failure as non-fatal — this never blocks daemon start.
pub fn migrate_skills_to_agents_dir(conn: &Connection) -> Result<usize> {
    if super::pusher::reconciler_disabled() {
        return Ok(0);
    }

    let Some(claude_skills) = super::paths::claude_skills_link_dir() else {
        return Ok(0);
    };
    let Some(canonical_root) = super::paths::agents_skills_dir() else {
        return Ok(0);
    };

    // ── Repair pass ───────────────────────────────────────────────────────────
    // Runs FIRST and unconditionally, before any filesystem walk, because the
    // re-key must not depend on a move happening in the same call. A crash
    // between the `rename` and the `UPDATE` used to leave a row keyed on the
    // old Claude path that nothing could ever repair: the entry is a symlink
    // by then, so the migration loop skips it forever. Driving the repair off
    // the DB instead of off the directory entry also covers the case where the
    // symlink itself was deleted — there is no entry left to walk, but the
    // stale row is still there.
    if let Err(e) = repair_stale_skill_markers(conn, &claude_skills, &canonical_root) {
        warn!(error = %e, "migrator: stale skill marker repair failed (non-fatal)");
    }

    if !claude_skills.is_dir() {
        return Ok(0);
    }

    let mut migrated = 0usize;

    for entry in std::fs::read_dir(&claude_skills)
        .with_context(|| format!("migrator: failed to read {}", claude_skills.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "migrator: failed to read dir entry — skipping");
                continue;
            }
        };
        let old_path = entry.path();
        let Some(slug) = old_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };

        match migrate_one_legacy_skill(conn, &old_path, &canonical_root, &slug) {
            Ok(true) => {
                migrated += 1;
                info!(slug, "migrator: skill migrated to ~/.agents/skills/");
            }
            Ok(false) => {}
            Err(e) => {
                // Per-entry isolation: one bad skill must never block the rest,
                // on this run or on any subsequent daemon start.
                warn!(
                    slug,
                    error = %e,
                    "migrator: failed to migrate skill — skipping (other skills unaffected)"
                );
            }
        }
    }

    Ok(migrated)
}

/// Migrate a single `~/.claude/skills/<slug>` entry.
///
/// `Ok(true)` means content was moved to the canonical root; `Ok(false)` means
/// the entry needed no move (already migrated, not ours, or repaired in place).
fn migrate_one_legacy_skill(
    conn: &Connection,
    old_path: &Path,
    canonical_root: &Path,
    slug: &str,
) -> Result<bool> {
    // Already migrated, or someone else's link — leave it. Any stale DB row
    // for this path was already fixed by `repair_stale_skill_markers`.
    if old_path.is_symlink() || !old_path.is_dir() {
        return Ok(false);
    }
    // Not ours — user-authored skill. Never moved, linked, backed up or marked.
    if !vectorhawkd_mcp::ownership::is_vectorhawk_managed(old_path) {
        return Ok(false);
    }

    let new_path = canonical_root.join(slug);

    if !new_path.exists() {
        std::fs::create_dir_all(canonical_root)
            .with_context(|| format!("migrator: failed to create {}", canonical_root.display()))?;
        std::fs::rename(old_path, &new_path).with_context(|| {
            format!(
                "migrator: failed to move {} -> {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        super::links::link_dir(&new_path, old_path)
            .with_context(|| format!("migrator: failed to link {}", old_path.display()))?;
        rekey_skill_marker(conn, old_path, &new_path)?;
        return Ok(true);
    }

    // ── Canonical already exists ──────────────────────────────────────────────
    // This is not legacy content awaiting a move — it is a Claude-side copy
    // sitting alongside a canonical directory that already has the content.
    // Hand it to `link_dir`, which owns the decision:
    //
    //   * already byte-identical to canonical — returns `LinkMode::Copy`
    //     having touched nothing, so a copy-mode host does not mint a fresh
    //     backup on every daemon start;
    //   * diverged — backs the Claude-side content up to
    //     `~/.claude/.vectorhawk-backup/<ts>/links/` (which `migrate rollback`
    //     deliberately does not list — manual recovery only) and replaces it
    //     with a link.
    //
    // Either way the DB row must end up keyed on canonical.
    debug!(
        slug,
        canonical = %new_path.display(),
        "migrator: canonical dir already exists — reconciling the Claude-side copy"
    );
    super::links::link_dir(&new_path, old_path)
        .with_context(|| format!("migrator: failed to link {}", old_path.display()))?;
    rekey_skill_marker(conn, old_path, &new_path)?;
    Ok(false)
}

/// Re-point the `managed_path_markers` row for a skill from `old_path` to the
/// canonical `new_path`. Idempotent, and safe to call when no row exists.
///
/// If a canonical row already exists (a `push_skill` between the move and this
/// call inserts one, and `PRIMARY KEY (path)` happily lets both coexist), the
/// stale row is deleted rather than merged: the canonical row carries the
/// fresher hash, whereas the orphan carries the pre-push one and would report
/// `DriftStatus::Drifted` on every reconcile cycle — `drift::current_hash_for`
/// reads `<marker.path>/SKILL.md`, which follows the symlink through to the
/// canonical content.
fn rekey_skill_marker(conn: &Connection, old_path: &Path, new_path: &Path) -> Result<()> {
    let old_key = old_path.to_string_lossy().to_string();
    let new_key = new_path.to_string_lossy().to_string();
    if old_key == new_key {
        return Ok(());
    }

    conn.execute(
        "DELETE FROM managed_path_markers \
         WHERE path = ?1 AND kind = 'skill' \
           AND EXISTS (SELECT 1 FROM managed_path_markers WHERE path = ?2 AND kind = 'skill')",
        rusqlite::params![old_key, new_key],
    )
    .context("migrator: failed to drop superseded managed_path_markers row")?;

    conn.execute(
        "UPDATE managed_path_markers SET path = ?1 WHERE path = ?2 AND kind = 'skill'",
        rusqlite::params![new_key, old_key],
    )
    .context("migrator: failed to repoint managed_path_markers")?;

    Ok(())
}

/// Re-point every `kind='skill'` marker row still keyed under
/// `~/.claude/skills/<slug>` onto a canonical counterpart that exists on disk
/// **and is VectorHawk-managed**.
///
/// Two conditions must both hold, and neither is sufficient alone:
///
///   * The Claude-side entry is NOT a real directory — a symlink (the settled
///     post-migration state) or nothing at all (the link was deleted). A real
///     directory still sitting there is deliberately left to the migration
///     loop, which is the only place that knows whether that content is ours.
///   * The canonical directory carries a `.vectorhawk-managed.json` marker.
///     Slug collision is not proof of identity: a row for an F1-adopted *user*
///     skill whose directory the user deleted would otherwise be re-keyed onto
///     an unrelated same-slug directory, whose sha can never match the row's —
///     a permanent false-drift row. Left alone, drift correctly classifies the
///     vanished path as DELETED and drops the marker.
///
/// This pass runs only at daemon start, so it repairs rows that exist *at that
/// moment*; a row orphaned by a `push_skill` on an already-running daemon
/// survives until the next start. That bounds the orphan by a restart rather
/// than eliminating it.
///
/// Returns the number of rows repaired.
fn repair_stale_skill_markers(
    conn: &Connection,
    claude_skills: &Path,
    canonical_root: &Path,
) -> Result<usize> {
    let keys: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT path FROM managed_path_markers WHERE kind = 'skill'")
            .context("migrator: failed to prepare marker scan")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .context("migrator: failed to query managed_path_markers")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("migrator: failed to read managed_path_markers rows")?
    };

    let mut repaired = 0usize;
    for key in keys {
        let old_path = PathBuf::from(&key);
        if old_path.parent() != Some(claude_skills) {
            continue;
        }
        // A real directory here is still the migration loop's business.
        if matches!(std::fs::symlink_metadata(&old_path), Ok(m) if m.is_dir()) {
            continue;
        }
        let Some(name) = old_path.file_name() else {
            continue;
        };
        let new_path = canonical_root.join(name);
        if !new_path.is_dir() {
            continue;
        }
        // Same slug is not the same skill. Only re-key onto content we own.
        if !ownership::is_vectorhawk_managed(&new_path) {
            debug!(
                from = %key,
                candidate = %new_path.display(),
                "migrator: canonical dir with this slug is not VectorHawk-managed — \
                 leaving the stale row for drift to retire"
            );
            continue;
        }

        rekey_skill_marker(conn, &old_path, &new_path)?;
        repaired += 1;
        info!(
            from = %key,
            to = %new_path.display(),
            "migrator: repaired stale managed_path_markers row"
        );
    }

    Ok(repaired)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "migrator_tests.rs"]
mod migrator_tests;
