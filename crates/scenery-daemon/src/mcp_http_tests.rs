//! Tests for the local MCP HTTP resource server (Zero-CLI Phase 2).
//!
//! Every test binds a real ephemeral `127.0.0.1:0` port via `serve_on` and
//! drives it with a real HTTP client — no mocked router/service calls —
//! because the hardening rules (`Host`, `Origin`, body cap, content-type)
//! only actually fire at the HTTP layer. `/oauth/jwks` and
//! `/oauth/device-exchange` are mocked with `mockito`; the daemon's own
//! `issuer` is pointed at the mockito server so both the JWKS fetch and the
//! device-exchange call land on it.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use scenery_core::state::AppState;
use scenery_mcp::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    backend::RealBackend,
    protocol::JsonRpcRequest,
};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{broadcast, Notify},
};

use super::DaemonContext;
use crate::oauth_state::OAuthState;

// ── Test ES256 key material ──────────────────────────────────────────────
//
// Generated once, offline (`openssl ecparam -name prime256v1 -genkey`), for
// this test binary only. Never used outside this file.
const TEST_KID: &str = "test-kid-1";
const TEST_PRIVATE_KEY_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgoRpjCSoEG7uc0V1O\n\
29PtIFRMnIywhVePVYLYpPCpGV2hRANCAAQM/zDFa4bJWPLrjv2RnXqe6amYaLRn\n\
ku7fpvO9F3kfhP+l0rG6+Fp/mgLNNjIJsEkilsDS+Adm6ECiWOJBItIr\n\
-----END PRIVATE KEY-----\n";
const TEST_PUBLIC_X: &str = "DP8wxWuGyVjy6479kZ16numpmGi0Z5Lu36bzvRd5H4Q";
const TEST_PUBLIC_Y: &str = "_6XSsbr4Wn-aAs02MgmwSSKWwNL4B2boQKJY4kEi0is";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

#[derive(Serialize)]
struct TestClaims {
    sub: String,
    aud: String,
    iss: String,
    client_id: String,
    scope: String,
    jti: String,
    exp: i64,
    nbf: i64,
    iat: i64,
}

/// Builder for test tokens. Every field has a sane default matching a
/// genuinely valid access token for `issuer`; individual tests override the
/// one field they're probing.
struct TokenOpts {
    sub: String,
    aud: String,
    iss: String,
    client_id: String,
    scope: String,
    jti: String,
    exp_offset: i64,
    nbf_offset: i64,
    kid: Option<String>,
    alg: Algorithm,
}

impl TokenOpts {
    fn valid(issuer: &str) -> Self {
        TokenOpts {
            sub: "user-1".to_string(),
            aud: "urn:scenery:local-mcp".to_string(),
            iss: issuer.to_string(),
            client_id: "scenery-claude-code".to_string(),
            scope: "mcp offline_access".to_string(),
            jti: uuid::Uuid::new_v4().to_string(),
            exp_offset: 600,
            nbf_offset: -5,
            kid: Some(TEST_KID.to_string()),
            alg: Algorithm::ES256,
        }
    }

    fn encode(self) -> String {
        let n = now_secs();
        let claims = TestClaims {
            sub: self.sub,
            aud: self.aud,
            iss: self.iss,
            client_id: self.client_id,
            scope: self.scope,
            jti: self.jti,
            exp: n + self.exp_offset,
            nbf: n + self.nbf_offset,
            iat: n,
        };
        let mut header = Header::new(self.alg);
        header.kid = self.kid;
        let key = match self.alg {
            Algorithm::ES256 => {
                EncodingKey::from_ec_pem(TEST_PRIVATE_KEY_PKCS8_PEM.as_bytes()).unwrap()
            }
            _ => EncodingKey::from_secret(b"wrong-alg-test-secret"),
        };
        encode(&header, &claims, &key).unwrap()
    }
}

fn jwks_body() -> serde_json::Value {
    serde_json::json!({
        "keys": [{
            "kty": "EC",
            "crv": "P-256",
            "kid": TEST_KID,
            "x": TEST_PUBLIC_X,
            "y": TEST_PUBLIC_Y,
            "use": "sig",
            "alg": "ES256",
        }]
    })
}

