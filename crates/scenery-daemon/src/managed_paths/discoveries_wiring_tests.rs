//! Discovery wiring: classification in the report, the runner-version header,
//! directive/policy parsing, the suspicious check, and a full tick against a
//! mock backend.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::ENV_MUTEX;
use mockito::{Matcher, Server};
use std::fs;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        Self {
            _tmp: tmp,
            home,
            state,
            prev,
            _lock: lock,
        }
    }
    fn agents(&self) -> PathBuf {
        self.home.join(".agents/skills")
    }
    fn skill(&self, root: &Path, slug: &str) -> PathBuf {
        let d = root.join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {slug}\n---\nbody\n"),
        )
        .unwrap();
        d
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
fn the_report_carries_the_local_verdict_in_detail_and_the_batch_is_unchanged_for_convert() {
    let env = Env::new();
    env.skill(&env.agents(), "plain");
    let git = env.skill(&env.agents(), "devrepo");
    fs::create_dir_all(git.join(".git")).unwrap();
    let leaky = env.skill(&env.agents(), "leaky");
    fs::write(leaky.join(".env"), "A=1").unwrap();

    let items = collect_discoveries(&[env.agents()], &env.state);
    let by = |slug: &str| items.iter().find(|i| i.slug == slug).unwrap();
    assert!(by("plain").detail.is_none(), "absent detail = convert");
    let d = by("devrepo").detail.as_ref().unwrap();
    assert_eq!(
        (d["treatment"].as_str(), d["tracked_reason"].as_str()),
        (Some("track"), Some("git_tracked"))
    );
    let h = by("leaky").detail.as_ref().unwrap();
    assert_eq!(
        (h["treatment"].as_str(), h["hold_reason"].as_str()),
        (Some("hold"), Some("secrets_detected"))
    );

    // `local` never reaches the wire.
    let wire = serde_json::to_value(by("plain")).unwrap();
    assert!(wire.get("local").is_none());
    assert!(by("plain").local.as_ref().unwrap().treatment.is_convert());
    assert_eq!(by("plain").local.as_ref().unwrap().tree_hash.len(), 64);
}

#[test]
fn an_edited_auto_managed_skill_is_not_reported_suspicious_but_an_edited_catalog_one_is() {
    use crate::managed_paths::ManagedPathsPusher;
    let env = Env::new();
    env.state
        .record_pending_auto_managed_takeover(
            "mine",
            &env.agents().join("mine").to_string_lossy(),
            "adam~mine",
            "u",
        )
        .unwrap();
    let pusher = ManagedPathsPusher::new(&env.state);
    pusher
        .push_skill("mine", None, b"---\nname: mine\n---\nv1", &[])
        .unwrap();
    pusher
        .push_skill("catalog", None, b"---\nname: catalog\n---\nv1", &[])
        .unwrap();

    // Both edited after Scenery wrote them.
    fs::write(
        env.agents().join("mine/SKILL.md"),
        "---\nname: mine\n---\nedited",
    )
    .unwrap();
    fs::write(
        env.agents().join("catalog/SKILL.md"),
        "---\nname: catalog\n---\nedited",
    )
    .unwrap();

    let items = collect_discoveries(&[env.agents()], &env.state);
    let slugs: Vec<&str> = items.iter().map(|i| i.slug.as_str()).collect();
    assert_eq!(
        slugs,
        vec!["catalog"],
        "own edit is a normal edit, not discovered"
    );
    let d = items[0].detail.as_ref().unwrap();
    assert_eq!(d["suspicious_reason"], "managed_hash_mismatch");
    assert_eq!(d["treatment"], "hold", "suspicious implies hold");
    assert_eq!(d["suspicious"], true);
    assert!(matches!(
        items[0].local.as_ref().unwrap().treatment,
        Treatment::Hold(_)
    ));
}

