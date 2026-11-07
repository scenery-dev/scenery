//! HTTP (streamable) backend tests: Accept header, JSON/SSE responses,
//! session-id echo and the body size cap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use mockito::Matcher;

const ACCEPT: &str = "application/json, text/event-stream";
const LIST_JSON: &str =
    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ask","description":"d"}]}}"#;
const CALL_JSON: &str =
    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hi"}]}}"#;

fn sse(json: &str) -> String {
    format!("event: message\ndata: {json}\n\n")
}

#[tokio::test]
async fn tools_list_json_sends_accept() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/mcp")
        .match_header("accept", ACCEPT)
        .with_header("content-type", "application/json")
        .with_body(LIST_JSON)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let tools = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
    m.assert_async().await;
}

#[tokio::test]
async fn tools_list_sse_single_event() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/mcp")
        .match_header("accept", ACCEPT)
        .with_header("content-type", "text/event-stream")
        .with_body(sse(LIST_JSON))
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let tools = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    assert_eq!(tools[0].name, "ask");
    m.assert_async().await;
}

#[tokio::test]
async fn tools_list_sse_notification_before_result() {
    let mut server = mockito::Server::new_async().await;
    let body = format!(
        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}\n\n{}",
        sse(LIST_JSON)
    );
    let _m = server
        .mock("POST", "/mcp")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let tools = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
}

#[tokio::test]
async fn tools_list_sse_multiline_data() {
    let mut server = mockito::Server::new_async().await;
    let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"tools\":[{\"name\":\"ask\"}]}}\n\n";
    let _m = server
        .mock("POST", "/mcp")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let tools = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
}

#[tokio::test]
async fn tools_list_missing_content_type_sse_body() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/mcp")
        .with_body(sse(LIST_JSON))
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let tools = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
}

#[tokio::test]
async fn tools_call_json_and_sse() {
    let mut server = mockito::Server::new_async().await;
    let json_mock = server
        .mock("POST", "/json")
        .match_header("accept", ACCEPT)
        .with_header("content-type", "application/json")
        .with_body(CALL_JSON)
        .create_async()
        .await;
    let sse_mock = server
        .mock("POST", "/sse")
        .match_header("accept", ACCEPT)
        .match_header("authorization", "Bearer tok")
        .with_header("content-type", "text/event-stream")
        .with_body(format!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\n{}",
            sse(CALL_JSON)
        ))
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let args = serde_json::json!({});
    let a = reg
        .call_http_tool(&format!("{}/json", server.url()), "ask", &args, None)
        .await
        .unwrap();
    assert_eq!(a["content"][0]["text"], "hi");
    let b = reg
        .call_http_tool(&format!("{}/sse", server.url()), "ask", &args, Some("tok"))
        .await
        .unwrap();
    assert_eq!(b["content"][0]["text"], "hi");
    json_mock.assert_async().await;
    sse_mock.assert_async().await;
}

#[tokio::test]
async fn session_id_is_captured_and_echoed() {
    let mut server = mockito::Server::new_async().await;
    let first = server
        .mock("POST", "/mcp")
        .match_header("mcp-session-id", Matcher::Missing)
        .with_header("content-type", "application/json")
        .with_header("mcp-session-id", "sess-42")
        .with_body(LIST_JSON)
        .expect(1)
        .create_async()
        .await;
    let second = server
        .mock("POST", "/mcp")
        .match_header("mcp-session-id", "sess-42")
        .with_header("content-type", "application/json")
        .with_body(CALL_JSON)
        .expect(1)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    reg.fetch_tools_http(&url, None).await.unwrap();
    reg.call_http_tool(&url, "ask", &serde_json::json!({}), None)
        .await
        .unwrap();
    first.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/mcp")
        .with_header("content-type", "application/json")
        .with_body(vec![b' '; crate::http_rpc::MAX_RESPONSE_BYTES + 1])
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let err = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("byte limit"));
}