// ── DaemonContext / AppState test fixtures (mirrors socket_dispatch_tests) ─

fn stub_backend(server_id: &str, tools: &[&str]) -> BackendEntry {
    BackendEntry {
        server_id: server_id.to_string(),
        name: server_id.to_string(),
        transport: BackendTransport::Stub,
        tools: tools
            .iter()
            .map(|n| ToolDefinition {
                name: n.to_string(),
                description: Some(format!("desc for {n}")),
                input_schema: None,
            })
            .collect(),
        tool_visibility: ToolVisibility::All,
        priority: 50,
        consecutive_errors: 0,
        unhealthy: false,
    }
}

fn make_test_state() -> AppState {
    let root = Utf8PathBuf::from_path_buf(tempfile::tempdir().unwrap().keep())
        .expect("temp dir path must be UTF-8");
    AppState::bootstrap_in(root).expect("failed to bootstrap test AppState")
}

fn make_sync_controller(
    registry: Arc<BackendRegistry>,
    state: AppState,
) -> Arc<crate::SyncController> {
    let (tx, _rx) = broadcast::channel(16);
    Arc::new(crate::SyncController::new(
        "https://example.invalid".to_string(),
        Arc::new(state),
        tx,
        registry,
        None,
    ))
}

fn make_ctx(registry_url: &str, state: AppState) -> DaemonContext {
    let registry = Arc::new(BackendRegistry::new());
    registry.register_backend(stub_backend("echo", &["echo"]));
    let (list_changed_tx, _) = broadcast::channel(16);
    DaemonContext {
        backend: Arc::new(RealBackend::new(Arc::clone(&registry))),
        oauth_state: Arc::new(OAuthState::new()),
        listener_port: None,
        list_changed_tx,
        discoveries_kick: Arc::new(Notify::new()),
        sync_controller: make_sync_controller(registry, state.clone()),
        pending_alert: Arc::new(std::sync::Mutex::new(None)),
        state,
        registry_url: registry_url.to_string(),
    }
}

/// Bind an ephemeral port and start serving. Returns the base URL
/// (`http://127.0.0.1:<port>`) and the join handle (aborted on drop by the
/// caller if it cares — most tests just let the process exit).
///
/// Also returns the H3 local key `serve_on` generated/loaded for this
/// `ctx.state` (via `AppState::local_mcp_key`), so callers can attach it to
/// their requests — every route requires it (see `local_key_guard`).
async fn start_test_listener(ctx: DaemonContext) -> (String, String) {
    let local_key = ctx.state.local_mcp_key().expect("local_mcp_key");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let (addr, _handle) = super::serve_on(ctx, listener, port)
        .await
        .expect("serve_on should start");
    (format!("http://{addr}"), local_key)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
}

/// `X-Scenery-Local-Key` header name, matching
/// `super::LOCAL_KEY_HEADER` — redeclared here (rather than reaching into
/// the private const) so tests exercise the real wire header name a client
/// would actually send, the same way `reqwest::header::HOST`/`ORIGIN` are
/// used as real header values elsewhere in this file.
const LOCAL_KEY_HEADER: &str = "x-scenery-local-key";

// ── Hardening ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejects_wrong_host_header() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;

    let resp = http_client()
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .header(reqwest::header::HOST, "evil.example:1234")
        .header(LOCAL_KEY_HEADER, &local_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "wrong Host must be rejected");
}

#[tokio::test]
async fn rejects_any_origin_header() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;
    let port = base.rsplit(':').next().unwrap();

    let resp = http_client()
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .header(reqwest::header::HOST, format!("127.0.0.1:{port}"))
        .header(reqwest::header::ORIGIN, "http://evil.example")
        .header(LOCAL_KEY_HEADER, &local_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "any Origin header must be rejected");
}

