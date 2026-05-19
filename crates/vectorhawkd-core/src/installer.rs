use crate::state::AppState;
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use vectorhawkd_manifest::SkillPackage;

/// Best-effort registration of an installed skill into `<home>/.claude/skills/`.
///
/// Creates a symlink at `<home>/.claude/skills/<skill_id>` that points at the
/// runner-managed `active` directory. This makes the skill visible to
/// Claude Code's native Skills mechanism without restarting the client.
///
/// Idempotent: if the path is already our symlink we replace it. If a real
/// directory exists there (user-managed skill), we leave it alone so the
/// runner never clobbers user content. Failures are non-fatal — we log
/// and continue, since Claude Code may not be installed.
fn register_in_claude_skills_at(
    home: &std::path::Path,
    skill_id: &str,
    active_dir: &Utf8Path,
) {
    let skills_dir = home.join(".claude").join("skills");
    if let Err(e) = fs::create_dir_all(&skills_dir) {
        tracing::warn!(error = %e, "could not create ~/.claude/skills/");
        return;
    }
    let link = skills_dir.join(skill_id);

    if link.is_symlink() {
        let _ = fs::remove_file(&link);
    } else if link.exists() {
        tracing::warn!(
            skill_id,
            path = %link.display(),
            "~/.claude/skills/<id> is a real directory; not overwriting user content"
        );
        return;
    }

    #[cfg(target_family = "unix")]
    if let Err(e) = std::os::unix::fs::symlink(active_dir.as_std_path(), &link) {
        tracing::warn!(skill_id, error = %e, "could not symlink into ~/.claude/skills/");
    }
}

/// Remove the `<home>/.claude/skills/<skill_id>` symlink we created. Only
/// removes the entry if it's a symlink — never touches a real directory.
fn unregister_from_claude_skills_at(home: &std::path::Path, skill_id: &str) {
    let link = home.join(".claude").join("skills").join(skill_id);
    if link.is_symlink() {
        let _ = fs::remove_file(&link);
    }
}

/// Public wrappers used by production call sites — resolve home from $HOME
/// at call time. Returns early when $HOME is unset or when the
/// `VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK` env var is set (used by tests so
/// they never touch the developer's real `~/.claude/skills/`).
fn register_in_claude_skills(skill_id: &str, active_dir: &Utf8Path) {
    if std::env::var_os("VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK").is_some() {
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        register_in_claude_skills_at(std::path::Path::new(&home), skill_id, active_dir);
    }
}
fn unregister_from_claude_skills(skill_id: &str) {
    if std::env::var_os("VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK").is_some() {
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        unregister_from_claude_skills_at(std::path::Path::new(&home), skill_id);
    }
}

/// Controls how a skill source directory is placed into the versioned install layout.
#[derive(Clone, Copy, Debug)]
pub enum InstallMode {
    /// Copy the source directory into `versions/{ver}/` (default, used by registry installs).
    Copy,
    /// Make `versions/{ver}/` itself a symlink pointing at the source directory.
    /// Changes to the source directory are immediately visible through `active/`.
    /// Only supported on Unix; returns an error on other platforms.
    Symlink,
}

pub fn install_unpacked_skill(
    state: &AppState,
    skill: &SkillPackage,
    mode: InstallMode,
) -> Result<()> {
    let install_root = state.root_dir.join("skills").join(&skill.manifest.id);
    let versions_dir = install_root.join("versions");
    fs::create_dir_all(&versions_dir)?;

    let version_dir = versions_dir.join(skill.manifest.version.to_string());

    let source_type = install_with_mode(&skill.root, &version_dir, mode)?;

    let active_dir = install_root.join("active");
    // Use `.exists() || .is_symlink()` so that a dangling symlink (e.g. from a
    // previous --link install whose source was moved) is also cleaned up.
    // `.exists()` alone returns false for dangling symlinks, which would leave a
    // stale symlink entry and cause the following `symlink()` call to fail with
    // EEXIST on a re-install.
    if active_dir.exists() || active_dir.is_symlink() {
        fs::remove_file(&active_dir)
            .or_else(|_| fs::remove_dir_all(&active_dir))
            .ok();
    }
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&version_dir, &active_dir)?;

    let conn = Connection::open(&state.db_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO installed_skills(skill_id, active_version, install_root, channel, current_status) VALUES (?, ?, ?, ?, 'active')",
        params![
            skill.manifest.id,
            skill.manifest.version.to_string(),
            install_root.as_str(),
            skill.manifest.update.as_ref().and_then(|u| u.channel.clone()).unwrap_or_else(|| "stable".to_string())
        ],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO skill_versions(skill_id, version, install_path, source_type) VALUES (?, ?, ?, ?)",
        params![
            skill.manifest.id,
            skill.manifest.version.to_string(),
            version_dir.as_str(),
            source_type,
        ],
    )?;

    register_in_claude_skills(&skill.manifest.id, &active_dir);

    Ok(())
}

