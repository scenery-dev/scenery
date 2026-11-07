//! Shared streamable-HTTP JSON-RPC plumbing for remote MCP backends.
//!
//! The MCP streamable-HTTP transport requires clients to send
//! `Accept: application/json, text/event-stream` and allows the server to
//! answer with either a plain JSON body or an SSE stream. Every HTTP request
//! the aggregator makes goes through [`post_rpc`], so the Accept header,
//! response parsing (JSON / SSE / sniffed), the body size cap and
//! `initialize` handshake / `Mcp-Session-Id` handling (see `http_session`)
//! live in exactly one place.

use anyhow::{Context, Result};
use scenery_core::http::AsyncSendTrackedExt;
use serde_json::Value;
use tracing::debug;

pub use crate::http_session::SessionStore;
use crate::http_session::{Session, PROTOCOL_HEADER};

/// `Accept` value mandated by the streamable-HTTP spec for every POST.
pub const ACCEPT_HEADER_VALUE: &str = "application/json, text/event-stream";

/// Session header assigned by the server (usually on `initialize`).
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Hard cap on a response body. Tool results are text; 8 MiB is generous and
/// stops a misbehaving or hostile backend from exhausting daemon memory.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// How a response body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Json,
    Sse,
}

fn detect_framing(content_type: Option<&str>, body: &str) -> Framing {
    let ct = content_type
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ct.contains("text/event-stream") {
        return Framing::Sse;
    }
    if ct.contains("json") {
        return Framing::Json;
    }
    // Missing or unknown Content-Type: sniff.
    let head = body.trim_start();
    if head.starts_with("event:") || head.starts_with("data:") || head.starts_with(':') {
        Framing::Sse
    } else {
        Framing::Json
    }
}

/// Pick the JSON-RPC response for `id` out of a parsed value (object or batch).
fn match_response(value: &Value, id: &Value) -> Option<Value> {
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(found) = match_response(item, id) {
                    return Some(found);
                }
            }
            None
        }
        Value::Object(map) => {
            let is_response = map.contains_key("result") || map.contains_key("error");
            if is_response && map.get("id") == Some(id) {
                Some(value.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse one SSE event's accumulated `data` as JSON and match it against `id`.
fn match_event(data: &str, id: &Value) -> Option<Value> {
    if data.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(data).ok()?;
    match_response(&parsed, id)
}

fn parse_sse(body: &str, id: &Value) -> Result<Value> {
    let mut data = String::new();
    let mut has_data = false;
    let mut event_type = String::new();

    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            // Blank line dispatches the event.
            let is_message = event_type.is_empty() || event_type == "message";
            if has_data && is_message {
                if let Some(found) = match_event(&data, id) {
                    return Ok(found);
                }
            }
            data.clear();
            has_data = false;
            event_type.clear();
            continue;
        }
        if line.starts_with(':') {
            continue; // comment / keep-alive ping
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if has_data {
                    data.push('\n');
                }
                data.push_str(value);
                has_data = true;
            }
            "event" => event_type = value.to_string(),
            _ => {} // id:, retry:, unknown — ignore
        }
    }
    // Stream ended without a trailing blank line: flush the last event.
    let is_message = event_type.is_empty() || event_type == "message";
    if has_data && is_message {
        if let Some(found) = match_event(&data, id) {
            return Ok(found);
        }
    }
    anyhow::bail!("SSE response contained no JSON-RPC response for id {id}")
}

/// Parse a response body (already size-capped) into the JSON-RPC response
/// envelope matching `id`, according to its Content-Type.
pub fn parse_rpc_body(content_type: Option<&str>, body: &str, id: &Value) -> Result<Value> {
    match detect_framing(content_type, body) {
        Framing::Sse => parse_sse(body, id),
        Framing::Json => {
            let value: Value =
                serde_json::from_str(body).context("response body is not valid JSON")?;
            // Prefer an id match, but a single plain object is accepted as-is
            // (some servers echo a different id type).
            if let Some(found) = match_response(&value, id) {
                return Ok(found);
            }
            if value.is_object() {
                Ok(value)
            } else {
                anyhow::bail!("JSON response contained no JSON-RPC response for id {id}")
            }
        }
    }
}

/// Read a response body, failing once it exceeds [`MAX_RESPONSE_BYTES`].
/// The client-level total timeout also bounds this read.
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("failed reading response body")? {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!("response body exceeds {MAX_RESPONSE_BYTES} byte limit");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Why a single request failed. `SessionExpired` is split out so `post_rpc`
/// can re-initialize once.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RpcError {
    #[error("MCP session expired (HTTP 404 on a request carrying a session id)")]
    SessionExpired,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl RpcError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            RpcError::SessionExpired => anyhow::Error::new(RpcError::SessionExpired),
            RpcError::Other(e) => e,
        }
    }
}

