use crate::{
    installer::{
        deactivate_skill, install_unpacked_skill, reactivate_skill, uninstall_skill, InstallMode,
    },
    policy::Policy,
    registry::RegistryClient,
    state::AppState,
};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use sha2::{Digest, Sha256};
use tar::Archive;
use tracing::info;
use vectorhawkd_manifest::SkillPackage;

/// Silently update `skill_id` to `policy.target_version` if the currently
/// installed version is below `policy.minimum_allowed_version`.
///
/// Returns `true` if an update was performed, `false` if none was needed.
pub fn auto_update_if_needed(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    policy: &Policy,
) -> Result<bool> {
    let (Some(min_ver), Some(target_ver)) =
        (&policy.minimum_allowed_version, &policy.target_version)
    else {
        return Ok(false);
    };

    let installed_str = query_installed_version(state, skill_id)?;
    let Some(installed_str) = installed_str else {
        return Ok(false);
    };

    let installed = Version::parse(&installed_str)
        .with_context(|| format!("installed version '{installed_str}' is not valid semver"))?;

    if &installed >= min_ver {
        return Ok(false); // Already at or above the minimum.
    }

    info!(
        skill_id,
        installed = %installed,
        target = %target_ver,
        "installed version below minimum; auto-updating"
    );

    let target_str = target_ver.to_string();
    download_and_install(state, registry, skill_id, &target_str)?;

    info!(skill_id, version = %target_ver, "auto-update complete");
    Ok(true)
}

/// Install a skill from the registry by ID.
///
/// If `version` is `None`, resolves the latest published version.
/// Returns the installed version string.
pub fn install_from_registry(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    version: Option<&str>,
) -> Result<String> {
    let version = match version {
        Some(v) => v.to_string(),
        None => {
            let detail = registry
                .fetch_skill_detail(skill_id)
                .with_context(|| format!("failed to look up '{skill_id}' in the registry"))?;
            detail
                .latest_version
                .ok_or_else(|| anyhow::anyhow!("skill '{skill_id}' has no published versions"))?
        }
    };

    info!(skill_id, version, "installing from registry");
    download_and_install(state, registry, skill_id, &version)?;
    Ok(version)
}

/// Archive a skill source directory into an in-memory gzipped tar.
///
/// Unlike [`package_skill`] this does **not** validate the bundle — the
/// registry compile endpoint does all validation server-side. This is the
/// upload payload for `POST /portal/skills/compile`.
pub fn tar_gz_skill_source(skill_dir: &Utf8Path) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let enc = GzEncoder::new(&mut out, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", skill_dir.as_std_path())
            .with_context(|| format!("failed to archive {skill_dir}"))?;
        let gz = tar.into_inner().context("failed to finalize tar")?;
        gz.finish().context("failed to finalize gzip stream")?;
    }
    info!(
        path = %skill_dir,
        size_bytes = out.len(),
        "archived SKILL.md source tree"
    );
    Ok(out)
}

/// Package a skill directory into a `.cskill` tar.gz archive.
///
/// Validates the bundle first, then creates the archive in a temp directory.
/// Returns `(archive_path, sha256_hex)`.
pub fn package_skill(skill_dir: &Utf8Path) -> Result<(Utf8PathBuf, String)> {
    let pkg = SkillPackage::load_from_dir(skill_dir)
        .with_context(|| format!("skill at {skill_dir} failed validation"))?;

    let filename = format!("{}-{}.cskill", pkg.manifest.id, pkg.manifest.version);
    let archive_path = Utf8PathBuf::from_path_buf(std::env::temp_dir().join(&filename))
        .map_err(|_| anyhow::anyhow!("temp dir path is not valid UTF-8"))?;

    let file = std::fs::File::create(&archive_path)
        .with_context(|| format!("failed to create {archive_path}"))?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", skill_dir.as_std_path())
        .with_context(|| format!("failed to build archive from {skill_dir}"))?;
    tar.finish().context("failed to finalize archive")?;
    drop(tar);

    let archive_bytes = std::fs::read(&archive_path)
        .with_context(|| format!("failed to read archive {archive_path}"))?;
    let sha = hex::encode(Sha256::digest(&archive_bytes));

    info!(
        skill_id = %pkg.manifest.id,
        version = %pkg.manifest.version,
        path = %archive_path,
        sha256 = %sha,
        "packaged skill"
    );

    Ok((archive_path, sha))
}

