#![allow(clippy::unwrap_used)]

use super::super::scan_tests::{args, block, Fixture};
use super::super::{run_to, ScanArgs, ScanEnv};
use super::{sign_in, TIMED_OUT};
use mockito::{Matcher, Server, ServerGuard};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, SystemTime};

const ACCESS: &str = "AT_SECRET_value_123";
const REFRESH: &str = "RT_SECRET_value_456";
const RECEIPT: &str = r#"{"device_id":"d1","accepted":4,"deduped":0,"rejected":[],
    "mcp_submitted":0,"inventory_url":"https://x.test/inv"}"#;

/// The callback port the last sign-in used (read from its redirect URI).
static PORT: AtomicU16 = AtomicU16::new(0);

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn query(url: &str, key: &str) -> String {
    let q = url.split_once('?').unwrap().1;
    let raw = q
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap();
    percent_decode(raw)
}

/// Record the callback port from the authorize URL; return its host:port.
fn record_port(url: &str) -> String {
    let redirect = query(url, "redirect_uri");
    let rest = redirect.strip_prefix("http://127.0.0.1:").unwrap();
    let port: u16 = rest.split_once('/').unwrap().0.parse().unwrap();
    assert!((39127..=39136).contains(&port), "{port}");
    assert_eq!(
        redirect,
        format!("http://127.0.0.1:{port}/oauth/cli/callback")
    );
    PORT.store(port, Ordering::SeqCst);
    format!("127.0.0.1:{port}")
}

/// A "browser" that completes sign-in: GETs the callback with the URL's state.
fn browser_completes(url: &str) -> bool {
    let host = record_port(url);
    let state = query(url, "state");
    let mut s = std::net::TcpStream::connect(&host).unwrap();
    write!(
        s,
        "GET /oauth/cli/callback?code=CODE123&state={state} HTTP/1.1\r\n\
         Host: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    true
}

/// A browser that never opens, so the user never signs in.
fn browser_declines(url: &str) -> bool {
    record_port(url);
    false
}

/// The listener's port is free again.
fn assert_port_released() {
    let port = PORT.load(Ordering::SeqCst);
    assert_ne!(port, 0);
    std::net::TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("port {port} still bound: {e}"));
}

fn mock_token(s: &mut ServerGuard) -> mockito::Mock {
    s.mock("POST", "/portal/auth/cli/token")
        .match_body(Matcher::Regex("code=CODE123".to_string()))
        .with_status(200)
        .with_body(
            serde_json::json!({"access_token": ACCESS, "refresh_token": REFRESH}).to_string(),
        )
        .expect(1)
        .create()
}

fn mock_revoke(s: &mut ServerGuard, n: usize) -> mockito::Mock {
    s.mock("POST", "/portal/auth/revoke")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"refresh_token": REFRESH, "scope": "family"}),
        ))
        .with_status(200)
        .with_body(r#"{"revoked":true,"families_revoked":1,"device_id":null}"#)
        .expect(n)
        .create()
}

fn mock_session(s: &mut ServerGuard) -> mockito::Mock {
    s.mock("GET", "/api/scan/session")
        .match_header("authorization", format!("Bearer {ACCESS}").as_str())
        .with_status(200)
        .with_body(
            serde_json::json!({"org_name":"Acme","attribution":"user",
                "user_email":"ann@acme.test","auto_submit_mcp":false,"max_items":5000})
            .to_string(),
        )
        .expect(1)
        .create()
}

fn mock_reports(s: &mut ServerGuard, status: usize, n: usize) -> mockito::Mock {
    s.mock("POST", "/api/scan/reports")
        .match_header("authorization", format!("Bearer {ACCESS}").as_str())
        .with_status(status)
        .with_body(RECEIPT)
        .expect(n)
        .create()
}

fn login_args(s: &ServerGuard) -> ScanArgs {
    let mut a = args(false, false);
    a.login = true;
    a.registry_url = s.url();
    a
}

/// Run the command; the port check runs before the runtime (and with it any
/// leaked listener task) is dropped.
fn run_login(env: &ScanEnv, a: ScanArgs) -> (i32, String) {
    let mut out = Vec::new();
    let code = block(async {
        let code = run_to(a, env, &mut out).await;
        assert_port_released();
        code
    })
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains(ACCESS) && !text.contains(REFRESH),
        "token in output"
    );
    (code, text)
}

