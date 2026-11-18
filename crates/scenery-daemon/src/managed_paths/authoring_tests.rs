//! `scenery_create_skill`'s registry-and-convert step against a mock backend and
//! a fake `$HOME`. As in `auto_manage_tests`, the registry version is seeded on
//! disk (`skills/<slug>/versions/<v>`), so no signed download is needed while
//! the conversion, marker and links are the production code.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::ENV_MUTEX;
use mockito::{Server, ServerGuard};
use std::{fs, path::PathBuf};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    server: ServerGuard,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    async fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        let server = Server::new_async().await;
        Self {
            _tmp: tmp,
            home,
            state,
            server,
            prev_home,
            _lock: lock,
        }
    }

    fn login(&self) {
        scenery_core::auth::save_tokens(&self.state, &self.server.url(), "tok", "refresh").unwrap();
        self.state.set_sync_state("device_id", "dev-1").unwrap();
    }

    /// What the MCP tool leaves behind: a fresh, unmarked folder.
    fn scaffold(&self, slug: &str) -> PathBuf {
        let d = self.home.join(".agents/skills").join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {slug}\ndescription: \"Use when testing\"\n---\n\nORIGINAL body\n"),
        )
        .unwrap();
        d
    }

    fn seed_registry_version(&self, slug: &str, version: &str) {
        let v = self
            .state
            .root_dir
            .join("skills")
            .join(slug)
            .join("versions")
            .join(version);
        fs::create_dir_all(v.as_std_path()).unwrap();
        fs::write(
            v.join("SKILL.md").as_std_path(),
            format!("---\nname: {slug}\ndescription: \"Use when testing\"\nmetadata:\n  scenery:\n    version: {version}\n---\n\nCOMPILED body\n"),
        )
        .unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn managed_body(slug: &str) -> String {
    serde_json::json!({
        "outcome": "managed", "mode": "manage", "reason": null,
        "skill_id": format!("adam~{slug}"),
        "skill_uuid": "11111111-2222-3333-4444-555555555555",
        "owner_namespace": "adam", "version": "0.1.0",
        "installation_id": "8b3f6b8a-6e1e-4f39-9b8a-000000000001",
        "installation_state": "desired", "idempotent": false,
        "scan": {"verdict": "clean", "decision": "accept", "findings": []},
        "errors": [], "files": []
    })
    .to_string()
}

#[tokio::test]
async fn registers_and_converts_the_new_folder_in_place() {
    let mut env = Env::new().await;
    env.login();
    let dir = env.scaffold("notes");
    env.seed_registry_version("notes", "0.1.0");
    let m = env
        .server
        .mock("POST", "/runner/skills/register")
        .match_header("authorization", "Bearer tok")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(managed_body("notes"))
        .expect(1)
        .create();

    let created = register_and_convert(Arc::clone(&env.state), env.server.url(), "notes".into())
        .await
        .unwrap();
    m.assert();
    assert_eq!(created.skill_id, "adam~notes");
    assert_eq!(created.version, "0.1.0");
    assert_eq!(created.owner_namespace.as_deref(), Some("adam"));

    // Managed from the first save: marker file + DB row, origin auto_managed, ids.
    let mk: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join(".scenery-managed.json")).unwrap()).unwrap();
    assert_eq!(mk["origin"], "auto_managed");
    assert_eq!(mk["skill_id"], "adam~notes");
    assert_eq!(mk["skill_uuid"], "11111111-2222-3333-4444-555555555555");
    let conn = rusqlite::Connection::open(&env.state.db_path).unwrap();
    let prov = super::super::marker::marker_provenance(&conn, &dir.to_string_lossy());
    assert!(prov.is_auto_managed());
    assert_eq!(prov.skill_id.as_deref(), Some("adam~notes"));
    // The registry bytes are what is on disk, and Claude Code can see it.
    assert!(fs::read_to_string(dir.join("SKILL.md"))
        .unwrap()
        .contains("COMPILED body"));
    assert!(env.home.join(".claude/skills/notes").exists());
    // The local folder name maps to the registry id (events and revocations).
    assert_eq!(env.state.skill_local_name("adam~notes"), "notes");

    // Not rediscovered: the next scan skips it.
    let found = super::super::discoveries::collect_discoveries(
        &[env.home.join(".agents/skills")],
        &env.state,
    );
    assert!(found.iter().all(|i| i.slug != "notes"), "{found:?}");
}

#[tokio::test]
async fn nothing_changes_on_disk_unless_the_registry_accepts() {
    let mut env = Env::new().await;
    env.login();
    let dir = env.scaffold("notes");
    let before = fs::read(dir.join("SKILL.md")).unwrap();

    // 426: an old backend/runner pairing
    let _m = env
        .server
        .mock("POST", "/runner/skills/register")
        .with_status(426)
        .with_body(r#"{"code":"runner_upgrade_required","min":"2.4.0"}"#)
        .create();
    let err = register_and_convert(Arc::clone(&env.state), env.server.url(), "notes".into())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CreateError::Refused { ref code, .. } if code == "runner_upgrade_required"),
        "{err:?}"
    );
    assert_eq!(fs::read(dir.join("SKILL.md")).unwrap(), before);
    assert!(!dir.join(".scenery-managed.json").exists());

    // blocked by the registry (reserved / secrets / scan)
    env.server.reset();
    let _b = env
        .server
        .mock("POST", "/runner/skills/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"outcome":"blocked","mode":"manage","reason":"scan_rejected"}"#)
        .create();
    let err = register_and_convert(Arc::clone(&env.state), env.server.url(), "notes".into())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CreateError::Refused { ref code, .. } if code == "scan_rejected"),
        "{err:?}"
    );
    assert_eq!(fs::read(dir.join("SKILL.md")).unwrap(), before);
}

#[tokio::test]
async fn signed_out_or_unpaired_or_unreachable_is_offline_and_touches_nothing() {
    let env = Env::new().await;
    let dir = env.scaffold("notes");
    let before = fs::read(dir.join("SKILL.md")).unwrap();
    // signed out
    let err = register_and_convert(Arc::clone(&env.state), env.server.url(), "notes".into())
        .await
        .unwrap_err();
    assert!(matches!(err, CreateError::Offline(_)), "{err:?}");
    // signed in but unreachable registry
    scenery_core::auth::save_tokens(&env.state, "http://127.0.0.1:1", "tok", "r").unwrap();
    env.state.set_sync_state("device_id", "dev-1").unwrap();
    let err = register_and_convert(
        Arc::clone(&env.state),
        "http://127.0.0.1:1".into(),
        "notes".into(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CreateError::Offline(_)), "{err:?}");
    assert_eq!(fs::read(dir.join("SKILL.md")).unwrap(), before);
    assert!(!dir.join(".scenery-managed.json").exists());
}
