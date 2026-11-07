//! MCP streamable-HTTP session lifecycle: the `initialize` handshake.
//!
//! Per the spec a client must send `initialize`, read the `InitializeResult`,
//! then send `notifications/initialized` before any other request. The server
//! may assign an `Mcp-Session-Id` on the initialize response; every later
//! request must echo it and carry `MCP-Protocol-Version: <negotiated>`.
//!
//! State is kept per endpoint URL behind an async mutex. Holding that mutex
//! across the handshake is what makes concurrent first callers share a single
//! `initialize` instead of racing. A failed handshake is remembered for
//! [`INIT_FAILURE_COOLDOWN`] so a broken backend is not hammered by every
//! tool call or sync tick.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::http_rpc;

/// Protocol revision the runner offers in `initialize`. 2025-11-25 is the
/// newest published MCP revision known to this codebase. Like the official
/// SDKs we offer our latest and then adopt whatever the server answers with:
/// the runner only uses `tools/list` and `tools/call`, whose shape is stable
/// across 2024-11-05 through 2025-11-25, so any server-chosen revision is fine.
pub const OFFERED_PROTOCOL_VERSION: &str = "2025-11-25";

/// Header carrying the negotiated revision on every post-initialize request.
pub const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// How long a failed handshake is remembered before we try again.
pub const INIT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// Negotiated state for one endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub protocol_version: String,
    /// `None` for stateless servers that never assign a session id.
    pub session_id: Option<String>,
}

#[derive(Debug)]
enum Slot {
    Empty,
    Ready(Session),
    Failed { at: Instant, message: String },
}

type SlotHandle = Arc<tokio::sync::Mutex<Slot>>;

/// Per-endpoint session state, shared across registry clones.
#[derive(Clone)]
pub struct SessionStore {
    slots: Arc<Mutex<HashMap<String, SlotHandle>>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn slot(&self, url: &str) -> SlotHandle {
        // A poisoned map still holds valid handles; recover rather than panic.
        let mut map = match self.slots.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.entry(url.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Slot::Empty)))
            .clone()
    }

    /// Return the session for `url`, running the handshake if there is none.
    pub async fn ensure(
        &self,
        client: &reqwest::Client,
        url: &str,
        auth_token: Option<&str>,
    ) -> Result<Session> {
        let slot = self.slot(url);
        let mut guard = slot.lock().await;
        match &*guard {
            Slot::Ready(session) => return Ok(session.clone()),
            Slot::Failed { at, message } if at.elapsed() < INIT_FAILURE_COOLDOWN => {
                anyhow::bail!(
                    "{message} (not retrying for {}s)",
                    INIT_FAILURE_COOLDOWN.as_secs()
                );
            }
            Slot::Failed { .. } | Slot::Empty => {}
        }
        match handshake(client, url, auth_token).await {
            Ok(session) => {
                *guard = Slot::Ready(session.clone());
                Ok(session)
            }
            Err(e) => {
                let message = format!("MCP initialize failed for {url}: {e:#}");
                *guard = Slot::Failed {
                    at: Instant::now(),
                    message: message.clone(),
                };
                anyhow::bail!(message)
            }
        }
    }

    /// Drop `stale` (after a 404) unless another caller already replaced it.
    pub async fn invalidate(&self, url: &str, stale: &Session) {
        let slot = self.slot(url);
        let mut guard = slot.lock().await;
        if matches!(&*guard, Slot::Ready(current) if current == stale) {
            *guard = Slot::Empty;
        }
    }
}

/// Run `initialize` + `notifications/initialized` against `url`.
async fn handshake(
    client: &reqwest::Client,
    url: &str,
    auth_token: Option<&str>,
) -> Result<Session> {
    let params = json!({
        "protocolVersion": OFFERED_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "scenery", "version": env!("CARGO_PKG_VERSION") },
    });
    let reply = http_rpc::send_request(client, url, auth_token, None, "initialize", &params)
        .await
        .map_err(|e| e.into_anyhow())?;
    if let Some(err) = reply.body.get("error") {
        anyhow::bail!("server rejected initialize: {err}");
    }
    let result = reply
        .body
        .get("result")
        .context("initialize response had no result")?;
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(OFFERED_PROTOCOL_VERSION)
        .to_string();
    let session = Session {
        protocol_version,
        session_id: reply.session_id,
    };
    debug!(url, version = %session.protocol_version, "MCP initialize ok");

    // The spec says the server answers 202 with no body. A server that fails
    // here will reject the next request with a clearer error, so do not abort.
    if let Err(e) = http_rpc::send_notification(
        client,
        url,
        auth_token,
        &session,
        "notifications/initialized",
    )
    .await
    {
        warn!(url, error = %format!("{e:#}"), "notifications/initialized failed; continuing");
    }
    Ok(session)
}