/// Perform the file-system placement for one version slot, returning the
/// `source_type` string to record in `skill_versions`.
fn install_with_mode(
    source: &Utf8Path,
    version_dir: &Utf8Path,
    mode: InstallMode,
) -> Result<&'static str> {
    match mode {
        InstallMode::Copy => {
            if version_dir.exists() {
                fs::remove_dir_all(version_dir)
                    .with_context(|| format!("failed to remove existing {version_dir}"))?;
            }
            copy_dir_all(source, version_dir)
                .with_context(|| format!("failed to copy skill into {version_dir}"))?;
            Ok("local_dir")
        }
        InstallMode::Symlink => {
            symlink_version_dir(source, version_dir)?;
            Ok("local_symlink")
        }
    }
}

/// Create `version_dir` as a symlink pointing at the canonical (absolute) path
/// of `source`.
///
/// Only available on Unix. On other platforms this always returns an error.
fn symlink_version_dir(source: &Utf8Path, version_dir: &Utf8Path) -> Result<()> {
    #[cfg(target_family = "unix")]
    {
        let abs_source = std::fs::canonicalize(source)
            .with_context(|| format!("failed to canonicalize source path {source}"))?;

        if version_dir.exists() || version_dir.is_symlink() {
            fs::remove_file(version_dir)
                .or_else(|_| fs::remove_dir_all(version_dir))
                .with_context(|| format!("failed to remove existing {version_dir}"))?;
        }
        std::os::unix::fs::symlink(&abs_source, version_dir).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                version_dir,
                abs_source.display()
            )
        })?;
        Ok(())
    }
    #[cfg(not(target_family = "unix"))]
    {
        let _ = (source, version_dir);
        Err(anyhow::anyhow!(
            "--link (Symlink install mode) is only supported on Unix; \
             use the default copy mode on this platform"
        ))
    }
}

/// Uninstall a skill completely.
///
/// Returns `Ok(Some(version))` with the previously active version string, or
/// `Ok(None)` if the skill was not installed.
pub fn uninstall_skill(state: &AppState, skill_id: &str) -> Result<Option<String>> {
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT install_root, active_version FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (install_root, active_version) = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }

    let skill_dir = std::path::Path::new(&install_root);
    if skill_dir.exists() {
        fs::remove_dir_all(skill_dir)?;
    }

    conn.execute("DELETE FROM skill_versions WHERE skill_id = ?1", [skill_id])?;
    conn.execute(
        "DELETE FROM installed_skills WHERE skill_id = ?1",
        [skill_id],
    )?;

    unregister_from_claude_skills(skill_id);

    Ok(Some(active_version))
}

/// Deactivate an installed skill (set status = 'deactivated', remove active symlink).
///
/// Returns `true` if the skill was active and has been deactivated,
/// `false` if the skill was not found or was already deactivated.
pub fn deactivate_skill(state: &AppState, skill_id: &str) -> Result<bool> {
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT install_root, current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (install_root, current_status) = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    if current_status != "active" {
        return Ok(false);
    }

    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }

    conn.execute(
        "UPDATE installed_skills SET current_status = 'deactivated' WHERE skill_id = ?1",
        [skill_id],
    )?;

    unregister_from_claude_skills(skill_id);

    Ok(true)
}