#[test]
fn login_sign_in_returns_tokens_and_frees_port() {
    let _fx = Fixture::new();
    let mut s = Server::new();
    let token = mock_token(&mut s);
    let session = block(async {
        let r = sign_in(&s.url(), browser_completes, Duration::from_secs(10)).await;
        assert_port_released();
        r
    })
    .unwrap();
    assert_eq!(session.access_token, ACCESS);
    assert_eq!(session.refresh_token, REFRESH);
    assert_eq!(format!("{session:?}"), "LoginSession(..)");
    assert_port_released();
    token.assert();
}

#[test]
fn login_happy_path_revokes_session() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let token = mock_token(&mut s);
    let sess = mock_session(&mut s);
    let post = mock_reports(&mut s, 200, 1);
    let revoke = mock_revoke(&mut s, 1);
    let mut env = fx.env(501);
    env.open_browser = browser_completes;
    let (code, text) = run_login(&env, login_args(&s));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Signed in as ann@acme.test."), "{text}");
    assert!(text.contains("Uploaded to Acme"), "{text}");
    token.assert();
    sess.assert();
    post.assert();
    revoke.assert();
    assert_port_released();
}

#[test]
fn login_revokes_even_when_upload_fails() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let _token = mock_token(&mut s);
    let _sess = mock_session(&mut s);
    let post = mock_reports(&mut s, 500, 3);
    let revoke = mock_revoke(&mut s, 1);
    let mut env = fx.env(501);
    env.open_browser = browser_completes;
    let (code, text) = run_login(&env, login_args(&s));
    assert_eq!(code, 3, "{text}");
    post.assert();
    revoke.assert();
}

#[test]
fn login_times_out() {
    let fx = Fixture::new();
    let mut s = Server::new();
    let token = s.mock("POST", "/portal/auth/cli/token").expect(0).create();
    let revoke = s.mock("POST", "/portal/auth/revoke").expect(0).create();
    let err = block(async {
        let r = sign_in(&s.url(), browser_declines, Duration::from_secs(1)).await;
        assert_port_released();
        r
    })
    .unwrap_err();
    assert_eq!(format!("{err:#}"), TIMED_OUT);
    assert_port_released();
    // Through the command: exit 2, the message, and no session to revoke.
    let mut env = fx.env(501);
    env.open_browser = browser_declines;
    env.login_timeout = Duration::from_secs(1);
    let (code, text) = run_login(&env, login_args(&s));
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("Sign-in timed out."), "{text}");
    assert_port_released();
    token.assert();
    revoke.assert();
}

type Snapshot = BTreeMap<PathBuf, (u64, Option<SystemTime>)>;

fn snapshot(dir: &Path, out: &mut Snapshot) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let m = std::fs::symlink_metadata(&p).unwrap();
        out.insert(p.clone(), (m.len(), m.modified().ok()));
        if m.is_dir() {
            snapshot(&p, out);
        }
    }
}

/// With the keychain disabled, any token save would land in the secret file
/// under the state dir in this HOME, so an unchanged HOME proves none happened.
#[test]
fn login_writes_no_tokens() {
    let fx = Fixture::new();
    let prev = std::env::var_os("SCENERY_DISABLE_KEYCHAIN");
    std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
    let mut s = Server::new();
    let _token = mock_token(&mut s);
    let _sess = mock_session(&mut s);
    let _post = mock_reports(&mut s, 200, 1);
    let revoke = mock_revoke(&mut s, 1);
    let root = fx.home.parent().unwrap().to_path_buf();
    let mut before = Snapshot::new();
    snapshot(&root, &mut before);
    let mut env = fx.env(501);
    env.open_browser = browser_completes;
    let (code, text) = run_login(&env, login_args(&s));
    let mut after = Snapshot::new();
    snapshot(&root, &mut after);
    match prev {
        Some(v) => std::env::set_var("SCENERY_DISABLE_KEYCHAIN", v),
        None => std::env::remove_var("SCENERY_DISABLE_KEYCHAIN"),
    }
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        before, after,
        "the login scan wrote to HOME or the save dir"
    );
    let state = scenery_core::state::AppState::resolve_root_dir().unwrap();
    assert!(state.as_std_path().starts_with(&fx.home), "{state}");
    assert!(!state.exists(), "{state} was created");
    revoke.assert();
}