#[tokio::test]
async fn get_mcp_returns_405() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;

    let resp = http_client()
        .get(format!("{base}/mcp"))
        .header(LOCAL_KEY_HEADER, &local_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
}

#[tokio::test]
async fn post_mcp_wrong_content_type_is_rejected() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;

    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .header(LOCAL_KEY_HEADER, &local_key)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 415);
}

#[tokio::test]
async fn post_mcp_oversize_body_is_rejected() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;

    let oversize = vec![b'a'; 2 * 1024 * 1024]; // 2 MiB > 1 MiB cap
    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(LOCAL_KEY_HEADER, &local_key)
        .body(oversize)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

// ── PRM ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn prm_body_matches_contract_exactly_on_both_paths() {
    let state = make_test_state();
    let ctx = make_ctx("https://dev.scenery.dev", state);
    let (base, local_key) = start_test_listener(ctx).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

    let expected = serde_json::json!({
        "resource": format!("http://127.0.0.1:{port}/mcp"),
        "authorization_servers": ["https://dev.scenery.dev"],
        "scopes_supported": ["mcp"],
        "bearer_methods_supported": ["header"],
    });

    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resp = http_client()
            .get(format!("{base}{path}"))
            .header(LOCAL_KEY_HEADER, &local_key)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "path {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, expected, "PRM body mismatch at {path}");
    }
}

// ── Auth ──────────────────────────────────────────────────────────────────

async fn post_mcp(
    base: &str,
    local_key: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> reqwest::Response {
    let mut req = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(LOCAL_KEY_HEADER, local_key);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.json(&body).send().await.unwrap()
}

fn initialize_request() -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
}

/// Seed `state` with an already-bound session for `sub`, so the token-checks
/// under test hit `bind_to_existing_session` rather than the first-run
/// device-exchange path.
fn seed_existing_session(state: &AppState, sub: &str) {
    state
        .set_sync_state("device_id", "already-registered")
        .unwrap();
    state.set_sync_state("mcp_user_id", sub).unwrap();
}

#[tokio::test]
async fn no_token_returns_401_with_bare_www_authenticate() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

    let resp = post_mcp(&base, &local_key, None, initialize_request()).await;
    assert_eq!(resp.status(), 401);
    let header = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        header,
        format!(
            "Bearer resource_metadata=\"http://127.0.0.1:{port}/.well-known/oauth-protected-resource\""
        ),
        "no-token WWW-Authenticate must have no error= param"
    );
}

async fn jwks_mock_server() -> mockito::ServerGuard {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .expect_at_least(1)
        .create_async()
        .await;
    server
}

async fn assert_invalid_token(base: &str, local_key: &str, token: &str) {
    let resp = post_mcp(base, local_key, Some(token), initialize_request()).await;
    assert_eq!(resp.status(), 401);
    let header = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        header.contains("error=\"invalid_token\""),
        "invalid-token WWW-Authenticate must carry error=invalid_token: {header}"
    );
}

#[tokio::test]
async fn bad_signature_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut token = TokenOpts::valid(&jwks.url()).encode();
    // Flip a character in the signature segment.
    let last = token.pop().unwrap();
    token.push(if last == 'A' { 'B' } else { 'A' });

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn wrong_alg_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.alg = Algorithm::HS256;
    opts.kid = Some(TEST_KID.to_string());
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn wrong_issuer_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.iss = "https://not-the-real-issuer.invalid".to_string();
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn wrong_audience_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.aud = "urn:someone-else:resource".to_string();
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn wrong_client_id_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.client_id = "some-other-client".to_string();
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn missing_scope_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.scope = "offline_access".to_string(); // no "mcp"
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn expired_token_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.exp_offset = -600;
    opts.nbf_offset = -700;
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

#[tokio::test]
async fn unknown_kid_triggers_refetch_then_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.kid = Some("some-other-kid-not-in-jwks".to_string());
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
    // jwks_mock_server's mock asserts `expect_at_least(1)` on drop — the
    // unknown kid must have caused an actual JWKS fetch attempt.
}

