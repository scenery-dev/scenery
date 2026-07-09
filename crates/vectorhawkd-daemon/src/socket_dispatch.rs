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
//!
//! # `notifications/tools/list_changed` (GAP-03)
//!
//! After any `tools/call` whose tool name is one of the mutating management
//! tools (`vectorhawk_install`, `vectorhawk_uninstall`, `vectorhawk_update`,
//! `vectorhawk_import`, `vectorhawk_mcp_install`, `vectorhawk_mcp_uninstall`),
//! the dispatch loop:
//!
//! 1. Writes a framed `notifications/tools/list_changed` frame to **this**
//!    connection's writer immediately after the response.
//! 2. Fires `DaemonContext::list_changed_tx` so that every other currently-
//!    connected shim connection also writes the same notification.
//!
//! Each `serve_connection` task subscribes to `list_changed_tx` at start via
//! a `broadcast::Receiver<()>`.  A dedicated sub-task inside `run_loop` polls
//! the receiver and writes the notification frame whenever the channel fires.

use anyhow::Result;
use std::sync::Arc;
use tokio::{
    net::UnixStream,
    sync::{broadcast, Notify},
};
use tracing::{debug, error, info, warn};
use vectorhawkd_mcp::{
    backend::{read_framed, write_framed, Backend, RealBackend},
    protocol::{
        JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ToolCallParams, INTERNAL_ERROR,
        INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR,
    },
};

use crate::{
    auth_dispatch::{handle_get_oauth_listener_port, handle_reload, handle_wait_for_callback},
    oauth_state::OAuthState,
};

/// Tool names whose successful dispatch should trigger `list_changed`.
const MUTATING_TOOLS: &[&str] = &[
    "vectorhawk_install",
    "vectorhawk_uninstall",
    "vectorhawk_update",
    "vectorhawk_import",
    "vectorhawk_mcp_install",
    "vectorhawk_mcp_uninstall",
];

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
    /// Broadcast channel used to notify **all** connected shims that the tool
    /// set has changed.  Fired after any mutating management tool call and after
    /// successful background sync ticks.  Each `serve_connection` task
    /// subscribes at start; sending `()` triggers every subscriber to write a
    /// `notifications/tools/list_changed` frame to its client.
    pub list_changed_tx: broadcast::Sender<()>,
    /// Kick signal for the discoveries scanner. Fired when an MCP client
    /// completes the `initialize` handshake so the scanner runs immediately on
    /// first connect rather than waiting for the next 5-minute tick.
    /// Debounced inside `DiscoveriesScanner::spawn_loop` (10 s window).
    pub discoveries_kick: Arc<Notify>,
    /// Controller for the SSE sync subsystem. Lets `auth/reload` (re)start sync
    /// after the user authenticates against an already-running daemon, without
    /// requiring a daemon restart.
    pub sync_controller: Arc<crate::SyncController>,
    /// One-shot adoption alert delivered to the first connecting client as an
    /// MCP `notifications/message`. Set once at startup by the F1 reconciler
    /// when it auto-adopts tools (suppressed in headless mode). `take()`n by the
    /// first connection so the user is told, in-client, that tools were adopted.
    pub pending_alert: Arc<std::sync::Mutex<Option<String>>>,
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

    // Subscribe to the list_changed broadcast before entering the loop so we
    // don't miss any notifications fired during this session.
    let list_changed_rx = ctx.list_changed_tx.subscribe();

    if let Err(e) = run_loop(stream, ctx, list_changed_rx).await {
        debug!(peer = %peer, error = %e, "connection loop ended");
    }

    info!(peer = %peer, "shim disconnected");
}

async fn run_loop(
    stream: UnixStream,
    ctx: DaemonContext,
    mut list_changed_rx: broadcast::Receiver<()>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let (mut reader, mut writer) = stream.into_split();

    // Deliver a one-shot adoption alert to this client, if one is pending.
    // Taken so exactly one connection surfaces it; the portal banner is the
    // durable record for other clients / later sessions.
    let pending = ctx.pending_alert.lock().ok().and_then(|mut g| g.take());
    if let Some(message) = pending {
        send_alert_frame(&mut writer, &message).await;
    }

    loop {
        tokio::select! {
            // ── Incoming request from shim ─────────────────────────────────
            read_result = read_framed(&mut reader) => {
                let raw = match read_result {
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

                // Snapshot the tool name before moving `request` into dispatch.
                let is_mutating = request.method == "tools/call"
                    && serde_json::from_value::<ToolCallParams>(request.params.clone())
                        .map(|p| MUTATING_TOOLS.contains(&p.name.as_str()))
                        .unwrap_or(false);

                let response = dispatch(&ctx, request).await;
                send_response(&mut writer, &response).await;

                // After a successful (non-JSON-RPC-error) mutating tool call,
                // send list_changed to this connection and broadcast to others.
                if is_mutating && response.error.is_none() {
                    send_list_changed_frame(&mut writer).await;
                    // Fire the broadcast so all other shims are notified.
                    // Ignore errors: lagging receivers are dropped automatically.
                    let _ = ctx.list_changed_tx.send(());
                }
            }

            // ── Broadcast notification from another connection or sync tick ─
            broadcast_result = list_changed_rx.recv() => {
                match broadcast_result {
                    Ok(()) => {
                        send_list_changed_frame(&mut writer).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Fell behind — drain and send one notification.
                        debug!(skipped = n, "list_changed broadcast: receiver lagged, coalescing");
                        send_list_changed_frame(&mut writer).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Sender dropped — daemon is shutting down. Exit cleanly.
                        debug!("list_changed broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }

    // Flush any pending writes before dropping.
    let _ = writer.flush().await;
    Ok(())
}

/// Dispatch one JSON-RPC request and return a response.
pub(crate) async fn dispatch(ctx: &DaemonContext, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => match ctx.backend.initialize(request.params).await {
            Ok(result) => {
                info!("discoveries: shim initialize → kicking scan");
                ctx.discoveries_kick.notify_one();
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

        "auth/reload" => handle_reload(id, Arc::clone(&ctx.sync_controller)).await,

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

/// Write a `notifications/tools/list_changed` JSON-RPC notification as a
/// length-prefixed frame to the shim connection.
///
/// Failures are logged at DEBUG level and do not abort the connection loop —
/// the worst outcome is a stale tool list on the AI client side, which is
/// correctable by the user reconnecting.
async fn send_list_changed_frame<W>(writer: &mut W)
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let notification = JsonRpcNotification::new("notifications/tools/list_changed");
    let body = match serde_json::to_vec(&notification) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "failed to serialize list_changed notification");
            return;
        }
    };
    if let Err(e) = write_framed(writer, &body).await {
        debug!(error = %e, "failed to write list_changed notification frame");
    }
}

/// Send an MCP `notifications/message` (logging notification) carrying a
/// human-readable adoption alert. AI clients surface these to the user.
async fn send_alert_frame<W>(writer: &mut W, message: &str)
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": "info", "logger": "vectorhawk", "data": message },
    });
    let body = match serde_json::to_vec(&notification) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "failed to serialize adoption alert notification");
            return;
        }
    };
    if let Err(e) = write_framed(writer, &body).await {
        debug!(error = %e, "failed to write adoption alert frame");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "socket_dispatch_tests.rs"]
mod tests;
