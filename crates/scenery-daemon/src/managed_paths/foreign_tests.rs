#![allow(clippy::unwrap_used)]

use super::*;
use std::fs;

/// `<root>/<slug>/SKILL.md` + `references/x.md`, with an old mtime so the
/// quiescence deferral does not fire unless a test asks for it.
fn skill(root: &Path, slug: &str) -> PathBuf {
    let d = root.join(slug);
    fs::create_dir_all(d.join("references")).unwrap();
    fs::write(
        d.join("SKILL.md"),
        format!("---\nname: {slug}\ndescription: demo\n---\nbody\n"),
    )
    .unwrap();
    fs::write(d.join("references/x.md"), "ref").unwrap();
    d
}

fn old_ctx(canonical_root: Option<&Path>, home: Option<&Path>) -> ClassifyContext {
    ClassifyContext {
        canonical_root: canonical_root.map(Path::to_path_buf),
        home: home.map(Path::to_path_buf),
        // Far in the future: every file is "old".
        now: Some(SystemTime::now() + Duration::from_secs(86_400)),
    }
}

fn treat(dir: &Path, slug: &str, ctx: &ClassifyContext) -> Treatment {
    classify_skill(dir, slug, ctx).treatment
}

#[test]
fn an_ordinary_skill_converts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "plain");
    let c = classify_skill(&d, "plain", &old_ctx(Some(&root), Some(tmp.path())));
    assert_eq!(c.treatment, Treatment::Convert);
    assert!(!c.tree_hash.is_empty());
    assert!(!c.recently_modified);
    assert!(c.treatment.detail().is_none(), "absent detail = convert");
}

#[cfg(unix)]
#[test]
fn a_symlinked_skill_directory_is_tracked_as_dev_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = skill(&tmp.path().join("code"), "dev");
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    let link = root.join("dev");
    std::os::unix::fs::symlink(&repo, &link).unwrap();
    assert_eq!(
        treat(&link, "dev", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("dev_symlink".into())
    );
}

#[test]
fn git_anywhere_up_to_root_tracks_dir_file_and_dotfiles_repo() {
    // .git inside the skill.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "inside");
    fs::create_dir_all(d.join(".git")).unwrap();
    assert_eq!(
        treat(&d, "inside", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("git_tracked".into())
    );

    // `.git` as a FILE (worktree / submodule) in the skill dir.
    let d2 = skill(&root, "wt");
    fs::write(d2.join(".git"), "gitdir: ../x").unwrap();
    assert!(matches!(
        treat(&d2, "wt", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track(r) if r == "git_tracked"
    ));

    // A dotfiles repo rooted two levels ABOVE the skills dir (`~/.git`).
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".git")).unwrap();
    let root2 = home.path().join(".agents").join("skills");
    let d3 = skill(&root2, "dotfiled");
    assert_eq!(
        treat(&d3, "dotfiled", &old_ctx(Some(&root2), Some(home.path()))),
        Treatment::Track("git_tracked".into())
    );
}

#[cfg(unix)]
#[test]
fn any_symlink_inside_the_tree_tracks_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "linky");
    std::os::unix::fs::symlink("references/x.md", d.join("alias.md")).unwrap();
    assert_eq!(
        treat(&d, "linky", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("symlink_in_tree".into())
    );
}

#[test]
fn foreign_installer_markers_track() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "aws");
    fs::write(d.join(".aws-skill-metadata"), "{}").unwrap();
    assert_eq!(
        treat(&d, "aws", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("foreign_installer:.aws-skill-metadata".into())
    );
}

#[test]
fn the_npx_skills_lock_and_chezmoi_source_track() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".agents").join("skills");
    let d = skill(&root, "from-npx");
    fs::write(
        home.path().join(".agents/.skill-lock.json"),
        r#"{"version":3,"skills":{"from-npx":{"source":"x"}}}"#,
    )
    .unwrap();
    assert_eq!(
        treat(&d, "from-npx", &old_ctx(Some(&root), Some(home.path()))),
        Treatment::Track("foreign_installer:npx_skills_lock".into())
    );

    let d2 = skill(&root, "dotted");
    let src = home
        .path()
        .join(".local/share/chezmoi/dot_agents/skills/private_dotted");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("SKILL.md"), "x").unwrap();
    assert_eq!(
        treat(&d2, "dotted", &old_ctx(Some(&root), Some(home.path()))),
        Treatment::Track("dotfiles_managed:chezmoi".into())
    );
}

