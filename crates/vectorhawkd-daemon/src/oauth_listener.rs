//! Fixed-port HTTP listener that receives OAuth redirect callbacks from the browser.
//!
//! Binds `127.0.0.1:39127` (or the first free port in `39127..=39136`).
//! Exposes a single route: `GET /oauth/cli/callback`.
//!
//! On a valid callback the handler calls `OAuthState::notify`, which wakes any
//! CLI that is waiting in `auth/wait_for_callback` over the Unix socket.
//!
//! # Port selection
//!
//! The port range `[39127, 39137)` mirrors what the registry validates as an
//! acceptable `redirect_uri`.  If all 10 ports are in use the function returns
//! `Ok(None)` and the daemon continues without a listener — `auth login` will
//! report an error when it calls `auth/get_oauth_listener_port`.
//!
//! # Security
//!
//! Binding to `127.0.0.1` (not `0.0.0.0`) ensures only processes on the same
//! host can reach this endpoint.  On macOS and Linux the loopback interface is
//! per-user-session, providing an adequate security boundary for the OAuth
//! callback flow.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::{info, warn};

use crate::oauth_state::OAuthState;

/// First port to try when binding the OAuth callback listener.
const OAUTH_PORT_BASE: u16 = 39127;
/// Number of ports to try before giving up.
const OAUTH_PORT_RANGE: u16 = 10;

/// Query parameters expected on `GET /oauth/cli/callback`.
#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Shared state threaded through the axum handlers.
#[derive(Clone)]
struct ListenerState {
    oauth_state: Arc<OAuthState>,
}

/// Handler for `GET /oauth/cli/callback`.
async fn handle_callback(
    State(app): State<ListenerState>,
    Query(params): Query<CallbackParams>,
) -> Response {
    // Surface OAuth-level errors (e.g. access_denied) to the user with a
    // clear HTML page; still return 200 so the browser renders nicely.
    if let Some(err) = params.error {
        warn!(oauth_error = %err, "OAuth callback received an error response");
        return Html(build_error_page(&err)).into_response();
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing required query parameter: code",
            )
                .into_response();
        }
    };

    let state = match params.state {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing required query parameter: state",
            )
                .into_response();
        }
    };

    // Notify the waiting CLI subscriber.  If there is no subscriber (orphaned
    // callback), log a warning but still return 200 to the browser.
    if let Err(e) = app.oauth_state.notify(state, code).await {
        warn!(error = %e, "callback received but no CLI subscriber was waiting");
    }

    Html(build_success_page()).into_response()
}

/// Attempt to bind the listener on the first available port in the range.
///
/// Returns `(SocketAddr, JoinHandle<()>)` where `SocketAddr` is the actual
/// address the listener is bound to.  Returns `Ok(None)` if all ports in the
/// range are already in use — the daemon stays up, but OAuth login will fail.
pub async fn start_listener(
    oauth_state: Arc<OAuthState>,
) -> Result<Option<(SocketAddr, JoinHandle<()>)>> {
    let listener_state = ListenerState {
        oauth_state: Arc::clone(&oauth_state),
    };

    let app = Router::new()
        .route("/oauth/cli/callback", get(handle_callback))
        .with_state(listener_state);

    for offset in 0..OAUTH_PORT_RANGE {
        let port = OAUTH_PORT_BASE + offset;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();

        match TcpListener::bind(addr).await {
            Ok(tcp_listener) => {
                let bound_addr = tcp_listener
                    .local_addr()
                    .context("failed to get local address after bind")?;

                info!(port = bound_addr.port(), "OAuth callback listener bound");

                let handle = tokio::spawn(async move {
                    if let Err(e) = axum::serve(tcp_listener, app).await {
                        warn!(error = %e, "OAuth callback listener exited with error");
                    }
                });

                return Ok(Some((bound_addr, handle)));
            }
            Err(e) if is_addr_in_use(&e) => {
                // Port in use — try next one.
                continue;
            }
            Err(e) => {
                // Unexpected bind error — surface it.
                return Err(e).with_context(|| format!("failed to bind OAuth listener on {addr}"));
            }
        }
    }

    warn!(
        base_port = OAUTH_PORT_BASE,
        range = OAUTH_PORT_RANGE,
        "all OAuth callback ports are in use — auth login will not work"
    );
    Ok(None)
}

/// Returns `true` if the I/O error indicates the address is already in use.
fn is_addr_in_use(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AddrInUse
}

/// HTML page returned on a successful OAuth callback.
fn build_success_page() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><title>VectorHawk Login</title></head>
<body>
<h2>VectorHawk login complete.</h2>
<p>You can close this window.</p>
<script>window.close();</script>
</body>
</html>"#
        .to_string()
}

/// HTML page shown when the authorization server returns an error.
fn build_error_page(error: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><title>VectorHawk Login Failed</title></head>
<body>
<h2>VectorHawk login failed.</h2>
<p>The authorization server returned an error: <code>{error}</code></p>
<p>You can close this window and try again.</p>
</body>
</html>"#
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "oauth_listener_tests.rs"]
mod tests;
