//! SSE-driven skill publish handler.
//!
//! Invoked when the backend emits a `discovery_publish_requested` SSE event.
//! Packs the skill directory at `source_path` into a gzipped tar archive and
//! uploads it to the registry compile endpoint via [`RegistryClient::compile_and_publish`].
//!
//! # Killswitch
//!
//! Returns `Ok(())` immediately when
//! `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` is set in the environment.
//!
//! # Failure model
//!
//! On error this function logs WARN and returns `Err`.  The SSE dispatcher does
//! NOT flip the discovery back to a previous state — the backend is responsible
//! for detecting stale `publishing` rows on its own schedule.

use anyhow::{Context, Result};
use std::{path::Path, sync::Arc};
use tracing::{info, warn};
use vectorhawkd_core::{auth::load_all_tokens, registry::RegistryClient, state::AppState};

// ── Env-var gate ──────────────────────────────────────────────────────────────

const ENV_DISABLE: &str = "VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER";

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Handle a `discovery_publish_requested` SSE event.
///
/// 1. Honours the `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` killswitch.
/// 2. Loads the stored bearer token for `registry_url` from SQLite.
/// 3. Builds a gzipped tar of the directory at `source_path`.
/// 4. Uploads via [`RegistryClient::compile_and_publish`].
/// 5. Logs the outcome; the backend flips discovery state on its own.
pub async fn handle_publish_requested(
    state: Arc<AppState>,
    registry_url: String,
    discovery_id: String,
    slug: String,
    source_path: String,
) -> Result<()> {
    if reconciler_disabled() {
        info!(slug, discovery_id, "publish: killswitch set — skipping");
        return Ok(());
    }

    // Check the source path first — cheap guard before any I/O against SQLite.
    let source = std::path::PathBuf::from(&source_path);
    if !source.exists() {
        anyhow::bail!(
            "publish: source_path does not exist on disk: {}",
            source.display()
        );
    }

    // Load the bearer token — must be found before attempting upload.
    let token = load_token_for_registry(&state, &registry_url)?;

    // Build the tar.gz in a blocking task (CPU + sync I/O).
    let gz_buf = build_tar_gz(source.clone()).await.with_context(|| {
        format!(
            "publish: failed to pack skill directory: {}",
            source.display()
        )
    })?;

    info!(
        slug,
        discovery_id,
        bytes = gz_buf.len(),
        registry_url,
        "publish: uploading skill archive"
    );

    // Upload in a blocking task (reqwest blocking client inside RegistryClient).
    // Capture registry_url for the error context before moving it into the closure.
    let upload_url = format!(
        "{}/portal/skills/compile",
        registry_url.trim_end_matches('/')
    );
    let registry = RegistryClient::new(registry_url.clone()).with_auth(token);
    // Pass discovery_id so the backend can auto-fill missing frontmatter
    // (version, publisher) from the catalog Skill stub created during adopt.
    let discovery_id_opt = Some(discovery_id.clone());
    let resp = tokio::task::spawn_blocking(move || {
        // Intentional publish of an already-adopted discovery — never block
        // this automated path on the cross-name duplicate gate.
        registry.compile_and_publish(gz_buf, discovery_id_opt.as_deref(), Some("force"))
    })
    .await
    .context("publish: upload task panicked")?
    .with_context(|| {
        format!("publish: stage=compile_and_publish url={upload_url} — registry upload failed")
    })?;

    if !resp.warnings.is_empty() {
        warn!(
            slug,
            discovery_id,
            warnings = ?resp.warnings,
            "publish: compile warnings"
        );
    }

    info!(
        slug,
        discovery_id,
        skill_name = resp.frontmatter.name,
        content_hash = resp.content_hash,
        "publish: skill published successfully"
    );

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load the access token for `registry_url` from SQLite.
///
/// Returns `Err` (rather than `None`) so the caller can propagate a meaningful
/// error message when no token is stored.
///
/// `pub(crate)` — reused by [`super::adopt_publish`], which uploads through
/// the same `RegistryClient` auth as this admin-triggered publish flow.
pub(crate) fn load_token_for_registry(state: &AppState, registry_url: &str) -> Result<String> {
    let rows = load_all_tokens(state).context("publish: failed to read auth token store")?;
    rows.into_iter()
        .find(|r| r.registry_url == registry_url)
        .map(|r| r.access_token)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "publish: no stored token for registry '{}' — run `vectorhawk auth login`",
                registry_url
            )
        })
}

