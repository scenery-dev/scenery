//! Local HTTP MCP resource server — Zero-CLI Phase 2.
//!
//! Exposes the same tool surface as the Unix-socket path
//! (`crate::socket_dispatch::dispatch` against the daemon's shared
//! `RealBackend`) over `POST http://127.0.0.1:<port>/mcp`, so Claude Code can
//! authenticate with native OAuth (`/mcp` → Authenticate) instead of a CLI
//! step. See `context/2026-10-13-zero-cli-http-mcp-oauth-design.md` — this
//! module implements §3.4 (daemon RS), §3.5 (first-run device login), and
//! must match §8 (the frozen backend↔daemon interface contract) exactly.
//!
//! # Threat model summary (see the design doc §4 for the full table)
//!
//! - Bound to `127.0.0.1` only, never `0.0.0.0`.
//! - Every request must present `Host: 127.0.0.1:<port>` or `localhost:<port>`
//!   exactly, or it is rejected with 403 — defeats DNS rebinding.
//! - Any `Origin` header at all is rejected with 403 and no CORS headers are
//!   ever sent — a real MCP client (Claude Code) never sends `Origin`; a
//!   malicious web page always does. See [`ORIGIN_ALLOWLIST`] if that changes.
//! - Every `POST /mcp` call requires the H3 per-user local key
//!   (`X-VectorHawk-Local-Key`, constant-time compared), checked before
//!   anything else on this route — deliberately NOT required on the PRM GET
//!   routes (Claude Code's own OAuth helper process never sends it; see
//!   `build_router`).
//! - Every `/mcp` call requires a Bearer JWT, verified locally (ES256 via
//!   JWKS, no per-request backend round trip) against `iss`/`aud`/
//!   `client_id`/`scope`/`exp`/`nbf`.
//! - The access token's `sub` must equal the user this daemon is already
//!   paired to. A token for a different user is rejected, never silently
//!   re-paired.
//! - Port is chosen once and persisted (`sync_state["mcp_http_port"]`) —
//!   never hopped silently on a later run, since the Claude Code config
//!   written by `vectorhawk mcp setup` pins the port number.
//!
//! # First-run device login (§3.5)
//!
//! When this daemon has no device session yet and receives a *valid* MCP
//! access token, it exchanges that token for a device-bound portal token
//! pair via `POST <issuer>/oauth/device-exchange`, saves them exactly as
//! `auth pair` would, and starts the sync subsystem — so `/mcp` → Authenticate
//! in Claude Code is the *entire* login experience, no CLI step required.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::{error, info, warn};
use vectorhawkd_core::{
    auth::{decode_jwt_sub, load_tokens, save_tokens},
    state::{self, AppState},
};
use vectorhawkd_mcp::protocol::{
    with_client_meta, JsonRpcRequest, JsonRpcResponse, CLAUDE_CODE_CLIENT_NAME, PARSE_ERROR,
};

use crate::DaemonContext;

/// Default port for the local MCP HTTP resource server (§8 of the contract).
const DEFAULT_PORT: u16 = 39140;
/// Last port tried on a first-ever bind (inclusive) — `DEFAULT_PORT..=PORT_RANGE_END`
/// is exactly the 10-port range `39140–39149` the contract reserves for this.
const PORT_RANGE_END: u16 = 39149;
/// The `sync_state` key the resolved user id (the access token's `sub` this
/// daemon is bound to) is cached under, so repeated requests don't have to
/// re-decode the stored portal JWT every time. Shared with
/// `vectorhawkd_core::state::AppState::clear_device_session` (M6) — a single
/// constant so logout/revocation and this module's cache can never drift.
const SYNC_STATE_USER_KEY: &str = state::MCP_USER_ID_SYNC_STATE_KEY;

/// Header carrying the per-user local key (H3). Checked on every `POST
/// /mcp` — before any token validation or first-run device exchange —
/// defeating another OS user on this machine reaching `127.0.0.1:<port>`,
/// which (unlike the Unix socket the stdio path uses) has no same-user ACL
/// of its own. Deliberately NOT checked on the PRM GET routes; see
/// `build_router`'s comment for why. Header *names* are case-insensitive by
/// HTTP spec and the underlying `http`/`hyper` stack already normalizes
/// incoming names before this is looked up, so a client sending
/// `X-VectorHawk-Local-Key` or any other casing matches this lowercase
/// constant equally.
const LOCAL_KEY_HEADER: &str = "x-vectorhawk-local-key";