#[tokio::test]
async fn sub_mismatch_against_bound_session_is_rejected() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "the-real-bound-user");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let mut opts = TokenOpts::valid(&jwks.url());
    opts.sub = "a-different-user".to_string();
    let token = opts.encode();

    assert_invalid_token(&base, &local_key, &token).await;
}

// ── MCP round trip (through the real RealBackend) ───────────────────────

#[tokio::test]
async fn mcp_round_trip_initialize_list_call() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();

    let init_resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(init_resp.status(), 200);
    let init_body: serde_json::Value = init_resp.json().await.unwrap();
    assert_eq!(init_body["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init_body["result"]["serverInfo"]["name"], "scenery");

    let list_resp = post_mcp(
        &base,
        &local_key,
        Some(&token),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(list_resp.status(), 200);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let names: Vec<&str> = list_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"echo__echo"), "got {names:?}");

    let call_resp = post_mcp(
        &base,
        &local_key,
        Some(&token),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "echo__echo", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(call_resp.status(), 200);
    let call_body: serde_json::Value = call_resp.json().await.unwrap();
    assert!(call_body["result"]["content"].as_array().is_some());
}

#[tokio::test]
async fn notification_without_id_returns_202_no_body() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();
    let resp = post_mcp(
        &base,
        &local_key,
        Some(&token),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await;
    assert_eq!(resp.status(), 202);
    let bytes = resp.bytes().await.unwrap();
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn unknown_method_returns_jsonrpc_method_not_found_at_http_200() {
    // §9 (Phase 0 spike): the v2 runtime's `server/discover` probe must get
    // a JSON-RPC -32601 at HTTP 200, not an HTTP-level error, so Claude Code
    // falls back cleanly to `initialize`.
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();
    let resp = post_mcp(
        &base,
        &local_key,
        Some(&token),
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32601);
}

// ── Device exchange (first-run) ───────────────────────────────────────────

#[tokio::test]
async fn first_run_device_exchange_succeeds_and_starts_session() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .create_async()
        .await;
    let exchange_mock = server
        .mock("POST", "/oauth/device-exchange")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "device_id": "device-abc",
                "access_token": "portal-access-token",
                "refresh_token": "portal-refresh-token",
            })
            .to_string(),
        )
        .create_async()
        .await;

    let state = make_test_state();
    // No device_id / mcp_user_id seeded — this is a brand-new daemon.
    let ctx = make_ctx(&server.url(), state.clone());
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&server.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(
        resp.status(),
        200,
        "first-run device-exchange should let the call through"
    );

    exchange_mock.assert_async().await;
    assert_eq!(
        state.get_sync_state("device_id").unwrap(),
        Some("device-abc".to_string())
    );
    assert_eq!(
        state.get_sync_state("mcp_user_id").unwrap(),
        Some("user-1".to_string())
    );
}

#[tokio::test]
async fn device_exchange_conflict_without_local_session_fails_closed() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .create_async()
        .await;
    server
        .mock("POST", "/oauth/device-exchange")
        .with_status(409)
        .create_async()
        .await;

    let state = make_test_state();
    let ctx = make_ctx(&server.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&server.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(
        resp.status(),
        401,
        "409 with no local session must fail closed"
    );
}

#[tokio::test]
async fn device_exchange_revoked_marks_device_revoked_locally() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .create_async()
        .await;
    server
        .mock("POST", "/oauth/device-exchange")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(serde_json::json!({"code": "device_revoked"}).to_string())
        .create_async()
        .await;

    let state = make_test_state();
    let ctx = make_ctx(&server.url(), state.clone());
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&server.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(resp.status(), 401);
    assert!(
        state.get_sync_state("device_revoked_at").unwrap().is_some(),
        "device_revoked_at must be stamped locally after a device_revoked response"
    );
}

