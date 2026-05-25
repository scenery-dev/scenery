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
use serde::Deserialize;
use std::{fs, path::Path};
use tracing::{debug, warn};
use vectorhawkd_core::{auth::load_all_tokens, state::AppState};

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

    // ── 4. Write marker ───────────────────────────────────────────────────────
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

    // ── 5. Audit event ────────────────────────────────────────────────────────
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

    let request_body = serde_json::json!({
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "migrator_tests.rs"]
mod migrator_tests;
