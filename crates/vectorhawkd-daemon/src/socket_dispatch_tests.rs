//! Tests for socket_dispatch — use `dispatch()` directly to avoid needing a
//! real Unix socket in unit tests.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use vectorhawkd_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::RealBackend,
    protocol::{JsonRpcRequest, METHOD_NOT_FOUND},
};

use super::dispatch;

fn make_backend() -> Arc<RealBackend> {
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
    Arc::new(RealBackend::new(registry))
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
    let backend = make_backend();
    let resp = dispatch(&*backend, make_request("initialize", serde_json::json!({}))).await;
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
    let backend = make_backend();
    let resp = dispatch(&*backend, make_request("tools/list", serde_json::json!({}))).await;
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
    let backend = make_backend();
    let resp = dispatch(
        &*backend,
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
    let backend = make_backend();
    let resp = dispatch(
        &*backend,
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
    let backend = make_backend();
    let resp = dispatch(
        &*backend,
        make_request("nonexistent/method", serde_json::json!({})),
    )
    .await;
    assert!(resp.error.is_some());
    assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
}

#[tokio::test]
async fn dispatch_invalid_tool_call_params_returns_invalid_params() {
    let backend = make_backend();
    // tools/call requires a "name" field
    let resp = dispatch(
        &*backend,
        make_request("tools/call", serde_json::json!({"bad": "params"})),
    )
    .await;
    assert!(resp.error.is_some());
    use vectorhawkd_mcp::protocol::INVALID_PARAMS;
    assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
}