#[tokio::test]
async fn already_revoked_device_rejects_before_any_exchange_attempt() {
    let mut server = mockito::Server::new_async().await;
    // No mocks registered for /oauth/device-exchange — if the daemon called
    // it despite the device being revoked, mockito would 501 and we'd still
    // expect 401, but we also assert zero calls were attempted below.
    let exchange_mock = server
        .mock("POST", "/oauth/device-exchange")
        .with_status(200)
        .expect(0)
        .create_async()
        .await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .create_async()
        .await;

    let state = make_test_state();
    state.mark_device_revoked().unwrap();
    let ctx = make_ctx(&server.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&server.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(resp.status(), 401);
    exchange_mock.assert_async().await;
}

// ── H3: per-user local key ───────────────────────────────────────────────
//
// Correction from an empirical run against real Claude Code 2.1.238: its
// OAuth discovery/code-exchange runs in a separate Bun helper process that
// never sends custom headers, so the local key must NOT gate the PRM GET
// routes (that would break login outright). The main client *does* send it
// on every `/mcp` POST, which is what these tests hold the line on instead.

#[tokio::test]
async fn prm_get_without_local_key_still_succeeds() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, _local_key) = start_test_listener(ctx).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

    let resp = http_client()
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .header(reqwest::header::HOST, format!("127.0.0.1:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "PRM must stay public — Claude Code's OAuth helper process never sends the local key"
    );
}

#[tokio::test]
async fn post_mcp_missing_local_key_is_rejected_and_never_triggers_device_exchange() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/oauth/jwks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(jwks_body().to_string())
        .create_async()
        .await;
    // Brand-new daemon (no device_id seeded) — if the local key guard ran
    // after (or not at all before) first-run device exchange, this request
    // would reach `device_exchange_first_run` and call this endpoint.
    let exchange_mock = server
        .mock("POST", "/oauth/device-exchange")
        .with_status(200)
        .expect(0)
        .create_async()
        .await;

    let state = make_test_state();
    let ctx = make_ctx(&server.url(), state);
    let (base, _local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&server.url()).encode();
    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(&token)
        .json(&initialize_request())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "missing local key on POST /mcp must be rejected"
    );
    let body = resp.bytes().await.unwrap();
    assert!(
        body.is_empty(),
        "H3: a missing/wrong local key must leak nothing in the body, got {body:?}"
    );
    exchange_mock.assert_async().await;
}

#[tokio::test]
async fn post_mcp_wrong_local_key_is_rejected_with_403_and_no_body_detail() {
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;
    assert!(!local_key.is_empty());

    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(LOCAL_KEY_HEADER, "not-the-right-key")
        .json(&initialize_request())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "wrong local key header must be rejected"
    );
    let body = resp.bytes().await.unwrap();
    assert!(body.is_empty(), "got {body:?}");
}

#[tokio::test]
async fn post_mcp_wrong_local_key_different_length_is_also_rejected() {
    // Exercises the length-mismatch short-circuit branch of the
    // constant-time comparison separately from the equal-length branch
    // above.
    let state = make_test_state();
    let ctx = make_ctx("https://example.invalid", state);
    let (base, local_key) = start_test_listener(ctx).await;
    assert!(!local_key.is_empty());

    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(LOCAL_KEY_HEADER, "short")
        .json(&initialize_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn local_key_guard_rejects_even_a_fully_valid_bearer_token() {
    // H3 ordering requirement: the local key check must run BEFORE token
    // validation — a request with a perfectly valid, correctly-bound access
    // token must still be rejected if the local key is missing/wrong. If
    // this regresses to "local key OR valid token", a co-resident OS user
    // with no key could still reach governed tools by stealing a token some
    // other way.
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, _local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();
    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(LOCAL_KEY_HEADER, "wrong-local-key")
        .bearer_auth(&token)
        .json(&initialize_request())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "a valid bearer token must not bypass a missing/wrong local key"
    );
}

#[tokio::test]
async fn post_mcp_correct_local_key_allows_the_request_through() {
    // The positive case, so a bug that made the guard reject everything
    // (rather than just unauthenticated/mismatched requests) would show up
    // here rather than only as every *other* test mysteriously failing.
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&token), initialize_request()).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn post_mcp_local_key_header_matches_case_insensitively() {
    let jwks = jwks_mock_server().await;
    let state = make_test_state();
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state);
    let (base, local_key) = start_test_listener(ctx).await;

    let token = TokenOpts::valid(&jwks.url()).encode();
    // HTTP header *names* are case-insensitive by spec; send a deliberately
    // mixed-case variant of the real header name a client might use.
    let resp = http_client()
        .post(format!("{base}/mcp"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("X-Scenery-LOCAL-key", &local_key)
        .bearer_auth(&token)
        .json(&initialize_request())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the local key header name must match regardless of case"
    );
}

