//! Rollback support for F1-managed-paths backups.
//!
//! Provides two entry points:
//!
//! - [`list_backups`] — enumerate `<home>/.claude/.vectorhawk-backup/*/manifest.json`
//!   and return a summary for each completed backup run.
//! - [`rollback`] — restore one backup run (or a single slug within it) to its
//!   original path, remove the F2 SQLite marker, and notify the backend.
//!
//! # Failure model
//!
//! Individual item failures are appended to `RollbackReport::errors` rather
//! than propagated.  Partial rollback is always preferred over aborting.

use crate::managed_paths::migrator::{BackupManifest, ManifestItem};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{debug, info, warn};
use vectorhawkd_core::{auth::load_all_tokens, state::AppState};

// ── Public types ──────────────────────────────────────────────────────────────

/// Summary for one backup run directory.
#[derive(Debug, Clone)]
pub struct BackupSummary {
    /// The ISO-8601 timestamp extracted from the backup directory name.
    pub ts: String,
    /// Total number of items in the manifest.
    pub item_count: usize,
    /// All items from the manifest.
    pub items: Vec<ManifestItem>,
}

/// Result of a rollback operation.
#[derive(Debug, Default)]
pub struct RollbackReport {
    /// Slugs that were restored successfully.
    pub restored: Vec<String>,
    /// Per-item failures (non-fatal; rollback continues after recording each).
    pub errors: Vec<RollbackError>,
}

/// One item-level failure recorded during a rollback run.
#[derive(Debug)]
pub struct RollbackError {
    pub slug: String,
    pub message: String,
}

// ── list_backups ──────────────────────────────────────────────────────────────

/// Enumerate all backup runs under `<home>/.claude/.vectorhawk-backup/`.
///
/// Returns one [`BackupSummary`] per subdirectory that contains a readable
/// `manifest.json`.  Directories without a manifest or with an unparseable
/// manifest are skipped with a debug log.
///
/// The returned slice is sorted in ascending timestamp order (oldest first).
pub fn list_backups(home: &Path) -> Result<Vec<BackupSummary>> {
    let backup_base = home.join(".claude").join(".vectorhawk-backup");

    if !backup_base.exists() {
        return Ok(vec![]);
    }

    let entries = fs::read_dir(&backup_base).with_context(|| {
        format!(
            "list_backups: failed to read backup dir: {}",
            backup_base.display()
        )
    })?;

    let mut summaries: Vec<BackupSummary> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "list_backups: error reading backup dir entry");
                continue;
            }
        };

        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let ts = entry.file_name().to_string_lossy().into_owned();

        let manifest_path = entry_path.join("manifest.json");
        if !manifest_path.exists() {
            debug!(
                path = %manifest_path.display(),
                "list_backups: no manifest.json in backup dir — skipping"
            );
            continue;
        }

        let data = match fs::read(&manifest_path) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %manifest_path.display(),
                    "list_backups: failed to read manifest.json — skipping"
                );
                continue;
            }
        };

        let manifest: BackupManifest = match serde_json::from_slice(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %manifest_path.display(),
                    "list_backups: failed to parse manifest.json — skipping"
                );
                continue;
            }
        };

        let item_count = manifest.items.len();
        summaries.push(BackupSummary {
            ts,
            item_count,
            items: manifest.items,
        });
    }

    // Sort ascending by timestamp string (ISO-8601 lexicographic order is chronological).
    summaries.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(summaries)
}

// ── rollback ──────────────────────────────────────────────────────────────────

