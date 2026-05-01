//! Tests for socket_dispatch — use `dispatch()` directly to avoid needing a
//! real Unix socket in unit tests.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::RealBackend,
    protocol::{JsonRpcRequest, METHOD_NOT_FOUND},
};

use crate::oauth_state::OAuthState;

use super::{dispatch, DaemonContext};

fn make_ctx() -> DaemonContext {
    let registry = Arc::new(BackendRegistry::new());
    registry.register_backend(BackendEntry {
        server_id: "echo".to_string(),
        name: "echo".to_string(),
        transport: BackendTransport::Stub,
        tools: vec![ToolDefinition {
            name: "echo".to_string(),
            description: Some("Echoes input back".to_string()),
            input_schema: None,
        }],
        tool_visibility: ToolVisibility::All,
        priority: 50,
        consecutive_errors: 0,
        unhealthy: false,
    });
    DaemonContext {
        backend: Arc::new(RealBackend::new(registry)),
        oauth_state: Arc::new(OAuthState::new()),
        listener_port: Some(39127),
    }
}

fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: method.to_string(),
        params,
    }
}

#[tokio::test]
async fn dispatch_initialize_returns_protocol_version() {
    let ctx = make_ctx();
    let resp = dispatch(&ctx, make_request("initialize", serde_json::json!({}))).await;
    assert!(
        resp.error.is_none(),
        "initialize should not error: {:?}",
        resp.error
    );
    let result = resp.result.unwrap();
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "vectorhawkd");
    assert!(result["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn dispatch_list_tools_returns_namespaced_stub_tool() {
    let ctx = make_ctx();
    let resp = dispatch(&ctx, make_request("tools/list", serde_json::json!({}))).await;
    assert!(
        resp.error.is_none(),
        "tools/list should not error: {:?}",
        resp.error
    );
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"echo__echo"),
        "expected echo__echo, got {names:?}"
    );
}

#[tokio::test]
async fn dispatch_call_stub_tool_returns_content() {
    let ctx = make_ctx();
    let resp = dispatch(
        &ctx,
        make_request(
            "tools/call",
            serde_json::json!({"name": "echo__echo", "arguments": {"input": "hello"}}),
        ),
    )
    .await;
    assert!(
        resp.error.is_none(),
        "call should not error: {:?}",
        resp.error
    );
    let content = resp.result.unwrap()["content"].as_array().unwrap().clone();
    assert!(!content.is_empty(), "content should not be empty");
}

#[tokio::test]
async fn dispatch_call_unknown_tool_returns_error_content() {
    let ctx = make_ctx();
    let resp = dispatch(
        &ctx,
        make_request(
            "tools/call",
            serde_json::json!({"name": "echo__nonexistent", "arguments": {}}),
        ),
    )
    .await;
    // RealBackend returns a ToolCallResult with is_error=true, not a JsonRpcError.
    assert!(resp.error.is_none(), "should not be a JSON-RPC error");
    let result = resp.result.unwrap();
    assert_eq!(result["isError"], true, "should be a tool-level error");
}

#[tokio::test]
async fn dispatch_unknown_method_returns_method_not_found() {
    let ctx = make_ctx();
    let resp = dispatch(
        &ctx,
        make_request("nonexistent/method", serde_json::json!({})),
    )
    .await;
    assert!(resp.error.is_some());
    assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
}

#[tokio::test]
async fn dispatch_invalid_tool_call_params_returns_invalid_params() {
    let ctx = make_ctx();
    // tools/call requires a "name" field
    let resp = dispatch(
        &ctx,
        make_request("tools/call", serde_json::json!({"bad": "params"})),
    )
    .await;
    assert!(resp.error.is_some());
    use vectorhawkd_mcp::protocol::INVALID_PARAMS;
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}

#[tokio::test]
async fn dispatch_auth_get_oauth_listener_port_returns_port() {
    let ctx = make_ctx(); // listener_port = Some(39127)
    let resp = dispatch(
        &ctx,
        make_request("auth/get_oauth_listener_port", serde_json::json!({})),
    )
    .await;
    assert!(resp.error.is_none(), "should not error: {:?}", resp.error);
    assert_eq!(resp.result.unwrap()["port"], 39127);
}

#[tokio::test]
async fn dispatch_auth_get_oauth_listener_port_when_none() {
    let registry = Arc::new(BackendRegistry::new());
    let ctx = DaemonContext {
        backend: Arc::new(RealBackend::new(registry)),
        oauth_state: Arc::new(OAuthState::new()),
        listener_port: None,
    };
    let resp = dispatch(
        &ctx,
        make_request("auth/get_oauth_listener_port", serde_json::json!({})),
    )
    .await;
    use vectorhawkd_mcp::protocol::INTERNAL_ERROR;
    assert_eq!(resp.error.unwrap().code, INTERNAL_ERROR);
}