// ── M6: logout / re-pair as a different user ─────────────────────────────

#[tokio::test]
async fn logout_then_login_as_different_user_succeeds_without_sub_mismatch() {
    let mut jwks = jwks_mock_server().await;
    let state = make_test_state();
    // user-1 is logged in and bound.
    seed_existing_session(&state, "user-1");
    let ctx = make_ctx(&jwks.url(), state.clone());
    let (base, local_key) = start_test_listener(ctx).await;

    let user_1_token = TokenOpts::valid(&jwks.url()).encode();
    let resp = post_mcp(&base, &local_key, Some(&user_1_token), initialize_request()).await;
    assert_eq!(resp.status(), 200, "user-1 should work before logout");

    // `scenery auth logout` calls exactly this (plus clear_tokens, not
    // exercised by this daemon-level test).
    state.clear_device_session().expect("clear_device_session");

    // A different user's valid MCP token must now go through the first-run
    // device-exchange path again (no bound session left) rather than being
    // rejected as a sub_mismatch against the stale user-1 binding.
    let mut user_2_opts = TokenOpts::valid(&jwks.url());
    user_2_opts.sub = "user-2".to_string();
    let user_2_token = user_2_opts.encode();

    let device_exchange_mock = jwks
        .mock("POST", "/oauth/device-exchange")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "device_id": "device-user-2",
                "access_token": "portal-access-token-user-2",
                "refresh_token": "portal-refresh-token-user-2",
            })
            .to_string(),
        )
        .create_async()
        .await;

    let resp = post_mcp(&base, &local_key, Some(&user_2_token), initialize_request()).await;
    assert_eq!(
        resp.status(),
        200,
        "user-2 must be able to log in via MCP after user-1's logout, with no stale \
         sub_mismatch"
    );
    device_exchange_mock.assert_async().await;
    assert_eq!(
        state.get_sync_state("mcp_user_id").unwrap(),
        Some("user-2".to_string())
    );
}

// ── M9: issuer must be https:// unless explicitly overridden ────────────

/// Serializes tests that mutate the process-global
/// `SCENERY_MCP_HTTP_ALLOW_INSECURE_ISSUER` env var.
static MCP_HTTP_ISSUER_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn check_issuer_scheme_requires_https_without_override() {
    let _guard = MCP_HTTP_ISSUER_ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("SCENERY_MCP_HTTP_ALLOW_INSECURE_ISSUER");

    assert!(super::check_issuer_scheme("https://dev.scenery.dev"));
    assert!(!super::check_issuer_scheme("http://dev.scenery.dev"));
}

#[tokio::test]
async fn check_issuer_scheme_allows_insecure_with_override_env() {
    let _guard = MCP_HTTP_ISSUER_ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("SCENERY_MCP_HTTP_ALLOW_INSECURE_ISSUER", "1");
    let allowed = super::check_issuer_scheme("http://127.0.0.1:1234");
    std::env::remove_var("SCENERY_MCP_HTTP_ALLOW_INSECURE_ISSUER");
    assert!(
        allowed,
        "the explicit dev override must allow a non-https issuer"
    );
}

