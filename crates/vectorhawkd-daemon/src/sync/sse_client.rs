//! Persistent SSE client for the backend `/api/sync/events` stream.
//!
//! # Lifecycle
//!
//! [`run`] loops forever:
//! 1. Acquires the current JWT from the daemon's token store.
//! 2. Opens an HTTP GET to `{registry_url}/api/sync/events` with auth headers.
//! 3. Streams lines, parses SSE events per RFC 8607.
//! 4. On each complete event: sends a [`SyncEvent`] to the reconciler and
//!    persists `last_event_id` to SQLite.
//! 5. On EOF, error, or watchdog timeout: exponential backoff (1s → 60s max),
//!    then reconnect.
//! 6. On HTTP 401: attempts token refresh via a new `AuthClient` call, then
//!    reconnects immediately.
//!
//! # Watchdog
//!
//! If no SSE line (including `: ping` keep-alive comments) is received for 60
//! seconds, the connection is treated as stale and rebuilt.
//!
//! # Backoff
//!
//! Reconnect delays: 1 s → 2 s → 4 s → 8 s → 16 s → 32 s → 60 s (capped).
//! A successful connection resets the backoff counter.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::sync::SyncConfig;
use vectorhawkd_core::{
    auth::{load_all_tokens, save_tokens, AuthClient},
    state::AppState,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// How long to wait with no data before treating the connection as dead.
const WATCHDOG_SECS: u64 = 60;

/// Starting reconnect delay.
const BACKOFF_INIT_SECS: u64 = 1;

/// Maximum reconnect delay.
const BACKOFF_MAX_SECS: u64 = 60;

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the SSE client loop (never returns unless the channel is closed).
pub async fn run(config: SyncConfig, state: Arc<AppState>, tx: mpsc::Sender<SyncEvent>) {
    let mut backoff_secs = BACKOFF_INIT_SECS;
    let mut last_event_id = config.last_event_id.clone();
    let mut current_token = config.token.clone();

    loop {
        match connect_and_stream(
            &config.registry_url,
            &current_token,
            &config.device_id,
            last_event_id.clone(),
            Arc::clone(&state),
            &tx,
        )
        .await
        {
            ConnectResult::Reconnect { new_last_id } => {
                // Clean reconnect (EOF or watchdog).  Reset backoff.
                backoff_secs = BACKOFF_INIT_SECS;
                if let Some(id) = new_last_id {
                    last_event_id = Some(id);
                }
            }
            ConnectResult::Unauthorized => {
                // 401: try to refresh the JWT before reconnecting.
                info!("SSE: received 401 — attempting token refresh");
                match try_refresh_token(&config.registry_url, Arc::clone(&state)).await {
                    Ok(new_token) => {
                        current_token = new_token;
                        info!("SSE: token refreshed — reconnecting immediately");
                        backoff_secs = BACKOFF_INIT_SECS;
                        continue; // no delay
                    }
                    Err(e) => {
                        warn!(error = %e, "SSE: token refresh failed — backing off");
                    }
                }
            }
            ConnectResult::ChannelClosed => {
                info!("SSE: reconciler channel closed — stopping SSE client");
                return;
            }
            ConnectResult::Error(e) => {
                warn!(error = %e, backoff_secs, "SSE: connection error — backing off");
            }
        }

        // Wait before reconnecting.
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
    }
}

// ── Connection result ─────────────────────────────────────────────────────────

enum ConnectResult {
    /// Clean disconnect (EOF or watchdog).  Carry forward last_event_id.
    Reconnect { new_last_id: Option<String> },
    /// Server returned 401 — caller should refresh token then reconnect.
    Unauthorized,
    /// Downstream channel closed — caller should stop the loop.
    ChannelClosed,
    /// Any other connection or I/O error.
    Error(anyhow::Error),
}

// ── SSE stream ────────────────────────────────────────────────────────────────

/// Open one SSE connection and stream events until EOF, watchdog, or error.
async fn connect_and_stream(
    registry_url: &str,
    token: &str,
    device_id: &str,
    last_event_id: Option<String>,
    state: Arc<AppState>,
    tx: &mpsc::Sender<SyncEvent>,
) -> ConnectResult {
    let url = format!("{}/api/sync/events", registry_url.trim_end_matches('/'));
    debug!(url, device_id, "SSE: opening connection");

    // Build an async reqwest client (not the blocking one used elsewhere).
    // Do NOT set a request timeout — SSE streams are long-lived. Only the
    // connect_timeout is set to avoid hanging indefinitely on unreachable hosts.
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ConnectResult::Error(anyhow::anyhow!("SSE: failed to build HTTP client: {e}"))
        }
    };

    let mut req = client
        .get(&url)
        .bearer_auth(token)
        .header("X-Device-ID", device_id)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache");

    if let Some(ref id) = last_event_id {
        req = req.header("Last-Event-ID", id.as_str());
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return ConnectResult::Error(anyhow::anyhow!("SSE: HTTP request failed: {e}")),
    };

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return ConnectResult::Unauthorized;
    }

    if !resp.status().is_success() {
        return ConnectResult::Error(anyhow::anyhow!(
            "SSE: server returned HTTP {}",
            resp.status()
        ));
    }

    info!(device_id, "SSE: connected to {url}");

    // Stream lines.
    let mut new_last_id: Option<String> = last_event_id;
    let stream_result = stream_events(resp, state, tx, &mut new_last_id).await;

    match stream_result {
        Ok(StreamEnd::Eof) | Ok(StreamEnd::Watchdog) => {
            info!("SSE: stream ended — will reconnect");
            ConnectResult::Reconnect { new_last_id }
        }
        Ok(StreamEnd::ChannelClosed) => ConnectResult::ChannelClosed,
        Err(e) => ConnectResult::Error(e),
    }
}

