//! Migration logic: backup, POST to backend, write markers.
//!
//! For each `MigrationItem`:
//! 1. Check the SQLite `managed_path_markers` table (path-keyed, canonicalised)
//!    — if already tracked, skip. The `.scenery-managed.json` file is never
//!    trusted for this.
//! 2. Back up original files to the run-scoped backup directory.
//! 3. POST to `POST /portal/managed-paths/migrate` (single-item batch).
//! 4. Write the marker (file sidecar or SQLite row).
//! 5. Buffer an audit event via the existing `audit_events` table.
//!
//! If the backend POST fails, the item is NOT marked and will be retried on
//! the next daemon start.  The backup is still preserved.

use super::{
    marker::{
        insert_db_marker, is_already_marked, tracked_marker_for_path, write_file_marker,
        ManagedPathMarker,
    },
    mcp_discovery::MIGRATE_MCP_CONFIG_FIELDS,
    scanner::MigrationItem,
    ItemKind,
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use scenery_core::http::AsyncSendTrackedExt;
use scenery_core::{
    auth::load_all_tokens,
    restore_journal::{JournalEntry, JournalOp, JournalSource, RestoreJournal},
    state::AppState,
};
use scenery_mcp::ownership;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use tracing::{debug, info, warn};

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

    // ── 0.2 Never adopt an artifact the org has revoked ───────────────────────
    // Adopting uploads the SKILL.md, compiles and publishes it as a new
    // owner-private registry artifact (and queues an install row), so a
    // revoked skill's leftover dir must never get here. Skip it untouched —
    // no backup, no POST, no marker: the reconciler's revoke pass removes it.
    // A failed cache read is an error (retried next start), never a pass.
    //
    // KNOWN LIMIT: the cache is filled by the sync snapshot, and this
    // migration runs at startup before the first snapshot. On a fresh state
    // DB (or first run) the cache is empty, so this gate fails open. Deferring
    // the migration until after the first snapshot is not a contained change
    // (it runs inline in `run_daemon`, the sync loop is a separate task), so
    // the backend's `artifact_revoked` 403 on `/portal/managed-paths/migrate`
    // is the check for that window; the runner handles it in `post_migrate`.
    if is_revoked_locally(state, item)? {
        info!(
            slug = %item.slug,
            kind = %item.kind,
            "managed_paths: skipping revoked artifact (left for the revoke pass)"
        );
        return Ok(false);
    }

    // ── 0.5 / 1. Never re-adopt content Scenery already tracks ─────────────
    // Ownership comes from `managed_path_markers` alone (F13). The
    // `.scenery-managed.json` sidecar is attacker-controllable — any skill
    // or plugin can ship one — so it is not consulted here.
    //
    // The lookup compares canonicalised paths. Since the pivot to
    // `~/.agents/skills`, a pushed skill's row is keyed on the canonical
    // `.agents` dir while the scanner walks `~/.claude/skills` and hands us
    // the *symlink*; canonicalising resolves the link onto the row, so our own
    // skill is never POSTed back as a native discovery or re-marked through
    // the link.
    //
    // A tracked path whose hash no longer matches is still skipped: it is
    // already governed, and the drift reconciler reports (and per policy
    // quarantines) it. Adopting it again would create a duplicate row and
    // write a marker through the link.
    {
        let conn = Connection::open(&state.db_path).context("migrator: failed to open state DB")?;
        if is_already_marked(&conn, &path_key)
            .context("migrator: failed to check managed_path_markers")?
            || is_tracked(&conn, item)?
        {
            debug!(slug = %item.slug, path = %path_key, "managed_paths: already tracked — skipping");
            return Ok(false);
        }
    }

    // ── 2. Backup ─────────────────────────────────────────────────────────────
    backup_item(item, backup_root)
        .with_context(|| format!("managed_paths: backup failed for {}", item.slug))?;

    // ── 3. POST to backend ────────────────────────────────────────────────────
    let maybe_installation_id = match post_migrate(item, registry_url, http_client, state).await {
        Ok(id) => id,
        Err(e) => {
            // Revoked server-side: terminal. Nothing was marked, and the
            // local revocation cache will skip it once a snapshot lands.
            if let Some(ArtifactRevoked(msg)) = e.downcast_ref::<ArtifactRevoked>() {
                warn!(
                    slug = %item.slug,
                    kind = %item.kind,
                    message = %msg,
                    "managed_paths: backend refused to adopt a revoked artifact — leaving it for the revoke pass"
                );
                // Drop the backup copy made above: nothing was adopted, so
                // there is nothing to roll back to.
                let backup_dir = match item.kind {
                    ItemKind::Plugin => Some(backup_root.join("plugins").join(&item.slug)),
                    ItemKind::Mcp => None,
                };
                if let Some(dir) = backup_dir {
                    fs::remove_dir_all(dir).ok();
                }
                return Ok(false);
            }
            warn!(slug = %item.slug, error = %e, "managed_paths: backend migrate call failed; will retry next start");
            return Err(e);
        }
    };

    // ── 4. Write marker (replace-with-managed) ────────────────────────────────
    // Writing the marker is how Scenery "replaces the local copy with a
    // managed copy": the item becomes Scenery-owned in place, and the drift
    // reconciler governs it from here (keep-in-sync, quarantine, or kill).
    //
    // NOTE: fully destructive sole-source replacement for MCP servers (rewriting
    // ~/.claude.json to route the server through the gateway) is intentionally
    // NOT done here — the daemon cannot yet serve adopted MCP backends via the
    // aggregator until the approved-servers/gateway pipeline lands
    // (`/runner/approved-servers` is currently a stub). Removing the user's
    // direct entry before then would break the tool. See the board card.
    let now_ts = scenery_core::timefmt::now_rfc3339();
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
        ItemKind::Plugin => {
            if let Err(e) = write_file_marker(
                &item.source_path,
                maybe_installation_id.as_deref(),
                &item.canonical_hash,
                &now_ts,
            ) {
                // Non-fatal: SQLite row is the authoritative idempotency key.
                warn!(slug = %item.slug, error = %e, "managed_paths: failed to write .scenery-managed.json (non-fatal)");
            }
        }
        ItemKind::Mcp => {
            // No sidecar possible for JSON entries.
        }
    }

    // ── 5. Manifest entry ─────────────────────────────────────────────────────
    // Derive the backup path for this item so the rollback module can restore it.
    let backup_path = match item.kind {
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
    // legacy `.scenery-backup/<ts>/manifest.json` this function has always
    // written (kept as-is for `migrate rollback` backwards compat). Reuses
    // the SAME `backup_path` computed above — no second copy is made.
    //
    // Skill/Plugin: the native dir gets a `.scenery-managed.json` sidecar
    // written into it in place, so this is a `file_replace` of that dir.
    // Mcp: no live rewrite happens yet (see the NOTE above) but the entry is
    // still recorded now — same `config_edit` vocabulary as `write_mcp_entry`
    // — so the ledger already has an entry once the live rewrite lands.
    let (journal_op, journal_target) = match item.kind {
        ItemKind::Plugin => (JournalOp::FileReplace, path_key.clone()),
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
/// Symlink-safe: links are copied as links and never followed, special files
/// are skipped, and the walk is size-capped.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    scenery_core::safe_copy::copy_tree(src, dest)
        .map(|_| ())
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))
}