/// Frozen per §8 — overrides the design doc's §3.4 "aud = this RS resource".
const JWT_AUDIENCE: &str = "urn:vectorhawk:local-mcp";
/// The only client this resource server accepts tokens for (§8/§3.2 — no DCR).
const JWT_CLIENT_ID: &str = "vectorhawk-claude-code";
/// Scope every valid MCP access token must carry.
const JWT_REQUIRED_SCOPE: &str = "mcp";
/// Clock skew allowance for `exp`/`nbf`, matching the task's "≤60s skew".
/// `jsonwebtoken::Validation::new` already defaults `leeway` to this value;
/// set explicitly so the invariant doesn't silently drift if that default
/// ever changes upstream.
const CLOCK_SKEW_SECS: u64 = 60;

/// Request body cap for `POST /mcp`.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Minimum time between JWKS refetches triggered by an unknown `kid`. Caps
/// how often a malicious or buggy caller sending bogus `kid`s can make this
/// daemon hammer the registry's `/oauth/jwks` endpoint.
const JWKS_REFETCH_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Per the design doc's threat model: a request carrying any `Origin` header
/// is rejected outright (a real MCP client sends none; a browser always
/// does — this is the DNS-rebinding/CSRF defense). Empty by default. If the
/// Phase 0 spike on spaceghost finds that Claude Code *does* send an
/// `Origin` header, add its exact value here rather than loosening the
/// check to "allow all".
const ORIGIN_ALLOWLIST: &[&str] = &[];

/// Start the local MCP HTTP resource server.
///
/// Returns `Ok(None)` when the feature is disabled, no port could be bound,
/// or the persisted port is already held by something else — in every case
/// the daemon continues on the stdio/Unix-socket path unaffected. Must be
/// called only after the daemon's single-instance lock (`daemon.lock`) is
/// held — `run_daemon` satisfies this by construction (this is wired in
/// after that lock is acquired at the top of `run_daemon`).
pub async fn start_listener(ctx: DaemonContext) -> Result<Option<(SocketAddr, JoinHandle<()>)>> {
    // `AppState::mcp_http_enabled` (VECTORHAWK_MCP_HTTP env / sync_state
    // override, default on) is shared with `vectorhawkd-cli`'s `mcp setup`
    // so both sides of the "is HTTP+OAuth in play" decision read the exact
    // same flag.
    if !ctx.state.mcp_http_enabled() {
        info!("mcp http: disabled (VECTORHAWK_MCP_HTTP / mcp_http_enabled) — stdio-only");
        return Ok(None);
    }

    // M9: never serve this listener against a plaintext issuer — JWKS and
    // token validation would happen over unauthenticated HTTP. Checked
    // before binding anything, so an insecure issuer never even reserves a
    // port.
    if !check_issuer_scheme(&ctx.registry_url) {
        error!(
            registry_url = %ctx.registry_url,
            "mcp http: refusing to start — issuer/registry URL is not https:// (M9). \
             Set VECTORHAWK_MCP_HTTP_ALLOW_INSECURE_ISSUER=1 to override for local dev \
             only — never in production. Staying stdio-only."
        );
        return Ok(None);
    }

    let Some((listener, port)) = bind_listener(&ctx.state).await? else {
        return Ok(None);
    };

    serve_on(ctx, listener, port).await.map(Some)
}

/// M9: whether `registry_url` may be used as the OAuth issuer for this
/// listener. Delegates to [`vectorhawkd_core::state::mcp_http_issuer_allowed`]
/// (shared with `vectorhawkd-cli`'s `resolve_mcp_http_port`, so both sides of
/// the "would the RS actually start" decision agree) and logs loudly the one
/// time the override env var is what let a non-`https://` issuer through.
fn check_issuer_scheme(registry_url: &str) -> bool {
    let allowed = state::mcp_http_issuer_allowed(registry_url);
    if allowed && !registry_url.starts_with("https://") {
        warn!(
            registry_url,
            "mcp http: VECTORHAWK_MCP_HTTP_ALLOW_INSECURE_ISSUER=1 — serving the local MCP \
             HTTP resource server against a non-https issuer. NEVER set this in production; \
             JWKS and token validation happen over plaintext HTTP while this is set."
        );
    }
    allowed
}