// ── Download + install flow ───────────────────────────────────────────────────

fn download_and_install(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    version: &str,
) -> Result<()> {
    // 1. Fetch artifact metadata (download URL + expected hash).
    let metadata = registry
        .fetch_artifact_metadata(skill_id, version)
        .with_context(|| format!("failed to fetch metadata for {skill_id}@{version}"))?;

    // 2. Download the .cskill archive to a temp file.
    let tmp_dir = tempfile::TempDir::new().context("failed to create temp dir for download")?;
    let archive_path = Utf8PathBuf::from_path_buf(tmp_dir.path().join("bundle.cskill"))
        .map_err(|_| anyhow::anyhow!("temp dir path is not valid UTF-8"))?;

    registry
        .download_artifact(&metadata.download_url, &metadata.sha256, &archive_path)
        .with_context(|| format!("failed to download {skill_id}@{version}"))?;

    // 3. Extract the tar.gz archive to a staging directory.
    let staging_path = Utf8PathBuf::from_path_buf(tmp_dir.path().join("staging"))
        .map_err(|_| anyhow::anyhow!("staging path is not valid UTF-8"))?;
    std::fs::create_dir_all(&staging_path).context("failed to create staging dir")?;

    extract_skill(&archive_path, &staging_path)
        .with_context(|| format!("failed to extract {skill_id}@{version}"))?;

    // 4. Validate the extracted bundle.
    let pkg = SkillPackage::load_from_dir(&staging_path)
        .with_context(|| format!("downloaded bundle for {skill_id}@{version} failed validation"))?;

    // 5. Install via the standard installer. Registry installs always copy; symlinks
    //    are only for local developer workflows via --link.
    install_unpacked_skill(state, &pkg, InstallMode::Copy)
        .with_context(|| format!("failed to install updated {skill_id}@{version}"))?;

    Ok(())
}

/// Extract a `.cskill` (tar.gz) archive into `dest`.
fn extract_skill(archive_path: &Utf8PathBuf, dest: &Utf8PathBuf) -> Result<()> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive {archive_path}"))?;
    let gz = GzDecoder::new(file);
    let mut tar = Archive::new(gz);
    tar.unpack(dest.as_std_path())
        .with_context(|| format!("failed to unpack archive to {dest}"))?;
    Ok(())
}

fn query_installed_version(state: &AppState, skill_id: &str) -> Result<Option<String>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let ver: Option<String> = conn
        .query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(ver)
}

/// Check whether a newer version of `skill_id` is available in the registry.
///
/// Returns `Some(latest_version)` when the registry advertises a version newer
/// than what is currently installed.  Returns `None` when the skill is already
/// up-to-date, is not installed, or the registry call fails (caller should
/// treat `None` as "proceed normally" so registry unavailability is not
/// user-visible during skill execution).
///
/// This function is intentionally read-only — it never installs anything.
pub fn check_for_update(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
) -> Result<Option<Version>> {
    let installed_str = match query_installed_version(state, skill_id)? {
        Some(v) => v,
        None => return Ok(None), // not installed
    };

    let installed = Version::parse(&installed_str)
        .with_context(|| format!("installed version '{installed_str}' is not valid semver"))?;

    let detail = registry
        .fetch_skill_detail(skill_id)
        .with_context(|| format!("failed to query registry for '{skill_id}'"))?;

    let latest_str = match detail.latest_version {
        Some(v) => v,
        None => return Ok(None), // no published versions
    };

    let latest = Version::parse(&latest_str)
        .with_context(|| format!("registry returned invalid semver '{latest_str}'"))?;

    if latest > installed {
        Ok(Some(latest))
    } else {
        Ok(None)
    }
}