/// The backend refused a migrate/adopt with 403 `artifact_revoked`.
#[derive(Debug, thiserror::Error)]
#[error("artifact revoked: {0}")]
struct ArtifactRevoked(String);

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
            "canonical_hash": upload_hash(item),
            "payload": migrate_payload(item),
        }]
    });

    let resp = http_client
        .post(&url)
        .bearer_auth(&token)
        .json(&request_body)
        .send_tracked()
        .await
        .with_context(|| format!("managed_paths: HTTP POST to {url} failed"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("managed_paths: backend returned 401 — token expired or invalid");
    }

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        let body = resp.text().await.unwrap_or_default();
        if let Some(message) = scenery_core::registry::artifact_revoked_message(&body) {
            return Err(anyhow::Error::new(ArtifactRevoked(message)));
        }
        anyhow::bail!("managed_paths: backend migrate returned HTTP 403 Forbidden: {body}");
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

    // A per-item error is distinct from "backend skipped" (`Ok(None)`): it
    // must stop the backup/marker/audit steps. The request is a single-item
    // batch, so an error naming another slug is ignored and one naming none
    // is ours. HTTP 200 + `errors` is how the backend reports a revoked item.
    for err in &migrate_resp.errors {
        let names_other = err
            .get("slug")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s != item.slug);
        if names_other {
            continue;
        }
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        if message.starts_with("artifact_revoked") {
            return Err(anyhow::Error::new(ArtifactRevoked(message.to_string())));
        }
        anyhow::bail!(
            "managed_paths: backend reported an error for {}: {message}",
            item.slug
        );
    }

    let first = migrate_resp.migrated.into_iter().next();
    let installation_id = first.map(|m| m.installation_id);

    Ok(installation_id)
}