/// Build the router for `(ctx, port)` and start serving on an
/// already-bound `listener`. Split out of [`start_listener`] so tests can
/// bind an ephemeral `127.0.0.1:0` port directly — bypassing the persisted-
/// port / fallback-range logic in [`bind_listener`] entirely — while
/// exercising the exact same router, middleware, and auth/dispatch code the
/// real daemon runs.
async fn serve_on(
    ctx: DaemonContext,
    listener: TcpListener,
    port: u16,
) -> Result<(SocketAddr, JoinHandle<()>)> {
    let addr = listener
        .local_addr()
        .context("mcp http: failed to read bound local address")?;

    // Issuer per §8: "the registry base URL with no trailing slash".
    let issuer = ctx.registry_url.trim_end_matches('/').to_string();

    // H3: the per-user local key this listener will require on every
    // request. Generated once and persisted in `sync_state` — `mcp setup`
    // reads the exact same value back out of the same `state.db` to write
    // into Claude Code's HTTP entry headers.
    let local_key = ctx
        .state
        .local_mcp_key()
        .context("mcp http: failed to load/generate the per-user local key (H3)")?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("mcp http: failed to build outbound HTTP client")?;

    let state = McpHttpState {
        ctx,
        issuer,
        port,
        local_key,
        http,
        jwks: Arc::new(tokio::sync::Mutex::new(JwksCache {
            keys: HashMap::new(),
            last_fetch: None,
        })),
        device_exchange_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    let app = build_router(state);

    info!(
        port,
        "mcp http: local MCP resource server listening on 127.0.0.1"
    );

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "mcp http: listener task exited with an error");
        }
    });

    Ok((addr, handle))
}

/// Resolve the port to bind, applying the "never hop silently" rule.
///
/// - A persisted port is tried exactly once; on failure the daemon logs
///   loudly and stays stdio-only (`Ok(None)`) — it never falls through to a
///   different port, because the Claude Code config written by
///   `vectorhawk mcp setup` pins this exact number.
/// - With no persisted port (first run), the first free port in
///   `DEFAULT_PORT..=PORT_RANGE_END` is bound and persisted.
async fn bind_listener(state: &AppState) -> Result<Option<(TcpListener, u16)>> {
    if let Some(persisted) =
        state.get_sync_state(vectorhawkd_core::state::MCP_HTTP_PORT_SYNC_STATE_KEY)?
    {
        let port: u16 = match persisted.parse() {
            Ok(p) => p,
            Err(e) => {
                error!(
                    persisted, error = %e,
                    "mcp http: persisted mcp_http_port is not a valid port number — \
                     staying stdio-only"
                );
                return Ok(None);
            }
        };
        return match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => Ok(Some((listener, port))),
            Err(e) => {
                error!(
                    port, error = %e,
                    "mcp http: persisted mcp_http_port is already in use by another \
                     process — staying stdio-only. This port is pinned in the Claude \
                     Code config written by `vectorhawk mcp setup`; it will NOT be \
                     hopped to a different port automatically. Free the port, or \
                     re-run `vectorhawk mcp setup` after resolving the conflict."
                );
                Ok(None)
            }
        };
    }

    for port in DEFAULT_PORT..=PORT_RANGE_END {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => {
                state
                    .set_sync_state(
                        vectorhawkd_core::state::MCP_HTTP_PORT_SYNC_STATE_KEY,
                        &port.to_string(),
                    )
                    .context("mcp http: failed to persist chosen mcp_http_port")?;
                info!(
                    port,
                    "mcp http: bound and persisted port for the first time"
                );
                return Ok(Some((listener, port)));
            }
            Err(_) => continue,
        }
    }

    error!(
        range_start = DEFAULT_PORT,
        range_end = PORT_RANGE_END,
        "mcp http: every port in the fallback range is already in use — staying stdio-only"
    );
    Ok(None)
}

/// JWKS cache: `kid` → verified EC public key, plus the last time a refetch
/// was attempted (for rate-limiting refetches triggered by unknown `kid`s).
struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    last_fetch: Option<Instant>,
}