/// Restore items from a single backup run.
///
/// Steps for each item:
/// 1. Copy/rename backed-up files back to `original_path` atomically.
/// 2. Delete the `managed_path_markers` SQLite row keyed at `f2_marker_path`.
/// 3. Call `DELETE /portal/managed-paths/catalog/{slug}` (non-fatal on 404/403).
/// 4. Call `DELETE /installations/{installation_id}` if set (non-fatal).
///
/// Per-item failures are recorded in `RollbackReport::errors` and the rollback
/// continues with the remaining items.
pub async fn rollback(
    state: &AppState,
    registry_url: &str,
    home: &Path,
    ts: &str,
    slug_filter: Option<&str>,
) -> Result<RollbackReport> {
    let backup_root = home.join(".claude").join(".vectorhawk-backup").join(ts);

    let manifest_path = backup_root.join("manifest.json");

    let data = fs::read(&manifest_path).with_context(|| {
        format!(
            "rollback: failed to read manifest at: {}",
            manifest_path.display()
        )
    })?;
    let manifest: BackupManifest =
        serde_json::from_slice(&data).context("rollback: failed to parse manifest.json")?;

    let mut report = RollbackReport::default();

    // Load bearer token once — used for all HTTP calls this run.
    let bearer_token = load_bearer_token(state, registry_url);

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("rollback: failed to build HTTP client")?;

    for item in &manifest.items {
        // Apply optional slug filter.
        if let Some(filter) = slug_filter {
            if item.slug != filter {
                continue;
            }
        }

        info!(slug = %item.slug, kind = %item.kind, "rollback: restoring item");

        // ── 1. Restore files ──────────────────────────────────────────────────
        let original_path = PathBuf::from(&item.original_path);
        let backup_path = PathBuf::from(&item.backup_path);

        if let Err(e) = restore_item(&backup_path, &original_path, &item.kind, &item.slug, home) {
            report.errors.push(RollbackError {
                slug: item.slug.clone(),
                message: format!("file restore failed: {e:#}"),
            });
            continue;
        }

        // ── 2. Delete SQLite marker ───────────────────────────────────────────
        // By (kind, slug), not by the manifest's f2_marker_path — see
        // `delete_db_marker`.
        let db_path = state.db_path.clone();
        let kind = item.kind.clone();
        let slug = item.slug.clone();
        let marker_result =
            tokio::task::spawn_blocking(move || delete_db_marker(&db_path, &kind, &slug))
                .await
                .context("rollback: marker delete task panicked")?;

        match marker_result {
            Ok(0) => debug!(
                slug = %item.slug,
                "rollback: no managed_path_markers row to delete (already clean)"
            ),
            Ok(n) => debug!(slug = %item.slug, rows = n, "rollback: marker row(s) deleted"),
            Err(e) => {
                // Non-fatal: the files are already restored; marker cleanup
                // failure is annoying but not catastrophic.
                warn!(slug = %item.slug, error = %e, "rollback: marker delete failed (non-fatal)");
                report.errors.push(RollbackError {
                    slug: item.slug.clone(),
                    message: format!("SQLite marker delete failed: {e:#}"),
                });
            }
        }

        // ── 3. DELETE catalog entry (non-fatal) ───────────────────────────────
        match bearer_token {
            None => {
                warn!(
                    slug = %item.slug,
                    "rollback: no bearer token available — skipping catalog DELETE"
                );
                report.errors.push(RollbackError {
                    slug: item.slug.clone(),
                    message: "catalog DELETE skipped: no auth token available".to_string(),
                });
            }
            Some(ref token) => {
                // Skills delete via the migrated-catalog endpoint (by slug).
                // Plugins/MCP delete via the custom-tools endpoint (by kind +
                // backend row id) — the catalog endpoint is skills-only and would
                // 404 for them, silently leaving the adopted row behind.
                let base = registry_url.trim_end_matches('/');
                let catalog_url = match item.kind.as_str() {
                    "plugin" | "mcp" if item.installation_id.is_some() => format!(
                        "{base}/portal/custom-tools/{}/{}",
                        item.kind,
                        item.installation_id.as_ref().unwrap()
                    ),
                    _ => format!("{base}/portal/managed-paths/catalog/{}", item.slug),
                };
                match http_client
                    .delete(&catalog_url)
                    .bearer_auth(token)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            info!(slug = %item.slug, %status, "rollback: catalog DELETE succeeded");
                        } else if status == reqwest::StatusCode::NOT_FOUND {
                            info!(
                                slug = %item.slug,
                                "rollback: catalog DELETE 404 — entry already removed"
                            );
                        } else if status == reqwest::StatusCode::FORBIDDEN {
                            let body = resp.text().await.unwrap_or_default();
                            warn!(
                                slug = %item.slug,
                                %status,
                                body,
                                "rollback: catalog DELETE 403 — insufficient permissions"
                            );
                            report.errors.push(RollbackError {
                                slug: item.slug.clone(),
                                message: format!("catalog DELETE forbidden (403): {body}"),
                            });
                        } else {
                            let body = resp.text().await.unwrap_or_default();
                            warn!(
                                slug = %item.slug,
                                %status,
                                body,
                                "rollback: catalog DELETE failed"
                            );
                            report.errors.push(RollbackError {
                                slug: item.slug.clone(),
                                message: format!("catalog DELETE returned HTTP {status}: {body}"),
                            });
                        }
                    }
                    Err(e) => {
                        warn!(
                            slug = %item.slug,
                            error = %e,
                            "rollback: catalog DELETE HTTP call failed"
                        );
                        report.errors.push(RollbackError {
                            slug: item.slug.clone(),
                            message: format!("catalog DELETE HTTP error: {e}"),
                        });
                    }
                }
            }
        }

        // ── 4. DELETE installation row (non-fatal) ────────────────────────────
        match (&bearer_token, &item.installation_id) {
            (None, Some(_)) => {
                warn!(
                    slug = %item.slug,
                    "rollback: no bearer token available — skipping installation DELETE"
                );
                report.errors.push(RollbackError {
                    slug: item.slug.clone(),
                    message: "installation DELETE skipped: no auth token available".to_string(),
                });
            }
            (Some(token), Some(iid)) => {
                let install_url = format!(
                    "{}/installations/{}",
                    registry_url.trim_end_matches('/'),
                    iid
                );
                match http_client
                    .delete(&install_url)
                    .bearer_auth(token)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            info!(
                                slug = %item.slug,
                                installation_id = %iid,
                                %status,
                                "rollback: installation DELETE succeeded"
                            );
                        } else if status == reqwest::StatusCode::NOT_FOUND {
                            info!(
                                slug = %item.slug,
                                installation_id = %iid,
                                "rollback: installation DELETE 404 — row already removed"
                            );
                        } else if status == reqwest::StatusCode::FORBIDDEN {
                            let body = resp.text().await.unwrap_or_default();
                            warn!(
                                slug = %item.slug,
                                installation_id = %iid,
                                %status,
                                body,
                                "rollback: installation DELETE 403 — insufficient permissions"
                            );
                            report.errors.push(RollbackError {
                                slug: item.slug.clone(),
                                message: format!("installation DELETE forbidden (403): {body}"),
                            });
                        } else {
                            let body = resp.text().await.unwrap_or_default();
                            warn!(
                                slug = %item.slug,
                                installation_id = %iid,
                                %status,
                                body,
                                "rollback: installation DELETE failed"
                            );
                            report.errors.push(RollbackError {
                                slug: item.slug.clone(),
                                message: format!(
                                    "installation DELETE returned HTTP {status}: {body}"
                                ),
                            });
                        }
                    }
                    Err(e) => {
                        warn!(
                            slug = %item.slug,
                            installation_id = %iid,
                            error = %e,
                            "rollback: installation DELETE HTTP call failed"
                        );
                        report.errors.push(RollbackError {
                            slug: item.slug.clone(),
                            message: format!("installation DELETE HTTP error: {e}"),
                        });
                    }
                }
            }
            // No installation_id in manifest — nothing to delete.
            (_, None) => {}
        }

        info!(slug = %item.slug, "rollback: item restored");
        report.restored.push(item.slug.clone());
    }

    Ok(report)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Restore a single backed-up item to its original location.