// ── Line streaming and SSE parsing ───────────────────────────────────────────

enum StreamEnd {
    Eof,
    Watchdog,
    ChannelClosed,
}

/// Stream bytes from the SSE response, parse events, and send to the reconciler.
///
/// Collects SSE fields across newlines and dispatches a complete event when a
/// blank line is encountered (per RFC 8607 §9.2.6).
async fn stream_events(
    resp: reqwest::Response,
    state: Arc<AppState>,
    tx: &mpsc::Sender<SyncEvent>,
    last_event_id: &mut Option<String>,
) -> Result<StreamEnd> {
    use futures::StreamExt;

    let mut byte_stream = resp.bytes_stream();

    // Buffer for accumulated bytes (may span multiple chunks).
    let mut line_buf = String::new();
    // Current event fields.
    let mut event_type = String::new();
    let mut event_data = String::new();
    let mut event_id: Option<String> = None;

    let watchdog_duration = Duration::from_secs(WATCHDOG_SECS);
    let watchdog = tokio::time::sleep(watchdog_duration);
    // Pin the sleep future so we can reset it.
    tokio::pin!(watchdog);

    loop {
        tokio::select! {
            chunk = byte_stream.next() => {
                // Reset the watchdog whenever data arrives.
                watchdog.as_mut().reset(Instant::now() + watchdog_duration);

                let bytes = match chunk {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => return Err(anyhow::anyhow!("SSE: stream read error: {e}")),
                    None => return Ok(StreamEnd::Eof),
                };

                // Append bytes to line buffer and process complete lines.
                let text = std::str::from_utf8(&bytes)
                    .context("SSE: non-UTF-8 bytes in stream")?;
                line_buf.push_str(text);

                // Process all complete lines (terminated by \n).
                while let Some(pos) = line_buf.find('\n') {
                    let line: String = line_buf.drain(..=pos).collect();
                    let line = line.trim_end_matches('\n').trim_end_matches('\r');

                    let end = process_sse_line(
                        line,
                        &mut event_type,
                        &mut event_data,
                        &mut event_id,
                        last_event_id,
                        &state,
                        tx,
                    ).await?;
                    if let Some(e) = end {
                        return Ok(e);
                    }
                }
            }
            _ = &mut watchdog => {
                warn!("SSE: watchdog triggered — no data for {WATCHDOG_SECS}s, reconnecting");
                return Ok(StreamEnd::Watchdog);
            }
        }
    }
}

