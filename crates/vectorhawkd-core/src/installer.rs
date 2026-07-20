use crate::state::AppState;
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use vectorhawkd_manifest::SkillPackage;

// ── `~/.claude/skills/` ownership ─────────────────────────────────────────────
//
// As of v1.0.51 the installer NO LONGER creates symlinks under
// `~/.claude/skills/<skill_id>`.  That path is owned by the F2 pusher
// (`managed_paths::pusher::push_skill`) which writes a real directory plus a
// `.vectorhawk-managed.json` marker.  Having two writers caused state to
// resurrect itself after F2-driven cleanup — see the v1.0.50→1.0.51 fix.
//
// The legacy `register_in_claude_skills` / `unregister_from_claude_skills`
// helpers were removed.  Since the `~/.agents/skills` pivot the canonical
// managed-skill directory lives there, and `~/.claude/skills/<slug>` holds
// only a symlink to it.  Any pre-existing symlink or stale directory at the
// Claude path is healed by the daemon at startup via
// `managed_paths::pusher::push_missing_active_skills`, whose `push_skill`
// call re-writes the canonical dir (with its `.vectorhawk-managed.json`
// marker) and re-points the Claude link at it.

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

    // NOTE: ~/.claude/skills/<id> is owned by the F2 pusher; the installer no
    // longer writes a symlink there. F2's push_skill creates a real directory
    // + .vectorhawk-managed.json marker, so there's a single writer per path.
    let _ = active_dir;

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

    // ~/.claude/skills/<id> is owned by the F2 pusher; the installer does not
    // unlink it here. The reconciler's deactivate / purge handler invokes
    // `ManagedPathsPusher::remove_skill` for that side.

    Ok(Some(active_version))
}

/// Deactivate an installed skill because the registry reports it unpublished
/// (set status = 'deactivated', remove active symlink).
///
/// Deliberately leaves the `deactivated` integer column untouched: this
/// lifecycle path is meant to auto-reverse itself via [`reactivate_skill`]
/// when the skill is republished. Do not confuse this with the reconciler's
/// per-device kill switch (`deactivate_skill_blocking` in
/// `vectorhawkd-daemon/src/sync/reconciler.rs`), which sets `deactivated = 1`
/// precisely so that a later republish-reactivate cycle here does *not* apply
/// to it.
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

    // F2 pusher owns ~/.claude/skills/<id>; reconciler invokes
    // `ManagedPathsPusher::remove_skill` separately for that side.

    Ok(true)
}

/// Reactivate a deactivated skill (restore active symlink, set status = 'active').
///
/// Returns `true` if the skill was deactivated and has been reactivated,
/// `false` if the skill was not found, was not deactivated, or is currently
/// held deactivated by a per-device kill switch (see below).
///
/// `deactivated` (the integer column, distinct from `current_status`) is the
/// authoritative flag for a per-device kill switch set by the reconciler's
/// SSE-driven Deactivate handler (`deactivate_skill_blocking` in
/// `vectorhawkd-daemon/src/sync/reconciler.rs`), which writes it together
/// with `current_status = 'deactivated'` in one statement. This function must
/// never clear that kill switch — a skill being republished in the registry
/// is not consent to resurrect an installation an admin explicitly killed on
/// this device. The *only* legitimate caller of this reactivation path is the
/// registry unpublish/republish auto-cycle (`check_skill_updates` in
/// `updater.rs`), whose deactivation (`deactivate_skill`, below) intentionally
/// never touches `deactivated`, leaving it `0`.
pub fn reactivate_skill(state: &AppState, skill_id: &str) -> Result<bool> {
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String, String, i64)> = conn
        .query_row(
            "SELECT install_root, active_version, current_status, deactivated \
             FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    let (install_root, active_version, current_status, deactivated) = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    if current_status != "deactivated" {
        return Ok(false);
    }

    if deactivated != 0 {
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

    // Reset `deactivated`/`deactivated_at` alongside `current_status` even
    // though `deactivated` is already 0 here (checked above) — this keeps the
    // write self-contained and matches the sibling write in
    // `sync/reconciler.rs::flip_active_symlink`, so the two fields can never
    // be observed to disagree by another reader.
    conn.execute(
        "UPDATE installed_skills \
         SET current_status = 'active', deactivated = 0, deactivated_at = NULL \
         WHERE skill_id = ?1",
        [skill_id],
    )?;

    // F2 push happens in the reconciler when the desired-state row flips back
    // to installed. The installer no longer creates ~/.claude/skills/<id>.
    let _ = (active_path, Utf8PathBuf::new);

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
        // The installer no longer touches ~/.claude/skills/ — that's owned by
        // the F2 pusher. The legacy VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK
        // escape hatch is therefore no longer needed.
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

    /// Architectural invariant: install_unpacked_skill must NOT write into
    /// `~/.claude/skills/`. That path is owned exclusively by the F2 pusher
    /// in vectorhawkd-daemon. Regression test for the v1.0.51 architectural
    /// fix where a second writer caused state to resurrect after F2 cleanup.
    #[test]
    fn install_does_not_touch_claude_skills_dir() {
        let root = temp_root("install-no-claude-skills");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        // Point HOME at a temp dir so we can observe whether anything got
        // written under it. We do NOT set VECTORHAWK_DISABLE_CLAUDE_SKILLS_LINK
        // — that's a legacy escape hatch and irrelevant now.
        let fake_home = temp_root("install-no-claude-skills-home");
        fs::create_dir_all(fake_home.as_std_path()).expect("create fake home");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", fake_home.as_std_path());

        let skill =
            SkillPackage::load_from_dir(example_skill_path()).expect("example skill should load");
        install_unpacked_skill(&state, &skill, InstallMode::Copy).expect("install should succeed");

        let claude_skills = fake_home.as_std_path().join(".claude").join("skills");
        assert!(
            !claude_skills.exists(),
            "installer must NOT create ~/.claude/skills/ — that's F2's job"
        );

        if let Some(v) = prev_home {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&state.root_dir);
        let _ = fs::remove_dir_all(fake_home.as_std_path());
    }
}