#[test]
fn two_noncanonical_sources_for_one_slug_are_both_reported_held() {
    let env = Env::new();
    let extra_a = env.home.join("a");
    let extra_b = env.home.join("b");
    env.skill(&extra_a, "twin");
    env.skill(&extra_b, "Twin");
    let items = collect_discoveries(&[extra_a, extra_b], &env.state);
    assert_eq!(items.len(), 2);
    for i in &items {
        assert_eq!(i.detail.as_ref().unwrap()["hold_reason"], "duplicate_slug");
    }
}

#[tokio::test]
async fn the_upload_sends_the_runner_version_and_parses_directives_and_policy() {
    let env = Env::new();
    env.skill(&env.agents(), "alpha");
    let items = collect_discoveries(&[env.agents()], &env.state);
    let mut server = Server::new_async().await;
    let m = server
        .mock("POST", "/portal/managed-paths/discoveries")
        .match_header("x-scenery-runner-version", env!("CARGO_PKG_VERSION"))
        .match_header("authorization", "Bearer tok")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "directives": [{"discovery_id": "d1", "source_path": "/x", "action": "manage", "reason": "ok"},
                               {"discovery_id": "d2", "source_path": "/y", "action": "track", "reason": "r", "tracked_reason": "git_tracked"}],
                "policy": {"auto_manage_skills": true, "auto_submit_mcp": false},
                "upserted": 1
            })
            .to_string(),
        )
        .create();
    let http = reqwest::Client::new();
    let resp = post_discoveries(&items, None, &server.url(), &http, "tok")
        .await
        .unwrap()
        .response;
    m.assert();
    let dirs = resp.directives.unwrap();
    assert_eq!(dirs.len(), 2);
    assert_eq!(dirs[1].tracked_reason.as_deref(), Some("git_tracked"));
    assert_eq!(
        resp.policy,
        Some(scenery_core::state::AutoManagePolicy {
            auto_manage_skills: true,
            auto_submit_mcp: false
        })
    );
}

#[tokio::test]
async fn an_old_backend_body_with_no_directives_parses_to_nothing_to_do() {
    let env = Env::new();
    env.skill(&env.agents(), "alpha");
    let items = collect_discoveries(&[env.agents()], &env.state);
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/portal/managed-paths/discoveries")
        .with_status(200)
        .with_body(r#"{"items": [], "upserted": 1}"#)
        .create();
    let resp = post_discoveries(&items, None, &server.url(), &reqwest::Client::new(), "tok")
        .await
        .unwrap()
        .response;
    assert!(resp.directives.is_none() && resp.policy.is_none());
}

/// A whole tick: scan, report, directive back, upload, convert. One token read.
#[tokio::test]
async fn run_once_scans_reports_and_converts_from_the_directive() {
    let env = Env::new();
    let dir = env.skill(&env.agents(), "e2e-plain");
    // Make the skill old enough to not be deferred.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    for p in [dir.join("SKILL.md"), dir.clone()] {
        let f = fs::File::open(&p).unwrap();
        f.set_modified(old).unwrap();
    }
    env.state.set_sync_state("device_id", "dev-1").unwrap();
    let mut server = Server::new_async().await;
    scenery_core::auth::save_tokens(&env.state, &server.url(), "tok", "refresh").unwrap();

    // Registry version already local (the reconciler's reuse path).
    let v = env.state.root_dir.join("skills/e2e-plain/versions/0.1.0");
    fs::create_dir_all(v.as_std_path()).unwrap();
    fs::write(
        v.join("SKILL.md").as_std_path(),
        "---\nname: e2e-plain\nmetadata:\n  scenery:\n    version: 0.1.0\n---\nCOMPILED\n",
    )
    .unwrap();

    let src = dir.to_string_lossy().to_string();
    let disc = server
        .mock("POST", "/portal/managed-paths/discoveries")
        .match_header("x-scenery-runner-version", Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "accepted": [{"id": "d1", "kind": "skill", "slug": "e2e-plain"}],
                "directives": [{"discovery_id": "d1", "source_path": src, "action": "manage", "reason": "ok"}],
                "policy": {"auto_manage_skills": true, "auto_submit_mcp": true}
            })
            .to_string(),
        )
        .create();
    let am = server
        .mock("POST", "/runner/skills/auto-manage")
        .match_header("authorization", "Bearer tok")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "outcome": "managed", "mode": "manage", "skill_id": "adam~e2e-plain",
                "skill_uuid": "u-9", "owner_namespace": "adam", "version": "0.1.0",
                "installation_id": "8b3f6b8a-6e1e-4f39-9b8a-000000000002",
                "installation_state": "desired", "idempotent": false,
                "scan": {"verdict": "ALLOW"}, "errors": [], "files": []
            })
            .to_string(),
        )
        .create();

    // `extra_roots()` is `~/.agents/skills` + `~/.pi/agent/skills`.
    let n = run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    assert_eq!(n, 1, "one item the backend saw for the first time");
    disc.assert();
    am.assert();
    let body = fs::read_to_string(dir.join("SKILL.md")).unwrap();
    assert!(
        body.contains("COMPILED"),
        "converted to the registry copy: {body}"
    );
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join(".scenery-managed.json")).unwrap()).unwrap();
    assert_eq!(marker["origin"], "auto_managed");
    assert_eq!(
        env.state
            .cached_auto_manage_policy()
            .map(|p| p.auto_manage_skills),
        Some(true)
    );
}

