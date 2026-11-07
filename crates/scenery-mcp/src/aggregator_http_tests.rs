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

const INIT_JSON: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}"#;

fn method_matcher(method: &str) -> Matcher {
    Matcher::PartialJsonString(format!(r#"{{"method":"{method}"}}"#))
}

/// Mock the `initialize` + `notifications/initialized` pair for `path`.
/// Returns the mocks so they stay alive for the test's duration.
async fn mock_init(
    server: &mut mockito::ServerGuard,
    path: &str,
    session_id: Option<&str>,
) -> Vec<mockito::Mock> {
    let mut init = server
        .mock("POST", path)
        .match_body(method_matcher("initialize"))
        .with_header("content-type", "application/json")
        .with_body(INIT_JSON);
    if let Some(sid) = session_id {
        init = init.with_header("mcp-session-id", sid);
    }
    let notif = server
        .mock("POST", path)
        .match_body(method_matcher("notifications/initialized"))
        .with_status(202);
    vec![init.create_async().await, notif.create_async().await]
}

fn sse(json: &str) -> String {
    format!("event: message\ndata: {json}\n\n")
}

#[tokio::test]
async fn tools_list_json_sends_accept() {
    let mut server = mockito::Server::new_async().await;
    let _init = mock_init(&mut server, "/mcp", None).await;
    let m = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/list"}"#.into(),
        ))
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
    let _init = mock_init(&mut server, "/mcp", None).await;
    let m = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/list"}"#.into(),
        ))
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
    let _init = mock_init(&mut server, "/mcp", None).await;
    let body = format!(
        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}\n\n{}",
        sse(LIST_JSON)
    );
    let _m = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/list"}"#.into(),
        ))
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
    let _init = mock_init(&mut server, "/mcp", None).await;
    let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"tools\":[{\"name\":\"ask\"}]}}\n\n";
    let _m = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/list"}"#.into(),
        ))
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
    let _init = mock_init(&mut server, "/mcp", None).await;
    let _m = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/list"}"#.into(),
        ))
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
    let _i1 = mock_init(&mut server, "/json", None).await;
    let _i2 = mock_init(&mut server, "/sse", None).await;
    let json_mock = server
        .mock("POST", "/json")
        .match_body(Matcher::PartialJsonString(
            r#"{"method":"tools/call"}"#.into(),
        ))
        .match_header("accept", ACCEPT)
        .with_header("content-type", "application/json")
        .with_body(CALL_JSON)
        .create_async()
        .await;
    let sse_mock = server
        .mock("POST", "/sse")
        .match_body(Matcher::PartialJsonString(r#"{"method":"tools/call"}"#.into()))
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

// ── initialize handshake ─────────────────────────────────────────────────────

type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Mock that records `tag|protocol-version|session-id` for each hit.
fn recording(
    server: &mut mockito::ServerGuard,
    method: &str,
    log: &Log,
    body: &str,
    status: usize,
    session_header: Option<&str>,
) -> mockito::Mock {
    let log = log.clone();
    let tag = method.to_string();
    let body = body.as_bytes().to_vec();
    let mut m = server
        .mock("POST", "/mcp")
        .match_body(method_matcher(method))
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let h = |n: &str| {
                req.header(n)
                    .first()
                    .and_then(|v| v.to_str().ok().map(str::to_string))
                    .unwrap_or_else(|| "-".to_string())
            };
            let line = format!(
                "{tag}|{}|{}",
                h("mcp-protocol-version"),
                h("mcp-session-id")
            );
            log.lock().unwrap().push(line);
            body.clone()
        });
    if let Some(sid) = session_header {
        m = m.with_header("mcp-session-id", sid);
    }
    m
}

#[tokio::test]
async fn handshake_runs_in_order_with_version_and_session_headers() {
    let mut server = mockito::Server::new_async().await;
    let log: Log = Default::default();
    let _a = recording(
        &mut server,
        "initialize",
        &log,
        INIT_JSON,
        200,
        Some("sess-42"),
    )
    .create_async()
    .await;
    let _b = recording(
        &mut server,
        "notifications/initialized",
        &log,
        "",
        202,
        None,
    )
    .create_async()
    .await;
    let _c = recording(&mut server, "tools/list", &log, LIST_JSON, 200, None)
        .create_async()
        .await;
    let _d = recording(&mut server, "tools/call", &log, CALL_JSON, 200, None)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    reg.fetch_tools_http(&url, None).await.unwrap();
    reg.call_http_tool(&url, "ask", &serde_json::json!({}), None)
        .await
        .unwrap();
    // Server negotiated 2025-06-18 (we offered newer); that is what we echo.
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "initialize|-|-",
            "notifications/initialized|2025-06-18|sess-42",
            "tools/list|2025-06-18|sess-42",
            "tools/call|2025-06-18|sess-42",
        ]
    );
}