/// Shared state for every handler on this listener.
#[derive(Clone)]
struct McpHttpState {
    /// Same context the Unix-socket path uses — same `RealBackend`, same
    /// `sync_controller`, same `AppState`. Dispatch goes through
    /// `crate::socket_dispatch::dispatch(&ctx, ..)` so the tool surface,
    /// audit, and governance are identical to the socket path.
    ctx: DaemonContext,
    /// Registry base URL with no trailing slash — the OAuth issuer.
    issuer: String,
    /// Bound port, needed to build the exact `Host`/PRM/`WWW-Authenticate`
    /// values this listener must check or emit.
    port: u16,
    /// H3 per-user local key. Compared against the `X-VectorHawk-Local-Key`
    /// header on every request, constant-time, before any Host/Origin-
    /// -passed request reaches auth or dispatch.
    local_key: String,
    http: reqwest::Client,
    jwks: Arc<tokio::sync::Mutex<JwksCache>>,
    /// Serializes first-run device-exchange attempts so two concurrent
    /// requests arriving before any session exists can't both race to
    /// generate a `device_uuid` / call `/oauth/device-exchange`.
    device_exchange_lock: Arc<tokio::sync::Mutex<()>>,
}

impl McpHttpState {
    async fn jwks_key_for(&self, kid: &str) -> Option<DecodingKey> {
        self.jwks.lock().await.keys.get(kid).cloned()
    }

    /// Refetch the JWKS, rate-limited to one attempt per
    /// [`JWKS_REFETCH_MIN_INTERVAL`]. Returns `Err` both on a genuine
    /// network/decode failure and when the rate limit is in effect — either
    /// way the caller should treat the token as currently unverifiable.
    async fn refresh_jwks_if_allowed(&self) -> Result<()> {
        {
            let mut guard = self.jwks.lock().await;
            if let Some(last) = guard.last_fetch {
                if last.elapsed() < JWKS_REFETCH_MIN_INTERVAL {
                    anyhow::bail!("jwks refetch rate-limited");
                }
            }
            guard.last_fetch = Some(Instant::now());
        }

        let url = format!("{}/oauth/jwks", self.issuer);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("mcp http: failed to reach JWKS endpoint")?;

        if !resp.status().is_success() {
            anyhow::bail!("mcp http: JWKS endpoint returned {}", resp.status());
        }

        let body: JwksResponse = resp
            .json()
            .await
            .context("mcp http: failed to decode JWKS response")?;

        let mut keys = HashMap::new();
        for jwk in body.keys {
            if jwk.kty != "EC" || jwk.crv.as_deref() != Some("P-256") {
                continue;
            }
            let (Some(kid), Some(x), Some(y)) =
                (jwk.kid.clone(), jwk.x.as_deref(), jwk.y.as_deref())
            else {
                continue;
            };
            match DecodingKey::from_ec_components(x, y) {
                Ok(key) => {
                    keys.insert(kid, key);
                }
                Err(e) => {
                    warn!(error = %e, "mcp http: skipping malformed JWKS entry");
                }
            }
        }

        self.jwks.lock().await.keys = keys;
        Ok(())
    }
}

#[derive(Deserialize)]
struct JwksResponse {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
    kid: Option<String>,
}

/// Custom claims read out of a validated MCP access token. Standard claims
/// (`iss`, `aud`, `exp`, `nbf`) are checked by `jsonwebtoken` directly against
/// the raw token payload — they deliberately do NOT need to appear here for
/// that validation to happen (see the module's test coverage).
#[derive(Debug, Deserialize)]
struct McpClaims {
    sub: String,
    client_id: String,
    scope: String,
    #[allow(dead_code)] // kept for future replay/audit use; not checked yet
    jti: String,
}

