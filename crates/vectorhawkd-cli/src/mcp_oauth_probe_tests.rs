//! Tests for the `mcp setup` gate that only writes Claude Code's HTTP+OAuth
//! entry when the registry actually serves the /oauth authorization server.
//! Without it, a runner auto-updated onto a backend that predates /oauth/*
//! would write an entry whose Authenticate step can never succeed.
#![allow(clippy::unwrap_used)]

use super::{oauth_metadata_matches_issuer, registry_serves_mcp_oauth};

#[test]
fn metadata_with_matching_issuer_is_accepted() {
    let body = r#"{"issuer":"https://dev.vectorhawk.ai","token_endpoint":"https://dev.vectorhawk.ai/oauth/token"}"#;
    assert!(oauth_metadata_matches_issuer(
        body,
        "https://dev.vectorhawk.ai"
    ));
}

#[test]
fn issuer_comparison_ignores_a_trailing_slash_on_the_registry_url() {
    let body = r#"{"issuer":"https://dev.vectorhawk.ai"}"#;
    assert!(oauth_metadata_matches_issuer(
        body,
        "https://dev.vectorhawk.ai/"
    ));
}

#[test]
fn metadata_for_a_different_issuer_is_rejected() {
    let body = r#"{"issuer":"https://evil.example"}"#;
    assert!(!oauth_metadata_matches_issuer(
        body,
        "https://dev.vectorhawk.ai"
    ));
}

#[test]
fn non_json_or_missing_issuer_is_rejected() {
    assert!(!oauth_metadata_matches_issuer(
        "<html>portal</html>",
        "https://dev.vectorhawk.ai"
    ));
    assert!(!oauth_metadata_matches_issuer(
        r#"{"detail":"Not Found"}"#,
        "https://dev.vectorhawk.ai"
    ));
}

#[tokio::test]
async fn registry_serving_matching_metadata_is_supported() {
    let mut server = mockito::Server::new_async().await;
    let issuer = server.url();
    server
        .mock("GET", "/.well-known/oauth-authorization-server")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(r#"{{"issuer":"{issuer}"}}"#))
        .create_async()
        .await;
    assert!(registry_serves_mcp_oauth(&issuer).await);
}

#[tokio::test]
async fn registry_without_oauth_endpoints_is_not_supported() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/.well-known/oauth-authorization-server")
        .with_status(404)
        .with_body(r#"{"detail":"Not Found"}"#)
        .create_async()
        .await;
    assert!(!registry_serves_mcp_oauth(&server.url()).await);
}

#[tokio::test]
async fn registry_with_signing_key_unset_503_is_not_supported() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/.well-known/oauth-authorization-server")
        .with_status(503)
        .create_async()
        .await;
    assert!(!registry_serves_mcp_oauth(&server.url()).await);
}

#[tokio::test]
async fn unreachable_registry_is_not_supported() {
    // Port 9 (discard) on loopback: nothing listens, the connect fails fast.
    assert!(!registry_serves_mcp_oauth("http://127.0.0.1:9").await);
}