///
/// For skills and plugins: `backup_path` is the backed-up directory.
/// For MCP: `backup_path` is the backed-up `claude.json`.
///
/// Removes the F2-managed directory/file at `original_path` before restoring,
/// then tries `rename` first (same filesystem).  On cross-filesystem rename
/// failure falls back to recursive copy then delete.
///
/// # Pivot awareness
///
/// F1 adopts skills in place at `~/.claude/skills/<slug>`, so that is what the
/// manifest records. `migrate_skills_to_agents_dir` then moves the adopted
/// directory to `~/.agents/skills/<slug>` and leaves a symlink at the old
/// path. `original_path.is_dir()` follows that symlink and returns true, so
/// removing it naively unlinks only the link and the adopted copy survives —
/// still live for every client that scans the canonical root. When
/// `original_path` is a symlink into `<home>/.agents/skills/<slug>`, both are
/// removed.
///
/// `home` is threaded through from [`rollback`] rather than read from
/// `dirs::home_dir()` so the canonical root always matches the backup tree
/// being restored.
fn restore_item(
    backup_path: &Path,
    original_path: &Path,
    kind: &str,
    slug: &str,
    home: &Path,
) -> Result<()> {
    if !backup_path.exists() {
        anyhow::bail!(
            "restore_item: backup does not exist: {}",
            backup_path.display()
        );
    }

    // Remove whatever F2 has put at original_path. Check `is_symlink` FIRST:
    // `exists`/`is_dir` both follow links and would misclassify the
    // post-migration layout.
    if original_path.is_symlink() {
        // Only follow the link when it lands exactly where the pivot puts
        // managed skills. Anything else is a link we do not own, and deleting
        // whatever it happens to point at would be unbounded.
        let canonical = home.join(".agents").join("skills").join(slug);
        let points_at_canonical = fs::canonicalize(original_path)
            .ok()
            .zip(fs::canonicalize(&canonical).ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false);

        if points_at_canonical {
            fs::remove_dir_all(&canonical).with_context(|| {
                format!(
                    "restore_item: failed to remove migrated adoption: {}",
                    canonical.display()
                )
            })?;
            debug!(
                slug,
                canonical = %canonical.display(),
                "restore_item: removed the migrated copy behind the Claude-path symlink"
            );
        } else {
            warn!(
                slug,
                link = %original_path.display(),
                "restore_item: symlink at original_path does not resolve to the canonical \
                 skills root — unlinking it only"
            );
        }

        fs::remove_file(original_path).with_context(|| {
            format!(
                "restore_item: failed to remove symlink: {}",
                original_path.display()
            )
        })?;
    } else if original_path.exists() {
        if original_path.is_dir() {
            fs::remove_dir_all(original_path).with_context(|| {
                format!(
                    "restore_item: failed to remove F2-managed dir: {}",
                    original_path.display()
                )
            })?;
        } else {
            fs::remove_file(original_path).with_context(|| {
                format!(
                    "restore_item: failed to remove F2-managed file: {}",
                    original_path.display()
                )
            })?;
        }
    }

    // Ensure parent exists.
    if let Some(parent) = original_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "restore_item: failed to create parent dir: {}",
                parent.display()
            )
        })?;
    }

    // Attempt atomic rename first.
    match fs::rename(backup_path, original_path) {
        Ok(()) => {
            debug!(
                backup = %backup_path.display(),
                original = %original_path.display(),
                "restore_item: rename succeeded"
            );
            return Ok(());
        }
        Err(e) => {
            debug!(
                error = %e,
                "restore_item: rename failed (cross-device?), falling back to copy+delete"
            );
        }
    }

    // Fallback: copy then delete.
    if kind == "mcp" || backup_path.is_file() {
        fs::copy(backup_path, original_path).with_context(|| {
            format!(
                "restore_item: failed to copy file {} → {}",
                backup_path.display(),
                original_path.display()
            )
        })?;
        fs::remove_file(backup_path).with_context(|| {
            format!(
                "restore_item: copy+delete: failed to remove source: {}",
                backup_path.display()
            )
        })?;
    } else {
        copy_dir_recursive(backup_path, original_path)?;
        fs::remove_dir_all(backup_path).with_context(|| {
            format!(
                "restore_item: copy+delete: failed to remove source dir: {}",
                backup_path.display()
            )
        })?;
    }

    Ok(())
}