// The guard below is held across the `.await` on `start_listener`, which
// clippy flags on principle (`await_holding_lock`). It's safe here: this
// test's registry_url is insecure with the override unset, so
// `start_listener` returns `Ok(None)` from its synchronous issuer check
// before ever reaching an actual yield point (`bind_listener`'s
// `TcpListener::bind`) — the awaited future never truly suspends, so there
// is nothing else on this single-threaded test runtime that could be
// blocked by holding the guard a little longer.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn start_listener_refuses_insecure_issuer_and_binds_nothing() {
    let _guard = MCP_HTTP_ISSUER_ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("SCENERY_MCP_HTTP_ALLOW_INSECURE_ISSUER");

    let state = make_test_state();
    // make_ctx's registry_url is plain http:// — M9 must refuse to start.
    let ctx = make_ctx("http://insecure.invalid", state.clone());

    let result = super::start_listener(ctx).await.expect("not an error");
    assert!(
        result.is_none(),
        "an insecure issuer must stay stdio-only, never bind a port"
    );
    assert_eq!(
        state
            .get_sync_state(scenery_core::state::MCP_HTTP_PORT_SYNC_STATE_KEY)
            .unwrap(),
        None,
        "refusing before bind_listener means no port is ever persisted"
    );
}

/// Only Claude Code connects over this transport, and `tools/list` carries no
/// clientInfo, so the request is tagged before dispatch. Without the tag
/// Claude Code would get every governed skill twice: natively and as a tool.
#[test]
fn tools_list_is_tagged_as_coming_from_claude_code() {
    let request = |method: &str| JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: method.to_string(),
        params: serde_json::json!({}),
    };

    let list = super::from_claude_code(request("tools/list"));
    assert!(scenery_mcp::protocol::client_loads_native_skills(
        &list.params
    ));

    let call = super::from_claude_code(request("tools/call"));
    assert_eq!(call.params, serde_json::json!({}));
}

/// F14: when the persisted port is held by another process, the daemon must
/// fail closed — Claude Code's HTTP entry for that port goes back to stdio,
/// the local key it may have sent there is rotated, the user is alerted,
/// and the squatted port is never bound or kept as the pinned port.
///
/// Reverting the entry goes through `write_mcp_entry`, which records a
/// restore-journal entry under the data dir resolved from `$HOME`, so `HOME`
/// points at a temp dir for the whole call (holding the shared `ENV_MUTEX`
/// across the `.await` is the point) and is restored before any assertion.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn pinned_port_held_by_another_process_fails_closed() {
    use scenery_core::state::MCP_HTTP_PORT_SYNC_STATE_KEY;

    let _guard = crate::managed_paths::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let original_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let squatted = squatter.local_addr().unwrap().port();

    let state = make_test_state();
    state
        .set_sync_state(MCP_HTTP_PORT_SYNC_STATE_KEY, &squatted.to_string())
        .unwrap();
    let old_key = state.local_mcp_key().unwrap();

    let claude_json = home.path().join(".claude.json");
    let root = serde_json::json!({
        "mcpServers": {
            "scenery": scenery_mcp::setup::build_claude_code_http_entry(squatted, 39200, &old_key)
        }
    });
    std::fs::write(&claude_json, serde_json::to_string(&root).unwrap()).unwrap();

    let alert = std::sync::Mutex::new(None);
    let bound = super::bind_listener(&state, Some(&claude_json), &alert).await;

    match original_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    drop(squatter);

    let bound = bound.expect("a conflict is handled, not an error");
    if let Some((_, port)) = &bound {
        assert_ne!(*port, squatted, "the squatted port must never be bound");
    }
    let persisted = state.get_sync_state(MCP_HTTP_PORT_SYNC_STATE_KEY).unwrap();
    assert_ne!(persisted, Some(squatted.to_string()));
    assert_ne!(
        state.local_mcp_key().unwrap(),
        old_key,
        "local key must rotate"
    );

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    let entry = &after["mcpServers"]["scenery"];
    assert!(
        entry.get("type").is_none() && entry.get("headers").is_none(),
        "{entry}"
    );
    assert!(entry.get("command").is_some(), "{entry}");

    let message = alert.lock().unwrap().clone().map(|(m, _)| m);
    assert!(
        message
            .as_deref()
            .is_some_and(|m| m.contains(&squatted.to_string())),
        "user must be alerted: {message:?}"
    );
}
