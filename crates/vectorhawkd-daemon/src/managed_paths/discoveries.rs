//! F-Tier2: Report-only discovery for non-canonical skill roots.
//!
//! Scans extra roots (`~/.agents/skills/` and any paths in
//! `VECTORHAWK_EXTRA_SKILL_ROOTS`) for skills that are NOT already tracked
//! in `managed_path_markers`. Found items are POSTed to the backend in a
//! single batch so the portal can present them to the user for optional
//! adoption.
//!
//! **This module is REPORT-ONLY.** It never writes markers, never moves files,
//! and never takes any local action. If the POST fails the daemon continues
//! without error.
//!
//! ## Killswitch
//!
//! Setting `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER=1` makes `run_once` a
//! no-op.
//!
//! ## Cadence
//!
//! Called once at daemon startup. Discoveries are stable; users adopt or
//! ignore at their own pace.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{debug, info, warn};
use vectorhawkd_core::{auth::load_all_tokens, state::AppState};

const ENV_DISABLE: &str = "VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER";
const ENV_EXTRA_ROOTS: &str = "VECTORHAWK_EXTRA_SKILL_ROOTS";

// ── Public types ──────────────────────────────────────────────────────────────

/// One discovered item that may be reported to the backend.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryItem {
    pub kind: String,
    pub slug: String,
    pub source_path: String,
    pub canonical_hash: String,
}

/// Top-level reporter: wraps state + registry URL. Cheaply clonable via `Arc`.
pub struct DiscoveriesScanner {
    state: Arc<AppState>,
    registry_url: String,
}

impl DiscoveriesScanner {
    pub fn new(state: Arc<AppState>, registry_url: String) -> Self {
        Self {
            state,
            registry_url,
        }
    }

    /// Run discovery once. Delegates to the free function.
    pub async fn run(&self) -> Result<usize> {
        run_once(Arc::clone(&self.state), self.registry_url.clone()).await
    }
}

// ── Free-function entry point (testable) ──────────────────────────────────────

/// Scan extra roots, filter already-managed slugs, POST the batch.
///
/// Returns the count of items reported (0 when no new discoveries or when
/// killswitch is active).
pub async fn run_once(state: Arc<AppState>, registry_url: String) -> Result<usize> {
    if reconciler_disabled() {
        debug!("discoveries: VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER set — skipping");
        return Ok(0);
    }

    let roots = extra_roots();
    let items = collect_discoveries(&roots, &state);

    if items.is_empty() {
        debug!("discoveries: no new items found");
        return Ok(0);
    }

    let count = items.len();
    let http_client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "discoveries: failed to build HTTP client — skipping POST");
            return Ok(0);
        }
    };

    post_discoveries(&items, &state, &registry_url, &http_client).await;

    Ok(count)
}

// ── Root resolution ───────────────────────────────────────────────────────────

/// Build the list of extra roots to scan.
///
/// Always includes `~/.agents/skills/` (if it exists).
/// Also includes any paths from `VECTORHAWK_EXTRA_SKILL_ROOTS` (comma-separated
/// absolute paths).  The canonical F1 path (`~/.claude/skills/`) is
/// deliberately excluded — F1 owns that.
pub fn extra_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".agents").join("skills"));
    }

    if let Ok(extra) = std::env::var(ENV_EXTRA_ROOTS) {
        for part in extra.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                roots.push(PathBuf::from(trimmed));
            }
        }
    }

    roots
}

// ── Scan ──────────────────────────────────────────────────────────────────────

/// Scan all `roots`, skip roots that do not exist, and return only items whose
/// slug is NOT already present in `managed_path_markers` (kind='skill').
pub fn collect_discoveries(roots: &[PathBuf], state: &AppState) -> Vec<DiscoveryItem> {
    let mut items: Vec<DiscoveryItem> = Vec::new();

    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "discoveries: cannot open state DB — reporting nothing");
            return items;
        }
    };

    for root in roots {
        if !root.exists() {
            debug!(root = %root.display(), "discoveries: root does not exist — skipping");
            continue;
        }

        match scan_root(root) {
            Ok(found) => {
                for item in found {
                    if is_already_managed(&conn, &item.slug) {
                        debug!(slug = %item.slug, "discoveries: already managed — skipping");
                        continue;
                    }
                    items.push(item);
                }
            }
            Err(e) => {
                warn!(root = %root.display(), error = %e, "discoveries: scan failed — skipping root");
            }
        }
    }

    items
}

