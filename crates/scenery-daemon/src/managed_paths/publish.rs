//! SSE-driven skill publish handler.
//!
//! Invoked when the backend emits a `discovery_publish_requested` SSE event.
//! Packs the skill directory at `source_path` into a gzipped tar archive and
//! uploads it to the registry compile endpoint via [`RegistryClient::compile_and_publish`].
//!
//! # Killswitch
//!
//! Returns `Ok(())` immediately when
//! `SCENERY_DISABLE_FILESYSTEM_RECONCILER` is set in the environment.
//!
//! # Failure model
//!
//! On error this function logs WARN and returns `Err`.  The SSE dispatcher does
//! NOT flip the discovery back to a previous state — the backend is responsible
//! for detecting stale `publishing` rows on its own schedule.

use anyhow::{Context, Result};
use scenery_core::{auth::load_all_tokens, registry::RegistryClient, state::AppState};
use std::{path::Path, sync::Arc};
use tracing::{debug, info, warn};

// ── Env-var gate ──────────────────────────────────────────────────────────────

const ENV_DISABLE: &str = "SCENERY_DISABLE_FILESYSTEM_RECONCILER";

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Handle a `discovery_publish_requested` SSE event.
///
/// 1. Honours the `SCENERY_DISABLE_FILESYSTEM_RECONCILER` killswitch.
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
                "publish: no stored token for registry '{}' — run `scenery auth login`",
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

/// A skill directory that must not be uploaded. Carried inside the
/// `anyhow::Error` so callers can recognise it (`downcast_ref`) and log the
/// skill as *held* rather than as a transient failure.
#[derive(Debug)]
pub struct PackRefused {
    pub dir: std::path::PathBuf,
    pub path: std::path::PathBuf,
    pub reason: String,
}

impl std::fmt::Display for PackRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "skill at {} held, not uploaded: {} ({})",
            self.dir.display(),
            self.reason,
            self.path.display()
        )
    }
}

impl std::error::Error for PackRefused {}

/// Upload caps: a skill is a handful of text files, not a project checkout.
const MAX_PACK_FILES: usize = 10_000;
const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PACK_DEPTH: usize = 32;

/// Directories that never belong in an upload.
fn is_excluded_dir(name: &str) -> bool {
    name == ".git" || name == "node_modules"
}

/// Files that commonly hold secrets: `.env*`, `*.pem`, `*.key`, and SSH
/// private keys (`id_rsa`, `id_ed25519`, ... but not their `.pub` halves).
fn is_excluded_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with(".env") || n.ends_with(".pem") || n.ends_with(".key") {
        return true;
    }
    const SSH_KEYS: [&str; 5] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "id_xmss"];
    SSH_KEYS.iter().any(|k| n.starts_with(k)) && !n.ends_with(".pub")
}