/// Is `item` covered by a whole-artifact revocation in the local cache?
///
/// Skills match by registry name or by the local folder the registry name is
/// recorded under; plugins by slug. A version-scoped revocation does not
/// apply: adoption publishes a fresh version of its own. MCP entries never
/// match — a revocation names an MCP server by UUID, which a `mcpServers`
/// key cannot be mapped to.
fn is_revoked_locally(state: &AppState, item: &MigrationItem) -> Result<bool> {
    match item.kind {
        ItemKind::Plugin => artifact_whole_revoked(state, "plugin", &item.slug),
        ItemKind::Mcp => Ok(false),
    }
}

fn artifact_whole_revoked(state: &AppState, artifact_type: &str, slug: &str) -> Result<bool> {
    let entries =
        scenery_core::revocation::list_all(state).context("failed to read the revocation cache")?;
    for e in entries {
        let whole = e.version.as_deref().map_or(true, |v| v.trim().is_empty());
        if e.artifact_type != artifact_type || !whole {
            continue;
        }
        if e.artifact_key == slug {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `item` is already tracked in `managed_path_markers` (path-keyed,
/// canonicalised — see step 0.5 in [`migrate_item`]).
fn is_tracked(conn: &Connection, item: &MigrationItem) -> Result<bool> {
    let kind = match item.kind {
        ItemKind::Plugin => "plugin",
        // MCP rows are keyed on the virtual `<claude.json>:<key>` path, which
        // the exact-path check already covers.
        ItemKind::Mcp => return Ok(false),
    };
    if tracked_marker_for_path(conn, kind, &item.source_path)?.is_some() {
        return Ok(true);
    }
    Ok(false)
}

/// The payload actually POSTed for `item` (F9). Defense in depth on top of the
/// scanner, which already builds allowlisted payloads: whatever `item.payload`
/// holds, only these fields leave the machine.
///
/// - skill: `skill_md` (the skill's own content — what adoption uploads).
/// - plugin: `manifest_json` reduced to string `name` / `version` /
///   `description`.
/// - mcp: `mcp_config` reduced to the [`MIGRATE_MCP_CONFIG_FIELDS`] keys —
///   no env/header values, raw args, raw command or full URL.
pub(crate) fn migrate_payload(item: &MigrationItem) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    match item.kind {
        ItemKind::Plugin => {
            let raw = item
                .payload
                .get("manifest_json")
                .cloned()
                .unwrap_or_default();
            out.insert(
                "manifest_json".to_string(),
                super::scanner::sanitize_plugin_manifest(&raw),
            );
        }
        ItemKind::Mcp => {
            let mut cfg = serde_json::Map::new();
            if let Some(raw) = item.payload.get("mcp_config").and_then(|v| v.as_object()) {
                for field in MIGRATE_MCP_CONFIG_FIELDS {
                    if let Some(v) = raw.get(*field) {
                        cfg.insert((*field).to_string(), v.clone());
                    }
                }
            }
            // `url` is re-reduced to an origin in case a raw URL slipped in.
            if let Some(url) = cfg.get("url").and_then(|v| v.as_str()) {
                let origin = super::mcp_discovery::safe_url_origin(url);
                cfg.insert("url".to_string(), serde_json::json!(origin));
            }
            out.insert("mcp_config".to_string(), serde_json::Value::Object(cfg));
        }
    }
    serde_json::Value::Object(out)
}

/// The hash that is sent to the backend (migrate body and audit event).
///
/// For an MCP entry `canonical_hash` is a SHA-256 of the raw entry, env and
/// header secrets included, and a low-entropy secret could be brute-forced
/// from it. It stays local (the drift baseline); what leaves the machine is
/// the hash of the sanitized upload payload. Plugins hash a manifest that
/// carries no secrets, so they keep `canonical_hash`.
pub(crate) fn upload_hash(item: &MigrationItem) -> String {
    match item.kind {
        ItemKind::Mcp => scenery_core::restore_journal::sha256_hex(
            &serde_json::to_vec(&migrate_payload(item)).unwrap_or_default(),
        ),
        ItemKind::Plugin => item.canonical_hash.clone(),
    }
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
        "canonical_hash": upload_hash(item),
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "migrator_tests.rs"]
mod migrator_tests;