// ── Upload only on change ────────────────────────────────────────────────────

/// A paired env with one old (settled) skill and a token for `url`.
fn paired_with_skill(env: &Env, url: &str, slug: &str) -> PathBuf {
    let dir = env.skill(&env.agents(), slug);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    for p in [dir.join("SKILL.md"), dir.clone()] {
        fs::File::open(&p).unwrap().set_modified(old).unwrap();
    }
    env.state.set_sync_state("device_id", "dev-1").unwrap();
    scenery_core::auth::save_tokens(&env.state, url, "tok", "refresh").unwrap();
    dir
}

/// A discovery endpoint answering "skip" for every skill: nothing left to do.
fn skip_all(server: &mut mockito::ServerGuard, src: &str, hits: usize) -> mockito::Mock {
    server
        .mock("POST", "/portal/managed-paths/discoveries")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "accepted": [], "deduped": [{"id": "d1", "kind": "skill", "slug": "s"}],
                "directives": [{"discovery_id": "d1", "source_path": src, "action": "skip", "reason": "dev_skill"}],
                "policy": {"auto_manage_skills": true, "auto_submit_mcp": true}
            })
            .to_string(),
        )
        .expect(hits)
        .create()
}

#[tokio::test]
async fn an_unchanged_batch_is_uploaded_once_and_a_restart_does_not_resend_it() {
    let env = Env::new();
    let mut server = Server::new_async().await;
    let dir = paired_with_skill(&env, &server.url(), "quiet");
    let m = skip_all(&mut server, &dir.to_string_lossy(), 1);

    for _ in 0..3 {
        let n = run_once(Arc::clone(&env.state), server.url())
            .await
            .unwrap();
        assert_eq!(n, 0, "a re-sighting is not a new item");
    }
    // A daemon restart: a fresh state handle on the same state dir.
    let again = Arc::new(AppState::bootstrap_in(env.state.root_dir.clone()).unwrap());
    run_once(again, server.url()).await.unwrap();
    m.assert();
    assert!(load_upload_memo(&env.state).is_some());
}

#[tokio::test]
async fn an_edit_or_a_new_skill_is_uploaded_on_the_next_tick() {
    let env = Env::new();
    let mut server = Server::new_async().await;
    let dir = paired_with_skill(&env, &server.url(), "edited");
    let m = skip_all(&mut server, &dir.to_string_lossy(), 3);

    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    fs::write(dir.join("SKILL.md"), "---\nname: edited\n---\nnew body\n").unwrap();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    env.skill(&env.agents(), "brand-new");
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    m.assert();
}

