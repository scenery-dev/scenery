use super::*;
use mockito::{Matcher, Server};

struct Env {
    home: tempfile::TempDir,
    state: AppState,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        TEST_HOME.with(|t| *t.borrow_mut() = Some(home.path().to_path_buf()));
        let root = camino::Utf8PathBuf::from_path_buf(home.path().join("state")).unwrap();
        let state = AppState::bootstrap_in(root).unwrap();
        Env { home, state }
    }

    fn skills(&self) -> PathBuf {
        self.home.path().join(".agents").join("skills")
    }

    fn login(&self, url: &str) {
        scenery_core::auth::save_tokens(&self.state, url, "access", "refresh").unwrap();
        self.state.set_sync_state("device_id", "dev-1").unwrap();
    }

    /// A registered own skill folder: SKILL.md + a marker row with a registry id.
    fn own_skill(&self, slug: &str, body: &str) -> PathBuf {
        let dir = self.skills().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            authoring::skill_md(slug, "Use when the user wants to test this skill out", body),
        )
        .unwrap();
        let conn = Connection::open(&self.state.db_path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO managed_path_markers \
             (path, kind, slug, installation_id, source_sha256, migrated_at, origin, skill_id, skill_uuid) \
             VALUES (?1, 'skill', ?2, NULL, 'x', 'now', 'auto_managed', ?3, ?4)",
            rusqlite::params![
                dir.to_string_lossy().to_string(),
                slug,
                format!("adam~{slug}"),
                "11111111-1111-4111-8111-111111111111"
            ],
        )
        .unwrap();
        dir
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        TEST_HOME.with(|t| *t.borrow_mut() = None);
    }
}