#[tokio::test]
async fn initialize_request_carries_client_info_and_protocol_version() {
    let mut server = mockito::Server::new_async().await;
    let init = server
        .mock("POST", "/mcp")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "method": "initialize",
            "params": {
                "protocolVersion": crate::http_session::OFFERED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "scenery", "version": env!("CARGO_PKG_VERSION") },
            }
        })))
        .with_header("content-type", "application/json")
        .with_body(INIT_JSON)
        .expect(1)
        .create_async()
        .await;
    let _n = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("notifications/initialized"))
        .with_status(202)
        .create_async()
        .await;
    let _l = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/list"))
        .with_header("content-type", "application/json")
        .with_body(LIST_JSON)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    reg.fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap();
    init.assert_async().await;
}

#[tokio::test]
async fn stateless_server_without_session_id_works() {
    let mut server = mockito::Server::new_async().await;
    let _init = mock_init(&mut server, "/mcp", None).await;
    let list = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/list"))
        .match_header("mcp-session-id", Matcher::Missing)
        .match_header("mcp-protocol-version", "2025-06-18")
        .with_header("content-type", "application/json")
        .with_body(LIST_JSON)
        .expect(2)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    reg.fetch_tools_http(&url, None).await.unwrap();
    reg.fetch_tools_http(&url, None).await.unwrap();
    list.assert_async().await;
}

#[tokio::test]
async fn expired_session_reinitializes_once_and_retries_once() {
    let mut server = mockito::Server::new_async().await;
    let log: Log = Default::default();
    let _i1 = recording(&mut server, "initialize", &log, INIT_JSON, 200, Some("old"))
        .expect(1)
        .create_async()
        .await;
    let _n1 = recording(
        &mut server,
        "notifications/initialized",
        &log,
        "",
        202,
        None,
    )
    .match_header("mcp-session-id", "old")
    .create_async()
    .await;
    let expired = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/call"))
        .match_header("mcp-session-id", "old")
        .with_status(404)
        .expect(1)
        .create_async()
        .await;
    // Registered after the first initialize mock is exhausted (expect 1).
    let _i2 = recording(&mut server, "initialize", &log, INIT_JSON, 200, Some("new"))
        .expect(1)
        .create_async()
        .await;
    let _n2 = recording(
        &mut server,
        "notifications/initialized",
        &log,
        "",
        202,
        None,
    )
    .match_header("mcp-session-id", "new")
    .create_async()
    .await;
    let ok = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/call"))
        .match_header("mcp-session-id", "new")
        .with_header("content-type", "application/json")
        .with_body(CALL_JSON)
        .expect(1)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    // The first call initializes as "old", gets 404, re-initializes as "new".
    let out = reg
        .call_http_tool(&url, "ask", &serde_json::json!({}), None)
        .await
        .unwrap();
    assert_eq!(out["content"][0]["text"], "hi");
    expired.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn persistent_404_does_not_loop() {
    let mut server = mockito::Server::new_async().await;
    let init = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("initialize"))
        .with_header("content-type", "application/json")
        .with_header("mcp-session-id", "s")
        .with_body(INIT_JSON)
        .expect(2) // first handshake + exactly one re-initialize
        .create_async()
        .await;
    let _n = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("notifications/initialized"))
        .with_status(202)
        .create_async()
        .await;
    let gone = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/list"))
        .with_status(404)
        .expect(2) // original + exactly one retry
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let err = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("404") || format!("{err:#}").contains("expired"));
    init.assert_async().await;
    gone.assert_async().await;
}

#[tokio::test]
async fn concurrent_first_calls_share_one_initialize() {
    let mut server = mockito::Server::new_async().await;
    let init = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("initialize"))
        .with_header("content-type", "application/json")
        .with_header("mcp-session-id", "shared")
        .with_body(INIT_JSON)
        .expect(1)
        .create_async()
        .await;
    let _n = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("notifications/initialized"))
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let list = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("tools/list"))
        .match_header("mcp-session-id", "shared")
        .with_header("content-type", "application/json")
        .with_body(LIST_JSON)
        .expect(8)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let reg = reg.clone();
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            reg.fetch_tools_http(&url, None).await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().len(), 1);
    }
    init.assert_async().await;
    list.assert_async().await;
}

#[tokio::test]
async fn initialize_jsonrpc_error_surfaces_and_is_not_hammered() {
    let mut server = mockito::Server::new_async().await;
    let init = server
        .mock("POST", "/mcp")
        .match_body(method_matcher("initialize"))
        .with_header("content-type", "application/json")
        .with_body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad version"}}"#)
        .expect(1)
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let url = format!("{}/mcp", server.url());
    let err = reg.fetch_tools_http(&url, None).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("initialize") && text.contains("bad version"),
        "{text}"
    );
    // Second call within the cooldown fails fast without another request.
    let again = reg
        .call_http_tool(&url, "ask", &serde_json::json!({}), None)
        .await
        .unwrap_err();
    assert!(format!("{again:#}").contains("bad version"));
    init.assert_async().await;
}

#[tokio::test]
async fn initialize_non_2xx_surfaces_cleanly() {
    let mut server = mockito::Server::new_async().await;
    let _init = server
        .mock("POST", "/mcp")
        .with_status(500)
        .with_body("boom")
        .create_async()
        .await;
    let reg = BackendRegistry::new();
    let err = reg
        .fetch_tools_http(&format!("{}/mcp", server.url()), None)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("initialize failed") && text.contains("500"),
        "{text}"
    );
}