/// Recursively copy `src` directory into `dest` (same semantics as `migrator::copy_dir_recursive`).
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("copy_dir_recursive: create dest: {}", dest.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("copy_dir_recursive: read src: {}", src.display()))?
    {
        let entry = entry.context("copy_dir_recursive: read entry")?;
        let entry_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        let meta = entry
            .metadata()
            .with_context(|| format!("copy_dir_recursive: stat: {}", entry_path.display()))?;

        if meta.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path).with_context(|| {
                format!(
                    "copy_dir_recursive: copy {} → {}",
                    entry_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Delete the `managed_path_markers` row for `(kind, slug)`.
///
/// Keyed on `(kind, slug)` rather than on the manifest's `f2_marker_path`
/// because that path goes stale: `migrate_skills_to_agents_dir` re-keys skill
/// rows from `~/.claude/skills/<slug>` to `~/.agents/skills/<slug>` after F1
/// wrote the manifest, so a `WHERE path = <claude path>` delete matches
/// nothing and drift keeps governing the orphaned row. `(kind, slug)` is
/// stable across the pivot and across any future relocation.
///
/// Returns the number of rows deleted, so the caller can tell a real cleanup
/// from a silent no-match.
///
/// Called inside `spawn_blocking` — synchronous SQLite I/O.
fn delete_db_marker(db_path: &camino::Utf8PathBuf, kind: &str, slug: &str) -> Result<usize> {
    let conn =
        Connection::open(db_path).context("rollback: failed to open state DB for marker delete")?;
    let deleted = conn
        .execute(
            "DELETE FROM managed_path_markers WHERE kind = ?1 AND slug = ?2",
            rusqlite::params![kind, slug],
        )
        .context("rollback: failed to delete managed_path_markers row")?;
    Ok(deleted)
}

/// Load the access token for `registry_url` from SQLite.
fn load_bearer_token(state: &AppState, registry_url: &str) -> Option<String> {
    match load_all_tokens(state) {
        Ok(rows) => rows
            .into_iter()
            .find(|r| r.registry_url == registry_url)
            .map(|r| r.access_token),
        Err(e) => {
            warn!(error = %e, "rollback: failed to load auth tokens");
            None
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::managed_paths::migrator::{append_manifest_item, ManifestItem};
    use tempfile::TempDir;

    fn make_home() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    // ── list_backups tests ────────────────────────────────────────────────────

    #[test]
    fn list_backups_returns_empty_when_no_dir_exists() {
        let home = make_home();
        let result = list_backups(home.path()).unwrap();
        assert!(result.is_empty(), "expected empty vec, got {result:?}");
    }

    #[test]
    fn list_backups_skips_dirs_without_manifest() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");
        fs::create_dir_all(backup_base.join("2025-01-01T000000Z")).unwrap();

        let result = list_backups(home.path()).unwrap();
        assert!(
            result.is_empty(),
            "expected empty — dir has no manifest.json"
        );
    }

    #[test]
    fn list_backups_parses_manifest_and_returns_summary() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");
        let run_dir = backup_base.join("2025-06-01T120000Z");
        fs::create_dir_all(&run_dir).unwrap();

        // Write a valid manifest via append_manifest_item.
        let item = ManifestItem {
            kind: "skill".to_string(),
            slug: "my-skill".to_string(),
            original_path: "/fake/home/.claude/skills/my-skill".to_string(),
            backup_path: run_dir
                .join("skills")
                .join("my-skill")
                .to_string_lossy()
                .to_string(),
            f2_marker_path: "/fake/home/.claude/skills/my-skill".to_string(),
            catalog_skill_id: None,
            installation_id: Some("install-uuid-123".to_string()),
        };
        append_manifest_item(&run_dir, "2025-06-01T120000Z", item).unwrap();

        let result = list_backups(home.path()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ts, "2025-06-01T120000Z");
        assert_eq!(result[0].item_count, 1);
        assert_eq!(result[0].items[0].slug, "my-skill");
    }

    #[test]
    fn list_backups_sorts_ascending_by_ts() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");

        for ts in &[
            "2025-06-03T000000Z",
            "2025-06-01T000000Z",
            "2025-06-02T000000Z",
        ] {
            let run_dir = backup_base.join(ts);
            fs::create_dir_all(&run_dir).unwrap();
            let item = ManifestItem {
                kind: "skill".to_string(),
                slug: "s".to_string(),
                original_path: "/p".to_string(),
                backup_path: run_dir.join("skills/s").to_string_lossy().to_string(),
                f2_marker_path: "/p".to_string(),
                catalog_skill_id: None,
                installation_id: None,
            };
            append_manifest_item(&run_dir, ts, item).unwrap();
        }

        let result = list_backups(home.path()).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].ts, "2025-06-01T000000Z");
        assert_eq!(result[1].ts, "2025-06-02T000000Z");
        assert_eq!(result[2].ts, "2025-06-03T000000Z");
    }

    // ── rollback tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rollback_restores_files_from_backup_dir() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");
        let ts = "2025-06-10T000000Z";
        let run_dir = backup_base.join(ts);

        // Create a fake backed-up skill dir.
        let backup_skill = run_dir.join("skills").join("test-skill");
        fs::create_dir_all(&backup_skill).unwrap();
        fs::write(backup_skill.join("SKILL.md"), "# test").unwrap();

        // The original_path where the skill should be restored.
        let original_skill = home
            .path()
            .join(".claude")
            .join("skills")
            .join("test-skill");
        // Do NOT create original_skill — rollback should create it.

        let item = ManifestItem {
            kind: "skill".to_string(),
            slug: "test-skill".to_string(),
            original_path: original_skill.to_string_lossy().to_string(),
            backup_path: backup_skill.to_string_lossy().to_string(),
            f2_marker_path: original_skill.to_string_lossy().to_string(),
            catalog_skill_id: None,
            installation_id: None,
        };
        append_manifest_item(&run_dir, ts, item).unwrap();

        // Bootstrap a minimal AppState with a temp DB (no SQLite ops for this test).
        let db_dir = home.path().join(".vectorhawk");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = camino::Utf8PathBuf::from(db_dir.join("state.db").to_string_lossy().as_ref());
        // Create minimal SQLite tables so delete_db_marker doesn't crash.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_tokens (id INTEGER PRIMARY KEY, registry_url TEXT, access_token TEXT, refresh_token TEXT, expires_at INTEGER);
             CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS managed_path_markers (path TEXT PRIMARY KEY, kind TEXT, slug TEXT, installation_id TEXT, source_sha256 TEXT, migrated_at TEXT);",
        )
        .unwrap();
        drop(conn);

        let root_dir = camino::Utf8PathBuf::from(db_dir.to_string_lossy().as_ref());
        let state = AppState { root_dir, db_path };

        let report = rollback(&state, "https://app.vectorhawk.ai", home.path(), ts, None)
            .await
            .unwrap();

        assert_eq!(report.restored, vec!["test-skill"]);
        // No auth token is stored for the test registry URL, so the catalog
        // DELETE leg will report a "skipped" error.  That is expected and correct
        // behaviour — the important assertion is that files are actually restored.
        let fatal_errors: Vec<_> = report
            .errors
            .iter()
            .filter(|e| !e.message.contains("no auth token"))
            .collect();
        assert!(
            fatal_errors.is_empty(),
            "unexpected fatal errors: {fatal_errors:?}"
        );
        // File was restored.
        assert!(
            original_skill.join("SKILL.md").exists(),
            "SKILL.md should be restored"
        );
    }

    /// **Regression (rollback is not pivot-aware).**
    ///
    /// F1 adopts a user skill in place at `~/.claude/skills/<slug>` and writes
    /// a manifest whose `original_path` and `f2_marker_path` both point there.
    /// The next daemon start's `migrate_skills_to_agents_dir` then MOVES that
    /// directory to `~/.agents/skills/<slug>`, leaves a symlink behind, and
    /// re-keys the SQLite row to the canonical path.
    ///
    /// Rollback — the user-facing undo for adoption — must cope with that:
    ///
    /// * `original_path.is_dir()` follows the symlink and reports true, so a
    ///   naive `remove_dir_all` removes only the link. The adopted copy at
    ///   `~/.agents/skills/<slug>` survives, marker intact, still surfaced to
    ///   Cursor / Codex / Gemini.
    /// * `DELETE ... WHERE path = <claude path>` matches nothing, because the
    ///   row was re-keyed. The marker row survives and drift keeps governing
    ///   the orphan.
    ///
    /// …and the report says "restored" through both.
    #[tokio::test]
    async fn rollback_undoes_an_adoption_that_was_migrated_to_the_agents_dir() {
        let home = make_home();
        let ts = "2025-07-19T000000Z";
        let run_dir = home
            .path()
            .join(".claude")
            .join(".vectorhawk-backup")
            .join(ts);
        let slug = "adopted-skill";

        // The user's pre-adoption content, as F1 backed it up.
        let backup_dir = run_dir.join("skills").join(slug);
        fs::create_dir_all(&backup_dir).unwrap();
        fs::write(backup_dir.join("SKILL.md"), b"the user's original").unwrap();

        // Post-Task-5 on-disk state: a symlink at the Claude path pointing at
        // the real, marked, adopted directory in the canonical root.
        let claude_path = home.path().join(".claude").join("skills").join(slug);
        let canonical = home.path().join(".agents").join("skills").join(slug);
        fs::create_dir_all(&canonical).unwrap();
        fs::write(canonical.join("SKILL.md"), b"the F2-managed adoption").unwrap();
        fs::write(canonical.join(".vectorhawk-managed.json"), b"{}").unwrap();
        fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&canonical, &claude_path).unwrap();
        assert!(claude_path.is_symlink() && claude_path.is_dir());

        // The manifest still records the pre-migration Claude path.
        let item = ManifestItem {
            kind: "skill".to_string(),
            slug: slug.to_string(),
            original_path: claude_path.to_string_lossy().to_string(),
            backup_path: backup_dir.to_string_lossy().to_string(),
            f2_marker_path: claude_path.to_string_lossy().to_string(),
            catalog_skill_id: None,
            installation_id: None,
        };
        append_manifest_item(&run_dir, ts, item).unwrap();

        let db_dir = home.path().join(".vectorhawk");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = camino::Utf8PathBuf::from(db_dir.join("state.db").to_string_lossy().as_ref());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_tokens (id INTEGER PRIMARY KEY, registry_url TEXT, access_token TEXT, refresh_token TEXT, expires_at INTEGER);
             CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS managed_path_markers (path TEXT PRIMARY KEY, kind TEXT, slug TEXT, installation_id TEXT, source_sha256 TEXT, migrated_at TEXT);",
        )
        .unwrap();
        // The row as `migrate_skills_to_agents_dir` re-keyed it: canonical path.
        conn.execute(
            "INSERT INTO managed_path_markers (path, kind, slug, installation_id, source_sha256, migrated_at) \
             VALUES (?1, 'skill', ?2, NULL, 'abc', '2025-07-01T00:00:00Z')",
            rusqlite::params![canonical.to_string_lossy(), slug],
        )
        .unwrap();
        drop(conn);

        let root_dir = camino::Utf8PathBuf::from(db_dir.to_string_lossy().as_ref());
        let state = AppState { root_dir, db_path };

        let report = rollback(&state, "https://app.vectorhawk.ai", home.path(), ts, None)
            .await
            .unwrap();

        // The adopted copy must be gone from the canonical root — otherwise it
        // stays live for every non-Claude client that scans ~/.agents/skills.
        assert!(
            !canonical.exists(),
            "BUG: the adopted copy survived rollback at {}",
            canonical.display()
        );

        // The user's original must be back at the Claude path, as a real
        // directory — not a symlink to something we failed to delete.
        assert!(!claude_path.is_symlink(), "the stale symlink must be gone");
        assert_eq!(
            fs::read(claude_path.join("SKILL.md")).unwrap(),
            b"the user's original",
            "the user's pre-adoption content must be restored"
        );

        // The marker row was re-keyed to canonical, so deleting by the
        // manifest's stale Claude path would leave it behind for drift.
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_path_markers WHERE slug = ?1",
                rusqlite::params![slug],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 0,
            "BUG: the re-keyed marker row survived rollback — drift will keep governing the orphan"
        );

        let fatal: Vec<_> = report
            .errors
            .iter()
            .filter(|e| !e.message.contains("no auth token"))
            .collect();
        assert!(fatal.is_empty(), "unexpected fatal errors: {fatal:?}");
        assert_eq!(report.restored, vec![slug], "report must say restored");
    }

    #[tokio::test]
    async fn rollback_with_slug_filter_only_touches_one_item() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");
        let ts = "2025-06-11T000000Z";
        let run_dir = backup_base.join(ts);

        // Two items in the manifest.
        for slug in &["skill-a", "skill-b"] {
            let backup_dir = run_dir.join("skills").join(slug);
            fs::create_dir_all(&backup_dir).unwrap();
            fs::write(backup_dir.join("SKILL.md"), "# test").unwrap();

            let original = home.path().join(".claude").join("skills").join(slug);

            let item = ManifestItem {
                kind: "skill".to_string(),
                slug: slug.to_string(),
                original_path: original.to_string_lossy().to_string(),
                backup_path: backup_dir.to_string_lossy().to_string(),
                f2_marker_path: original.to_string_lossy().to_string(),
                catalog_skill_id: None,
                installation_id: None,
            };
            append_manifest_item(&run_dir, ts, item).unwrap();
        }

        // Minimal AppState.
        let db_dir = home.path().join(".vectorhawk");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = camino::Utf8PathBuf::from(db_dir.join("state.db").to_string_lossy().as_ref());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_tokens (id INTEGER PRIMARY KEY, registry_url TEXT, access_token TEXT, refresh_token TEXT, expires_at INTEGER);
             CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS managed_path_markers (path TEXT PRIMARY KEY, kind TEXT, slug TEXT, installation_id TEXT, source_sha256 TEXT, migrated_at TEXT);",
        )
        .unwrap();
        drop(conn);
        let root_dir = camino::Utf8PathBuf::from(db_dir.to_string_lossy().as_ref());
        let state = AppState { root_dir, db_path };

        let report = rollback(
            &state,
            "https://app.vectorhawk.ai",
            home.path(),
            ts,
            Some("skill-a"),
        )
        .await
        .unwrap();

        assert_eq!(report.restored, vec!["skill-a"]);
        // No auth token is stored for the test registry URL, so the catalog
        // DELETE leg will report a "skipped" error.  Filter those out — the
        // important assertion is that only skill-a is touched.
        let fatal_errors: Vec<_> = report
            .errors
            .iter()
            .filter(|e| !e.message.contains("no auth token"))
            .collect();
        assert!(
            fatal_errors.is_empty(),
            "unexpected fatal errors: {fatal_errors:?}"
        );

        // skill-a restored; skill-b untouched (backup still there, original not created).
        assert!(
            home.path().join(".claude/skills/skill-a/SKILL.md").exists(),
            "skill-a should be restored"
        );
        assert!(
            !home.path().join(".claude/skills/skill-b").exists(),
            "skill-b should NOT be restored"
        );
    }

    /// Verify that a 500 response from the catalog DELETE endpoint is surfaced
    /// as an entry in `RollbackReport::errors` rather than silently swallowed.
    #[tokio::test]
    async fn rollback_reports_catalog_delete_failure_in_errors() {
        let home = make_home();
        let backup_base = home.path().join(".claude").join(".vectorhawk-backup");
        let ts = "2025-06-12T000000Z";
        let run_dir = backup_base.join(ts);

        let slug = "bad-skill";
        let backup_dir = run_dir.join("skills").join(slug);
        fs::create_dir_all(&backup_dir).unwrap();
        fs::write(backup_dir.join("SKILL.md"), "# test").unwrap();
        let original = home.path().join(".claude").join("skills").join(slug);

        let item = ManifestItem {
            kind: "skill".to_string(),
            slug: slug.to_string(),
            original_path: original.to_string_lossy().to_string(),
            backup_path: backup_dir.to_string_lossy().to_string(),
            f2_marker_path: original.to_string_lossy().to_string(),
            catalog_skill_id: None,
            // No installation_id so only the catalog DELETE is attempted.
            installation_id: None,
        };
        append_manifest_item(&run_dir, ts, item).unwrap();

        // Minimal AppState with a token stored so load_bearer_token returns Some.
        let db_dir = home.path().join(".vectorhawk");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = camino::Utf8PathBuf::from(db_dir.join("state.db").to_string_lossy().as_ref());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_tokens \
                (id INTEGER PRIMARY KEY, registry_url TEXT, access_token TEXT, \
                 refresh_token TEXT, expires_at INTEGER, \
                 refresh_failures INTEGER NOT NULL DEFAULT 0, \
                 next_refresh_attempt_at INTEGER, \
                 last_refresh_status TEXT); \
             CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT); \
             CREATE TABLE IF NOT EXISTS managed_path_markers \
                (path TEXT PRIMARY KEY, kind TEXT, slug TEXT, installation_id TEXT, \
                 source_sha256 TEXT, migrated_at TEXT);",
        )
        .unwrap();
        // Insert a fake token so the HTTP leg fires.
        conn.execute(
            "INSERT INTO auth_tokens (registry_url, access_token, refresh_token, expires_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "__mock__",
                "fake-access-token",
                "fake-refresh-token",
                9_999_999_999_i64
            ],
        )
        .unwrap();
        drop(conn);
        let root_dir = camino::Utf8PathBuf::from(db_dir.to_string_lossy().as_ref());
        let state = AppState { root_dir, db_path };

        // Start a mock server that always returns 500 for the catalog DELETE.
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock(
                "DELETE",
                format!("/portal/managed-paths/catalog/{slug}").as_str(),
            )
            .with_status(500)
            .with_body("internal server error")
            .create_async()
            .await;

        let registry_url = server.url();
        // Override the token registry_url to match the mock server URL so
        // load_bearer_token finds it.
        let conn2 = rusqlite::Connection::open(&state.db_path).unwrap();
        conn2
            .execute(
                "UPDATE auth_tokens SET registry_url = ?1",
                rusqlite::params![registry_url],
            )
            .unwrap();
        drop(conn2);

        let report = rollback(&state, &registry_url, home.path(), ts, None)
            .await
            .unwrap();

        // File restore should have succeeded.
        assert!(
            original.join("SKILL.md").exists(),
            "SKILL.md should be restored even when HTTP DELETE fails"
        );
        assert_eq!(
            report.restored,
            vec![slug],
            "item should still be in restored list"
        );

        // The catalog DELETE failure must appear in errors.
        let catalog_err = report
            .errors
            .iter()
            .find(|e| e.slug == slug && e.message.contains("500"));
        assert!(
            catalog_err.is_some(),
            "expected an error entry for the 500 catalog DELETE; errors: {:?}",
            report.errors
        );
    }
}