fn text(r: &ToolCallResult) -> String {
    r.content
        .iter()
        .map(|c| c.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn json(r: &ToolCallResult) -> serde_json::Value {
    serde_json::from_str(&text(r)).unwrap_or_else(|_| panic!("not json: {}", text(r)))
}

fn call(name: &str, args: serde_json::Value, env: &Env, url: &Option<String>) -> ToolCallResult {
    handle(name, &args, &env.state, url)
}

#[test]
fn the_three_tools_are_defined_with_required_arguments() {
    let defs = tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, TOOL_NAMES);
    for d in &defs {
        assert!(is_authoring_tool(&d.name));
        assert!(d.input_schema.get("properties").is_some());
    }
    assert!(!is_authoring_tool("scenery_author"));
}

// ── create ──

#[test]
fn create_writes_the_skill_folder() {
    let env = Env::new();
    let r = call(
        "scenery_create_skill",
        serde_json::json!({"name": "Meeting-Notes", "description": "Summarize \"meeting\" notes. Use after calls.", "instructions": "# Steps\n1. read"}),
        &env,
        &None,
    );
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    // The create hook is process-global (another test registers one), so a
    // process with no hook answers created_locally, one with it managed.
    assert!(["created_locally", "managed"].contains(&json(&r)["status"].as_str().unwrap()));
    let md = std::fs::read_to_string(env.skills().join("meeting-notes/SKILL.md")).unwrap();
    assert!(md.starts_with("---\nname: meeting-notes\n"), "{md}");
    assert!(authoring::check_tree(&env.skills().join("meeting-notes")).ok());
}

#[test]
fn create_refuses_bad_reserved_and_existing_names_and_secrets() {
    let env = Env::new();
    let args = |n: &str, ins: &str| serde_json::json!({"name": n, "description": "Use when testing the create tool", "instructions": ins});
    for bad in [
        "",
        "a/b",
        "..",
        ".x",
        "authoring-with-scenery",
        "AUTHORING-WITH-SCENERY",
        "a~b",
    ] {
        let r = call("scenery_create_skill", args(bad, "body"), &env, &None);
        assert_eq!(r.is_error, Some(true), "{bad}: {}", text(&r));
    }
    // missing arguments
    let r = call(
        "scenery_create_skill",
        serde_json::json!({"name": "x"}),
        &env,
        &None,
    );
    assert_eq!(r.is_error, Some(true));
    // an existing folder (the user's own) is never touched
    std::fs::create_dir_all(env.skills().join("mine")).unwrap();
    std::fs::write(env.skills().join("mine/SKILL.md"), "mine").unwrap();
    let r = call("scenery_create_skill", args("mine", "body"), &env, &None);
    assert_eq!(r.is_error, Some(true));
    assert_eq!(
        std::fs::read_to_string(env.skills().join("mine/SKILL.md")).unwrap(),
        "mine"
    );
    // a pasted private key: nothing is created
    let r = call(
        "scenery_create_skill",
        args("leaky", "-----BEGIN RSA PRIVATE KEY-----\nabc\n"),
        &env,
        &None,
    );
    assert_eq!(r.is_error, Some(true));
    assert!(!env.skills().join("leaky").exists());
    assert!(!text(&r).contains("abc"));
}

#[test]
fn create_registers_through_the_hook_and_reports_each_outcome() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static MODE: AtomicUsize = AtomicUsize::new(0);
    fn hook(_s: &AppState, _u: &str, slug: &str) -> Result<authoring::CreatedSkill, CreateError> {
        match MODE.load(Ordering::SeqCst) {
            0 => Ok(authoring::CreatedSkill {
                skill_id: format!("adam~{slug}"),
                skill_uuid: "u".into(),
                version: "0.1.0".into(),
                owner_namespace: Some("adam".into()),
                scan_verdict: Some("clean".into()),
            }),
            1 => Err(CreateError::Offline("registry down".into())),
            2 => Err(CreateError::Refused {
                code: "scan_rejected".into(),
                message: "no".into(),
            }),
            _ => Err(CreateError::Failed("boom".into())),
        }
    }
    authoring::register_create_hook(hook);
    let env = Env::new();
    let mk = |n: &str| serde_json::json!({"name": n, "description": "Use when testing the hook path", "instructions": "body"});
    for (mode, name, status) in [
        (0, "one", "managed"),
        (1, "two", "created_locally"),
        (2, "three", "created_not_registered"),
        (3, "four", "created_not_registered"),
    ] {
        MODE.store(mode, Ordering::SeqCst);
        let r = call("scenery_create_skill", mk(name), &env, &None);
        assert_ne!(r.is_error, Some(true), "{}", text(&r));
        assert_eq!(json(&r)["status"], status, "{}", text(&r));
        // the folder is always kept
        assert!(env.skills().join(name).join("SKILL.md").exists());
    }
    MODE.store(0, Ordering::SeqCst);
    let r = call("scenery_create_skill", mk("five"), &env, &None);
    assert_eq!(json(&r)["skill_id"], "adam~five");
}

// ── check ──

#[test]
fn check_reports_layout_problems_and_secret_file_names_only() {
    let env = Env::new();
    let dir = env.own_skill("notes", "body");
    std::fs::write(dir.join(".env"), "TOKEN=hunter2").unwrap();
    let r = call(
        "scenery_check_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &None,
    );
    let v = json(&r);
    assert_eq!(v["status"], "needs_fixes");
    assert_eq!(v["layout"]["secret_files"][0], ".env");
    assert!(!text(&r).contains("hunter2"));

    std::fs::remove_file(dir.join(".env")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "no frontmatter").unwrap();
    let v = json(&call(
        "scenery_check_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &None,
    ));
    assert_eq!(v["status"], "needs_fixes");
    assert!(v["layout"]["problems"].to_string().contains("frontmatter"));

    let missing = call(
        "scenery_check_skill",
        serde_json::json!({"name": "nope"}),
        &env,
        &None,
    );
    assert_eq!(missing.is_error, Some(true));
}

#[test]
fn check_offline_reports_the_layout_and_that_the_scan_needs_sign_in() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let v = json(&call(
        "scenery_check_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &Some("http://127.0.0.1:1".into()),
    ));
    assert_eq!(v["status"], "layout_ok");
    assert_eq!(v["scan"]["available"], false);
}

#[test]
fn check_previews_the_scan_and_stores_nothing() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let m = server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(200)
        .with_body(r#"{"verdict":"clean","decision":"accept","requires_review":false,"findings":[],"secrets":[],"name_clash":false,"suggestions":[]}"#)
        .expect(1)
        .create();
    let v = json(&call(
        "scenery_check_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &Some(server.url()),
    ));
    m.assert();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["scan"]["verdict"], "clean");
}

