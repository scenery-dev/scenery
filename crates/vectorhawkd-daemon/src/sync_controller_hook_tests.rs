//! Integration tests for `SyncControllerHook` (R1): confirms that a
//! `BackendRegistry`'s `notify_tokens_saved` — the hook
//! `tools::handle_login_with_oauth`'s background task fires after the
//! `vectorhawk_login` MCP tool saves fresh tokens — actually reaches
//! `SyncController::ensure_started` end to end, the same way `auth/reload`
//! already does for the CLI's `vectorhawk auth login` path. Modeled on
//! `auth_dispatch_tests.rs`'s `reload_returns_inactive_without_token_and_is_idempotent`
//! and `get_portal_session_returns_full_session_on_success`.
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
