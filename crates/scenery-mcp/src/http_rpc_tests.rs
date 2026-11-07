#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use serde_json::json;

const SSE: &str = "text/event-stream";

fn id() -> Value {
    json!(1)
}

#[test]
fn json_body_by_content_type() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
    let v = parse_rpc_body(Some("application/json; charset=utf-8"), body, &id()).unwrap();
    assert_eq!(v["result"]["tools"], json!([]));
}

#[test]
fn sse_single_event() {
    let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
    let v = parse_rpc_body(Some(SSE), body, &id()).unwrap();
    assert_eq!(v["result"]["ok"], json!(true));
}

#[test]
fn sse_skips_notification_ping_and_wrong_id() {
    let body = concat!(
        ": ping\n\n",
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"x\":1}}\n\n",
        "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":2}}\r\n\r\n",
    );
    let v = parse_rpc_body(Some(SSE), body, &id()).unwrap();
    assert_eq!(v["result"]["x"], json!(2));
}

#[test]
fn sse_multiline_data_and_unterminated_last_event() {
    let body =
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1,\ndata: \"result\":{\"m\":1}}";
    let v = parse_rpc_body(Some(SSE), body, &id()).unwrap();
    assert_eq!(v["result"]["m"], json!(1));
}

#[test]
fn missing_content_type_sniffs_sse_and_json() {
    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"s\":1}}\n\n";
    assert_eq!(
        parse_rpc_body(None, sse, &id()).unwrap()["result"]["s"],
        json!(1)
    );
    let js = r#"{"jsonrpc":"2.0","id":1,"result":{"j":1}}"#;
    assert_eq!(
        parse_rpc_body(None, js, &id()).unwrap()["result"]["j"],
        json!(1)
    );
    let odd = parse_rpc_body(Some("text/plain"), sse, &id()).unwrap();
    assert_eq!(odd["result"]["s"], json!(1));
}

#[test]
fn sse_without_response_is_error() {
    let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n";
    assert!(parse_rpc_body(Some(SSE), body, &id()).is_err());
}
