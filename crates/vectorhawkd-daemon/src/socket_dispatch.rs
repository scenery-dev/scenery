//! Per-connection dispatch loop for incoming shim sessions.
//!
//! Each accepted `UnixStream` is handed to `serve_connection`, which reads
//! length-prefixed JSON-RPC frames, dispatches via `RealBackend`, and writes
//! framed responses back.  The loop runs until the peer closes the connection
//! or a fatal I/O error occurs.
//!
//! Frame format (matches `vectorhawkd_mcp::backend`):
//!   4-byte big-endian length | UTF-8 JSON body
//!
//! # spawn_blocking requirement for audit (M1.4)
//!
//! When M1.4 wires audit emission into `RealBackend::call_tool`, the audit
//! `record()` call MUST be wrapped in `tokio::task::spawn_blocking`:
//!
//! ```ignore
//! // WRONG (blocks the current-thread executor):
//! audit_buffer.record(&event)?;
//!
//! // CORRECT:
//! let buf = Arc::clone(&audit_buffer);
//! let event_clone = event.clone();
//! tokio::task::spawn_blocking(move || buf.record(&event_clone)).await??;
//! ```
//!
//! `SqliteAuditBuffer::record` opens a `rusqlite::Connection` synchronously.
//! Running it on the executor thread serializes all concurrent tool calls.

use anyhow::Result;
use std::sync::Arc;
use tokio::net::UnixStream;
use tracing::{debug, error, info, warn};
use vectorhawkd_mcp::{
    backend::{read_framed, write_framed, Backend, RealBackend},
    protocol::{
        JsonRpcRequest, JsonRpcResponse, ToolCallParams, INTERNAL_ERROR, INVALID_PARAMS,
        METHOD_NOT_FOUND, PARSE_ERROR,
    },
};

use crate::{
    auth_dispatch::{handle_get_oauth_listener_port, handle_wait_for_callback},
    oauth_state::OAuthState,
};

/// Shared daemon context passed to every per-connection handler.
///
/// Adding new daemon-wide resources (auth state, rate limiters, etc.) should
/// be done by extending this struct rather than adding more individual
/// parameters to `serve_connection`.
#[derive(Clone)]
pub struct DaemonContext {
    /// Backend registry — tool dispatch.
    pub backend: Arc<RealBackend>,
    /// OAuth notification hub — `auth/wait_for_callback`.
    pub oauth_state: Arc<OAuthState>,
    /// Bound port of the HTTP callback listener, or `None` if it failed to start.
    pub listener_port: Option<u16>,
}

/// Drive a single shim connection to completion.
///
/// Spawned as a Tokio task per connection.  Errors inside the loop are logged
/// and cause the connection to close; they do not propagate to the caller.
pub async fn serve_connection(stream: UnixStream, ctx: DaemonContext) {
    let peer = stream
        .peer_addr()
        .map(|a| format!("{a:?}"))
        .unwrap_or_else(|_| "unknown".to_string());
    info!(peer = %peer, "shim connected");

    if let Err(e) = run_loop(stream, ctx).await {
        debug!(peer = %peer, error = %e, "connection loop ended");
    }

    info!(peer = %peer, "shim disconnected");
}

async fn run_loop(stream: UnixStream, ctx: DaemonContext) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    loop {
        // Read one length-prefixed frame.  Returns None on clean EOF.
        let raw = match read_framed(&mut reader).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                debug!("peer closed connection (EOF)");
                break;
            }
            Err(e) => {
                warn!(error = %e, "read_framed failed");
                break;
            }
        };

        // Parse the JSON-RPC envelope.
        let request: JsonRpcRequest = match serde_json::from_slice(&raw) {
            Ok(r) => r,
            Err(e) => {
                let response =
                    JsonRpcResponse::error(None, PARSE_ERROR, format!("invalid JSON: {e}"));
                send_response(&mut writer, &response).await;
                continue;
            }
        };

        debug!(method = %request.method, id = ?request.id, "dispatching");

        // Notifications (no id) require no response.
        if request.id.is_none() {
            continue;
        }

        let response = dispatch(&ctx, request).await;
        send_response(&mut writer, &response).await;
    }

    Ok(())
}

/// Dispatch one JSON-RPC request and return a response.
pub(crate) async fn dispatch(ctx: &DaemonContext, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => match ctx.backend.initialize(request.params).await {
            Ok(result) => {
                let v = serde_json::to_value(result).unwrap_or_default();
                JsonRpcResponse::success(id, v)
            }
            Err(e) => {
                warn!(error = %e, "initialize failed");
                JsonRpcResponse::error(id, INTERNAL_ERROR, format!("{e}"))
            }
        },

        "tools/list" => match ctx.backend.list_tools(request.params).await {
            Ok(result) => {
                let v = serde_json::to_value(result).unwrap_or_default();
                JsonRpcResponse::success(id, v)
            }
            Err(e) => {
                warn!(error = %e, "tools/list failed");
                JsonRpcResponse::error(id, INTERNAL_ERROR, format!("{e}"))
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
            match ctx.backend.call_tool(params).await {
                Ok(result) => {
                    let v = serde_json::to_value(result).unwrap_or_default();
                    JsonRpcResponse::success(id, v)
                }
                Err(e) => {
                    warn!(error = %e, "tools/call failed");
                    JsonRpcResponse::error(id, INTERNAL_ERROR, format!("{e}"))
                }
            }
        }

        "auth/get_oauth_listener_port" => {
            handle_get_oauth_listener_port(id, ctx.listener_port).await
        }

        "auth/wait_for_callback" => {
            handle_wait_for_callback(id, request.params, Arc::clone(&ctx.oauth_state)).await
        }

        other => {
            debug!(method = %other, "unknown method");
            JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("unknown method: {other}"))
        }
    }
}

/// Serialize a response and write it as a length-prefixed frame.
async fn send_response<W>(writer: &mut W, response: &JsonRpcResponse)
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let body = match serde_json::to_vec(response) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "failed to serialize JSON-RPC response");
            return;
        }
    };
    if let Err(e) = write_framed(writer, &body).await {
        debug!(error = %e, "failed to write response frame");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "socket_dispatch_tests.rs"]
mod tests;
