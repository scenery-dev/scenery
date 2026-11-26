#![allow(clippy::unwrap_used)]

use super::super::scan_tests::{args, block, item, Fixture};
use super::super::{report, run_to, ScanArgs};
use mockito::{Matcher, Server, ServerGuard};
use std::fs;

const TOKEN: &str = "scn_scan_SECRETVALUE123";
const AUTH: &str = "Bearer scn_scan_SECRETVALUE123";

fn session_body(auto: bool, max: usize) -> String {
    serde_json::json!({"org_name":"Acme","attribution":"token","user_email":null,
        "auto_submit_mcp":auto,"max_items":max})
    .to_string()
}

const RECEIPT: &str = r#"{"device_id":"d1","accepted":4,"deduped":1,"rejected":[],
    "mcp_submitted":1,"inventory_url":"https://x.test/inv"}"#;

fn mock_session(s: &mut ServerGuard, auto: bool, max: usize) -> mockito::Mock {
    s.mock("GET", "/api/scan/session")
        .match_header("authorization", AUTH)
        .match_header("x-scenery-runner-version", "2.8.0")
        .with_status(200)
        .with_body(session_body(auto, max))
        .expect(1)
        .create()
}

fn upload_args(server: &ServerGuard) -> ScanArgs {
    let mut a = args(false, false);
    a.registry_url = server.url();
    a
}

fn run_upload(fx: &Fixture, a: ScanArgs) -> (i32, String) {
    let mut env = fx.env(501);
    env.token = Some(TOKEN.to_string());
    let mut out = Vec::new();
    let code = block(run_to(a, &env, &mut out)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains(TOKEN), "token leaked into output");
    (code, text)
}

fn temp_files(fx: &Fixture) -> Vec<std::path::PathBuf> {
    fs::read_dir(&fx.tmpdir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

#[test]
fn token_upload_happy_path() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let sess = mock_session(&mut s, false, 5000);
    let post = s
        .mock("POST", "/api/scan/reports")
        .match_header("authorization", AUTH)
        .match_header("x-scenery-runner-version", "2.8.0")
        .with_status(200)
        .with_body(RECEIPT)
        .expect(1)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(
        "Uploaded to Acme: 4 new, 1 already known. IT reviews them at https://x.test/inv."
    ));
    assert!(temp_files(&fx).is_empty());
    sess.assert();
    post.assert();
}

/// Run an upload against a mock that records the posted body.
fn posted_body(auto: bool) -> String {
    let fx = Fixture::new();
    fs::write(
        fx.home.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"gh":{"command":"npx","args":["-y","@x/gh","--tok","ghp_abcdefghijklmnopqrstuvwxyz0123456789"]}}}"#,
    )
    .unwrap();
    let mut s = Server::new();
    let _sess = mock_session(&mut s, auto, 5000);
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();
    let post = s
        .mock("POST", "/api/scan/reports")
        .with_status(200)
        .with_body_from_request(move |req| {
            *sink.lock().unwrap() = req.body().unwrap().clone();
            RECEIPT.as_bytes().to_vec()
        })
        .expect(1)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 0, "{text}");
    post.assert();
    let body = captured.lock().unwrap().clone();
    String::from_utf8(body).unwrap()
}

#[test]
fn args_follow_session_policy() {
    let off = posted_body(false);
    assert!(
        !off.contains("\"args\""),
        "args sent while auto_submit_mcp is false"
    );
    assert!(!off.contains("ghp_"));
    let on = posted_body(true);
    let v: serde_json::Value = serde_json::from_str(&on).unwrap();
    let mcp = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "mcp")
        .unwrap();
    assert!(mcp["detail"]["args"].is_array());
    assert!(!on.contains("ghp_"), "unredacted secret in body");
}

#[test]
fn upload_failure_401_saves_report_without_retry() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _sess = mock_session(&mut s, false, 5000);
    let post = s
        .mock("POST", "/api/scan/reports")
        .with_status(401)
        .with_body(r#"{"detail":{"code":"scan_token_revoked"}}"#)
        .expect(1)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("The scan token was revoked by your IT admin."));
    assert!(text.contains("--from-file"));
    assert_saved_private(&fx);
    post.assert();
}