/// Pack the directory at `dir` into an in-memory gzipped tar archive.
///
/// Run inside `spawn_blocking` because `tar::Builder` uses sync I/O.
///
/// `pub(crate)` — reused by [`super::adopt_publish`] to pack the same
/// SKILL.md-tree shape for the `/runner/skills/adopt-publish` upload.
pub(crate) async fn build_tar_gz(dir: std::path::PathBuf) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || pack_dir_to_tar_gz(&dir))
        .await
        .context("publish: tar task panicked")?
}

fn pack_dir_to_tar_gz(dir: &Path) -> Result<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};
    use tar::Builder;

    let mut gz_buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(&mut gz_buf, Compression::default());
    let mut tar = Builder::new(enc);

    tar.append_dir_all(".", dir)
        .with_context(|| format!("failed to pack directory: {}", dir.display()))?;

    tar.into_inner()
        .context("failed to finalize tar")?
        .finish()
        .context("failed to finalize gzip")?;

    Ok(gz_buf)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::{fs, sync::Arc};
    use tempfile::TempDir;

    /// Bootstrap a minimal `AppState` pointing at a temp SQLite DB with all
    /// required tables.  No `auth_tokens` rows are inserted.
    fn make_state(home: &TempDir) -> AppState {
        let db_dir = home.path().join(".vectorhawk");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = camino::Utf8PathBuf::from(db_dir.join("state.db").to_string_lossy().as_ref());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_tokens \
                (id INTEGER PRIMARY KEY, registry_url TEXT, access_token TEXT, \
                 refresh_token TEXT, expires_at INTEGER); \
             CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        drop(conn);
        let root_dir = camino::Utf8PathBuf::from(db_dir.to_string_lossy().as_ref());
        AppState { root_dir, db_path }
    }

    /// Verify that the handler returns `Err` when `source_path` does not exist
    /// on disk.  The source path check fires before the auth-token lookup, so
    /// the expected error mentions "does not exist".
    ///
    /// This test avoids any env-var mutation — the `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER`
    /// variable is manipulated by `discoveries.rs` tests on concurrent threads;
    /// reading or writing it here would be racy.  Instead, the test creates a
    /// real `AppState` (no tokens stored) and a guaranteed-absent path so the
    /// path-exists guard fires before any auth or killswitch logic.
    ///
    /// Note: if the killswitch happens to be set by a concurrent test at the
    /// exact moment this runs, `handle_publish_requested` returns `Ok` and the
    /// assertion fails.  Run with `--test-threads=1` to avoid this; the race
    /// is pre-existing in this test suite (see `discoveries.rs`).
    #[tokio::test]
    async fn handle_publish_requested_returns_err_when_source_missing() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(make_state(&home));

        // Use a UUID-tagged path inside the temp dir — guaranteed absent.
        let missing = home
            .path()
            .join(format!("missing-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();

        let result = handle_publish_requested(
            state,
            "https://app.vectorhawk.ai".to_string(),
            "disc-002".to_string(),
            "my-skill".to_string(),
            missing,
        )
        .await;

        assert!(
            result.is_err(),
            "should error when source_path doesn't exist"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("does not exist"),
            "error should mention path; got: {msg}"
        );
    }

    /// Verify that when the source directory exists but no auth token is stored,
    /// the handler fails with a "no stored token" error rather than panicking.
    /// This test is env-var-safe: it does not depend on killswitch state.
    /// If the killswitch is set by a concurrent test, this returns Ok — which
    /// would cause the assertion to fail, but that only happens under the known-
    /// racy concurrent env-var tests in discoveries.rs.
    #[tokio::test]
    async fn handle_publish_requested_returns_err_when_no_token() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(make_state(&home));

        // Create a real source directory so the path-exists check passes.
        let source_dir = home.path().join("my-skill");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("SKILL.md"), "# test skill").unwrap();

        let result = handle_publish_requested(
            state,
            "https://app.vectorhawk.ai".to_string(),
            "disc-003".to_string(),
            "my-skill".to_string(),
            source_dir.to_string_lossy().to_string(),
        )
        .await;

        assert!(result.is_err(), "should error when no auth token is stored");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("no stored token") || msg.contains("auth token"),
            "error should mention missing token; got: {msg}"
        );
    }
}