/// Scan one root directory and return a `DiscoveryItem` per subdir containing
/// `SKILL.md`.
fn scan_root(root: &Path) -> Result<Vec<DiscoveryItem>> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("discoveries: failed to read dir {}", root.display()))?;

    let mut items = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "discoveries: error reading dir entry — skipping");
                continue;
            }
        };

        let path = entry.path();

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "discoveries: cannot stat entry — skipping");
                continue;
            }
        };

        if !meta.is_dir() {
            continue;
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => {
                warn!(path = %path.display(), "discoveries: non-UTF-8 dir name — skipping");
                continue;
            }
        };

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        let skill_md_bytes = match fs::read(&skill_md) {
            Ok(b) => b,
            Err(e) => {
                warn!(slug = %slug, error = %e, "discoveries: cannot read SKILL.md — skipping");
                continue;
            }
        };

        let canonical_hash = hex_sha256(&skill_md_bytes);
        let source_path = path.to_string_lossy().to_string();

        items.push(DiscoveryItem {
            kind: "skill".to_string(),
            slug,
            source_path,
            canonical_hash,
        });
    }

    Ok(items)
}

// ── Already-managed check ─────────────────────────────────────────────────────

/// Return `true` if there is already a `managed_path_markers` row with
/// `kind='skill'` and `slug=slug`.
fn is_already_managed(conn: &Connection, slug: &str) -> bool {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT 1 FROM managed_path_markers WHERE kind = 'skill' AND slug = ?1 LIMIT 1",
        [slug],
        |_row| Ok(()),
    )
    .optional()
    .unwrap_or(None)
    .is_some()
}

// ── HTTP report ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct DiscoveriesBody {
    device_id: Option<String>,
    items: Vec<DiscoveryItem>,
}

/// POST the batch to the backend. Logs warnings on failure but never returns
/// an error — this function is best-effort.
async fn post_discoveries(
    items: &[DiscoveryItem],
    state: &AppState,
    registry_url: &str,
    http_client: &reqwest::Client,
) {
    let token = access_token_for(state, registry_url).await;

    if token.is_empty() {
        debug!("discoveries: no auth token — skipping POST (will try again next daemon start)");
        return;
    }

    let device_id = state.get_sync_state("device_id").ok().flatten();

    let body = DiscoveriesBody {
        device_id,
        items: items.to_vec(),
    };

    let url = format!(
        "{}/portal/managed-paths/discoveries",
        registry_url.trim_end_matches('/')
    );

    let resp = match http_client
        .post(&url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "discoveries: POST failed — will retry on next daemon start");
            return;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        warn!(%status, body = %text, "discoveries: backend rejected discovery batch");
        return;
    }

    info!(
        count = items.len(),
        "discoveries: batch reported to backend"
    );
}

// ── Auth helper ───────────────────────────────────────────────────────────────