/// A successfully parsed JSON-RPC reply plus the `Mcp-Session-Id` it carried.
pub(crate) struct RpcReply {
    pub body: Value,
    pub session_id: Option<String>,
}

fn build_request(
    client: &reqwest::Client,
    url: &str,
    auth_token: Option<&str>,
    session: Option<&Session>,
    payload: &Value,
) -> reqwest::RequestBuilder {
    let mut req = client
        .post(url)
        .header("accept", ACCEPT_HEADER_VALUE)
        .json(payload);
    if let Some(token) = auth_token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if let Some(session) = session {
        req = req.header(PROTOCOL_HEADER, &session.protocol_version);
        if let Some(sid) = &session.session_id {
            req = req.header(SESSION_HEADER, sid);
        }
    }
    req
}

/// Send one JSON-RPC request (id 1) and parse the matching response.
pub(crate) async fn send_request(
    client: &reqwest::Client,
    url: &str,
    auth_token: Option<&str>,
    session: Option<&Session>,
    method: &str,
    params: &Value,
) -> Result<RpcReply, RpcError> {
    let id = Value::from(1);
    let payload = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params,
    });
    let resp = build_request(client, url, auth_token, session, &payload)
        .send_tracked()
        .await
        .with_context(|| format!("failed to reach HTTP backend at {url}"))?;

    let status = resp.status();
    let session_id = resp
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let bytes = read_capped(resp).await?;
    let text = String::from_utf8_lossy(&bytes);
    // Spec: 404 on a request carrying a session id means the session expired.
    if status == reqwest::StatusCode::NOT_FOUND && session.is_some_and(|s| s.session_id.is_some()) {
        return Err(RpcError::SessionExpired);
    }
    if !status.is_success() {
        let snippet: String = text.chars().take(512).collect();
        return Err(
            anyhow::anyhow!("HTTP backend returned HTTP {status} on {method}: {snippet}").into(),
        );
    }
    let body = parse_rpc_body(content_type.as_deref(), &text, &id)
        .with_context(|| format!("failed to parse {method} response"))?;
    Ok(RpcReply { body, session_id })
}

/// Send a JSON-RPC notification (no id). Any 2xx is success; the spec says 202.
pub(crate) async fn send_notification(
    client: &reqwest::Client,
    url: &str,
    auth_token: Option<&str>,
    session: &Session,
    method: &str,
) -> Result<()> {
    let payload = serde_json::json!({ "jsonrpc": "2.0", "method": method });
    let resp = build_request(client, url, auth_token, Some(session), &payload)
        .send_tracked()
        .await
        .with_context(|| format!("failed to reach HTTP backend at {url}"))?;
    let status = resp.status();
    // Drain (bounded) so the connection can be reused.
    let _ = read_capped(resp).await;
    if !status.is_success() {
        anyhow::bail!("HTTP backend returned HTTP {status} on {method}");
    }
    Ok(())
}

/// POST one JSON-RPC request to a streamable-HTTP MCP endpoint and return the
/// matching response envelope (`{"jsonrpc","id","result"|"error"}`).
///
/// Performs the `initialize` handshake first if this URL has no session yet
/// (shared across concurrent callers), then sends the request with
/// `MCP-Protocol-Version` and `Mcp-Session-Id`. If the server answers 404 to a
/// request carrying a session id, the session is dropped, re-initialized once,
/// and the request retried once.
pub async fn post_rpc(
    client: &reqwest::Client,
    sessions: &SessionStore,
    url: &str,
    auth_token: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value> {
    let session = sessions.ensure(client, url, auth_token).await?;
    match send_request(client, url, auth_token, Some(&session), method, &params).await {
        Ok(reply) => Ok(reply.body),
        Err(RpcError::SessionExpired) => {
            debug!(url, "MCP session expired; re-initializing once");
            sessions.invalidate(url, &session).await;
            let fresh = sessions.ensure(client, url, auth_token).await?;
            send_request(client, url, auth_token, Some(&fresh), method, &params)
                .await
                .map(|r| r.body)
                .map_err(RpcError::into_anyhow)
        }
        Err(other) => Err(other.into_anyhow()),
    }
}

#[cfg(test)]
#[path = "http_rpc_tests.rs"]
mod tests;