/// Process one SSE line.  Returns `Some(StreamEnd)` if the loop should stop.
async fn process_sse_line(
    line: &str,
    event_type: &mut String,
    event_data: &mut String,
    event_id: &mut Option<String>,
    last_event_id: &mut Option<String>,
    state: &AppState,
    tx: &mpsc::Sender<SyncEvent>,
) -> Result<Option<StreamEnd>> {
    // Blank line = dispatch event.
    if line.is_empty() {
        if !event_data.is_empty() {
            let dispatch_result =
                dispatch_event(event_type, event_data, event_id, last_event_id, state, tx).await?;
            if dispatch_result {
                return Ok(Some(StreamEnd::ChannelClosed));
            }
        }
        // Reset for next event.
        event_type.clear();
        event_data.clear();
        *event_id = None;
        return Ok(None);
    }

    // Comment line (keep-alive pings, etc.) — no action.
    if line.starts_with(':') {
        debug!("SSE: comment: {line}");
        return Ok(None);
    }

    // Field lines.
    if let Some(value) = line.strip_prefix("event:") {
        *event_type = value.trim_start().to_string();
    } else if let Some(value) = line.strip_prefix("data:") {
        if !event_data.is_empty() {
            event_data.push('\n');
        }
        event_data.push_str(value.trim_start());
    } else if let Some(value) = line.strip_prefix("id:") {
        *event_id = Some(value.trim_start().to_string());
    }
    // `retry:` field ignored (we control backoff ourselves).

    Ok(None)
}