#[test]
fn the_builtin_skill_is_never_checked_or_shared() {
    let env = Env::new();
    for n in ["scenery_check_skill", "scenery_share_skill"] {
        let r = call(
            n,
            serde_json::json!({"name": "authoring-with-scenery"}),
            &env,
            &None,
        );
        assert_eq!(r.is_error, Some(true), "{n}");
    }
    // ... also when addressed by a path and recorded as builtin
    let dir = env.skills().join("authoring-with-scenery");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "x").unwrap();
    let conn = Connection::open(&env.state.db_path).unwrap();
    conn.execute(
        "INSERT INTO managed_path_markers (path, kind, slug, source_sha256, migrated_at, origin) \
         VALUES (?1, 'skill', 'authoring-with-scenery', 'x', 'now', 'builtin')",
        [dir.to_string_lossy().to_string()],
    )
    .unwrap();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"path": dir.to_string_lossy()}),
        &env,
        &None,
    );
    assert_eq!(r.is_error, Some(true));
}

// ── share ──

fn ok_preview(server: &mut Server) -> mockito::Mock {
    server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(200)
        .with_body(r#"{"verdict":"clean","decision":"accept","requires_review":false,"findings":[],"secrets":[],"name_clash":false,"suggestions":[]}"#)
        .create()
}

fn ok_upload(server: &mut Server) -> mockito::Mock {
    server
        .mock("POST", "/runner/skills/register")
        .with_status(200)
        .with_body(r#"{"outcome":"managed","mode":"track","skill_id":"adam~notes","skill_uuid":"11111111-1111-4111-8111-111111111111","version":"0.1.0+abc","owner_namespace":"adam"}"#)
        .create()
}

fn publish_path() -> &'static str {
    "/portal/custom-tools/skill/11111111-1111-4111-8111-111111111111/publish"
}

fn step_one(env: &Env, url: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut args = serde_json::json!({"name": "notes"});
    if let (Some(a), Some(e)) = (args.as_object_mut(), extra.as_object()) {
        a.extend(e.clone());
    }
    json(&call(
        "scenery_share_skill",
        args,
        env,
        &Some(url.to_string()),
    ))
}

#[test]
fn share_is_two_steps_and_the_token_is_single_use() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let publish = server
        .mock("POST", publish_path())
        .match_body(Matcher::PartialJson(serde_json::json!({"catalog_name": "notes", "version": "0.1.0+abc", "force_review": true})))
        .with_status(200)
        .with_body(r#"{"kind":"skill","id":"x","adoption_status":"pending_review","visibility":"private","shared_skill_id":"notes"}"#)
        .expect(1)
        .create();
    let url = Some(server.url());

    let v = step_one(&env, &server.url(), serde_json::json!({}));
    assert_eq!(v["status"], "awaiting_confirmation", "{v}");
    assert_eq!(v["preview"]["catalog_name"], "notes");
    // An AI-client share always goes to IT review, whatever the scan gate says.
    assert_eq!(v["preview"]["it_review_required"], true);
    let tok = v["confirm_token"].as_str().unwrap().to_string();
    // nothing was published by step one
    assert!(!publish.matched());

    let done = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &url,
    );
    assert_ne!(done.is_error, Some(true), "{}", text(&done));
    assert_eq!(json(&done)["status"], "pending_review");
    publish.assert();

    // reuse is refused, and so is a made-up token
    for t in [tok.as_str(), "shr_deadbeef"] {
        let r = call(
            "scenery_share_skill",
            serde_json::json!({"name": "notes", "confirm_token": t}),
            &env,
            &url,
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(json(&r)["status"], "confirmation_refused");
    }
}

#[test]
fn a_changed_tree_or_a_different_name_refuses_the_token_and_spends_it() {
    let env = Env::new();
    let dir = env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let publish = server
        .mock("POST", publish_path())
        .with_status(200)
        .with_body("{}")
        .expect(0)
        .create();
    let url = Some(server.url());

    // tree changed after the preview
    let tok = step_one(&env, &server.url(), serde_json::json!({}))["confirm_token"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(dir.join("extra.md"), "added after the preview").unwrap();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &url,
    );
    assert_eq!(json(&r)["status"], "confirmation_refused");
    assert!(json(&r)["reason"].as_str().unwrap().contains("changed"));
    // it was spent
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &url,
    );
    assert!(json(&r)["reason"]
        .as_str()
        .unwrap()
        .contains("already used"));
    std::fs::remove_file(dir.join("extra.md")).unwrap();

    // a different catalog name than previewed
    let tok = step_one(&env, &server.url(), serde_json::json!({}))["confirm_token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "catalog_name": "other-name", "confirm_token": tok}),
        &env,
        &url,
    );
    assert_eq!(json(&r)["status"], "confirmation_refused");
    publish.assert();
}