/// Check all installed skills for lifecycle changes and available updates from the registry.
///
/// **Lifecycle phase** (runs first, requires `POST /skills/status`):
/// - Skills reported as "unpublished" are deactivated.
/// - Skills in the "unknown" list are fully uninstalled (deleted from registry).
/// - Skills reported as "published" that are locally deactivated are reactivated.
/// - If the status endpoint is unavailable (old registry / network error), the
///   lifecycle phase is skipped and all active skills proceed to version updates.
///
/// **Version update phase** (runs after lifecycle, only for published+active skills):
/// - Skips if `manifest.update.auto_update` is false.
/// - If policy forces an update (installed < minimum_allowed_version), always applies it.
/// - Otherwise, checks if a newer version is available from the registry and applies it.
///
/// Returns the total number of state transitions (lifecycle changes + version updates).
pub fn check_skill_updates(
    state: &AppState,
    registry: &RegistryClient,
    policy_client: &dyn crate::policy::PolicyClient,
) -> Result<usize> {
    // ── Phase 1: collect all installed skills (active + deactivated) ──────────
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut all_stmt = conn.prepare("SELECT skill_id, current_status FROM installed_skills")?;
    let all_installed: Vec<(String, String)> = all_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read installed skills")?;
    drop(all_stmt);
    drop(conn);

    let all_skill_ids: Vec<String> = all_installed.iter().map(|(id, _)| id.clone()).collect();
    let mut changes = 0usize;

    // ── Phase 2: lifecycle check ──────────────────────────────────────────────
    // `published_ids` is Some(set) when the endpoint succeeded — only skills in
    // the set are eligible for version updates.  None means "skip lifecycle, update all active".
    let published_ids: Option<std::collections::HashSet<String>> = match registry
        .check_skill_status(&all_skill_ids)
    {
        Ok(status_resp) => {
            let mut published = std::collections::HashSet::new();
            for (skill_id, local_status) in &all_installed {
                if status_resp.unknown.contains(skill_id) {
                    // Skill was deleted from registry — full cleanup.
                    match uninstall_skill(state, skill_id) {
                        Ok(Some(_)) => {
                            info!(skill_id, "sync: skill removed from registry, uninstalled");
                            changes += 1;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(skill_id, error = %e, "sync: failed to uninstall unknown skill");
                        }
                    }
                } else if let Some(entry) = status_resp.statuses.get(skill_id) {
                    match entry.status.as_str() {
                        "unpublished" => {
                            if local_status == "active" {
                                match deactivate_skill(state, skill_id) {
                                    Ok(true) => {
                                        info!(skill_id, "sync: skill unpublished, deactivated");
                                        changes += 1;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        tracing::warn!(skill_id, error = %e, "sync: failed to deactivate unpublished skill");
                                    }
                                }
                            }
                        }
                        "published" => {
                            published.insert(skill_id.clone());
                            if local_status == "deactivated" {
                                match reactivate_skill(state, skill_id) {
                                    Ok(true) => {
                                        info!(skill_id, "sync: skill republished, reactivated");
                                        changes += 1;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        tracing::warn!(skill_id, error = %e, "sync: failed to reactivate republished skill");
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(published)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "skill lifecycle check unavailable; skipping, proceeding with version updates only"
            );
            None
        }
    };

    // ── Phase 3: version updates ──────────────────────────────────────────────
    // Re-query active skills after lifecycle changes may have altered statuses.
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut stmt = conn.prepare(
        "SELECT skill_id, active_version, install_root FROM installed_skills WHERE current_status = 'active'",
    )?;
    let active_rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read installed skills")?;
    drop(stmt);
    drop(conn);

    for (skill_id, active_version, install_root) in active_rows {
        // When lifecycle check succeeded, only update skills that are published.
        if let Some(ref published) = published_ids {
            if !published.contains(&skill_id) {
                continue;
            }
        }

        let active_path = format!("{install_root}/active");
        let pkg = match SkillPackage::load_from_dir(&active_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(skill_id, error = %e, "failed to load manifest for installed skill; skipping");
                continue;
            }
        };

        // Respect the auto_update flag (default: true).
        let auto_update = pkg
            .manifest
            .update
            .as_ref()
            .and_then(|u| u.auto_update)
            .unwrap_or(true);
        if !auto_update {
            tracing::debug!(skill_id, "auto_update=false; skipping");
            continue;
        }

        let installed = match Version::parse(&active_version) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(skill_id, version = active_version, error = %e, "invalid semver; skipping");
                continue;
            }
        };

        // Fetch policy to check for forced update.
        let policy = match policy_client.fetch_policy(&skill_id) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(skill_id, error = %e, "failed to fetch policy; skipping");
                continue;
            }
        };

        // Policy-forced update: installed < minimum_allowed_version.
        let policy_forced = policy
            .minimum_allowed_version
            .as_ref()
            .map(|min| &installed < min)
            .unwrap_or(false);

        if policy_forced {
            match auto_update_if_needed(state, registry, &skill_id, &policy) {
                Ok(true) => {
                    info!(skill_id, "policy-forced update applied");
                    changes += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(skill_id, error = %e, "policy-forced update failed");
                }
            }
            continue;
        }

        // Voluntary update: check registry for a newer version.
        let latest_version = match registry.fetch_skill_detail(&skill_id) {
            Ok(detail) => match detail.latest_version {
                Some(v) => v,
                None => {
                    tracing::debug!(skill_id, "registry has no published versions; skipping");
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!(skill_id, error = %e, "failed to fetch skill detail; skipping");
                continue;
            }
        };

        let latest = match Version::parse(&latest_version) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(skill_id, version = latest_version, error = %e, "invalid semver from registry; skipping");
                continue;
            }
        };

        if latest <= installed {
            tracing::debug!(skill_id, installed = %installed, latest = %latest, "already up to date");
            continue;
        }

        match install_from_registry(state, registry, &skill_id, Some(&latest_version)) {
            Ok(_) => {
                info!(
                    skill_id,
                    from = %installed,
                    to = %latest,
                    "voluntary update applied"
                );
                changes += 1;
            }
            Err(e) => {
                tracing::warn!(skill_id, error = %e, "voluntary update failed");
            }
        }
    }

    Ok(changes)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        installer::install_unpacked_skill,
        policy::{Policy, PolicyStatus},
        registry::RegistryClient,
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use semver::Version;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-tests-updater-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_skill_bundle(root: &Utf8PathBuf, version: &str) {
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            format!(
                "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_version: {version}\nvh_publisher: skillclub\nvh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
    }

    /// Create a tar.gz archive of a skill bundle and return (tmp_dir, archive_path, sha256_hex).
    fn create_skill_archive(version: &str) -> (tempfile::TempDir, String, String) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use sha2::{Digest, Sha256};

        let tmp = tempfile::TempDir::new().unwrap();
        let bundle_dir = tmp.path().join("bundle");
        let bundle_utf8 = Utf8PathBuf::from_path_buf(bundle_dir.clone()).unwrap();
        write_skill_bundle(&bundle_utf8, version);

        let archive_path = tmp.path().join("bundle.cskill");
        let file = fs::File::create(&archive_path).unwrap();
        let enc = GzEncoder::new(file, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", &bundle_dir).unwrap();
        tar.finish().unwrap();
        drop(tar);

        let archive_bytes = fs::read(&archive_path).unwrap();
        let sha = hex::encode(Sha256::digest(&archive_bytes));
        let archive_path_str = archive_path.to_string_lossy().to_string();

        (tmp, archive_path_str, sha)
    }

    #[test]
    fn auto_update_skips_when_no_minimum_version() {
        let state_root = temp_root("no-min");
        let skill_root = temp_root("no-min-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "1.0.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: None,
        };
        // RegistryClient pointing at a non-existent URL; should not be called.
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(
            !updated,
            "should not update when no minimum_allowed_version"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn auto_update_skips_when_version_already_meets_minimum() {
        let state_root = temp_root("meets-min");
        let skill_root = temp_root("meets-min-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "1.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(
            !updated,
            "should not update when installed version meets minimum"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn auto_update_skips_when_skill_not_installed() {
        let state_root = temp_root("not-installed");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let policy = Policy {
            skill_id: "ghost-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "ghost-skill", &policy).unwrap();
        assert!(!updated, "should not update when skill is not installed");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn auto_update_downloads_extracts_and_installs_new_version() {
        use mockito::Server;

        let state_root = temp_root("auto-update-happy");
        let skill_root = temp_root("auto-update-happy-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Install v1.0.0 (below minimum)
        write_skill_bundle(&skill_root, "1.0.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        // Create a v2.0.0 archive to serve
        let (_tmp, archive_path, sha) = create_skill_archive("2.0.0");
        let archive_bytes = fs::read(&archive_path).unwrap();

        let mut server = Server::new();
        let download_path = "/download/test-skill-2.0.0.cskill";

        // Mock metadata endpoint
        let meta_mock = server
            .mock("GET", "/skills/test-skill/versions/2.0.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{
                    "skill_id": "test-skill",
                    "version": "2.0.0",
                    "download_url": "{}{download_path}",
                    "sha256": "{sha}",
                    "size_bytes": {}
                }}"#,
                server.url(),
                archive_bytes.len()
            ))
            .create();

        // Mock download endpoint
        let dl_mock = server
            .mock("GET", download_path)
            .with_status(200)
            .with_body(&archive_bytes)
            .create();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("2.0.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("2.0.0").unwrap()),
            blocked_message: None,
        };

        let registry = RegistryClient::new(server.url());
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(updated, "should have performed the update");

        // Verify the new version is now installed
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let active_ver: String = conn
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_ver, "2.0.0");

        // Verify files on disk
        let install_path = state
            .root_dir
            .join("skills/test-skill/versions/2.0.0/SKILL.md");
        assert!(install_path.exists(), "SKILL.md should exist for v2.0.0");

        meta_mock.assert();
        dl_mock.assert();
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    // ── check_skill_updates tests ─────────────────────────────────────────────

    #[test]
    fn check_skill_updates_updates_stale_skill() {
        use mockito::Server;

        let state_root = temp_root("csu-stale");
        let skill_root = temp_root("csu-stale-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Install v0.1.0
        write_skill_bundle(&skill_root, "0.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        // Create a v0.2.0 archive to serve
        let (_tmp, archive_path, sha) = create_skill_archive("0.2.0");
        let archive_bytes = fs::read(&archive_path).unwrap();

        let mut server = Server::new();
        let detail_path = "/portal/skills/test-skill";
        let meta_path = "/skills/test-skill/versions/0.2.0";
        let download_path = "/download/test-skill-0.2.0.cskill";

        // Mock detail endpoint (latest_version = 0.2.0)
        let detail_mock = server
            .mock("GET", detail_path)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"skill_id":"test-skill","name":"Test Skill","latest_version":"0.2.0","publisher_name":null,"description":null}"#)
            .create();

        // Mock artifact metadata
        let meta_mock = server
            .mock("GET", meta_path)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"skill_id":"test-skill","version":"0.2.0","download_url":"{}{download_path}","sha256":"{sha}","size_bytes":{}}}"#,
                server.url(),
                archive_bytes.len()
            ))
            .create();

        // Mock download
        let dl_mock = server
            .mock("GET", download_path)
            .with_status(200)
            .with_body(&archive_bytes)
            .create();

        let policy_client = crate::policy::MockPolicyClient::new();
        let registry = RegistryClient::new(server.url());

        let count = check_skill_updates(&state, &registry, &policy_client).unwrap();
        assert_eq!(count, 1, "one skill should have been updated");

        // Verify active version in DB
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let active_ver: String = conn
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_ver, "0.2.0");

        detail_mock.assert();
        meta_mock.assert();
        dl_mock.assert();

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn check_skill_updates_skips_current_version() {
        use mockito::Server;

        let state_root = temp_root("csu-current");
        let skill_root = temp_root("csu-current-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Install v0.2.0 (already at latest)
        write_skill_bundle(&skill_root, "0.2.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let mut server = Server::new();
        let detail_path = "/portal/skills/test-skill";

        // Registry says latest is 0.2.0 — same as installed
        let detail_mock = server
            .mock("GET", detail_path)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"skill_id":"test-skill","name":"Test Skill","latest_version":"0.2.0","publisher_name":null,"description":null}"#)
            .create();

        let policy_client = crate::policy::MockPolicyClient::new();
        let registry = RegistryClient::new(server.url());

        let count = check_skill_updates(&state, &registry, &policy_client).unwrap();
        assert_eq!(count, 0, "should not update when already at latest version");

        detail_mock.assert();

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    // ── lifecycle sync tests ──────────────────────────────────────────────────

    #[test]
    fn test_sync_deactivates_unpublished_skill() {
        use mockito::Server;

        let state_root = temp_root("sync-deactivate");
        let skill_root = temp_root("sync-deactivate-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "0.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let mut server = Server::new();

        // Status endpoint returns "unpublished"
        let status_mock = server
            .mock("POST", "/skills/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"statuses":{"test-skill":{"status":"unpublished"}},"unknown":[]}"#)
            .create();

        let registry = RegistryClient::new(server.url());
        let policy_client = crate::policy::MockPolicyClient::new();

        let count = check_skill_updates(&state, &registry, &policy_client).unwrap();
        assert_eq!(
            count, 1,
            "one lifecycle change (deactivation) should be counted"
        );

        // Verify skill is deactivated in DB
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "deactivated");

        // Verify active symlink is removed
        let install_root = state.root_dir.join("skills/test-skill");
        let active_path = install_root.join("active");
        assert!(
            !active_path.exists() && !active_path.is_symlink(),
            "active symlink should be removed after deactivation"
        );

        // Versioned files should still be present
        assert!(install_root.join("versions/0.1.0/SKILL.md").exists());

        status_mock.assert();
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn test_sync_deletes_unknown_skill() {
        use mockito::Server;

        let state_root = temp_root("sync-unknown");
        let skill_root = temp_root("sync-unknown-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "0.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let install_root = state.root_dir.join("skills/test-skill");
        assert!(install_root.exists(), "skill dir should exist before sync");

        let mut server = Server::new();

        // Status endpoint returns skill in "unknown" list (deleted from registry)
        let status_mock = server
            .mock("POST", "/skills/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"statuses":{},"unknown":["test-skill"]}"#)
            .create();

        let registry = RegistryClient::new(server.url());
        let policy_client = crate::policy::MockPolicyClient::new();

        let count = check_skill_updates(&state, &registry, &policy_client).unwrap();
        assert_eq!(
            count, 1,
            "one lifecycle change (uninstall) should be counted"
        );

        // Verify all files are gone
        assert!(!install_root.exists(), "skill dir should be removed");

        // Verify DB records are gone
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let installed_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(installed_count, 0, "installed_skills row should be deleted");

        let version_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM skill_versions WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version_count, 0, "skill_versions rows should be deleted");

        status_mock.assert();
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn test_sync_reactivates_republished_skill() {
        use mockito::Server;

        let state_root = temp_root("sync-reactivate");
        let skill_root = temp_root("sync-reactivate-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "0.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        // Deactivate the skill to set up the test scenario
        crate::installer::deactivate_skill(&state, "test-skill").unwrap();

        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "deactivated",
            "skill should be deactivated before sync"
        );
        drop(conn);

        let mut server = Server::new();

        // Status endpoint returns skill as "published"
        let status_mock = server
            .mock("POST", "/skills/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"statuses":{"test-skill":{"status":"published","latest_version":"0.1.0"}},"unknown":[]}"#,
            )
            .create();

        // After reactivation the version update loop will check for updates.
        // Return same version (0.1.0) so no update is applied.
        let detail_mock = server
            .mock("GET", "/portal/skills/test-skill")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"skill_id":"test-skill","name":"Test Skill","latest_version":"0.1.0","publisher_name":null,"description":null}"#,
            )
            .create();

        let registry = RegistryClient::new(server.url());
        let policy_client = crate::policy::MockPolicyClient::new();

        let count = check_skill_updates(&state, &registry, &policy_client).unwrap();
        assert_eq!(
            count, 1,
            "one lifecycle change (reactivation) should be counted"
        );

        // Verify skill is active in DB
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");

        // Verify active symlink is restored
        let active_path = state.root_dir.join("skills/test-skill/active");
        assert!(
            active_path.exists(),
            "active symlink should be restored after reactivation"
        );

        status_mock.assert();
        detail_mock.assert();
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
