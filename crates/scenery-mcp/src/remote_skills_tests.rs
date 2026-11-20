#![allow(clippy::unwrap_used)]
use super::*;
use scenery_core::state::AppState;

fn state(label: &str) -> (AppState, camino::Utf8PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = camino::Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("remote-skills-{label}-{nanos}")),
    )
    .unwrap();
    (AppState::bootstrap_in(root.clone()).unwrap(), root)
}

const OK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#;

#[test]
fn expired_token_refreshes_once_and_retries() {
    let (st, root) = state("retry");
    let mut server = mockito::Server::new();
    let url = server.url();
    auth::save_tokens(&st, &url, "old", "r1").unwrap();
    let _stale = server
        .mock("POST", "/mcp")
        .match_header("authorization", "Bearer old")
        .with_status(401)
        .create();
    let _refresh = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"access_token":"new","refresh_token":"r2","token_type":"bearer","expires_in":3600}"#)
        .create();
    let fresh = server
        .mock("POST", "/mcp")
        .match_header("authorization", "Bearer new")
        .with_status(200)
        .with_body(OK)
        .create();
    let r = handle(
        "search_skills",
        &serde_json::json!({}),
        &st,
        &Some(url.clone()),
    );
    fresh.assert();
    assert_eq!(r.content[0].text, "ok");
    assert_eq!(
        auth::load_tokens(&st, &url).unwrap().unwrap().access_token,
        "new"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn refresh_5xx_keeps_the_session() {
    let (st, root) = state("5xx");
    let mut server = mockito::Server::new();
    let url = server.url();
    auth::save_tokens(&st, &url, "old", "r1").unwrap();
    let _stale = server.mock("POST", "/mcp").with_status(401).create();
    let _refresh = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(503)
        .create();
    let r = handle(
        "search_skills",
        &serde_json::json!({}),
        &st,
        &Some(url.clone()),
    );
    assert_eq!(r.is_error, Some(true));
    assert!(
        auth::load_tokens(&st, &url).unwrap().is_some(),
        "tokens must stay"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn refresh_401_clears_the_session() {
    let (st, root) = state("401");
    let mut server = mockito::Server::new();
    let url = server.url();
    auth::save_tokens(&st, &url, "old", "r1").unwrap();
    let _stale = server.mock("POST", "/mcp").with_status(401).create();
    let _refresh = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(401)
        .create();
    let r = handle(
        "search_skills",
        &serde_json::json!({}),
        &st,
        &Some(url.clone()),
    );
    assert!(r.content[0].text.contains("scenery auth login"));
    assert!(auth::load_tokens(&st, &url).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn user_revoked_403_clears_the_session() {
    let (st, root) = state("revoked");
    let mut server = mockito::Server::new();
    let url = server.url();
    auth::save_tokens(&st, &url, "old", "r1").unwrap();
    let _m = server
        .mock("POST", "/mcp")
        .with_status(403)
        .with_body(r#"{"detail":{"code":"user_revoked"}}"#)
        .create();
    let r = handle(
        "load_skill",
        &serde_json::json!({"skill_id": "x"}),
        &st,
        &Some(url.clone()),
    );
    assert_eq!(r.is_error, Some(true));
    assert!(r.content[0].text.contains("removed or revoked"));
    assert!(auth::load_tokens(&st, &url).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&root);
}