fn build_router(state: McpHttpState) -> Router {
    Router::new()
        // §9 (Phase 0 spike, Claude Code 2.1.238): discovery is proactive —
        // Claude Code tries the path-aware PRM first, then the bare one, on
        // every `claude mcp list` health check and after any 401. Both must
        // serve the identical PRM body, unauthenticated and cheap.
        //
        // Deliberately NOT gated on the H3 local key (unlike `/mcp` below):
        // a later empirical run against real Claude Code 2.1.238 found that
        // `claude mcp login`'s OAuth discovery/code-exchange runs in a
        // separate Bun helper process that never sends custom headers. A
        // 403 here breaks login outright, since that helper builds its
        // authorize URL from this response. PRM is still behind the
        // Host/Origin guard below — it just isn't behind the local key.
        .route("/.well-known/oauth-protected-resource/mcp", get(handle_prm))
        .route("/.well-known/oauth-protected-resource", get(handle_prm))
        .route(
            "/mcp",
            post(handle_mcp_post)
                .route_layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
                // H3: every POST /mcp requires the local key — the main
                // Claude Code client (as opposed to the login helper above)
                // does send it on every `/mcp` request, including after a
                // token refresh, so this is what actually protects the
                // sensitive paths: token validation and first-run device
                // exchange, both further inside `handle_mcp_post`. Added
                // after `DefaultBodyLimit` so it wraps (and so runs before)
                // it — reject a bad key before even reading the body.
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    local_key_guard,
                )),
        )
        // Host/Origin applies to every route on this listener, PRM included.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            host_origin_guard,
        ))
        .with_state(state)
}

/// Constant-time byte comparison — avoids leaking how much of the local key
/// a caller guessed correctly through response timing. Short-circuits only
/// on length (an attacker who doesn't already know the key length gains
/// nothing from that: lengths aren't secret-dependent in any way that
/// matters here).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// H3: require an exact, constant-time match of the `X-VectorHawk-Local-Key`
/// header before any other `/mcp` route logic runs — in particular before
/// token validation and before the first-run device-exchange flow, both of
/// which live further in (inside `handle_mcp_post`). Scoped to the `/mcp`
/// route only (via `route_layer` in `build_router`), never the PRM GET
/// routes. Missing or wrong → 403 with no body detail and nothing logged
/// about the key's value or contents.
async fn local_key_guard(State(state): State<McpHttpState>, req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(LOCAL_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), state.local_key.as_bytes()) {
        warn!("mcp http: rejecting request — missing or incorrect local key header");
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

/// Hardening applied to every request on this listener, before routing to a
/// specific handler and before any auth check: exact `Host` match, and
/// outright rejection of any `Origin` header. No CORS headers are ever sent.
async fn host_origin_guard(
    State(state): State<McpHttpState>,
    req: Request,
    next: Next,
) -> Response {
    let expected_a = format!("127.0.0.1:{}", state.port);
    let expected_b = format!("localhost:{}", state.port);
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h == expected_a || h == expected_b)
        .unwrap_or(false);
    if !host_ok {
        warn!(
            port = state.port,
            "mcp http: rejecting request — bad or missing Host header"
        );
        return (StatusCode::FORBIDDEN, "invalid Host header").into_response();
    }

    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("<unparseable>");
        if !ORIGIN_ALLOWLIST.contains(&origin_str) {
            warn!(
                origin = origin_str,
                "mcp http: rejecting request carrying an Origin header"
            );
            return (StatusCode::FORBIDDEN, "Origin header not allowed").into_response();
        }
    }

    next.run(req).await
}

fn prm_resource_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

fn prm_metadata_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/.well-known/oauth-protected-resource")
}

/// Build the exact PRM body per §8:
/// `{resource, authorization_servers:[issuer], scopes_supported:["mcp"],
///   bearer_methods_supported:["header"]}`.
#[derive(Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    bearer_methods_supported: Vec<String>,
}

async fn handle_prm(State(state): State<McpHttpState>) -> Response {
    let body = ProtectedResourceMetadata {
        resource: prm_resource_url(state.port),
        authorization_servers: vec![state.issuer.clone()],
        scopes_supported: vec![JWT_REQUIRED_SCOPE.to_string()],
        bearer_methods_supported: vec!["header".to_string()],
    };
    json_response(StatusCode::OK, &body)
}

/// Builds the exact `WWW-Authenticate` challenge from §8. `invalid = false`
/// is the "no token at all" case (bare `resource_metadata`); `invalid = true`
/// adds `error="invalid_token"` for every other auth failure (bad signature,
/// wrong claims, expired, revoked, sub mismatch, ...).
fn www_authenticate(port: u16, invalid: bool) -> String {
    let base = format!("Bearer resource_metadata=\"{}\"", prm_metadata_url(port));
    if invalid {
        format!("{base}, error=\"invalid_token\"")
    } else {
        base
    }
}

