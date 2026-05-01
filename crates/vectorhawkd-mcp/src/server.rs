//! `Server<B: Backend>` — the MCP JSON-RPC dispatch loop.
//!
//! The server is generic over a `Backend` implementation:
//!
//! - The **shim** instantiates `Server<SocketBackend>` (normal path) or
//!   `Server<EmbeddedBackend>` (fallback when daemon unreachable).
//! - The **daemon** exposes a Unix socket listener; each incoming shim
//!   connection is served by a `Server<RealBackend>` task.
//!
//! # Transport
//!
//! For stdio (shim ↔ AI client): newline-delimited JSON (the MCP wire format).
//! For socket (shim ↔ daemon): 4-byte big-endian length-prefixed JSON
//! (see `backend::write_framed` / `backend::read_framed`).
//!
//! This module handles stdio only. The daemon socket listener lives in
//! `vectorhawkd-daemon` (Stream 4) and calls `Backend` methods directly.

use crate::{
    backend::Backend,
    protocol::{
        JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ToolCallParams, INVALID_PARAMS,
        METHOD_NOT_FOUND, PARSE_ERROR,
    },
};
use anyhow::Result;
use std::io::{self, BufRead, Write};
use tracing::{debug, error, info, warn};

/// MCP server that reads JSON-RPC messages from `stdin` and writes responses
/// to `stdout`, delegating all work to a `Backend` instance.
///
/// The type parameter `B` is the backend implementation. Construct the server
/// with `Server::new(backend)` and run it with `Server::run_stdio()`.
pub struct Server<B: Backend> {
    backend: B,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Run the MCP server loop over stdio.
    ///
    /// Reads newline-delimited JSON-RPC frames from stdin, dispatches to the
    /// backend, and writes responses to stdout. Blocks until stdin is closed
    /// (AI client disconnects).
    ///
    /// `on_start` is called once before the loop begins; `on_shutdown` is
    /// called once after the loop exits.
    pub async fn run_stdio(self) -> Result<()> {
        self.backend.on_start().await?;

        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut reader = io::BufReader::new(stdin.lock());
        let mut writer = io::BufWriter::new(stdout.lock());

        info!("MCP server starting (stdio)");

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF — client disconnected
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    error!(error = %e, "failed to read from stdin");
                    break;
                }
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let response = self.handle_line(line).await;
            if let Some(resp) = response {
                let serialized = match serde_json::to_string(&resp) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(error = %e, "failed to serialize response");
                        continue;
                    }
                };
                if let Err(e) = writeln!(writer, "{serialized}") {
                    error!(error = %e, "failed to write to stdout");
                    break;
                }
                if let Err(e) = writer.flush() {
                    error!(error = %e, "failed to flush stdout");
                    break;
                }
            }
        }

        info!("MCP server shutting down");
        self.backend.on_shutdown().await;
        Ok(())
    }

    /// Process a single JSON-RPC line. Returns `None` for notifications
    /// (which have no id and require no response).
    async fn handle_line(&self, line: &str) -> Option<JsonRpcResponse> {
        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(JsonRpcResponse::error(
                    None,
                    PARSE_ERROR,
                    format!("invalid JSON: {e}"),
                ));
            }
        };

        debug!(method = %request.method, id = ?request.id, "received request");

        // Notifications (no id) don't get responses.
        if request.id.is_none() {
            debug!(method = %request.method, "received notification, no response needed");
            return None;
        }

        let response = self.dispatch(request).await;
        Some(response)
    }

    /// Dispatch a request to the backend.
    async fn dispatch(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => match self.backend.initialize(request.params).await {
                Ok(result) => {
                    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
                }
                Err(e) => {
                    warn!(error = %e, "initialize failed");
                    JsonRpcResponse::error(id, crate::protocol::INTERNAL_ERROR, e.to_string())
                }
            },
            "tools/list" => match self.backend.list_tools(request.params).await {
                Ok(result) => {
                    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
                }
                Err(e) => {
                    warn!(error = %e, "tools/list failed");
                    JsonRpcResponse::error(id, crate::protocol::INTERNAL_ERROR, e.to_string())
                }
            },
            "tools/call" => {
                let params: ToolCallParams = match serde_json::from_value(request.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            format!("invalid tool call params: {e}"),
                        );
                    }
                };
                match self.backend.call_tool(params).await {
                    Ok(result) => JsonRpcResponse::success(
                        id,
                        serde_json::to_value(result).unwrap_or_default(),
                    ),
                    Err(e) => {
                        warn!(error = %e, "tools/call failed");
                        JsonRpcResponse::error(id, crate::protocol::INTERNAL_ERROR, e.to_string())
                    }
                }
            }
            other => {
                debug!(method = %other, "unknown method");
                JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("unknown method: {other}"))
            }
        }
    }
}

