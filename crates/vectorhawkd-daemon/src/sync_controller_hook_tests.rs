//! Integration tests for `SyncControllerHook` (R1): confirms that a
//! `BackendRegistry`'s `notify_tokens_saved` — the hook
//! `tools::handle_login_with_oauth`'s background task fires after the
//! `vectorhawk_login` MCP tool saves fresh tokens — actually reaches
//! `SyncController::ensure_started` end to end, the same way `auth/reload`
//! already does for the CLI's `vectorhawk auth login` path. Modeled on
//! `auth_dispatch_tests.rs`'s `reload_returns_inactive_without_token_and_is_idempotent`
//! and `get_portal_session_returns_full_session_on_success`.
//!
//! Also covers `ensure_started`'s credential-aware restart behavior directly
//! (the "needs re-auth" portal-badge bug): a daemon whose SSE loop is already
//! running must pick up a *newly saved* access token from a fresh `auth
//! pair`/`auth login`/`auth token` without a `daemon restart`, while a
//! second call with *unchanged* credentials must stay a cheap no-op (the
//! `hook_registers_device_exactly_once_even_when_fired_twice` test above
//! already covers that half end to end via the hook).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tokio::sync::broadcast;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::aggregator::BackendRegistry;

use crate::{SyncController, SyncControllerHook};

/// Force the SQLite fallback so these tests don't pollute the real macOS
/// keychain. Mirrors the identical helper in `auth_dispatch_tests.rs`.
struct KeychainOff {
    _g: std::sync::MutexGuard<'static, ()>,
}
static KEYCHAIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl KeychainOff {
    fn enable() -> Self {
        let _g = KEYCHAIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("VECTORHAWK_DISABLE_KEYCHAIN", "1");
        KeychainOff { _g }
    }
}
impl Drop for KeychainOff {
    fn drop(&mut self) {
        std::env::remove_var("VECTORHAWK_DISABLE_KEYCHAIN");
    }
}

fn bootstrap_state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    (Arc::new(state), tmp)
}

/// End to end: a `vectorhawk_login` MCP-tool login (simulated by saving
/// tokens directly, as the background task in `tools.rs` does) plus a
/// `notify_tokens_saved` call registers the device — without any CLI
/// `auth login`/`auth pair` call or daemon restart — and a second call is a
/// no-op that does not hit the registration endpoint again.
#[tokio::test]
async fn hook_registers_device_exactly_once_even_when_fired_twice() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let mock = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-abc123"}"#)
        .expect(1) // registration must happen at most once, even if the hook fires twice
        .create_async()
        .await;

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "acc-tok", "ref-tok")
        .expect("save_tokens");

    let (list_changed_tx, _rx) = broadcast::channel(16);
    let registry = Arc::new(BackendRegistry::new());
    let sync_controller = Arc::new(SyncController::new(
        registry_url.clone(),
        Arc::clone(&state),
        list_changed_tx,
        Arc::clone(&registry),
        None,
    ));
    registry.set_tokens_saved_hook(Arc::new(SyncControllerHook(Arc::clone(&sync_controller))));

    // Simulates the vectorhawk_login MCP-tool background task's call after
    // `auth::save_tokens` succeeds (tools.rs's `handle_login_with_oauth`).
    registry.notify_tokens_saved().await;
    // A second login attempt (or any other caller) firing the same hook again
    // must not re-register the device — mirrors `auth/reload`'s idempotency.
    registry.notify_tokens_saved().await;

    assert!(
        sync_controller.ensure_started().await,
        "sync should be active after the hook registered the device"
    );

    let device_id = state
        .get_sync_state("device_id")
        .expect("get_sync_state should not error")
        .expect("device_id should be persisted after registration");
    assert_eq!(device_id, "dev-abc123");

    mock.assert_async().await;
}

/// No auth token saved yet (e.g. a race where the hook fires before the
/// background task's `save_tokens` call lands) — must not panic and must
/// leave sync inactive, matching `auth/reload`'s existing behavior for the
/// same case.
#[tokio::test]
async fn hook_is_a_safe_noop_without_a_saved_token() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();
    let registry_url = "https://example.invalid".to_string();

    let (list_changed_tx, _rx) = broadcast::channel(16);
    let registry = Arc::new(BackendRegistry::new());
    let sync_controller = Arc::new(SyncController::new(
        registry_url,
        Arc::clone(&state),
        list_changed_tx,
        Arc::clone(&registry),
        None,
    ));
    registry.set_tokens_saved_hook(Arc::new(SyncControllerHook(Arc::clone(&sync_controller))));

    registry.notify_tokens_saved().await;

    assert!(
        !sync_controller.ensure_started().await,
        "sync must stay inactive when no token was ever saved"
    );
}