fn unauthorized_response(port: u16, invalid: bool) -> Response {
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    let value = www_authenticate(port, invalid);
    match HeaderValue::from_str(&value) {
        Ok(hv) => {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
        }
        Err(_) => {
            // Cannot happen (the string we build is always valid header
            // content), but never panic on a response-building path.
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
    }
    resp
}

fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response {
    match serde_json::to_vec(&value) {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(e) => {
            error!(error = %e, "mcp http: failed to serialize JSON response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /mcp` — the only way to reach the governed tool surface over HTTP.
/// `GET /mcp` is never routed to a handler, so axum's automatic
/// `MethodNotAllowed` already returns 405 for it.
async fn handle_mcp_post(
    State(state): State<McpHttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type_ok = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false);
    if !content_type_ok {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        )
            .into_response();
    }

    match authenticate(&state, &headers).await {
        Ok(()) => {}
        Err(AuthFailure::Missing) => return unauthorized_response(state.port, false),
        Err(AuthFailure::Invalid(reason)) => {
            warn!(reason, "mcp http: rejecting /mcp request — invalid token");
            return unauthorized_response(state.port, true);
        }
    }

    let request: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::error(None, PARSE_ERROR, format!("invalid JSON: {e}"));
            return json_response(StatusCode::OK, resp);
        }
    };

    // Notifications (no `id`) get no response body — `notifications/initialized`
    // is the one Claude Code actually sends over this transport.
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    // §9 (Phase 0 spike): the v2 runtime sends a `server/discover` probe
    // (with `mcp-method`/`mcp-protocol-version` headers we deliberately
    // ignore — no special casing needed) before `initialize`. `dispatch`'s
    // unknown-method arm already answers any unrecognized method, including
    // this one, with JSON-RPC `-32601` at HTTP 200 — never an HTTP error —
    // which is exactly what makes Claude Code fall back cleanly to
    // `initialize`.
    let response = crate::socket_dispatch::dispatch(&state.ctx, from_claude_code(request)).await;
    json_response(StatusCode::OK, response)
}

/// Tag `tools/list` as coming from Claude Code, the only client this
/// transport serves (`JWT_CLIENT_ID`). The request carries no clientInfo of
/// its own, and Claude Code loads governed skills natively, so the daemon
/// must leave them out of the tool list.
pub(crate) fn from_claude_code(mut request: JsonRpcRequest) -> JsonRpcRequest {
    if request.method == "tools/list" {
        request.params = with_client_meta(request.params, CLAUDE_CODE_CLIENT_NAME);
    }
    request
}

/// Why a request failed authentication/binding — only used to pick which
/// `WWW-Authenticate` shape to send; the specific reason is logged, not
/// leaked to the client.
enum AuthFailure {
    /// No `Authorization` header (or not a Bearer token) was present at all.
    Missing,
    /// A token was present but rejected for the given (logged-only) reason:
    /// bad signature/claims, expired, revoked, wrong user, or a
    /// device-exchange failure on first run.
    Invalid(&'static str),
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// Full authentication + user-binding + (first-run) device-exchange flow.
async fn authenticate(state: &McpHttpState, headers: &HeaderMap) -> Result<(), AuthFailure> {
    let token = extract_bearer(headers).ok_or(AuthFailure::Missing)?;

    let claims = validate_jwt(state, &token)
        .await
        .map_err(|_| AuthFailure::Invalid("jwt_validation_failed"))?;

    // Device/user revocation already reaches this daemon via the SSE sync
    // client (`AppState::mark_device_revoked`, shared with the `auth pair`
    // flow). Reject outright until the user re-pairs — never silently
    // re-exchange a revoked identity.
    if matches!(
        state.ctx.state.get_sync_state("device_revoked_at"),
        Ok(Some(_))
    ) {
        return Err(AuthFailure::Invalid("device_or_user_revoked"));
    }

    let has_session = matches!(state.ctx.state.get_sync_state("device_id"), Ok(Some(_)));
    if has_session {
        bind_to_existing_session(state, &claims)
    } else {
        device_exchange_first_run(state, &token, &claims).await
    }
}

fn bind_to_existing_session(state: &McpHttpState, claims: &McpClaims) -> Result<(), AuthFailure> {
    match resolve_bound_user_id(&state.ctx.state, &state.ctx.registry_url) {
        Some(uid) if uid == claims.sub => Ok(()),
        Some(_) => Err(AuthFailure::Invalid("sub_mismatch")),
        None => Err(AuthFailure::Invalid("no_bound_user_id")),
    }
}

/// The user id this daemon is already paired to, for comparison against an
/// incoming MCP access token's `sub`. Prefers the cached
/// `sync_state["mcp_user_id"]` (written by a prior HTTP-MCP device-exchange);
/// falls back to decoding `sub` out of the stored portal access token for
/// sessions established the old way (`auth login` / `auth pair`), caching
/// the result so subsequent calls don't re-decode it.
fn resolve_bound_user_id(state: &AppState, registry_url: &str) -> Option<String> {
    if let Ok(Some(uid)) = state.get_sync_state(SYNC_STATE_USER_KEY) {
        return Some(uid);
    }
    let stored = load_tokens(state, registry_url).ok().flatten()?;
    let sub = decode_jwt_sub(&stored.access_token)?;
    let _ = state.set_sync_state(SYNC_STATE_USER_KEY, &sub);
    Some(sub)
}

fn local_hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| {
        #[cfg(unix)]
        {
            let mut buf = vec![0u8; 256];
            if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
        }
        "unknown".to_string()
    })
}

#[derive(Deserialize)]
struct DeviceExchangeResponse {
    device_id: String,
    access_token: String,
    refresh_token: String,
}

/// §3.5: on the first valid MCP token when this daemon has no device
/// session yet, exchange it for a device-bound portal token pair and start
/// sync — behaving exactly like `auth pair`, but triggered by Claude Code's
/// `/mcp` → Authenticate instead of a CLI step.
async fn device_exchange_first_run(
    state: &McpHttpState,
    token: &str,
    claims: &McpClaims,
) -> Result<(), AuthFailure> {
    let _guard = state.device_exchange_lock.lock().await;

    // Re-check under the lock: a concurrent request may have already
    // completed the exchange while we were waiting for it.
    if matches!(state.ctx.state.get_sync_state("device_id"), Ok(Some(_))) {
        return bind_to_existing_session(state, claims);
    }
    if matches!(
        state.ctx.state.get_sync_state("device_revoked_at"),
        Ok(Some(_))
    ) {
        return Err(AuthFailure::Invalid("device_or_user_revoked"));
    }

    let device_uuid = match state.ctx.state.get_sync_state("device_uuid") {
        Ok(Some(u)) => u,
        _ => {
            let new_uuid = uuid::Uuid::new_v4().to_string();
            if let Err(e) = state.ctx.state.set_sync_state("device_uuid", &new_uuid) {
                warn!(error = %e, "mcp http: failed to persist device_uuid");
            }
            new_uuid
        }
    };

    let payload = serde_json::json!({
        "access_token": token,
        "device_uuid": device_uuid,
        "hostname": local_hostname(),
        "agent_version": env!("CARGO_PKG_VERSION"),
    });

    let url = format!("{}/oauth/device-exchange", state.issuer);
    let resp = state
        .http
        .post(&url)
        .timeout(Duration::from_secs(10))
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            warn!(error = %e, "mcp http: device-exchange request failed");
            AuthFailure::Invalid("device_exchange_transport_error")
        })?;

    match resp.status() {
        reqwest::StatusCode::OK => {
            let body: DeviceExchangeResponse = resp.json().await.map_err(|e| {
                warn!(error = %e, "mcp http: failed to decode device-exchange response");
                AuthFailure::Invalid("device_exchange_decode_error")
            })?;

            if let Err(e) = save_tokens(
                &state.ctx.state,
                &state.ctx.registry_url,
                &body.access_token,
                &body.refresh_token,
            ) {
                warn!(error = %e, "mcp http: failed to save tokens from device-exchange");
                return Err(AuthFailure::Invalid("save_tokens_failed"));
            }
            if let Err(e) = state.ctx.state.set_sync_state("device_id", &body.device_id) {
                warn!(error = %e, "mcp http: failed to persist device_id");
                return Err(AuthFailure::Invalid("persist_device_id_failed"));
            }
            let _ = state
                .ctx
                .state
                .set_sync_state(SYNC_STATE_USER_KEY, &claims.sub);

            info!(
                device_id = %body.device_id,
                "mcp http: first-run device login complete via /mcp Authenticate"
            );

            if state.ctx.sync_controller.ensure_started().await {
                info!("mcp http: sync subsystem started after device-exchange login");
            } else {
                warn!(
                    "mcp http: sync subsystem did not start after device-exchange login \
                     — will keep retrying in the background"
                );
            }

            Ok(())
        }
        reqwest::StatusCode::CONFLICT => {
            // jti already exchanged. If a concurrent caller finished first,
            // our local state now has a session — bind against it. If not,
            // we cannot recover this request's credentials; fail closed.
            if matches!(state.ctx.state.get_sync_state("device_id"), Ok(Some(_))) {
                bind_to_existing_session(state, claims)
            } else {
                warn!(
                    "mcp http: device-exchange returned 409 (jti already used) but no \
                     local session exists — failing closed"
                );
                Err(AuthFailure::Invalid("device_exchange_conflict_no_session"))
            }
        }
        reqwest::StatusCode::UNAUTHORIZED => {
            warn!("mcp http: device-exchange rejected the token as invalid/expired");
            Err(AuthFailure::Invalid("device_exchange_401"))
        }
        reqwest::StatusCode::FORBIDDEN => {
            let body = resp.text().await.unwrap_or_default();
            if crate::sync::sse_client::is_device_revoked_body(&body)
                || crate::sync::sse_client::is_user_revoked_body(&body)
            {
                if let Err(e) = state.ctx.state.mark_device_revoked() {
                    warn!(error = %e, "mcp http: failed to persist device-revoked state");
                }
                warn!(
                    "mcp http: device-exchange rejected — device/user revoked by an \
                     admin; re-pair required"
                );
            } else {
                warn!(body = %body, "mcp http: device-exchange returned 403");
            }
            Err(AuthFailure::Invalid("device_exchange_403"))
        }
        other => {
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %other, body = %body, "mcp http: device-exchange returned an unexpected status");
            Err(AuthFailure::Invalid("device_exchange_unexpected_status"))
        }
    }
}