/// Send a `notifications/tools/list_changed` notification to stdout.
///
/// This is called after any operation that changes the available tool set
/// (backend install/uninstall, skill update, aggregator sync).
pub fn send_list_changed_notification<W: Write>(writer: &mut W) -> Result<()> {
    let notification = JsonRpcNotification::new("notifications/tools/list_changed");
    let serialized = serde_json::to_string(&notification)?;
    writeln!(writer, "{serialized}")?;
    writer.flush()?;
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::EmbeddedBackend;
    #[allow(unused_imports)]
    use crate::protocol::{ToolCallResult, ToolsListResult};

    /// Drive a `Server<EmbeddedBackend>` through a sequence of JSON-RPC frames
    /// supplied via an in-memory channel, and collect the responses.
    ///
    /// This is the high-leverage integration test described in the spec: every
    /// other stream depends on this round-trip working correctly.
    #[tokio::test]
    async fn server_embedded_full_round_trip() {
        let backend = EmbeddedBackend::with_stub_backend(
            "github",
            &[
                ("create_issue", "Create a GitHub issue"),
                ("list_repos", "List repositories"),
                ("search_code", "Search code"),
                ("create_pr", "Create a pull request"),
                ("merge_pr", "Merge a pull request"),
            ],
        );

        let server = Server::new(backend);

        // ── initialize ──────────────────────────────────────────────────────
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let init_resp = server
            .handle_line(init_req)
            .await
            .expect("initialize should produce a response");

        assert!(
            init_resp.error.is_none(),
            "initialize should not error: {:?}",
            init_resp.error
        );
        let result = init_resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "vectorhawkd");

        // ── tools/list ──────────────────────────────────────────────────────
        let list_req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let list_resp = server
            .handle_line(list_req)
            .await
            .expect("tools/list should produce a response");

        assert!(list_resp.error.is_none(), "tools/list should not error");
        let result = list_resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"github__create_issue"),
            "expected github__create_issue in {names:?}"
        );
        assert_eq!(tools.len(), 5, "all five stub tools should appear");

        // ── tools/call × 5 ─────────────────────────────────────────────────
        let tool_names = [
            "github__create_issue",
            "github__list_repos",
            "github__search_code",
            "github__create_pr",
            "github__merge_pr",
        ];
        for (i, tool_name) in tool_names.iter().enumerate() {
            let call_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3 + i,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": {}
                }
            })
            .to_string();

            let call_resp = server
                .handle_line(&call_req)
                .await
                .expect("tools/call should produce a response");

            assert!(
                call_resp.error.is_none(),
                "tools/call should not error for {tool_name}: {:?}",
                call_resp.error
            );
            let result = call_resp.result.unwrap();
            let content = result["content"].as_array().unwrap();
            assert!(
                !content.is_empty(),
                "content should not be empty for {tool_name}"
            );
        }
    }

    #[tokio::test]
    async fn server_returns_method_not_found_for_unknown() {
        let backend = EmbeddedBackend::with_stub_backend("test", &[]);
        let server = Server::new(backend);

        let req = r#"{"jsonrpc":"2.0","id":1,"method":"nonexistent/method","params":{}}"#;
        let resp = server.handle_line(req).await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn server_returns_parse_error_for_invalid_json() {
        let backend = EmbeddedBackend::with_stub_backend("test", &[]);
        let server = Server::new(backend);

        let resp = server.handle_line("this is not json").await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, PARSE_ERROR);
    }

    #[tokio::test]
    async fn server_returns_none_for_notification() {
        let backend = EmbeddedBackend::with_stub_backend("test", &[]);
        let server = Server::new(backend);

        // Notifications have no id
        let notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        let resp = server.handle_line(notif).await;
        assert!(resp.is_none(), "notifications should produce no response");
    }

    #[tokio::test]
    async fn server_invalid_tool_call_params_returns_invalid_params() {
        let backend = EmbeddedBackend::with_stub_backend("test", &[]);
        let server = Server::new(backend);

        // tools/call params must have a "name" field
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"bad":"params"}}"#;
        let resp = server.handle_line(req).await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }
}