/// The actual bug fix under test: sync starts successfully with `tok-1`
/// (device `dev-1`), then the user re-authenticates — e.g. `vectorhawk auth
/// pair` after the daemon's SSE connection went stale — saving a *different*
/// access token (`tok-2`) while the SSE task from the first start is still
/// running. Before the fix, `ensure_started`'s `guard.is_some()` check made
/// the second call an unconditional no-op: the daemon kept looping on
/// `tok-1` forever (only a full `daemon restart` picked up `tok-2`). After
/// the fix, `ensure_started` detects the changed on-disk fingerprint, tears
/// down the stale SSE-client + reconciler tasks, and reconnects with
/// `tok-2` — asserted here by mocking `GET /api/sync/events` separately per
/// bearer token and confirming *both* are actually hit.
#[tokio::test]
async fn ensure_started_reconnects_sse_when_saved_token_changes() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    // `register_device` short-circuits once `sync_state["device_id"]` is
    // set (see lib.rs), so this must be hit at most once even though
    // `ensure_started` is called twice below — the credential-change restart
    // must not re-register the device over the network.
    let register_mock = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-1"}"#)
        .expect(1)
        .create_async()
        .await;

    let sse_tok1 = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-1")
        .match_header("x-device-id", "dev-1")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    let sse_tok2 = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-2")
        .match_header("x-device-id", "dev-1")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-1", "ref-1")
        .expect("save_tokens tok-1");

    let (list_changed_tx, _rx) = broadcast::channel(16);
    let registry = Arc::new(BackendRegistry::new());
    let sync_controller = Arc::new(SyncController::new(
        registry_url.clone(),
        Arc::clone(&state),
        list_changed_tx,
        Arc::clone(&registry),
        None,
    ));

    assert!(
        sync_controller.ensure_started().await,
        "sync should start with tok-1"
    );
    // Let the spawned SSE task actually dial in with tok-1 before asserting.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sse_tok1.assert_async().await;

    // Simulate `vectorhawk auth pair` (or `auth login`/`auth token`) saving a
    // fresh access token while the daemon's SSE loop from above is still
    // running — the exact "needs re-auth" reproduction.
    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-2", "ref-2")
        .expect("save_tokens tok-2");

    assert!(
        sync_controller.ensure_started().await,
        "sync should still report active after reload with new credentials"
    );
    // Let the freshly-spawned replacement SSE task dial in with tok-2.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sse_tok2.assert_async().await;

    register_mock.assert_async().await;
}