#[test]
fn over_caps_and_excluded_content_track_instead_of_holding() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let big = skill(&root, "big");
    for i in 0..=scenery_core::skill_tree::CONVERT_MAX_FILES {
        fs::write(big.join(format!("f{i}.md")), "x").unwrap();
    }
    assert_eq!(
        treat(&big, "big", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("over_caps".into())
    );

    let nm = skill(&root, "withdeps");
    fs::create_dir_all(nm.join("node_modules/x")).unwrap();
    assert!(matches!(
        treat(&nm, "withdeps", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track(r) if r == "excluded_content:node_modules"
    ));
}

#[test]
fn a_frontmatter_name_that_differs_from_the_folder_is_tracked_not_renamed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "folder-name");
    fs::write(
        d.join("SKILL.md"),
        "---\nname: Other Name\ndescription: demo\n---\nbody\n",
    )
    .unwrap();
    assert_eq!(
        treat(&d, "folder-name", &old_ctx(Some(&root), Some(tmp.path()))),
        Treatment::Track("name_differs_from_folder".into())
    );
}

#[test]
fn secrets_hold_the_skill_by_file_and_by_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let env = skill(&root, "with-env");
    fs::write(env.join(".env"), "TOKEN=1").unwrap();
    let pat = skill(&root, "with-key");
    fs::write(
        pat.join("references/x.md"),
        "-----BEGIN RSA PRIVATE KEY-----\nabc",
    )
    .unwrap();
    for (d, s) in [(&env, "with-env"), (&pat, "with-key")] {
        assert_eq!(
            treat(d, s, &old_ctx(Some(&root), Some(tmp.path()))),
            Treatment::Hold("secrets_detected".into())
        );
    }
    let detail = Treatment::Hold("secrets_detected".into()).detail().unwrap();
    assert_eq!(detail["treatment"], "hold");
    assert_eq!(detail["hold_reason"], "secrets_detected");
}

#[test]
fn a_noncanonical_skill_colliding_with_the_canonical_slug_is_held_case_and_nfc_folded() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().join("agents-skills");
    skill(&canonical, "pdf");
    let pi = tmp.path().join("pi-skills");
    let d = skill(&pi, "pdf");
    let ctx = old_ctx(Some(&canonical), Some(tmp.path()));
    assert_eq!(
        treat(&d, "pdf", &ctx),
        Treatment::Hold("slug_collision".into())
    );

    // Case-folded: `PDF` in pi collides with canonical `pdf` (APFS).
    let upper = skill(&pi, "PDF");
    assert_eq!(
        treat(&upper, "PDF", &ctx),
        Treatment::Hold("slug_collision".into())
    );

    // NFC vs NFD spellings of the same accented slug are one folder.
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    assert_eq!(fold_slug(nfc), fold_slug(nfd));
    skill(&canonical, nfc);
    let pi_nfd = pi.join(nfd);
    fs::create_dir_all(&pi_nfd).unwrap();
    fs::write(pi_nfd.join("SKILL.md"), "---\nname: cafe\n---\n").unwrap();
    assert_eq!(
        treat(&pi_nfd, nfd, &ctx),
        Treatment::Hold("slug_collision".into())
    );
}

#[test]
fn the_canonical_entry_itself_never_collides_with_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().join("agents-skills");
    let d = skill(&canonical, "pdf");
    assert_eq!(
        treat(&d, "pdf", &old_ctx(Some(&canonical), Some(tmp.path()))),
        Treatment::Convert
    );
}

#[test]
fn two_noncanonical_sources_for_one_slug_are_both_held() {
    let canonical = PathBuf::from("/h/.agents/skills");
    let items = vec![
        ("Pdf".to_string(), PathBuf::from("/h/.pi/agent/skills/Pdf")),
        ("pdf".to_string(), PathBuf::from("/h/extra/pdf")),
        ("solo".to_string(), PathBuf::from("/h/extra/solo")),
        ("pdf".to_string(), PathBuf::from("/h/.agents/skills/pdf")),
    ];
    assert_eq!(
        duplicate_non_canonical(&items, Some(&canonical)),
        vec![0, 1]
    );
}

#[test]
fn a_recent_edit_defers_and_swap_files_count_as_activity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let d = skill(&root, "busy");
    let ctx = ClassifyContext {
        canonical_root: Some(root.clone()),
        home: Some(tmp.path().to_path_buf()),
        now: None, // real clock: files were just written
    };
    assert!(classify_skill(&d, "busy", &ctx).recently_modified);
    assert!(!classify_skill(&d, "busy", &old_ctx(Some(&root), Some(tmp.path()))).recently_modified);
}

#[test]
fn treatment_detail_shapes_match_the_backend_contract() {
    let t = Treatment::Track("git_tracked".into()).detail().unwrap();
    assert_eq!(t["treatment"], "track");
    assert_eq!(t["tracked_reason"], "git_tracked");
    assert!(Treatment::Convert.detail().is_none());
}