#[test]
fn a_token_cannot_be_used_for_another_skill() {
    let env = Env::new();
    env.own_skill("notes", "body");
    env.own_skill("other", "body2");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let url = Some(server.url());
    let tok = step_one(&env, &server.url(), serde_json::json!({}))["confirm_token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "other", "confirm_token": tok}),
        &env,
        &url,
    );
    assert_eq!(json(&r)["status"], "confirmation_refused");
}

#[test]
fn name_clash_is_reported_with_suggestions_and_mints_no_token() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(200)
        .with_body(r#"{"verdict":"clean","decision":"accept","requires_review":false,"findings":[],"secrets":[],"name_clash":true,"suggestions":["notes-2"]}"#)
        .create();
    let v = step_one(&env, &server.url(), serde_json::json!({}));
    assert_eq!(v["status"], "name_clash");
    assert_eq!(v["suggestions"][0], "notes-2");
    assert!(v.get("confirm_token").is_none());
}

#[test]
fn a_branch_of_a_catalog_skill_gets_the_coming_soon_block() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let _pub = server
        .mock("POST", publish_path())
        .with_status(409)
        .with_body(r#"{"detail":{"code":"name_clash_branch","detail":"A catalog skill named notes already exists; submitting changes to it is coming soon."}}"#)
        .create();
    let url = Some(server.url());
    let tok = step_one(&env, &server.url(), serde_json::json!({}))["confirm_token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &url,
    );
    assert_eq!(r.is_error, Some(true));
    assert_eq!(json(&r)["reason"], "name_clash_branch");
    assert!(json(&r)["message"]
        .as_str()
        .unwrap()
        .contains("coming soon"));
}

#[test]
fn a_publish_time_name_clash_asks_for_another_name() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let _pub = server
        .mock("POST", publish_path())
        .with_status(409)
        .with_body(r#"{"detail":{"code":"name_clash","detail":"taken","suggestions":["notes-2"]}}"#)
        .create();
    let tok = step_one(&env, &server.url(), serde_json::json!({}))["confirm_token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &Some(server.url()),
    );
    assert_eq!(json(&r)["status"], "name_clash");
}

#[test]
fn it_review_is_surfaced_in_the_preview_and_the_result() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(200)
        .with_body(r#"{"verdict":"clean","decision":"queue-for-review","requires_review":true,"findings":[],"secrets":[],"name_clash":false,"suggestions":[]}"#)
        .create();
    let _u = ok_upload(&mut server);
    let _pub = server
        .mock("POST", publish_path())
        .with_status(200)
        .with_body(r#"{"kind":"skill","id":"x","adoption_status":"pending_review","visibility":"private","shared_skill_id":"notes"}"#)
        .create();
    let v = step_one(&env, &server.url(), serde_json::json!({}));
    assert_eq!(v["preview"]["it_review_required"], true);
    let tok = v["confirm_token"].as_str().unwrap().to_string();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes", "confirm_token": tok}),
        &env,
        &Some(server.url()),
    );
    assert_eq!(json(&r)["status"], "pending_review");
}