/// Lexically resolve `target` (a symlink's text) against `parent_rel` (the
/// link's directory relative to the pack root). `None` if it climbs above the
/// root or is absolute.
fn lexical_target_inside(parent_rel: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = parent_rel.components().count() as i64;
    for c in target.components() {
        match c {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

struct PackState {
    files: usize,
    bytes: u64,
}

fn pack_dir_to_tar_gz(dir: &Path) -> Result<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};
    use tar::Builder;

    let root = std::fs::canonicalize(dir)
        .with_context(|| format!("failed to resolve skill directory: {}", dir.display()))?;

    let mut gz_buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(&mut gz_buf, Compression::default());
    let mut tar = Builder::new(enc);
    // Never read through a link: a skill containing `creds -> ~/.aws` must not
    // upload those files. Links are archived as links (and only if they stay
    // inside the tree; see `pack_walk`).
    tar.follow_symlinks(false);

    tar.append_dir(".", &root)
        .with_context(|| format!("failed to pack directory: {}", dir.display()))?;
    let mut state = PackState { files: 0, bytes: 0 };
    pack_walk(&mut tar, dir, &root, Path::new(""), 0, &mut state)?;

    tar.into_inner()
        .context("failed to finalize tar")?
        .finish()
        .context("failed to finalize gzip")?;

    Ok(gz_buf)
}

fn pack_walk<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    top: &Path,
    root: &Path,
    rel: &Path,
    depth: usize,
    state: &mut PackState,
) -> Result<()> {
    let refuse = |path: &Path, reason: String| -> anyhow::Error {
        warn!(
            skill_dir = %top.display(),
            path = %path.display(),
            %reason,
            "publish: HOLDING skill, it will not be uploaded"
        );
        anyhow::Error::new(PackRefused {
            dir: top.to_path_buf(),
            path: path.to_path_buf(),
            reason,
        })
    };
    if depth > MAX_PACK_DEPTH {
        return Err(refuse(
            &root.join(rel),
            format!("directory nesting deeper than {MAX_PACK_DEPTH}"),
        ));
    }
    let abs_dir = root.join(rel);
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&abs_dir)
        .with_context(|| format!("failed to read {}", abs_dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();

    for name in names {
        let name_str = name.to_string_lossy().to_string();
        let child_rel = rel.join(&name);
        let abs = root.join(&child_rel);
        let archive_name = Path::new(".").join(&child_rel);
        let meta = std::fs::symlink_metadata(&abs)
            .with_context(|| format!("failed to stat {}", abs.display()))?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            let target = std::fs::read_link(&abs)
                .with_context(|| format!("failed to read link {}", abs.display()))?;
            let inside = match std::fs::canonicalize(&abs) {
                Ok(resolved) => resolved.starts_with(root),
                // Dangling or looping: judge the link text alone.
                Err(_) => lexical_target_inside(rel, &target),
            };
            if !inside {
                return Err(refuse(
                    &abs,
                    format!("symlink points outside the skill ({})", target.display()),
                ));
            }
            state.files += 1;
            tar.append_path_with_name(&abs, &archive_name)
                .with_context(|| format!("failed to pack link {}", abs.display()))?;
        } else if ft.is_dir() {
            if is_excluded_dir(&name_str) {
                debug!(path = %abs.display(), "publish: excluding directory from pack");
                continue;
            }
            tar.append_dir(&archive_name, &abs)
                .with_context(|| format!("failed to pack {}", abs.display()))?;
            pack_walk(tar, top, root, &child_rel, depth + 1, state)?;
        } else if ft.is_file() {
            if is_excluded_file(&name_str) {
                debug!(path = %abs.display(), "publish: excluding likely secret from pack");
                continue;
            }
            state.files += 1;
            state.bytes = state.bytes.saturating_add(meta.len());
            if state.files > MAX_PACK_FILES || state.bytes > MAX_PACK_BYTES {
                return Err(refuse(
                    &abs,
                    format!("skill exceeds the upload cap ({MAX_PACK_FILES} files / {MAX_PACK_BYTES} bytes)"),
                ));
            }
            tar.append_path_with_name(&abs, &archive_name)
                .with_context(|| format!("failed to pack {}", abs.display()))?;
        } else {
            warn!(path = %abs.display(), "publish: skipping special file (FIFO/socket/device)");
        }
    }
    Ok(())
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
        let db_dir = home.path().join(".scenery");
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
        AppState {
            root_dir,
            db_path,
            block_third_party_inference: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    // ── pack safety ───────────────────────────────────────────────────────────

    fn tar_names(gz: &[u8]) -> Vec<String> {
        let dec = flate2::read::GzDecoder::new(gz);
        let mut ar = tar::Archive::new(dec);
        let mut v: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn pack_includes_a_normal_skill_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("prompts")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("prompts/a.md"), "a").unwrap();
        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        assert!(names.contains(&"SKILL.md".to_string()), "{names:?}");
        assert!(names.contains(&"prompts/a.md".to_string()), "{names:?}");
    }

    #[test]
    fn pack_excludes_secrets_vcs_and_dependency_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::create_dir_all(d.join("node_modules/x")).unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join(".git/config"), "git").unwrap();
        fs::write(d.join("node_modules/x/i.js"), "js").unwrap();
        for f in [
            ".env",
            ".env.local",
            ".envrc",
            "server.pem",
            "tls.KEY",
            "id_rsa",
            "id_ed25519",
            "sub/id_ecdsa",
            "sub/.env.production",
        ] {
            fs::write(d.join(f), "secret").unwrap();
        }
        // Not secrets: must still travel.
        fs::write(d.join("id_rsa.pub"), "pub").unwrap();
        fs::write(d.join("id_helper.py"), "print(1)").unwrap();
        fs::write(d.join("keyboard.md"), "k").unwrap();
        fs::write(d.join("environment.md"), "e").unwrap();

        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        for banned in [
            ".git",
            "node_modules",
            ".env",
            ".envrc",
            "server.pem",
            "tls.KEY",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            ".env.production",
        ] {
            assert!(
                !names
                    .iter()
                    .any(|n| n.contains(banned) && !n.contains("id_rsa.pub")),
                "{banned} must not be packed: {names:?}"
            );
        }
        for kept in [
            "SKILL.md",
            "id_rsa.pub",
            "id_helper.py",
            "keyboard.md",
            "environment.md",
        ] {
            assert!(
                names.contains(&kept.to_string()),
                "{kept} missing: {names:?}"
            );
        }
    }

    /// `creds -> <dir outside the skill>`: the files behind it must not be
    /// uploaded. The skill is refused outright and the error says why.
    #[cfg(unix)]
    #[test]
    fn pack_refuses_a_symlink_that_points_outside_the_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("aws");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("credentials"), "AKIAFAKE").unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("creds")).unwrap();

        let err = pack_dir_to_tar_gz(&d).unwrap_err();
        let refused = err.downcast_ref::<PackRefused>().expect("PackRefused");
        assert!(refused.reason.contains("outside"), "{}", refused.reason);
        assert!(refused.path.ends_with("creds"));
    }

    #[cfg(unix)]
    #[test]
    fn pack_refuses_relative_and_absolute_escapes_and_file_links() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("outside.txt"), "o").unwrap();
        for (label, target) in [
            ("rel-dir", "../../"),
            ("rel-file", "../../outside.txt"),
            ("abs-file", "/etc/hosts"),
            ("dangling-up", "../../../nowhere"),
        ] {
            let d = tmp.path().join(label);
            fs::create_dir_all(d.join("sub")).unwrap();
            fs::write(d.join("SKILL.md"), "x").unwrap();
            std::os::unix::fs::symlink(target, d.join("sub/link")).unwrap();
            let err = pack_dir_to_tar_gz(&d).unwrap_err();
            assert!(
                err.downcast_ref::<PackRefused>().is_some(),
                "{label}: {err:#}"
            );
        }
    }

    /// A link that stays inside the tree is archived AS A LINK (never as the
    /// content it points at).
    #[cfg(unix)]
    #[test]
    fn pack_keeps_inside_symlinks_as_links_and_never_follows_them() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("real")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("real/data.md"), "data").unwrap();
        std::os::unix::fs::symlink("real", d.join("alias")).unwrap();
        std::os::unix::fs::symlink("real/data.md", d.join("data-link.md")).unwrap();
        // A loop that stays inside the tree must not recurse either.
        std::os::unix::fs::symlink(".", d.join("real/self")).unwrap();

        let gz = pack_dir_to_tar_gz(&d).unwrap();
        let dec = flate2::read::GzDecoder::new(&gz[..]);
        let mut ar = tar::Archive::new(dec);
        let mut links = Vec::new();
        let mut names = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            names.push(e.path().unwrap().to_string_lossy().to_string());
            if e.header().entry_type().is_symlink() {
                links.push(e.path().unwrap().to_string_lossy().to_string());
            }
        }
        links.sort();
        assert_eq!(links, vec!["alias", "data-link.md", "real/self"]);
        assert!(
            !names.iter().any(|n| n.starts_with("alias/")),
            "alias contents must not be duplicated: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pack_skips_fifos_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(d.join("pipe"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return;
        }
        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        assert!(!names.iter().any(|n| n.contains("pipe")), "{names:?}");
        assert!(names.contains(&"SKILL.md".to_string()));
    }

    /// Verify that the handler returns `Err` when `source_path` does not exist
    /// on disk.  The source path check fires before the auth-token lookup, so
    /// the expected error mentions "does not exist".
    ///
    /// This test avoids any env-var mutation — the `SCENERY_DISABLE_FILESYSTEM_RECONCILER`
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
            "https://app.scenery.dev".to_string(),
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
            "https://app.scenery.dev".to_string(),
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
