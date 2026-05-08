//! M6.2 integration tests: GatewayModelClient HTTP inference, auth token handling.

use camino::Utf8PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use vectorhawkd_core::{
    auth::save_tokens,
    gateway_model::GatewayModelClient,
    model::{ModelClient, ModelRequest, ModelSource},
    state::AppState,
};

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!("vh-m6-gw-tests-{label}-{nanos}")))
        .expect("temp path should be UTF-8")
}

fn minimal_request() -> ModelRequest {
    ModelRequest {
        system_prompt: "You are helpful.".to_string(),
        user_message: "Say hello".to_string(),
        json_output: false,
        prefer_local: false,
        ..Default::default()
    }
}

/// Test 1: gateway returns provider response → ModelSource::Provider, cost populated.
#[test]
fn gateway_successful_provider_response() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/gateway/v1/inference")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "vh_tier": "provider",
                "vh_provider": "anthropic",
                "vh_cost_usd": 0.00045,
                "usage": { "input_tokens": 100, "output_tokens": 50 },
                "content": [{ "type": "text", "text": "hello" }]
            }"#,
        )
        .create();

    let state_root = temp_root("gw-provider");
    let state = std::sync::Arc::new(AppState::bootstrap_in(state_root.clone()).expect("bootstrap"));

    // Save an auth token so the client can authenticate.
    save_tokens(
        &state,
        &server.url(),
        "test-access-token",
        "test-refresh-token",
    )
    .expect("save tokens");

    let client = GatewayModelClient::new(server.url(), std::sync::Arc::clone(&state));

    let resp = client
        .generate(minimal_request())
        .expect("generate should succeed");

    assert_eq!(resp.text, "hello");
    assert_eq!(resp.prompt_tokens, 100);
    assert_eq!(resp.completion_tokens, 50);
    assert!(
        (resp.cost_usd - 0.00045).abs() < 1e-9,
        "cost: {}",
        resp.cost_usd
    );
    assert_eq!(resp.source, ModelSource::Provider("anthropic".to_string()));

    let _ = std::fs::remove_dir_all(&state_root);
}

/// Test 2: no auth token in SQLite → error without making HTTP call.
#[test]
fn gateway_no_token_returns_error_before_http_call() {
    let state_root = temp_root("gw-noauth");
    let state = std::sync::Arc::new(AppState::bootstrap_in(state_root.clone()).expect("bootstrap"));

    // No token saved — the client must error before making a network call.
    // Use a port that is guaranteed unreachable to ensure no call was made.
    let client = GatewayModelClient::new(
        "http://127.0.0.1:19998".to_string(),
        std::sync::Arc::clone(&state),
    );

    let err = client
        .generate(minimal_request())
        .expect_err("should fail with no auth token");

    assert!(
        err.to_string().contains("not authenticated") || err.to_string().contains("auth login"),
        "expected unauthenticated error, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&state_root);
}

/// Test 3: gateway returns 401 → error message contains "auth token expired".
#[test]
fn gateway_401_returns_expired_token_error() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/gateway/v1/inference")
        .with_status(401)
        .with_body(r#"{"error": "Unauthorized"}"#)
        .create();

    let state_root = temp_root("gw-401");
    let state = std::sync::Arc::new(AppState::bootstrap_in(state_root.clone()).expect("bootstrap"));

    save_tokens(&state, &server.url(), "expired-token", "old-refresh").expect("save tokens");

    let client = GatewayModelClient::new(server.url(), std::sync::Arc::clone(&state));

    let err = client
        .generate(minimal_request())
        .expect_err("should fail on 401");

    assert!(
        err.to_string().contains("auth token expired") || err.to_string().contains("auth login"),
        "expected expired-token error, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&state_root);
}

/// Test 4: gateway returns vh_tier="internal" → ModelSource::Internal.
#[test]
fn gateway_internal_tier_maps_to_internal_source() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/gateway/v1/inference")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "vh_tier": "internal",
                "vh_provider": "phi-3",
                "vh_cost_usd": 0.0,
                "usage": { "input_tokens": 20, "output_tokens": 10 },
                "content": [{ "type": "text", "text": "internal response" }]
            }"#,
        )
        .create();

    let state_root = temp_root("gw-internal");
    let state = std::sync::Arc::new(AppState::bootstrap_in(state_root.clone()).expect("bootstrap"));

    save_tokens(&state, &server.url(), "tok", "ref").expect("save tokens");

    let client = GatewayModelClient::new(server.url(), std::sync::Arc::clone(&state));
    let resp = client.generate(minimal_request()).expect("generate");

    assert_eq!(resp.source, ModelSource::Internal("phi-3".to_string()));

    let _ = std::fs::remove_dir_all(&state_root);
}

/// Test 5: gateway returns vh_tier="local" → ModelSource::Local.
#[test]
fn gateway_local_tier_maps_to_local_source() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/gateway/v1/inference")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "vh_tier": "local",
                "vh_provider": "llama3",
                "vh_cost_usd": 0.0,
                "usage": { "input_tokens": 5, "output_tokens": 3 },
                "content": [{ "type": "text", "text": "local response" }]
            }"#,
        )
        .create();

    let state_root = temp_root("gw-local");
    let state = std::sync::Arc::new(AppState::bootstrap_in(state_root.clone()).expect("bootstrap"));

    save_tokens(&state, &server.url(), "tok", "ref").expect("save tokens");

    let client = GatewayModelClient::new(server.url(), std::sync::Arc::clone(&state));
    let resp = client.generate(minimal_request()).expect("generate");

    assert_eq!(resp.source, ModelSource::Local("llama3".to_string()));

    let _ = std::fs::remove_dir_all(&state_root);
}