/// Round-2 regression: the SSE client's own internal 401-triggered token
/// refresh (`sse_client::try_refresh_token`) silently rotates the on-disk
/// access token via `save_tokens` — bypassing the `notify_tokens_saved` hook
/// entirely, since it's not a user-initiated auth action. Before this fix,
/// `RunningSync`'s credential fingerprint captured the token ONCE at spawn
/// time, so the next legitimate `ensure_started()` call (a genuinely
/// unrelated `auth pair`/`auth login`, or the `vectorhawk_login` hook firing
/// again) compared the now-refreshed on-disk token against the STALE
/// spawn-time token, saw a spurious mismatch, and tore down + restarted a
/// perfectly healthy SSE connection for no reason.
///
/// This drives the real internal refresh path end to end: the SSE mock
/// returns 401 once, the mocked `/portal/auth/refresh` endpoint (matching
/// `AuthClient::refresh`'s shape — see `refresh_loop_tests.rs`) rotates
/// `tok-1` -> `tok-1-refreshed` with `device_id` unchanged, and the SSE
/// client reconnects and stays up. A subsequent `ensure_started()` call must
/// then be a true no-op: same task still running (proven by comparing the
/// spawned SSE task's `tokio::task::Id` before and after — a much stronger
/// assertion than "the mock got hit again", since a spurious restart reuses
/// the very same already-refreshed on-disk token and so would be invisible
/// to a plain request-count check).
#[tokio::test]
async fn ensure_started_is_noop_after_sse_clients_own_internal_refresh() {
    let _guard = KeychainOff::enable();
    let (state, _tmp) = bootstrap_state();

    let mut server = mockito::Server::new_async().await;
    let registry_url = server.url();

    let register_mock = server
        .mock("POST", "/api/devices/register")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"device_id":"dev-1"}"#)
        .expect(1)
        .create_async()
        .await;

    // First connection attempt: the SSE client dials in with the original
    // token and is told it's unauthorized (simulating the access token
    // having expired server-side moments after sync started).
    let sse_401 = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-1")
        .match_header("x-device-id", "dev-1")
        .with_status(401)
        .expect(1)
        .create_async()
        .await;

    // `sse_client::try_refresh_token` calls `AuthClient::refresh`, which
    // POSTs to `/portal/auth/refresh` with `{"refresh_token": "..."}` (see
    // `AuthClient::refresh_detailed` in vectorhawkd-core/src/auth.rs and its
    // mock shape in `refresh_loop_tests.rs`).
    let refresh_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"access_token":"tok-1-refreshed","refresh_token":"ref-1-refreshed","token_type":"bearer"}"#,
        )
        .expect(1)
        .create_async()
        .await;

    // After the internal refresh, the SSE client immediately reconnects with
    // the new token and this time succeeds (empty body -> clean EOF -> the
    // client's normal reconnect loop keeps re-dialing this same mock, which
    // is fine — we don't rely on request counts to prove no restart below).
    let sse_ok = server
        .mock("GET", "/api/sync/events")
        .match_header("authorization", "Bearer tok-1-refreshed")
        .match_header("x-device-id", "dev-1")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .expect_at_least(1)
        .create_async()
        .await;

    vectorhawkd_core::auth::save_tokens(&state, &registry_url, "tok-1", "ref-1")
        .expect("save_tokens tok-1");

    let (list_changed_tx, _rx) = broadcast::channel(16);
    let registry = Arc::new(BackendRegistry::new());
    let sync_controller = Arc::new(SyncController::new(
        registry_url.clone(),
        Arc::clone(&state),
        list_changed_tx,
        Arc::clone(&registry),
        None,
    ));

    assert!(
        sync_controller.ensure_started().await,
        "sync should start with tok-1"
    );

    // Give the SSE task time to: dial with tok-1, get 401, refresh via
    // /portal/auth/refresh, and reconnect with tok-1-refreshed.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    sse_401.assert_async().await;
    refresh_mock.assert_async().await;
    sse_ok.assert_async().await;

    // The on-disk token has genuinely rotated — confirms the internal
    // refresh actually happened, not just that the mocks were configured.
    let on_disk = vectorhawkd_core::auth::load_all_tokens(&state)
        .expect("load_all_tokens")
        .into_iter()
        .find(|r| r.registry_url == registry_url)
        .expect("token row should exist")
        .access_token;
    assert_eq!(
        on_disk, "tok-1-refreshed",
        "internal refresh should have rotated the on-disk token"
    );

    // Capture the identity of the currently-running SSE task before the
    // second `ensure_started()` call.
    let task_id_before = {
        let guard = sync_controller.handle.lock().await;
        guard
            .as_ref()
            .expect("sync subsystem should still be running")
            .sse_abort
            .id()
    };

    // The actual assertion: a later `ensure_started()` call (device_id
    // unchanged) must be a true no-op — it must NOT abort/restart the SSE
    // task that already picked up the refreshed token on its own.
    assert!(
        sync_controller.ensure_started().await,
        "sync should still report active"
    );

    let task_id_after = {
        let guard = sync_controller.handle.lock().await;
        guard
            .as_ref()
            .expect("sync subsystem should still be running")
            .sse_abort
            .id()
    };

    assert_eq!(
        task_id_before, task_id_after,
        "ensure_started must not abort/restart the SSE task after the \
         client's own internal refresh — same on-disk token, same device_id, \
         should be a no-op (this is what fails against the pre-fix \
         spawn-time-frozen fingerprint)"
    );

    // No second device registration, and neither endpoint the running
    // connection wasn't already using should have been touched.
    register_mock.assert_async().await;
}
