//! JSON-RPC dispatch handlers for `auth/*` methods.
//!
//! These methods are called by `vectorhawk auth login` (M3.3) over the existing
//! Unix socket.  They live in a separate module to keep `socket_dispatch.rs`
//! focused on the MCP tool-call path.
//!
//! # Methods
//!
//! | Method | Params | Returns |
//! |--------|--------|---------|
//! | `auth/get_oauth_listener_port` | `{}` | `{"port": <u16>}` |
//! | `auth/wait_for_callback` | `{"state": str, "timeout_secs": u64}` | `{"code": str}` |
//!
//! # Timeout semantics
//!
//! `timeout_secs` must satisfy `1 <= value <= 600`.  Values outside this range
//! are rejected with `INVALID_PARAMS`.  The default when the field is absent is
//! 300 seconds.

use std::sync::Arc;

use serde::Deserialize;
use tracing::debug;
use vectorhawkd_mcp::protocol::{JsonRpcError, JsonRpcResponse, INTERNAL_ERROR, INVALID_PARAMS};

use crate::oauth_state::OAuthState;

/// Minimum acceptable `timeout_secs` value.
const TIMEOUT_SECS_MIN: u64 = 1;
/// Maximum acceptable `timeout_secs` value.
const TIMEOUT_SECS_MAX: u64 = 600;
/// Default `timeout_secs` when the field is absent.
const TIMEOUT_SECS_DEFAULT: u64 = 300;

/// Params for `auth/wait_for_callback`.
#[derive(Debug, Deserialize)]
struct WaitForCallbackParams {
    state: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    TIMEOUT_SECS_DEFAULT
}

/// Handle `auth/get_oauth_listener_port`.
///
/// Returns `{"port": <u16>}` when the listener is running, or a JSON-RPC
/// error when the listener failed to bind at daemon startup.
pub async fn handle_get_oauth_listener_port(
    id: Option<serde_json::Value>,
    listener_port: Option<u16>,
) -> JsonRpcResponse {
    match listener_port {
        Some(port) => {
            debug!(port, "auth/get_oauth_listener_port requested");
            JsonRpcResponse::success(id, serde_json::json!({ "port": port }))
        }
        None => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: INTERNAL_ERROR,
                message: "OAuth callback listener is not running — all ports in 39127..=39136 were in use at daemon startup".to_string(),
                data: None,
            }),
        },
    }
}

/// Handle `auth/wait_for_callback`.
///
/// Subscribes to `OAuthState` for the given `state` value and awaits
/// notification from the HTTP listener, subject to `timeout_secs`.
pub async fn handle_wait_for_callback(
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    oauth_state: Arc<OAuthState>,
) -> JsonRpcResponse {
    let parsed: WaitForCallbackParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                format!("invalid params for auth/wait_for_callback: {e}"),
            );
        }
    };

    if parsed.timeout_secs < TIMEOUT_SECS_MIN || parsed.timeout_secs > TIMEOUT_SECS_MAX {
        return JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            format!(
                "timeout_secs must be between {TIMEOUT_SECS_MIN} and {TIMEOUT_SECS_MAX}; got {}",
                parsed.timeout_secs
            ),
        );
    }

    let rx = match oauth_state.subscribe(parsed.state.clone()).await {
        Ok(rx) => rx,
        Err(e) => {
            return JsonRpcResponse::error(id, INVALID_PARAMS, format!("{e}"));
        }
    };

    let duration = std::time::Duration::from_secs(parsed.timeout_secs);

    match tokio::time::timeout(duration, rx).await {
        Ok(Ok((code, _state))) => {
            debug!(state = %parsed.state, "auth/wait_for_callback delivered code");
            JsonRpcResponse::success(id, serde_json::json!({ "code": code }))
        }
        Ok(Err(_recv_err)) => {
            // Channel closed — daemon is shutting down.
            JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                "daemon is shutting down — auth/wait_for_callback aborted".to_string(),
            )
        }
        Err(_elapsed) => JsonRpcResponse::error(
            id,
            INTERNAL_ERROR,
            format!(
                "auth/wait_for_callback timed out after {} s — no browser callback received",
                parsed.timeout_secs
            ),
        ),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "auth_dispatch_tests.rs"]
mod tests;