/// Parse and dispatch one complete SSE event.
///
/// Returns `true` if the downstream channel is closed (caller should stop).
async fn dispatch_event(
    event_type: &str,
    event_data: &str,
    event_id: &Option<String>,
    last_event_id: &mut Option<String>,
    state: &AppState,
    tx: &mpsc::Sender<SyncEvent>,
) -> Result<bool> {
    debug!(event_type, "SSE: dispatching event");

    // F4: persist managed_paths mode before parse so AppState is in scope.
    // parse_sync_event is a pure parser and does not have access to AppState.
    if event_type == "managed_paths_policy_update" {
        #[derive(serde::Deserialize)]
        struct ModeOnly {
            mode: String,
        }
        if let Ok(w) = serde_json::from_str::<ModeOnly>(event_data) {
            if let Err(e) = state.set_sync_state("managed_paths_mode", &w.mode) {
                warn!(error = %e, mode = %w.mode, "managed_paths: failed to persist mode to sync_state");
            } else {
                debug!(mode = %w.mode, "managed_paths: mode persisted to sync_state");
            }
        }
    }

    // F3: admin resolved a drift event. Apply the resolution locally and ack
    // the backend. Spawned as its own task so SSE dispatch isn't blocked on
    // the disk + HTTP round-trip.
    // T2 follow-up (v1.0.54): user adopted a discovery in the portal. The
    // install row is already created server-side; the daemon now needs to
    // copy `source_path` into `~/.claude/skills/<slug>/` and write the F2
    // marker so Claude Code can actually see the skill. Spawned as its own
    // task so SSE dispatch isn't blocked on disk I/O.
    if event_type == "discovery_adopted" {
        #[derive(serde::Deserialize)]
        struct WireAdopted {
            slug: String,
            kind: String,
            source_path: String,
            #[allow(dead_code)]
            canonical_hash: Option<String>,
            #[allow(dead_code)]
            discovery_id: Option<String>,
        }
        match serde_json::from_str::<WireAdopted>(event_data) {
            Ok(w) => {
                let state_arc: Arc<AppState> = Arc::new(state.clone());
                tokio::spawn(async move {
                    if let Err(e) = crate::managed_paths::pusher::push_adopted_discovery(
                        &state_arc,
                        &w.slug,
                        &w.kind,
                        &w.source_path,
                    )
                    .await
                    {
                        warn!(slug = %w.slug, error = %e, "adopt: push from source_path failed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "adopt: malformed discovery_adopted payload");
            }
        }
    }

    if event_type == "managed_paths_drift_resolution" {
        #[derive(serde::Deserialize)]
        struct WireDriftResolution {
            drift_id: String,
            slug: String,
            kind: String,
            resolution: String,
        }
        match serde_json::from_str::<WireDriftResolution>(event_data) {
            Ok(w) => {
                let state_arc: Arc<AppState> = Arc::new(state.clone());
                let registry_url = state
                    .get_sync_state("registry_url")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                tokio::spawn(async move {
                    if registry_url.is_empty() {
                        warn!("drift: no registry_url in sync_state — cannot ack resolution");
                        return;
                    }
                    if let Err(e) = crate::managed_paths::drift::handle_drift_resolution(
                        state_arc,
                        registry_url,
                        w.drift_id,
                        w.slug,
                        w.kind,
                        w.resolution,
                    )
                    .await
                    {
                        warn!(error = %e, "drift: resolution handler failed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "drift: malformed managed_paths_drift_resolution payload");
            }
        }
    }

    let parsed = match parse_sync_event(event_type, event_data) {
        Ok(e) => e,
        Err(e) => {
            warn!(event_type, error = %e, "SSE: failed to parse event — skipping");
            return Ok(false);
        }
    };

    // Persist last_event_id so reconnects resume correctly.
    if let Some(id) = event_id {
        *last_event_id = Some(id.clone());
        // Best-effort persist — do not abort on error.
        if let Err(e) = state.set_sync_state("last_event_id", id) {
            warn!(error = %e, "SSE: failed to persist last_event_id");
        }
    }

    // Forward to reconciler.
    match tx.send(parsed).await {
        Ok(()) => Ok(false),
        Err(_) => Ok(true), // channel closed
    }
}

// ── SyncEvent types ───────────────────────────────────────────────────────────

/// An event received over the SSE stream.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// Full desired-state snapshot (sent on first connect and after long gaps).
    Snapshot {
        installations: Vec<InstallationRecord>,
        /// MCP installation desired-state from the snapshot payload.
        /// Empty when the backend is older and does not emit the key.
        mcp_installations: Vec<McpInstallationRecord>,
    },
    /// Install (or re-activate) a specific skill version.
    Install {
        installation_id: Uuid,
        skill_id: String,
        version: String,
    },
    /// Deactivate a skill (keep files; remove active symlink).
    Deactivate {
        installation_id: Uuid,
        skill_id: String,
    },
    /// Purge a skill (delete files and SQLite row).
    Purge {
        installation_id: Uuid,
        skill_id: String,
    },
    /// Install (or re-configure) a managed MCP server.
    InstallMcp {
        installation_id: Uuid,
        mcp_server_id: Uuid,
        mcp_server_name: String,
        package_source: String,
        version_pin: Option<String>,
        server_config: Option<serde_json::Value>,
        auth_type: String,
        gateway_server_id: Option<String>,
    },
    /// Deactivate (remove) a managed MCP server.
    DeactivateMcp {
        installation_id: Uuid,
        mcp_server_id: Uuid,
    },
}

/// One entry in a [`SyncEvent::Snapshot`] skill installations list.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InstallationRecord {
    pub installation_id: Uuid,
    pub skill_id: String,
    pub version: String,
    /// `"desired"` | `"installing"` | `"installed"` | `"deactivated"` | `"removed"` | `"error"`
    pub state: String,
}

/// One entry in a [`SyncEvent::Snapshot`] MCP installations list.
///
/// Mirrors the fields from a live `install_mcp` event payload, plus a
/// `state` field so the snapshot reconciler knows the desired disposition.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpInstallationRecord {
    pub installation_id: Uuid,
    pub mcp_server_id: Uuid,
    pub mcp_server_name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    pub server_config: Option<serde_json::Value>,
    pub auth_type: String,
    pub gateway_server_id: Option<String>,
    /// `"desired"` | `"installing"` | `"installed"` | `"deactivated"` | `"removed"`
    pub state: String,
}

// ── Wire types (SSE JSON payloads) ────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct WireSnapshot {
    installations: Vec<InstallationRecord>,
    /// MCP installation desired-state list.  `#[serde(default)]` so snapshots
    /// from older backends (which do not emit the key) parse successfully with
    /// an empty vec rather than failing deserialization.
    #[serde(default)]
    mcp_installations: Vec<McpInstallationRecord>,
}

#[derive(Debug, serde::Deserialize)]
struct WireInstall {
    installation_id: Uuid,
    skill_id: String,
    version: String,
}

#[derive(Debug, serde::Deserialize)]
struct WireDeactivate {
    installation_id: Uuid,
    skill_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct WirePurge {
    installation_id: Uuid,
    skill_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireInstallMcp {
    pub installation_id: Uuid,
    pub mcp_server_id: Uuid,
    pub mcp_server_name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    pub server_config: Option<serde_json::Value>,
    pub auth_type: String,
    pub gateway_server_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireDeactivateMcp {
    pub installation_id: Uuid,
    pub mcp_server_id: Uuid,
}

/// Wire payload for the `state` event. The daemon ignores `state` events with
/// `kind == "mcp"` that it doesn't recognize — same pattern as skill state events.
#[derive(Debug, serde::Deserialize)]
struct WireState {
    kind: Option<String>,
}

fn parse_sync_event(event_type: &str, data: &str) -> Result<SyncEvent> {
    match event_type {
        "snapshot" => {
            let wire: WireSnapshot = serde_json::from_str(data)
                .with_context(|| format!("failed to parse snapshot event: {data}"))?;
            Ok(SyncEvent::Snapshot {
                installations: wire.installations,
                mcp_installations: wire.mcp_installations,
            })
        }
        "install" => {
            let wire: WireInstall = serde_json::from_str(data)
                .with_context(|| format!("failed to parse install event: {data}"))?;
            Ok(SyncEvent::Install {
                installation_id: wire.installation_id,
                skill_id: wire.skill_id,
                version: wire.version,
            })
        }
        "deactivate" => {
            let wire: WireDeactivate = serde_json::from_str(data)
                .with_context(|| format!("failed to parse deactivate event: {data}"))?;
            Ok(SyncEvent::Deactivate {
                installation_id: wire.installation_id,
                skill_id: wire.skill_id,
            })
        }
        "purge" => {
            let wire: WirePurge = serde_json::from_str(data)
                .with_context(|| format!("failed to parse purge event: {data}"))?;
            Ok(SyncEvent::Purge {
                installation_id: wire.installation_id,
                skill_id: wire.skill_id,
            })
        }
        "install_mcp" => {
            let wire: WireInstallMcp = serde_json::from_str(data)
                .with_context(|| format!("failed to parse install_mcp event: {data}"))?;
            Ok(SyncEvent::InstallMcp {
                installation_id: wire.installation_id,
                mcp_server_id: wire.mcp_server_id,
                mcp_server_name: wire.mcp_server_name,
                package_source: wire.package_source,
                version_pin: wire.version_pin,
                server_config: wire.server_config,
                auth_type: wire.auth_type,
                gateway_server_id: wire.gateway_server_id,
            })
        }
        "deactivate_mcp" => {
            let wire: WireDeactivateMcp = serde_json::from_str(data)
                .with_context(|| format!("failed to parse deactivate_mcp event: {data}"))?;
            Ok(SyncEvent::DeactivateMcp {
                installation_id: wire.installation_id,
                mcp_server_id: wire.mcp_server_id,
            })
        }
        "state" => {
            // The backend sends `state` events after PATCH-backs. Parse the `kind`
            // field and skip — reconciler state transitions are handled via PATCH
            // callbacks, not inbound state events. Log at DEBUG for observability.
            let wire: WireState = serde_json::from_str(data).unwrap_or(WireState { kind: None });
            let kind = wire.kind.as_deref().unwrap_or("unknown");
            debug!("SSE: received state event (kind={kind}) — no-op");
            // Return a no-op Snapshot with empty lists so the reconciler ignores
            // this without special-casing it. The reconciler drops empty snapshot
            // diffs with zero derived events and does not wipe existing MCP state
            // because an empty `mcp_installations` vec is treated as "old backend
            // with no MCP key" rather than "zero desired servers".
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: vec![],
            })
        }
        "managed_paths_policy_update" => {
            // F4: backend broadcasts this when the admin changes the org's
            // managed-paths enforcement mode.  Parse and stash the new mode in
            // the sync_state KV table so F3's reconciler can read it without
            // an extra API round-trip.
            //
            // Expected payload:
            //   {"org_id": "default", "mode": "quarantine", "updated_at": "..."}
            //
            // F3 reads `sync_state["managed_paths_mode"]` to decide whether to
            // quarantine, warn-only, or just audit unmanaged drops.
            // For F4 we only receive + persist — no filesystem action yet.
            #[derive(Debug, serde::Deserialize)]
            struct WireManagedPathsPolicy {
                mode: String,
                #[allow(dead_code)]
                org_id: Option<String>,
                #[allow(dead_code)]
                updated_at: Option<String>,
            }

            let wire: WireManagedPathsPolicy = serde_json::from_str(data).with_context(|| {
                format!("failed to parse managed_paths_policy_update event: {data}")
            })?;

            info!(mode = %wire.mode, "managed_paths: policy update received");

            // Return an empty snapshot so the reconciler produces no diff actions.
            // The caller (dispatch_event) persists the mode to sync_state because
            // it has access to AppState; parse_sync_event is a pure parser.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: vec![],
            })
        }
        "discovery_adopted" => {
            // Handled in `dispatch_event` before this parser is called — no
            // additional reconciler action needed.  Return an empty snapshot so
            // the reconciler produces no diff actions.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: vec![],
            })
        }
        other => {
            anyhow::bail!("unknown SSE event type: '{other}'")
        }
    }
}

// ── Token refresh helper ──────────────────────────────────────────────────────

/// Attempt to refresh the stored JWT for `registry_url`.
///
/// Uses the existing token store (SQLite `auth_tokens`).  On success, saves the
/// new tokens back and returns the new access token.
async fn try_refresh_token(registry_url: &str, state: Arc<AppState>) -> Result<String> {
    let reg_url = registry_url.to_string();
    let state_clone = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        let rows =
            load_all_tokens(&state_clone).context("failed to load auth tokens for refresh")?;

        let row = rows
            .into_iter()
            .find(|r| r.registry_url == reg_url)
            .ok_or_else(|| anyhow::anyhow!("no stored token for {reg_url}"))?;

        let client = AuthClient::new(&reg_url);
        let new_tokens = client
            .refresh(&row.refresh_token)
            .context("token refresh HTTP call failed")?;

        save_tokens(
            &state_clone,
            &reg_url,
            &new_tokens.access_token,
            &new_tokens.refresh_token,
        )
        .context("failed to save refreshed tokens")?;

        Ok(new_tokens.access_token)
    })
    .await
    .context("token refresh task panicked")?
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "sse_client_tests.rs"]
mod tests;