#[test]
fn upload_failure_500_retries_then_saves_report() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _sess = mock_session(&mut s, false, 5000);
    let post = s
        .mock("POST", "/api/scan/reports")
        .with_status(500)
        .expect(3)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("HTTP 500"));
    assert_saved_private(&fx);
    post.assert();
}

#[test]
fn upload_failure_connection_refused_saves_report() {
    let fx = Fixture::new();
    let mut a = args(false, false);
    a.registry_url = "http://127.0.0.1:1".to_string();
    let (code, text) = run_upload(&fx, a);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("Could not reach the server"));
    assert_saved_private(&fx);
}

fn assert_saved_private(fx: &Fixture) {
    let files = temp_files(fx);
    assert_eq!(files.len(), 1, "{files:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(v["format"], "scenery-scan-report/1");
}

#[test]
fn over_limit_saves_and_fails() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _sess = mock_session(&mut s, false, 1);
    let post = s.mock("POST", "/api/scan/reports").expect(0).create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("over this org's limit of 1"));
    assert_saved_private(&fx);
    post.assert();
}

#[test]
fn runner_present_409_exits_zero() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _sess = mock_session(&mut s, false, 5000);
    let post = s
        .mock("POST", "/api/scan/reports")
        .with_status(409)
        .with_body(r#"{"detail":{"code":"runner_present"}}"#)
        .expect(1)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("already installed"));
    assert!(temp_files(&fx).is_empty());
    post.assert();
}

#[test]
fn too_old_426_reads_top_level_code() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _sess = s
        .mock("GET", "/api/scan/session")
        .with_status(426)
        .with_body(r#"{"code":"runner_upgrade_required","min":"2.8.0"}"#)
        .create();
    let (code, text) = run_upload(&fx, upload_args(&s));
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("too old"));
}

#[test]
fn from_file_posts_saved_report() {
    let fx = Fixture::new();
    let items: Vec<_> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| item("skill", n))
        .collect();
    let rep = report::build(items, fx.env(501).now).unwrap();
    let path = fx.tmpdir.join("saved.json");
    report::save(&rep, Some(&path)).unwrap();
    let before = fs::read(&path).unwrap();
    // Empty HOME: a scan here would find nothing.
    let empty_home = fx.home.join("empty");
    fs::create_dir_all(&empty_home).unwrap();
    let mut s = Server::new();
    let sess = s.mock("GET", "/api/scan/session").expect(0).create();
    let post = s
        .mock("POST", "/api/scan/reports")
        .match_header("authorization", AUTH)
        .match_body(Matcher::PartialJson(serde_json::json!({
            "format": "scenery-scan-report/1"
        })))
        .match_body(Matcher::Regex(r#"(?s)"slug":\s*"d""#.to_string()))
        .with_status(200)
        .with_body(RECEIPT)
        .expect(1)
        .create();
    let mut a = upload_args(&s);
    a.from_file = Some(path.clone());
    let mut env = fx.env(501);
    env.home = empty_home;
    env.token = Some(TOKEN.to_string());
    let mut out = Vec::new();
    let code = block(run_to(a, &env, &mut out)).unwrap();
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    assert_eq!(fs::read(&path).unwrap(), before);
    sess.assert();
    post.assert();
}

#[test]
fn from_file_rejects_other_formats() {
    let fx = Fixture::new();
    let path = fx.tmpdir.join("x.json");
    fs::write(&path, r#"{"format":"other/9"}"#).unwrap();
    let mut a = args(false, false);
    a.from_file = Some(path);
    let (code, text) = run_upload(&fx, a);
    assert_eq!(code, 2, "{text}");
}

#[test]
fn no_auth_exits_two() {
    let fx = Fixture::new();
    let mut out = Vec::new();
    let code = block(run_to(args(false, false), &fx.env(501), &mut out)).unwrap();
    assert_eq!(code, 2);
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("SCENERY_SCAN_TOKEN"));
}