/// Validate an MCP access token: ES256 signature against the cached JWKS
/// (refetching once, rate-limited, on an unknown `kid`), then `iss`/`aud`/
/// `exp`/`nbf` (via `jsonwebtoken::Validation`, which only enforces claims
/// actually present *unless* they're also listed in `required_spec_claims`
/// — hence listing all four there explicitly, so a token that simply omits
/// `aud`/`iss`/`nbf` is rejected rather than silently passing), then the
/// custom `client_id`/`scope` claims.
async fn validate_jwt(state: &McpHttpState, token: &str) -> Result<McpClaims> {
    let header = decode_header(token).context("malformed JWT header")?;
    if header.alg != Algorithm::ES256 {
        anyhow::bail!("rejected non-ES256 alg: {:?}", header.alg);
    }
    let kid = header.kid.context("JWT header missing kid")?;

    let mut key = state.jwks_key_for(&kid).await;
    if key.is_none() {
        state.refresh_jwks_if_allowed().await?;
        key = state.jwks_key_for(&kid).await;
    }
    let key = key.context("no JWKS key found for this kid")?;

    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[JWT_AUDIENCE]);
    validation.set_issuer(&[state.issuer.as_str()]);
    validation.validate_nbf = true;
    validation.leeway = CLOCK_SKEW_SECS;
    validation.required_spec_claims = ["exp", "nbf", "iss", "aud"]
        .into_iter()
        .map(String::from)
        .collect();

    let data = decode::<McpClaims>(token, &key, &validation)
        .context("JWT signature/claims validation failed")?;
    let claims = data.claims;

    if claims.client_id != JWT_CLIENT_ID {
        anyhow::bail!("unexpected client_id: {}", claims.client_id);
    }
    if !claims
        .scope
        .split_whitespace()
        .any(|s| s == JWT_REQUIRED_SCOPE)
    {
        anyhow::bail!("token scope missing '{JWT_REQUIRED_SCOPE}'");
    }

    Ok(claims)
}

#[cfg(test)]
#[path = "mcp_http_tests.rs"]
mod tests;