#[tokio::test]
async fn a_failed_upload_is_retried_every_tick_until_it_succeeds() {
    let env = Env::new();
    let mut server = Server::new_async().await;
    let dir = paired_with_skill(&env, &server.url(), "retry");
    let down = server
        .mock("POST", "/portal/managed-paths/discoveries")
        .with_status(503)
        .expect(2)
        .create();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    down.assert();
    assert!(
        load_upload_memo(&env.state).is_none(),
        "a failure is never memoised"
    );
    down.remove();

    let up = skip_all(&mut server, &dir.to_string_lossy(), 1);
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    up.assert();
}

#[tokio::test]
async fn an_unchanged_batch_is_re_sent_once_the_refresh_interval_passed() {
    let env = Env::new();
    let mut server = Server::new_async().await;
    let dir = paired_with_skill(&env, &server.url(), "stale");
    let m = skip_all(&mut server, &dir.to_string_lossy(), 2);
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    let mut memo = load_upload_memo(&env.state).unwrap();
    memo.uploaded_at -= refresh_secs();
    env.state
        .set_sync_state(UPLOAD_MEMO_KEY, &serde_json::to_string(&memo).unwrap())
        .unwrap();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    m.assert();
}

#[tokio::test]
async fn work_left_for_auto_manage_keeps_the_batch_uploading() {
    let env = Env::new();
    let mut server = Server::new_async().await;
    let dir = paired_with_skill(&env, &server.url(), "pending");
    // "manage", but the auto-manage upload fails: the attempt backs off, the
    // tick is not settled, and the memo is not written.
    let disc = server
        .mock("POST", "/portal/managed-paths/discoveries")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "directives": [{"discovery_id": "d1", "source_path": dir.to_string_lossy(), "action": "manage"}],
                "policy": {"auto_manage_skills": true, "auto_submit_mcp": true}
            })
            .to_string(),
        )
        .expect(2)
        .create();
    let am = server
        .mock("POST", "/runner/skills/auto-manage")
        .with_status(500)
        .expect_at_least(1)
        .create();
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    assert!(load_upload_memo(&env.state).is_none());
    // Next tick re-sends (fresh directives); the failed attempt is backing
    // off, so it is skipped and that tick settles with a retry time.
    run_once(Arc::clone(&env.state), server.url())
        .await
        .unwrap();
    disc.assert();
    am.assert();
    let memo = load_upload_memo(&env.state).unwrap();
    let retry = memo.retry_at.expect("the backoff end is remembered");
    assert!(!upload_due(&env.state, &memo.fingerprint, retry - 1));
    assert!(upload_due(&env.state, &memo.fingerprint, retry));
}

#[test]
fn the_fingerprint_ignores_listing_order_but_not_content_or_identity() {
    let item = |slug: &str, hash: &str| DiscoveryItem {
        kind: "skill".into(),
        slug: slug.into(),
        source_path: format!("/h/.agents/skills/{slug}"),
        canonical_hash: hash.into(),
        detail: None,
        local: None,
    };
    let a = vec![item("a", "1"), item("b", "2")];
    let b = vec![item("b", "2"), item("a", "1")];
    let fp = |v: &[DiscoveryItem], dev: &str, user: &str| {
        batch_fingerprint(v, Some(dev), "https://r", Some(user))
    };
    assert_eq!(fp(&a, "d", "u"), fp(&b, "d", "u"));
    assert_ne!(
        fp(&a, "d", "u"),
        fp(&[item("a", "1"), item("b", "3")], "d", "u")
    );
    assert_ne!(fp(&a, "d", "u"), fp(&a, "d2", "u"));
    assert_ne!(fp(&a, "d", "u"), fp(&a, "d", "u2"));
}
