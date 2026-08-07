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
//! | `auth/reload` | `{}` | `{"sync_active": bool}` |
//! | `auth/get_portal_session` | `{}` | `{"access_token": str, "refresh_token": str, "auth_scope": "portal", "user": {"id": str, "email": str, "display_name": str}}` |
//!
//! # Timeout semantics
//!
//! `timeout_secs` must satisfy `1 <= value <= 600`.  Values outside this range
//! are rejected with `INVALID_PARAMS`.  The default when the field is absent is
//! 300 seconds.

use std::sync::Arc;

use serde::Deserialize;
use tracing::debug;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::protocol::{JsonRpcError, JsonRpcResponse, INTERNAL_ERROR, INVALID_PARAMS};

use crate::oauth_state::OAuthState;
use crate::SyncController;

/// Minimum acceptable `timeout_secs` value.
const TIMEOUT_SECS_MIN: u64 = 1;
/// Maximum acceptable `timeout_secs` value.
const TIMEOUT_SECS_MAX: u64 = 600;
/// Default `timeout_secs` when the field is absent.
const TIMEOUT_SECS_DEFAULT: u64 = 300;

/// `auth/get_portal_session` error code for "daemon is reachable but has no
/// stored auth session." Deliberately distinct from `DAEMON_UNREACHABLE`
/// (`-32001`, defined in `vectorhawkd-shim/src/lib.rs`), which the shim uses
/// when it cannot reach the daemon *process* at all — an unrelated failure
/// mode. Reusing `-32001` here would let a client conflate "no daemon" with
/// "daemon is up but you're logged out."
const NOT_AUTHENTICATED: i64 = -32002;

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

/// Handle `auth/reload`.
///
/// Called by `vectorhawk auth login` / `auth pair` / `auth token` right after
/// saving new credentials.  Idempotently registers this device and starts the
/// SSE sync subsystem so freshly-authenticated daemons begin syncing without a
/// restart.  Returns `{"sync_active": bool}` — `false` means credentials are
/// still missing/invalid or device registration failed.
pub async fn handle_reload(
    id: Option<serde_json::Value>,
    sync_controller: Arc<SyncController>,
) -> JsonRpcResponse {
    let active = sync_controller.ensure_started().await;
    debug!(sync_active = active, "auth/reload processed");
    JsonRpcResponse::success(id, serde_json::json!({ "sync_active": active }))
}

/// Handle `auth/get_portal_session`.
///
/// Reuses `vectorhawkd_core::auth::load_tokens` (Keychain-vs-SQLite lookup
/// already handled there) and `AuthClient::me()` to hand the desktop app the
/// daemon's already-authenticated session — access token, refresh token, and
/// user info — so it can open a portal WebView pre-authenticated without
/// reaching into the daemon's storage directly.
///
/// `"auth_scope": "portal"` is correct (not a placeholder): the backend mints
/// CLI-flow tokens with `scope="portal"` (see `portal_auth.py`'s `cli_token`
/// handler), identical to `portal_login`. The frontend's `AuthContext` only
/// accepts `"admin" | "portal"` for `auth_scope`; CLI-issued tokens are
/// already portal-scoped, so no backend changes are needed here.
pub async fn handle_get_portal_session(
    id: Option<serde_json::Value>,
    state: &AppState,
    registry_url: &str,
) -> JsonRpcResponse {
    let state = state.clone();
    let registry_url = registry_url.to_string();

    let stored = tokio::task::spawn_blocking(move || {
        vectorhawkd_core::auth::load_tokens(&state, &registry_url)
    })
    .await;

    let stored = match stored {
        Ok(Ok(Some(tokens))) => tokens,
        Ok(Ok(None)) => {
            return JsonRpcResponse::error(id, NOT_AUTHENTICATED, "not authenticated".to_string());
        }
        Ok(Err(e)) => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("failed to load tokens: {e}"),
            );
        }
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("token lookup task panicked: {e}"),
            );
        }
    };

    let access_token = stored.access_token.clone();
    let registry_url = stored.registry_url.clone();
    // AuthClient::new builds a `reqwest::blocking::Client`, which internally
    // drives its own single-threaded runtime — constructing (or dropping) one
    // on the async executor thread panics ("cannot drop a runtime from within
    // a runtime"). Build and use it entirely inside spawn_blocking so its
    // whole lifecycle stays on the blocking thread pool.
    let user = tokio::task::spawn_blocking(move || {
        let client = vectorhawkd_core::auth::AuthClient::new(&registry_url);
        client.me(&access_token)
    })
    .await;

    let user = match user {
        Ok(Ok(user)) => user,
        Ok(Err(e)) => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("failed to fetch user info: {e}"),
            );
        }
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("user info task panicked: {e}"),
            );
        }
    };

    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "access_token": stored.access_token,
            "refresh_token": stored.refresh_token,
            "auth_scope": "portal",
            "user": {
                "id": user.id,
                "email": user.email,
                "display_name": user.display_name,
            },
        }),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "auth_dispatch_tests.rs"]
mod tests;