/// Reactivate a deactivated skill (restore active symlink, set status = 'active').
///
/// Returns `true` if the skill was deactivated and has been reactivated,
/// `false` if the skill was not found or was not deactivated.
pub fn reactivate_skill(state: &AppState, skill_id: &str) -> Result<bool> {
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT install_root, active_version, current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let (install_root, active_version, current_status) = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    if current_status != "deactivated" {
        return Ok(false);
    }

    let versions_dir = std::path::Path::new(&install_root).join("versions");
    let target_version_dir = versions_dir.join(&active_version);

    let version_dir = if target_version_dir.exists() {
        target_version_dir
    } else {
        let mut entries: Vec<std::path::PathBuf> = fs::read_dir(&versions_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        match entries.into_iter().last() {
            Some(p) => p,
            None => anyhow::bail!("no version directories found for skill '{skill_id}'"),
        }
    };

    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&version_dir, &active_path)?;

    conn.execute(
        "UPDATE installed_skills SET current_status = 'active' WHERE skill_id = ?1",
        [skill_id],
    )?;

    let active_utf8 = Utf8PathBuf::from_path_buf(active_path)
        .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().into_owned()));
    register_in_claude_skills(skill_id, &active_utf8);

    Ok(true)
}

/// Where an installed skill lives: in the global user store, or local to a project.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallScope {
    /// Global user install: `~/Library/Application Support/VectorHawk/skills/...`
    User,
    /// Project-local install: `{project_root}/.vectorhawk/skills/{id}/`
    Project(Utf8PathBuf),
}

