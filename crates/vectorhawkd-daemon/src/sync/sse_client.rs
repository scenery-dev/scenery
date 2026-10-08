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
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::sync::SyncConfig;
use vectorhawkd_core::{
    auth::{load_tokens, record_refresh_failure, save_tokens, AuthClient},
    state::AppState,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// How long to wait with no data before treating the connection as dead.
const WATCHDOG_SECS: u64 = 60;

/// Starting reconnect delay.
const BACKOFF_INIT_SECS: u64 = 1;

/// Maximum reconnect delay.
const BACKOFF_MAX_SECS: u64 = 60;

/// How many consecutive "refresh, then retry, still 401" cycles to tolerate
/// before giving up and treating this as "re-authentication needed" rather
/// than a transiently stale token. A single stale-token blip (the common
/// case: the access token merely expired) resolves on the very next retry,
/// so this is deliberately small — it exists to bound the revoked-user
/// case (refresh "succeeds" — the backend hands back a technically valid
/// token — but the very next request 401s again because the *account*,
/// not the token, is the problem) to a handful of attempts instead of
/// forever.
const MAX_CONSECUTIVE_POST_REFRESH_UNAUTHORIZED: u32 = 3;

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the SSE client loop (never returns unless the channel is closed).
///
/// `connected_tx` is set to `true` (once, permanently — never reset back to
/// `false`) the moment a connection attempt gets a successful (non-401, 2xx)
/// response from `/api/sync/events` — i.e. right where the `"SSE: connected
/// to {url}"` log line fires below, *before* the (potentially long-lived)
/// event stream is read. `SyncController::ensure_started` waits on this
/// (bounded) after (re)starting the subsystem to learn whether sync is
/// genuinely running rather than merely spawned (see that function's doc
/// comment for why "spawned" and "connected" used to be conflated). It is
/// intentionally a one-shot latch rather than a live up/down flag: a later
/// disconnect (watchdog, transient error, backoff) does not un-set it,
/// because `wait_for` on a `watch` channel only ever observes the *latest*
/// value — a producer that flipped true→false within the same scheduling
/// tick (as happens whenever a mocked SSE response closes immediately after
/// the connect, and in principle could happen with a real backend that
/// drops the connection right away) can race a waiter out of ever
/// observing the transient `true`. "Connected at least once since this
/// (re)start" is both race-free and exactly what `ensure_started` needs to
/// answer "is the credential-change restart it just performed working" —
/// ongoing health thereafter is `doctor`'s job, not `auth/reload`'s.
pub async fn run(
    config: SyncConfig,
    state: Arc<AppState>,
    tx: mpsc::Sender<SyncEvent>,
    connected_tx: watch::Sender<bool>,
) {
    let mut backoff_secs = BACKOFF_INIT_SECS;
    let mut last_event_id = config.last_event_id.clone();
    let mut current_token = config.token.clone();
    let mut current_device_id = config.device_id.clone();

    // Bug 1 (revoke-loop) state: tracks "refresh succeeded, but the very
    // next connect attempt STILL got a 401" across loop iterations, so a
    // revoked/deactivated account's refresh token — which keeps minting
    // technically-valid-but-useless access tokens — can't loop forever.
    // `post_refresh_unauthorized_streak` counts consecutive occurrences;
    // `awaiting_post_refresh_check` is true only for the one iteration
    // immediately following a successful refresh, so an UNRELATED 401 much
    // later (after a real, successful reconnect in between) never gets
    // misattributed to the same streak.
    let mut post_refresh_unauthorized_streak: u32 = 0;
    let mut awaiting_post_refresh_check = false;

    loop {
        // Re-read `device_id` from the on-disk source of truth before every
        // connection attempt, instead of trusting the value this task was
        // spawned with. `auth pair`/`auth login` can write a fresh
        // `device_id` to `sync_state` at any time, including while this
        // very task is sleeping out a backoff after a failed attempt with
        // the OLD id. `SyncController::ensure_started` normally reacts to
        // that by tearing this task down and spawning a replacement with
        // the new credentials — but that is a separate, external path; it
        // must not be the *only* way a live connection recovers. Without
        // this re-read, a stale `X-Device-ID` 404s ("Device not found or
        // not owned by this user") on every retry, and since a 404 is never
        // a clean connect, the exponential backoff (1s -> 60s) never resets
        // — a self-inflicted reconnect storm that persists until something
        // external restarts the daemon. Falling back to the last-known
        // value on a read error keeps this a pure improvement over the
        // frozen `config.device_id` it replaces.
        if let Ok(Some(id)) = state.get_sync_state("device_id") {
            current_device_id = id;
        }

        let result = connect_and_stream(
            &config.registry_url,
            &current_token,
            &current_device_id,
            last_event_id.clone(),
            Arc::clone(&state),
            &tx,
            &connected_tx,
        )
        .await;

        match result {
            ConnectResult::Reconnect { new_last_id } => {
                // Clean reconnect (EOF or watchdog) — proof the credentials
                // are good. Reset backoff AND the post-refresh-401 streak:
                // a real connection in between means a LATER 401 is a fresh
                // problem, not a continuation of an old one.
                backoff_secs = BACKOFF_INIT_SECS;
                post_refresh_unauthorized_streak = 0;
                awaiting_post_refresh_check = false;
                if let Some(id) = new_last_id {
                    last_event_id = Some(id);
                }
            }
            ConnectResult::Unauthorized => {
                // Count this 401 toward the post-refresh streak ONLY if it
                // immediately follows a refresh this same loop attempted —
                // otherwise (first 401 ever, or one after an unrelated
                // generic-error retry) it starts a fresh streak at 1.
                if awaiting_post_refresh_check {
                    post_refresh_unauthorized_streak += 1;
                } else {
                    post_refresh_unauthorized_streak = 1;
                }
                awaiting_post_refresh_check = false;

                if post_refresh_unauthorized_streak > MAX_CONSECUTIVE_POST_REFRESH_UNAUTHORIZED {
                    // Refresh keeps "succeeding" (the backend hands back a
                    // token) but the account itself is rejected on every
                    // subsequent request — a revoked/deactivated user, not
                    // a merely-stale token. Retrying can't fix this; only a
                    // human re-authenticating can. Stop outright rather
                    // than keep refreshing forever with no backoff — see
                    // the revoke-loop bug this guards against.
                    warn!(
                        streak = post_refresh_unauthorized_streak,
                        "SSE: repeated 401s immediately after a successful token \
                         refresh — treating as revoked/invalid account, not an \
                         expired token. Run `vectorhawk auth login` to \
                         re-authenticate."
                    );
                    return;
                }

                // 401: try to refresh the JWT before reconnecting.
                info!("SSE: received 401 — attempting token refresh");
                match try_refresh_token(
                    &config.registry_url,
                    Arc::clone(&state),
                    &config.live_token,
                )
                .await
                {
                    Ok(new_token) => {
                        current_token = new_token;
                        info!(
                            backoff_secs,
                            "SSE: token refreshed — will retry after backoff"
                        );
                        awaiting_post_refresh_check = true;
                        // Deliberately NOT resetting `backoff_secs` and NOT
                        // skipping the sleep at the bottom of the loop — a
                        // 401/refresh cycle must never retry without
                        // backing off, even on an apparently-successful
                        // refresh. A revoked account's refresh "succeeds"
                        // every time, so skipping the delay here is exactly
                        // what turned this into an unbounded ~120ms-interval
                        // request storm before this fix.
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
            ConnectResult::Revoked => {
                // Admin revoked this device (backend 403 + {"code":
                // "device_revoked"}) — permanent for this device_uuid/id,
                // per revocation-model-decisions (2026-10-06). Retrying
                // can never succeed again; there is no admin "restore"
                // any more, only a deliberate `vectorhawk auth pair` that
                // mints a brand-new device identity. Clear the local
                // device_uuid/device_id now so: (a) `vectorhawk auth
                // status`/`doctor` can tell the user plainly what
                // happened and what to do, and (b) the daemon's own
                // unattended register-on-startup path never again
                // presents this (now known-dead) identity — see
                // `AppState::mark_device_revoked`'s doc comment for why
                // that's safe and doesn't itself silently self-repair.
                //
                // Stop this task outright rather than backing off
                // forever: nothing is lost, because
                // `SyncController::ensure_started` tears this task down
                // and spawns a fresh one whenever `device_id` changes in
                // `sync_state` — exactly what a real re-pair does.
                if let Err(e) = state.mark_device_revoked() {
                    warn!(error = %e, "SSE: failed to persist device-revoked state locally");
                }
                info!(
                    "SSE: device revoked by admin — stopping sync. \
                     Run `vectorhawk auth pair` to re-pair as a new device."
                );
                return;
            }
            ConnectResult::UserRevoked => {
                // An admin revoked this user, which also revoked every one
                // of their devices — so this device identity is dead too.
                // Clear it like a device revoke; the way back is an admin
                // reinstating the user, then `auth login` + `auth pair`.
                if let Err(e) = state.mark_device_revoked() {
                    warn!(error = %e, "SSE: failed to persist device-revoked state locally");
                }
                info!(
                    "SSE: your VectorHawk account was revoked by an admin — stopping sync. \
                     After an admin reinstates it, run `vectorhawk auth login` then \
                     `vectorhawk auth pair`."
                );
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
    /// Server returned 403 with `{"code": "device_revoked"}` — caller
    /// should stop the loop permanently (see the `run()` match arm).
    Revoked,
    /// Server returned 403 with `{"code": "user_revoked"}` — the user's
    /// account was revoked (which also revoked this device). Terminal.
    UserRevoked,
    /// Any other connection or I/O error.
    Error(anyhow::Error),
}

/// Best-effort sniff of a 403 response body for the backend's device-
/// revocation signal (`portal_sync._reject_if_revoked` /
/// `portal_devices.register_device`): `{"detail": {"code":
/// "device_revoked", ...}, ...}`. Any other shape (older backend, a
/// generic 403, a body that isn't JSON at all) returns false and the
/// caller falls back to treating it like any other non-success status —
/// this is purely an optional fast path, never a correctness requirement.
pub(crate) fn is_device_revoked_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("detail").cloned().or(Some(v)))
        .and_then(|d| d.get("code").cloned())
        .and_then(|c| c.as_str().map(|s| s == "device_revoked"))
        .unwrap_or(false)
}

/// Same sniff for the user-revocation signal. The backend checks the user
/// before the device on `/sync/*` (`portal_sync._reject_if_inactive`), so a
/// revoked user's daemon gets `{"code": "user_revoked"}`, not
/// `device_revoked`, even though user revoke also revokes every device.
pub(crate) fn is_user_revoked_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("detail").cloned().or(Some(v)))
        .and_then(|d| d.get("code").cloned())
        .and_then(|c| c.as_str().map(|s| s == "user_revoked"))
        .unwrap_or(false)
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
    connected_tx: &watch::Sender<bool>,
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

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        // Read the body (best-effort) before falling through to the
        // generic error path, so a device-revocation 403 gets the clean
        // "stop retrying" signal instead of backing off forever against a
        // backend that will never let this device_id back in.
        let body = resp.text().await.unwrap_or_default();
        if is_device_revoked_body(&body) {
            return ConnectResult::Revoked;
        }
        if is_user_revoked_body(&body) {
            return ConnectResult::UserRevoked;
        }
        return ConnectResult::Error(anyhow::anyhow!("SSE: server returned HTTP 403"));
    }

    if !resp.status().is_success() {
        return ConnectResult::Error(anyhow::anyhow!(
            "SSE: server returned HTTP {}",
            resp.status()
        ));
    }

    info!(device_id, "SSE: connected to {url}");
    // Signal genuine connectivity — this is what `ensure_started` waits on
    // (with a bounded timeout) instead of returning as soon as this task was
    // merely spawned. See `run`'s doc comment.
    let _ = connected_tx.send(true);

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

    // Task 8: backend broadcasts this when an admin toggles the org's
    // block-third-party-inference policy. Persist to sync_state (so it
    // survives restarts / seeds correctly) and flip the live atomic that
    // Task 7 wired into the model client, so the change takes effect
    // immediately without a daemon restart.
    if event_type == "inference_policy_update" {
        #[derive(serde::Deserialize)]
        struct EnabledOnly {
            enabled: bool,
        }
        if let Ok(w) = serde_json::from_str::<EnabledOnly>(event_data) {
            let v = if w.enabled { "true" } else { "false" };
            if let Err(e) = state.set_sync_state("block_third_party_inference", v) {
                warn!(error = %e, "inference policy: failed to persist to sync_state");
            }
            state
                .block_third_party_inference
                .store(w.enabled, std::sync::atomic::Ordering::Relaxed);
            debug!(
                enabled = w.enabled,
                "inference policy: block-third-party updated live"
            );
        }
    }

    // F3: admin resolved a drift event. Apply the resolution locally and ack
    // the backend. Spawned as its own task so SSE dispatch isn't blocked on
    // the disk + HTTP round-trip.
    // T2 follow-up (v1.0.54): user adopted a discovery in the portal. The
    // install row is already created server-side; the daemon now needs to
    // copy `source_path` into the canonical `~/.agents/skills/<slug>/`, write
    // the F2 marker, and link it at `~/.claude/skills/<slug>` so every client
    // — Claude Code included — can see the skill. Spawned as its own task so
    // SSE dispatch isn't blocked on disk I/O.
    //
    // Adopt auto-upload + takeover: alongside that immediate local-copy push
    // (which gives the user an instant, usable copy regardless of how long
    // the scan/approval gate takes), also route the bytes through
    // `POST /runner/skills/adopt-publish` so the skill gets a real registry
    // artifact under the org's normal policy gate. Once that resolves to
    // `published`, `managed_paths::adopt_publish` installs the real artifact
    // and takes over from `source_path`; for `pending_review` / a strict-mode
    // reject it leaves both copies alone until IT approves later. Spawned as
    // its own task, independent of the local-copy push above.
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
                let slug = w.slug.clone();
                let kind = w.kind.clone();
                let source_path = w.source_path.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::managed_paths::pusher::push_adopted_discovery(
                        &state_arc,
                        &slug,
                        &kind,
                        &source_path,
                    )
                    .await
                    {
                        warn!(slug = %slug, error = %e, "adopt: push from source_path failed");
                    }
                });

                let state_arc: Arc<AppState> = Arc::new(state.clone());
                let registry_url = state
                    .get_sync_state("registry_url")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                tokio::spawn(async move {
                    if registry_url.is_empty() {
                        warn!("adopt-publish: no registry_url in sync_state — cannot upload");
                        return;
                    }
                    if let Err(e) = crate::managed_paths::adopt_publish::handle_discovery_adopted(
                        state_arc,
                        registry_url,
                        w.slug,
                        w.kind,
                        w.source_path,
                    )
                    .await
                    {
                        warn!(error = ?e, "adopt-publish: handler failed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "adopt: malformed discovery_adopted payload");
            }
        }
    }

    // T3 (v1.0.56): admin triggered a publish from the portal (discovery status
    // transitions to 'publishing').  The daemon packs source_path and uploads to
    // the registry compile endpoint.  Spawned as its own task so SSE dispatch is
    // not blocked on disk I/O or the HTTP upload.
    if event_type == "discovery_publish_requested" {
        #[derive(serde::Deserialize)]
        struct Wire {
            discovery_id: String,
            slug: String,
            source_path: String,
            #[allow(dead_code)]
            skill_db_id: Option<String>,
        }
        match serde_json::from_str::<Wire>(event_data) {
            Ok(w) => {
                let state_arc: Arc<AppState> = Arc::new(state.clone());
                let registry_url = state
                    .get_sync_state("registry_url")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                tokio::spawn(async move {
                    if registry_url.is_empty() {
                        warn!("publish: no registry_url in sync_state — cannot publish");
                        return;
                    }
                    if let Err(e) = crate::managed_paths::publish::handle_publish_requested(
                        state_arc,
                        registry_url,
                        w.discovery_id,
                        w.slug,
                        w.source_path,
                    )
                    .await
                    {
                        // Use debug formatter (?e) to expand the full anyhow
                        // error chain — stage, URL, HTTP status, and body are
                        // all in the chain and would be silently dropped by %e.
                        warn!(error = ?e, "publish: handler failed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "publish: malformed discovery_publish_requested payload");
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
        /// `None` when the backend is older and omits the key entirely —
        /// the reconciler must leave existing installs untouched.
        /// `Some(vec![])` is a real desired state of zero MCP servers —
        /// the reconciler must reconcile existing installs away. These two
        /// cases were conflated as a plain `Vec` (both collapsed to empty)
        /// until the fix for the kill-switch bug where an `Option<Vec<_>>`
        /// was introduced to tell them apart.
        mcp_installations: Option<Vec<McpInstallationRecord>>,
        /// Plugin installation desired-state from the snapshot payload
        /// (backend `plugin_installations` key — RB1). Delivers plugins
        /// durably to a device that wasn't connected when the live
        /// `install_plugin` SSE delta fired (a backfilled device, or any
        /// device reconnecting after a dropped delta). Empty when the
        /// backend is older and does not emit the key.
        plugin_installations: Vec<PluginInstallationRecord>,
        /// Durable artifact-revocation list (broadcast-model revoke).
        /// `None` when the backend omits the `revocations` key entirely
        /// (older backend) — the reconciler must leave the local revocation
        /// cache untouched in that case, exactly like `mcp_installations:
        /// None` above. `Some(vec![])` is a real "nothing is revoked right
        /// now" and must clear the local cache — that's how an admin
        /// "unblock" propagates: the entry just drops out of the list.
        revocations: Option<Vec<RevocationRecord>>,
    },
    /// Immediately remove a revoked artifact (broadcast-model kill switch).
    /// `version: None` means "the whole artifact, every version" — the
    /// default scope when an admin revokes without pinning a version.
    Revoke {
        artifact_type: String,
        artifact_key: String,
        version: Option<String>,
    },
    /// Live "unblock" push (Bug 2 fix, broadcast-model revoke's mirror
    /// image). Before this, `unrevoke_artifact` sent nothing live — a
    /// connected daemon only converged on its next periodic
    /// `GET /api/sync/snapshot` poll or SSE reconnect (up to
    /// `SYNC_INTERVAL_SECS`, ~5 minutes). This clears the local revocation
    /// gate immediately. Does NOT reinstall anything — same "unblock
    /// doesn't resurrect state" rule every other revoke/unrevoke path in
    /// this codebase follows; the user reinstalls explicitly.
    Unrevoke {
        artifact_type: String,
        artifact_key: String,
        version: Option<String>,
    },
    /// Install (or re-activate) a specific skill version.
    Install {
        installation_id: Uuid,
        skill_id: String,
        version: String,
        /// Installation source, if the backend sends it. `"migrated:local"` signals that
        /// this skill was adopted from a local path and has no downloadable artifact in the
        /// registry (the catalog stub uses a phantom `migrated/<slug>/0.0.0.cskill` key).
        /// `None` when the backend does not yet include this field (older backend versions).
        source: Option<String>,
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
        /// Full gateway proxy URL for a credential-brokered server. When set,
        /// the daemon connects here (with its own portal JWT) instead of the
        /// raw backend — the upstream URL and its credential stay server-side.
        gateway_url: Option<String>,
    },
    /// Deactivate (remove) a managed MCP server.
    DeactivateMcp {
        installation_id: Uuid,
        mcp_server_id: Uuid,
    },
    /// Install a governed plugin as a self-contained Claude Code plugin
    /// (registered in the local `vectorhawk` marketplace, bundling its skills).
    InstallPlugin {
        installation_id: Uuid,
        plugin_slug: String,
        plugin_name: String,
        description: String,
        version: String,
        author: String,
        skills: Vec<PluginSkillRef>,
    },
    /// Deactivate (remove) a governed plugin from Claude Code.
    DeactivatePlugin {
        installation_id: Uuid,
        plugin_slug: String,
    },
}

/// A skill reference carried by an `install_plugin` event.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginSkillRef {
    pub skill_id: String,
    pub version: String,
}

/// One entry in a [`SyncEvent::Snapshot`] skill installations list.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InstallationRecord {
    pub installation_id: Uuid,
    pub skill_id: String,
    pub version: String,
    /// `"desired"` | `"installing"` | `"installed"` | `"deactivated"` | `"removed"` | `"error"`
    pub state: String,
    /// Installation source. `"migrated:local"` for locally-adopted skills with no artifact in
    /// the registry. `None` when the backend does not yet emit this field (older backends).
    #[serde(default)]
    pub source: Option<String>,
}

/// One entry in a [`SyncEvent::Snapshot`] plugin installations list.
///
/// Mirrors the fields from a live `install_plugin` event payload (built by
/// the same `build_install_plugin_payload` the backend uses for the SSE
/// delta — see RB1 brief), plus a `state` field so the snapshot reconciler
/// knows the desired disposition, exactly like [`McpInstallationRecord`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginInstallationRecord {
    pub installation_id: Uuid,
    pub plugin_slug: String,
    #[serde(default)]
    pub plugin_name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub skills: Vec<PluginSkillRef>,
    /// `"desired"` | `"installing"` | `"installed"` | `"deactivated"` | `"removed"`
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
    /// Gateway proxy URL for credential-brokered servers. `#[serde(default)]`
    /// so snapshots from older backends parse with `None`.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// `"desired"` | `"installing"` | `"installed"` | `"deactivated"` | `"removed"`
    pub state: String,
}

/// One entry in a [`SyncEvent::Snapshot`] / live `revoke` revocation list.
///
/// `artifact_key` is the skill slug, the MCP server's UUID (as a string), or
/// the plugin slug — whatever `artifact_type` says it is. There is no FK or
/// typed union on the wire; the reconciler dispatches on `artifact_type`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RevocationRecord {
    pub artifact_type: String,
    pub artifact_key: String,
    /// `None` (or an absent field) means "the whole artifact, every version".
    #[serde(default)]
    pub version: Option<String>,
}

// ── Wire types (SSE JSON payloads) ────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct WireSnapshot {
    installations: Vec<InstallationRecord>,
    /// MCP installation desired-state list.  `#[serde(default)]` so a
    /// snapshot from an older backend that omits the key entirely parses
    /// to `None` rather than failing deserialization. A present-but-empty
    /// `"mcp_installations": []` parses to `Some(vec![])` — distinct from
    /// `None`, since the two mean different things to the reconciler (see
    /// `SyncEvent::Snapshot::mcp_installations`).
    #[serde(default)]
    mcp_installations: Option<Vec<McpInstallationRecord>>,
    /// Plugin installation desired-state list (RB1).  `#[serde(default)]` so
    /// snapshots from older backends (which do not emit the key) parse
    /// successfully with an empty vec rather than failing deserialization.
    #[serde(default)]
    plugin_installations: Vec<PluginInstallationRecord>,
    /// Durable artifact-revocation list (broadcast-model revoke).
    /// `#[serde(default)]` so a backend that doesn't yet emit this key
    /// parses to `None`, not `Some(vec![])` — see
    /// `SyncEvent::Snapshot::revocations`'s doc comment for why that
    /// distinction matters (it's the exact same `None` vs `Some(vec![])`
    /// pattern as `mcp_installations` above).
    #[serde(default)]
    revocations: Option<Vec<RevocationRecord>>,
}

#[derive(Debug, serde::Deserialize)]
struct WireRevoke {
    artifact_type: String,
    artifact_key: String,
    #[serde(default)]
    version: Option<String>,
    /// Not currently consumed — the daemon stamps its own local
    /// `revoked_at` on write. Present on the wire for forward-compat /
    /// future audit enrichment.
    #[allow(dead_code)]
    #[serde(default)]
    revoked_at: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct WireUnrevoke {
    artifact_type: String,
    artifact_key: String,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct WireInstall {
    installation_id: Uuid,
    skill_id: String,
    version: String,
    /// Optional source field; omitted by older backends → `None`.
    #[serde(default)]
    source: Option<String>,
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
    #[serde(default)]
    pub gateway_url: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireDeactivateMcp {
    pub installation_id: Uuid,
    pub mcp_server_id: Uuid,
}

#[derive(Debug, serde::Deserialize)]
struct WireInstallPlugin {
    installation_id: Uuid,
    plugin_slug: String,
    #[serde(default)]
    plugin_name: String,
    #[serde(default)]
    description: String,
    version: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    skills: Vec<PluginSkillRef>,
}

#[derive(Debug, serde::Deserialize)]
struct WireDeactivatePlugin {
    installation_id: Uuid,
    plugin_slug: String,
}

/// Wire payload for the `state` event. The daemon ignores `state` events with
/// `kind == "mcp"` that it doesn't recognize — same pattern as skill state events.
#[derive(Debug, serde::Deserialize)]
struct WireState {
    kind: Option<String>,
}

/// Parse a snapshot payload (`{"installations": [...], "mcp_installations":
/// [...]}`) into a [`SyncEvent::Snapshot`].
///
/// Shared by the SSE `snapshot` event handler below and by the daemon's
/// periodic snapshot-reconcile tick (`run_sync_tick` in `vectorhawkd-daemon`'s
/// `lib.rs`), which calls `GET /api/sync/snapshot` as a safety net against a
/// dropped SSE delta. Both paths must produce identical `SyncEvent`s so they
/// converge through the exact same reconciler diff logic.
pub fn snapshot_event_from_json(data: &str) -> Result<SyncEvent> {
    let wire: WireSnapshot = serde_json::from_str(data)
        .with_context(|| format!("failed to parse snapshot payload: {data}"))?;
    Ok(SyncEvent::Snapshot {
        installations: wire.installations,
        mcp_installations: wire.mcp_installations,
        plugin_installations: wire.plugin_installations,
        revocations: wire.revocations,
    })
}

fn parse_sync_event(event_type: &str, data: &str) -> Result<SyncEvent> {
    match event_type {
        "snapshot" => snapshot_event_from_json(data),
        "install" => {
            let wire: WireInstall = serde_json::from_str(data)
                .with_context(|| format!("failed to parse install event: {data}"))?;
            Ok(SyncEvent::Install {
                installation_id: wire.installation_id,
                skill_id: wire.skill_id,
                version: wire.version,
                source: wire.source,
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
                gateway_url: wire.gateway_url,
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
        "install_plugin" => {
            let wire: WireInstallPlugin = serde_json::from_str(data)
                .with_context(|| format!("failed to parse install_plugin event: {data}"))?;
            Ok(SyncEvent::InstallPlugin {
                installation_id: wire.installation_id,
                plugin_slug: wire.plugin_slug,
                plugin_name: wire.plugin_name,
                description: wire.description,
                version: wire.version,
                author: wire.author,
                skills: wire.skills,
            })
        }
        "deactivate_plugin" => {
            let wire: WireDeactivatePlugin = serde_json::from_str(data)
                .with_context(|| format!("failed to parse deactivate_plugin event: {data}"))?;
            Ok(SyncEvent::DeactivatePlugin {
                installation_id: wire.installation_id,
                plugin_slug: wire.plugin_slug,
            })
        }
        "revoke" => {
            // Broadcast-model kill switch: one event, fanned out to every
            // connected daemon in the org, for an admin revoking a skill,
            // MCP server, or plugin (whole-artifact or one version). See
            // `reconciler::spawn_revoke` for the teardown this triggers.
            let wire: WireRevoke = serde_json::from_str(data)
                .with_context(|| format!("failed to parse revoke event: {data}"))?;
            Ok(SyncEvent::Revoke {
                artifact_type: wire.artifact_type,
                artifact_key: wire.artifact_key,
                version: wire.version,
            })
        }
        "unrevoke" => {
            // Live "unblock" push (Bug 2 fix) — see `SyncEvent::Unrevoke`'s
            // doc comment. A brand-new event name is safe for runners that
            // predate it: the `other => anyhow::bail!(...)` arm below is
            // caught by `dispatch_event`, logged, and skipped, never
            // retried or crashed on.
            let wire: WireUnrevoke = serde_json::from_str(data)
                .with_context(|| format!("failed to parse unrevoke event: {data}"))?;
            Ok(SyncEvent::Unrevoke {
                artifact_type: wire.artifact_type,
                artifact_key: wire.artifact_key,
                version: wire.version,
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
            // this without special-casing it. `mcp_installations: None` tells
            // the reconciler "no data in this event" (same meaning as an old
            // backend omitting the key) rather than "zero desired servers",
            // which would wipe existing MCP installs.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: None,
                plugin_installations: vec![],
                revocations: None,
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
                mcp_installations: None,
                plugin_installations: vec![],
                revocations: None,
            })
        }
        "discovery_adopted" => {
            // Handled in `dispatch_event` before this parser is called — no
            // additional reconciler action needed.  Return an empty snapshot so
            // the reconciler produces no diff actions.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: None,
                plugin_installations: vec![],
                revocations: None,
            })
        }
        "discovery_publish_requested" => {
            // Handled in `dispatch_event` before this parser is called — no
            // reconciler action needed.  Return an empty snapshot so the
            // reconciler produces no diff actions.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: None,
                plugin_installations: vec![],
                revocations: None,
            })
        }
        "inference_policy_update" => {
            // Task 8: handled in `dispatch_event` before this parser is
            // called (persist to sync_state + flip the live atomic) — no
            // reconciler action needed. Return an empty snapshot so the
            // reconciler produces no diff actions.
            Ok(SyncEvent::Snapshot {
                installations: vec![],
                mcp_installations: None,
                plugin_installations: vec![],
                revocations: None,
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
/// Uses the existing token store (SQLite `auth_tokens`) and the SAME
/// persisted-backoff mechanism the daemon's periodic 60s token-refresh loop
/// (`refresh_one_tick` / `classify_refresh_failure` in
/// `vectorhawkd-daemon::lib`) already uses — `auth_tokens.
/// next_refresh_attempt_at` / `refresh_failures` / `last_refresh_status` are
/// one shared piece of state, not duplicated per call site. This matters for
/// bug-revoke-loop: before this, the SSE client called the bare, undetailed
/// `AuthClient::refresh` and tracked nothing durable, so a daemon RESTART
/// wiped out any notion of "this refresh token is dead" and re-entered the
/// tight loop from scratch. Now:
///   - a refresh attempt made while a prior failure's backoff window
///     (`next_refresh_attempt_at`) hasn't elapsed yet is skipped outright —
///     no network call — and this survives a restart, since the window is
///     read fresh from SQLite every time;
///   - a 401/403 from `/portal/auth/refresh` itself (the backend now
///     returns this for a revoked/inactive user, closing the loophole that
///     let refresh "succeed" forever) is classified via
///     `crate::classify_refresh_failure` and recorded via
///     `record_refresh_failure`, the exact exponential schedule (60s up to
///     1h) `refresh_one_tick` already uses for a dead refresh token.
///
/// On success, saves the new tokens back (which also clears the backoff
/// counters — see `save_tokens`), publishes the new access token to
/// `live_token` — so `SyncController::ensure_started`'s credential-
/// fingerprint comparison sees the token this connection is actually using
/// now, not a value frozen at spawn time (see the doc comment on
/// `SyncConfig::live_token`) — and returns the new access token.
async fn try_refresh_token(
    registry_url: &str,
    state: Arc<AppState>,
    live_token: &tokio::sync::RwLock<String>,
) -> Result<String> {
    let reg_url = registry_url.to_string();
    let state_clone = Arc::clone(&state);

    let new_access_token = tokio::task::spawn_blocking(move || {
        let row = load_tokens(&state_clone, &reg_url)
            .context("failed to load auth tokens for refresh")?
            .ok_or_else(|| anyhow::anyhow!("no stored token for {reg_url}"))?;

        // Honor the persisted backoff window set by a prior auth failure —
        // same guard `refresh_one_tick` applies, so the SSE path can't
        // hammer a refresh token the periodic loop already knows is dead
        // (or vice versa), and this holds across a daemon restart since the
        // window lives in SQLite, not in-process state.
        if let Some(next_at) = row.next_refresh_attempt_at {
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if next_at > now_unix {
                anyhow::bail!(
                    "refresh skipped — in backoff window after a prior auth failure \
                     ({} s remaining)",
                    next_at - now_unix
                );
            }
        }

        let client = AuthClient::new(&reg_url);
        match client.refresh_detailed(&row.refresh_token) {
            Ok(new_tokens) => {
                save_tokens(
                    &state_clone,
                    &reg_url,
                    &new_tokens.access_token,
                    &new_tokens.refresh_token,
                )
                .context("failed to save refreshed tokens")?;
                Ok(new_tokens.access_token)
            }
            Err(err) => {
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let (status_label, backoff) =
                    crate::classify_refresh_failure(&err, row.refresh_failures);
                let next_attempt_at = backoff.map(|secs| now_unix + secs as i64);
                if let Err(e) =
                    record_refresh_failure(&state_clone, &reg_url, status_label, next_attempt_at)
                {
                    warn!(error = %e, "SSE: failed to record refresh failure state");
                }
                Err(anyhow::Error::new(err).context("token refresh HTTP call failed"))
            }
        }
    })
    .await
    .context("token refresh task panicked")??;

    *live_token.write().await = new_access_token.clone();

    Ok(new_access_token)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "sse_client_tests.rs"]
mod tests;
