//! `scenery_core::installer::uninstall_skill` must take the F2 projection
//! (`~/.agents/skills/<slug>` + `~/.claude/skills/<slug>` link + marker row)
//! down with it, but only what carries OUR marker.
#![allow(clippy::unwrap_used)]

use super::{register_core_hooks, ManagedPathsPusher, ENV_MUTEX};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use scenery_core::{
    installer::{install_unpacked_skill, uninstall_skill, InstallMode},
    state::AppState,
};
use scenery_manifest::SkillPackage;

const SLUG: &str = "contract-compare";

fn with_fake_home<T>(f: impl FnOnce(&std::path::Path, &AppState) -> T) -> T {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(home.path().join("state")).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());
    std::env::remove_var("SCENERY_DISABLE_FILESYSTEM_RECONCILER");
    register_core_hooks();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home.path(), &state)));
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

fn install_and_push(state: &AppState) {
    let dir = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/skills")
        .join(SLUG);
    let skill = SkillPackage::load_from_dir(dir).unwrap();
    install_unpacked_skill(state, &skill, InstallMode::Copy).unwrap();
    ManagedPathsPusher::new(state)
        .push_skill(
            SLUG,
            Some("inst-1"),
            b"---\nname: contract-compare\n---\n",
            &[],
        )
        .unwrap();
}

fn marker_rows(state: &AppState) -> i64 {
    Connection::open(&state.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM managed_path_markers WHERE slug = ?1",
            [SLUG],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn uninstall_removes_native_dir_link_and_marker_row() {
    with_fake_home(|home, state| {
        install_and_push(state);
        let native = home.join(".agents/skills").join(SLUG);
        let link = home.join(".claude/skills").join(SLUG);
        assert!(native.join(".scenery-managed.json").exists());
        assert!(link.is_symlink());
        assert_eq!(marker_rows(state), 1);

        assert!(uninstall_skill(state, SLUG).unwrap().is_some());

        assert!(!native.exists(), "native dir left behind after uninstall");
        assert!(
            !link.exists() && !link.is_symlink(),
            "claude link left behind"
        );
        assert_eq!(marker_rows(state), 0, "marker row left behind");
    });
}

#[test]
fn uninstall_preserves_unmarked_user_content_at_the_same_slug() {
    with_fake_home(|home, state| {
        install_and_push(state);
        // Simulate the user's own pre-existing skill occupying the slug:
        // strip our marker file and row so the dir is no longer provably ours.
        let native = home.join(".agents/skills").join(SLUG);
        std::fs::remove_file(native.join(".scenery-managed.json")).unwrap();
        Connection::open(&state.db_path)
            .unwrap()
            .execute("DELETE FROM managed_path_markers", [])
            .unwrap();

        uninstall_skill(state, SLUG).unwrap();

        assert!(
            native.join("SKILL.md").exists(),
            "unmarked user content must survive uninstall"
        );
    });
}