#[test]
fn share_needs_secrets_clean_a_registered_skill_a_login_and_a_passing_scan() {
    let env = Env::new();
    let dir = env.own_skill("notes", "body");
    let mut server = Server::new();
    let url = Some(server.url());
    // not signed in
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &url,
    );
    assert_eq!(r.is_error, Some(true));
    env.login(&server.url());
    // a folder with no registry id
    std::fs::create_dir_all(env.skills().join("unregistered")).unwrap();
    std::fs::write(
        env.skills().join("unregistered/SKILL.md"),
        authoring::skill_md(
            "unregistered",
            "Use when testing the unregistered path",
            "b",
        ),
    )
    .unwrap();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "unregistered"}),
        &env,
        &url,
    );
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("not registered"));
    // a secret file blocks before anything is uploaded
    std::fs::write(dir.join(".env"), "K=v").unwrap();
    let upload = server
        .mock("POST", "/runner/skills/register")
        .with_status(200)
        .with_body("{}")
        .expect(0)
        .create();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &url,
    );
    assert_eq!(json(&r)["status"], "needs_fixes");
    std::fs::remove_file(dir.join(".env")).unwrap();
    // a rejecting scan blocks before the upload too
    let _p = server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(200)
        .with_body(r#"{"verdict":"critical","decision":"reject","requires_review":false,"findings":[{"rule_id":"R"}],"secrets":[],"name_clash":false,"suggestions":[]}"#)
        .create();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &url,
    );
    assert_eq!(json(&r)["reason"], "scan_rejected");
    upload.assert();
}

#[test]
fn an_old_runner_refusal_says_to_upgrade() {
    let env = Env::new();
    env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = server
        .mock("POST", "/runner/skills/preview-scan")
        .with_status(426)
        .with_body(r#"{"code":"runner_upgrade_required","min":"2.4.0"}"#)
        .create();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"name": "notes"}),
        &env,
        &Some(server.url()),
    );
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("brew upgrade"));
    assert_eq!(json(&r)["reason"], "runner_upgrade_required");
}

#[test]
fn a_successful_upload_moves_the_recorded_tree_hash_forward() {
    let env = Env::new();
    let dir = env.own_skill("notes", "body");
    let mut server = Server::new();
    env.login(&server.url());
    let _p = ok_preview(&mut server);
    let _u = ok_upload(&mut server);
    let v = step_one(&env, &server.url(), serde_json::json!({}));
    assert_eq!(v["status"], "awaiting_confirmation");
    let conn = Connection::open(&env.state.db_path).unwrap();
    let hash: Option<String> = conn
        .query_row(
            "SELECT tree_hash FROM managed_path_markers WHERE path = ?1",
            [dir.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash.unwrap(), authoring::check_tree(&dir).tree_hash);
}

#[test]
fn path_must_be_a_skill_folder_under_the_skill_roots() {
    let env = Env::new();
    env.own_skill("notes", "body");
    // inside the root: fine
    let inside = env.skills().join("notes");
    let r = call(
        "scenery_check_skill",
        serde_json::json!({"path": inside.to_string_lossy()}),
        &env,
        &None,
    );
    assert_ne!(r.is_error, Some(true), "{}", text(&r));

    // an arbitrary folder, the root itself, and a `..` escape are refused
    let outside = env.home.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "x").unwrap();
    let dotdot = inside.join("..").join("..").join("..").join("elsewhere");
    for p in [outside.clone(), env.skills(), dotdot] {
        for tool in ["scenery_check_skill", "scenery_share_skill"] {
            let r = call(
                tool,
                serde_json::json!({"path": p.to_string_lossy()}),
                &env,
                &None,
            );
            assert_eq!(r.is_error, Some(true), "{tool} {}", p.display());
            assert!(text(&r).contains("not a skill folder"), "{}", text(&r));
        }
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_the_root_pointing_outside_is_refused() {
    let env = Env::new();
    let outside = env.home.path().join("secrets-dir");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "x").unwrap();
    std::fs::create_dir_all(env.skills()).unwrap();
    let link = env.skills().join("sneaky");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let r = call(
        "scenery_share_skill",
        serde_json::json!({"path": link.to_string_lossy()}),
        &env,
        &None,
    );
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("not a skill folder"), "{}", text(&r));
}