async fn access_token_for(state: &AppState, registry_url: &str) -> String {
    let reg = registry_url.to_string();
    let db_path = state.db_path.clone();
    let root_dir = state.root_dir.clone();

    tokio::task::spawn_blocking(move || {
        let state_view = AppState { root_dir, db_path };
        load_all_tokens(&state_view)
            .ok()
            .and_then(|rows| {
                rows.into_iter()
                    .find(|r| r.registry_url == reg)
                    .map(|r| r.access_token)
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;
    use vectorhawkd_core::state::AppState;

    /// Bootstrap a minimal AppState in a temp dir. Returns both the state and
    /// the `TempDir` guard so the caller keeps the dir alive.
    fn temp_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let state = AppState::bootstrap_in(root).unwrap();
        (state, tmp)
    }

    /// Write a minimal SKILL.md at `root/<slug>/SKILL.md` and return the slug dir.
    fn write_skill(root: &Path, slug: &str) -> PathBuf {
        let dir = root.join(slug);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\nname: {slug}\n---\n")).unwrap();
        dir
    }

    // ── 1. Scans extra root and returns skill items ────────────────────────────

    #[test]
    fn scans_extra_root_and_returns_skill_md_items() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        write_skill(&root, "alpha");
        write_skill(&root, "beta");
        // Plain file — must be skipped.
        fs::write(root.join("README.md"), "hi").unwrap();
        // Dir without SKILL.md — must be skipped.
        fs::create_dir_all(root.join("no-skill-md")).unwrap();

        let (state, _guard) = temp_state();
        let items = collect_discoveries(&[root], &state);

        assert_eq!(items.len(), 2, "expected exactly 2 skills");
        let slugs: Vec<&str> = items.iter().map(|i| i.slug.as_str()).collect();
        assert!(slugs.contains(&"alpha"), "alpha missing");
        assert!(slugs.contains(&"beta"), "beta missing");
        for item in &items {
            assert_eq!(item.kind, "skill");
            assert!(!item.canonical_hash.is_empty());
            assert!(!item.source_path.is_empty());
        }
    }

    // ── 2. Skips slugs already in managed_path_markers ────────────────────────

    #[test]
    fn skips_slugs_already_in_managed_path_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        write_skill(&root, "managed-skill");
        write_skill(&root, "new-skill");

        let (state, _guard) = temp_state();

        // Pre-insert a marker for managed-skill.
        {
            let conn = Connection::open(&state.db_path).unwrap();
            conn.execute(
                "INSERT INTO managed_path_markers \
                 (path, kind, slug, installation_id, source_sha256, migrated_at) \
                 VALUES (?1, 'skill', ?2, NULL, 'aabbcc', '2026-01-01T000000Z')",
                rusqlite::params![
                    root.join("managed-skill").to_string_lossy().to_string(),
                    "managed-skill"
                ],
            )
            .unwrap();
        }

        let items = collect_discoveries(&[root], &state);

        assert_eq!(
            items.len(),
            1,
            "only the unmanaged skill should be returned"
        );
        assert_eq!(items[0].slug, "new-skill");
    }

    // ── 3. Skips root that does not exist ─────────────────────────────────────

    #[test]
    fn skips_root_that_does_not_exist() {
        let (state, _guard) = temp_state();
        let phantom = PathBuf::from("/tmp/vectorhawk-discoveries-phantom-root-xyzzy-9999");

        // Must not panic, must return empty vec.
        let items = collect_discoveries(&[phantom], &state);
        assert!(items.is_empty());
    }

    // ── 4. Respects VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER ─────────────────

    #[test]
    fn respects_disable_env_var() {
        // Use a unique env-var manipulation per test. `std::env::set_var` is not
        // thread-safe in concurrent test suites, but this test also does not rely
        // on any shared global state besides the env, so it is acceptable here.
        // The env var is unset immediately after the assertion.
        unsafe {
            std::env::set_var(ENV_DISABLE, "1");
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_skill(&root, "some-skill");

        let (state, _guard) = temp_state();

        // We test the killswitch gate directly via the sync helper.
        let disabled = reconciler_disabled();

        unsafe {
            std::env::remove_var(ENV_DISABLE);
        }

        assert!(
            disabled,
            "reconciler_disabled() must return true when env var is set"
        );

        // Verify that collect_discoveries itself still works once the var is gone.
        let items = collect_discoveries(&[root], &state);
        assert_eq!(items.len(), 1);
    }

    // ── 5. Canonical hash is sha256 of SKILL.md bytes ─────────────────────────

    #[test]
    fn canonical_hash_matches_sha256_of_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let content = "---\nname: hash-test\n---\n";
        write_skill(&root, "hash-test");
        // Overwrite with known content.
        fs::write(root.join("hash-test").join("SKILL.md"), content).unwrap();

        let (state, _guard) = temp_state();
        let items = collect_discoveries(&[root], &state);

        assert_eq!(items.len(), 1);
        let expected = hex_sha256(content.as_bytes());
        assert_eq!(items[0].canonical_hash, expected);
    }
}