fn copy_dir_all(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test_name: &str) -> Utf8PathBuf {
        // Tests must never touch the developer's real ~/.claude/skills/.
        // Setting this env var to any value disables the registration
        // side effect of install/uninstall/deactivate/reactivate.
        // The dedicated coverage below tests the `_at` helpers directly.
        std::env::set_var("VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK", "1");

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vh-installer-tests-{test_name}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn example_skill_path() -> Utf8PathBuf {
        // Relative to the crate root — works from `cargo test` invocation
        Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/skills/contract-compare")
    }

    #[test]
    fn install_unpacked_skill_copies_files_and_records_metadata() {
        let root = temp_root("install");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let skill =
            SkillPackage::load_from_dir(example_skill_path()).expect("example skill should load");

        let version = skill.manifest.version.to_string();
        install_unpacked_skill(&state, &skill, InstallMode::Copy).expect("install should succeed");

        let install_root = state.root_dir.join("skills").join("contract-compare");
        let version_dir = install_root.join("versions").join(&version);
        assert!(
            version_dir.join("SKILL.md").exists() || version_dir.join("workflow.yaml").exists()
        );

        #[cfg(target_family = "unix")]
        {
            let active_dir = install_root.join("active");
            assert!(active_dir.exists());
            let symlink_target = fs::read_link(&active_dir).expect("active symlink should exist");
            assert_eq!(symlink_target, version_dir.as_std_path());
        }

        let conn = Connection::open(&state.db_path).expect("state db should open");
        let installed_row: (String, String, String) = conn
            .query_row(
                "SELECT skill_id, active_version, current_status FROM installed_skills WHERE skill_id = ?1",
                ["contract-compare"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("installed skill row should exist");

        assert_eq!(
            installed_row,
            (
                "contract-compare".to_string(),
                version.clone(),
                "active".to_string()
            )
        );

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    fn install_example_skill(state: &AppState) -> String {
        let skill =
            SkillPackage::load_from_dir(example_skill_path()).expect("example skill should load");
        let version = skill.manifest.version.to_string();
        install_unpacked_skill(state, &skill, InstallMode::Copy).expect("install should succeed");
        version
    }

    #[test]
    fn test_uninstall_removes_files_and_db_records() {
        let root = temp_root("uninstall");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let version = install_example_skill(&state);

        let install_root = state.root_dir.join("skills").join("contract-compare");
        assert!(
            install_root.exists(),
            "install dir should exist before uninstall"
        );

        let result = uninstall_skill(&state, "contract-compare").expect("uninstall should succeed");
        assert_eq!(result, Some(version), "should return the active version");
        assert!(!install_root.exists(), "install dir should be removed");

        let conn = Connection::open(&state.db_path).expect("db should open");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .expect("should query");
        assert_eq!(count, 0, "installed_skills row should be deleted");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_uninstall_nonexistent_skill_returns_none() {
        let root = temp_root("uninstall-none");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let result = uninstall_skill(&state, "ghost-skill").expect("uninstall should not error");
        assert_eq!(result, None, "should return None for uninstalled skill");
        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_deactivate_skill_updates_status_and_removes_symlink() {
        let root = temp_root("deactivate");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);

        let changed =
            deactivate_skill(&state, "contract-compare").expect("deactivate should succeed");
        assert!(
            changed,
            "should return true when deactivating an active skill"
        );

        let conn = Connection::open(&state.db_path).expect("db should open");
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .expect("should query");
        assert_eq!(status, "deactivated");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_reactivate_skill_restores_symlink_and_status() {
        let root = temp_root("reactivate");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);
        deactivate_skill(&state, "contract-compare").expect("deactivate should succeed");

        let changed =
            reactivate_skill(&state, "contract-compare").expect("reactivate should succeed");
        assert!(
            changed,
            "should return true when reactivating a deactivated skill"
        );

        let conn = Connection::open(&state.db_path).expect("db should open");
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .expect("should query");
        assert_eq!(status, "active");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_deactivate_already_deactivated_returns_false() {
        let root = temp_root("deactivate-idempotent");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);
        deactivate_skill(&state, "contract-compare").expect("first deactivate should succeed");
        let changed = deactivate_skill(&state, "contract-compare")
            .expect("second deactivate should not error");
        assert!(!changed, "should return false when already deactivated");
        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_reactivate_nonexistent_skill_returns_false() {
        let root = temp_root("reactivate-none");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let changed = reactivate_skill(&state, "ghost-skill").expect("reactivate should not error");
        assert!(!changed, "should return false for non-existent skill");
        let _ = fs::remove_dir_all(&state.root_dir);
    }

    // ── Bug #6 regression test ──────────────────────────────────────────────
    // When `--link` mode is used and the source directory is edited (or
    // temporarily removed), the `active/` symlink becomes dangling.  A
    // subsequent re-install with `--link` must succeed even though the dangling
    // symlink still exists on disk.  The previous code used `active_dir.exists()`
    // which returns `false` for dangling symlinks, so the cleanup was skipped and
    // the following `symlink()` call failed with EEXIST.
    #[cfg(target_family = "unix")]
    #[test]
    fn symlink_install_survives_dangling_active_symlink_on_reinstall() {
        let root = temp_root("link-reinstall");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        // Use a temp source directory we control so we can make the symlink dangle.
        let source_dir = temp_root("link-reinstall-source");
        fs::create_dir_all(&source_dir).expect("create source dir");

        // Copy the example skill files into our controlled source dir so we get
        // a valid SkillPackage.
        let example = example_skill_path();
        for entry in fs::read_dir(&example).expect("read example dir") {
            let entry = entry.expect("read entry");
            let dest = source_dir.join(entry.file_name().to_string_lossy().as_ref());
            if entry.file_type().expect("file type").is_dir() {
                // shallow copy — sub-dirs are not needed for this test
                continue;
            }
            fs::copy(entry.path(), &dest).expect("copy file");
        }
        // Copy sub-directories too (prompts/, schemas/).
        for entry in fs::read_dir(&example).expect("read example dir") {
            let entry = entry.expect("read entry");
            if entry.file_type().expect("file type").is_dir() {
                let src_sub = entry.path();
                let dst_sub = source_dir.join(entry.file_name().to_string_lossy().as_ref());
                copy_dir_all(&src_sub, &dst_sub).expect("copy sub-dir");
            }
        }

        let pkg =
            SkillPackage::load_from_dir(source_dir.clone()).expect("source skill should load");
        let skill_id = pkg.manifest.id.clone();

        // First install with --link.
        install_unpacked_skill(&state, &pkg, InstallMode::Symlink)
            .expect("first symlink install should succeed");

        let install_root = state.root_dir.join("skills").join(&skill_id);
        let active_dir = install_root.join("active");

        // Confirm the active/ symlink is healthy after first install.
        assert!(
            active_dir.exists(),
            "active dir should exist after first install"
        );

        // Simulate "source edited": remove the source directory to make the
        // versions/{ver}/ → source chain dangle, then recreate it.
        fs::remove_dir_all(&source_dir).expect("remove source to simulate edit");

        // The active/ symlink now points through versions/{ver}/ which points at
        // the gone source — it is dangling and `active_dir.exists()` returns false.
        assert!(
            !active_dir.exists(),
            "active dir should appear absent when source is gone (dangling symlink)"
        );
        assert!(
            active_dir.is_symlink(),
            "but active dir IS a symlink (dangling) — this is the bug trigger condition"
        );

        // Recreate source and reinstall with --link — this must not error.
        fs::create_dir_all(&source_dir).expect("recreate source dir");
        for entry in fs::read_dir(&example).expect("read example dir") {
            let entry = entry.expect("read entry");
            let dest = source_dir.join(entry.file_name().to_string_lossy().as_ref());
            if entry.file_type().expect("file type").is_dir() {
                continue;
            }
            fs::copy(entry.path(), &dest).expect("copy file");
        }
        for entry in fs::read_dir(&example).expect("read example dir") {
            let entry = entry.expect("read entry");
            if entry.file_type().expect("file type").is_dir() {
                let src_sub = entry.path();
                let dst_sub = source_dir.join(entry.file_name().to_string_lossy().as_ref());
                copy_dir_all(&src_sub, &dst_sub).expect("copy sub-dir");
            }
        }

        let pkg2 =
            SkillPackage::load_from_dir(source_dir.clone()).expect("recreated source should load");

        // This is the call that previously failed with EEXIST due to the dangling symlink.
        install_unpacked_skill(&state, &pkg2, InstallMode::Symlink)
            .expect("second symlink install should succeed despite prior dangling active symlink");

        // After reinstall the active/ symlink must be healthy again.
        assert!(
            active_dir.exists(),
            "active dir should resolve after successful reinstall"
        );

        let _ = fs::remove_dir_all(&state.root_dir);
        let _ = fs::remove_dir_all(&source_dir);
    }

    #[test]
    fn register_in_claude_skills_creates_symlink() {
        let home = temp_root("claude-skills-register");
        fs::create_dir_all(home.as_std_path()).expect("create fake home");
        let active = home.join("some-version-dir");
        fs::create_dir_all(active.as_std_path()).expect("create active dir");

        register_in_claude_skills_at(home.as_std_path(), "demo-skill", &active);

        let link = home
            .as_std_path()
            .join(".claude")
            .join("skills")
            .join("demo-skill");
        assert!(link.is_symlink(), "should have written a symlink");
        let target = fs::read_link(&link).expect("read_link should succeed");
        assert_eq!(target, active.as_std_path());

        unregister_from_claude_skills_at(home.as_std_path(), "demo-skill");
        assert!(!link.exists() && !link.is_symlink());

        let _ = fs::remove_dir_all(home.as_std_path());
    }

    #[test]
    fn register_preserves_real_user_directory() {
        let home = temp_root("claude-skills-preserve");
        let skills = home.as_std_path().join(".claude").join("skills");
        let user_dir = skills.join("user-owned");
        fs::create_dir_all(&user_dir).expect("seed user directory");
        fs::write(user_dir.join("SKILL.md"), "user content").expect("write user SKILL.md");

        let active = home.join("some-version-dir");
        fs::create_dir_all(active.as_std_path()).expect("create active dir");

        register_in_claude_skills_at(home.as_std_path(), "user-owned", &active);

        // Real directory must be left untouched — never replaced with a symlink.
        let preserved = fs::read_to_string(user_dir.join("SKILL.md")).unwrap();
        assert_eq!(preserved, "user content");
        assert!(!user_dir.is_symlink());

        let _ = fs::remove_dir_all(home.as_std_path());
    }

    #[test]
    fn register_replaces_existing_symlink() {
        let home = temp_root("claude-skills-replace");
        fs::create_dir_all(home.as_std_path()).expect("create fake home");
        let v1 = home.join("v1");
        let v2 = home.join("v2");
        fs::create_dir_all(v1.as_std_path()).unwrap();
        fs::create_dir_all(v2.as_std_path()).unwrap();

        register_in_claude_skills_at(home.as_std_path(), "evolving", &v1);
        register_in_claude_skills_at(home.as_std_path(), "evolving", &v2);

        let link = home
            .as_std_path()
            .join(".claude")
            .join("skills")
            .join("evolving");
        let target = fs::read_link(&link).unwrap();
        assert_eq!(target, v2.as_std_path());

        let _ = fs::remove_dir_all(home.as_std_path());
    }
}
