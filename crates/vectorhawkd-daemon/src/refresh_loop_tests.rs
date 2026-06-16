//! Unit tests for the daemon token refresh loop.
//!
//! Tests drive `refresh_one_tick` directly — no async runtime needed — so
//! we can exercise the logic without waiting for the 60-second interval.

#![allow(clippy::unwrap_used)]

use camino::Utf8PathBuf;
use mockito::Server;
use std::time::{SystemTime, UNIX_EPOCH};
use vectorhawkd_core::{
    auth::{load_tokens, save_tokens, AuthClient},
    state::AppState,
};

use crate::refresh_one_tick;

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-daemon-refresh-{label}-{nanos}")),
    )
    .expect("temp path should be utf-8")
}

/// Force the SQLite fallback so refresh-loop tests don't pollute the real
/// macOS keychain. Holds a global mutex so concurrent tests can't race
/// each other's env-var set/clear (cargo test runs in parallel by default).
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

/// Build a minimal JWT with a given `exp` Unix timestamp.
/// No signature — only used to test expiry logic.
fn make_jwt_with_exp(exp: u64) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
    let payload = URL_SAFE_NO_PAD.encode(format!("{{\"sub\":\"u1\",\"exp\":{exp}}}").as_bytes());
    format!("{header}.{payload}.fakesig")
}

/// A near-expiry access token (expires in 2 minutes) causes refresh_one_tick
/// to call the refresh endpoint and overwrite the stored token.
#[test]
fn refresh_one_tick_rotates_near_expiry_token() {
    let _guard = KeychainOff::enable();
    let root = temp_root("rotate");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    // Access token that expires in 2 minutes (< 5 min threshold).
    let near_expiry_access = make_jwt_with_exp(now + 120);

    let mut server = Server::new();
    let registry_url = server.url();

    let _refresh_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"access_token":"rotated_acc","refresh_token":"rotated_ref","token_type":"bearer"}"#,
        )
        .create();

    // Save the near-expiry token.
    save_tokens(&state, &registry_url, &near_expiry_access, "old_refresh").expect("save_tokens");

    // Run one tick of the refresh loop.
    refresh_one_tick(&state, &registry_url).expect("refresh_one_tick should not error");

    // Verify the token was rotated in SQLite.
    let loaded = load_tokens(&state, &registry_url)
        .expect("load_tokens")
        .expect("token row should exist");

    assert_eq!(
        loaded.access_token, "rotated_acc",
        "access token should be updated after refresh"
    );
    assert_eq!(
        loaded.refresh_token, "rotated_ref",
        "refresh token should be updated after refresh"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A token that is NOT near expiry should not trigger a refresh call.
#[test]
fn refresh_one_tick_skips_healthy_token() {
    let _guard = KeychainOff::enable();
    let root = temp_root("skip");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    // Access token that expires in 10 minutes (> 5 min threshold).
    let healthy_access = make_jwt_with_exp(now + 600);

    let mut server = Server::new();
    let registry_url = server.url();

    // The refresh endpoint should NOT be called.
    let refresh_mock = server
        .mock("POST", "/portal/auth/refresh")
        .expect(0)
        .create();

    save_tokens(&state, &registry_url, &healthy_access, "ref_token").expect("save_tokens");

    refresh_one_tick(&state, &registry_url).expect("refresh_one_tick should not error");

    // Token should be unchanged.
    let loaded = load_tokens(&state, &registry_url)
        .expect("load_tokens")
        .expect("token should exist");

    assert_eq!(
        loaded.access_token, healthy_access,
        "healthy token must not be modified"
    );

    refresh_mock.assert();

    let _ = std::fs::remove_dir_all(&root);
}

/// A failed refresh call should log WARN and continue — refresh_one_tick
/// should return Ok(()) even when the HTTP call fails.
#[test]
fn refresh_one_tick_continues_after_refresh_failure() {
    let _guard = KeychainOff::enable();
    let root = temp_root("fail");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    let near_expiry_access = make_jwt_with_exp(now + 120);

    let mut server = Server::new();
    let registry_url = server.url();

    let _fail_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(401)
        .with_body(r#"{"error":"invalid_refresh_token"}"#)
        .create();

    save_tokens(&state, &registry_url, &near_expiry_access, "bad_refresh").expect("save_tokens");

    // Should not return Err even though the HTTP call failed.
    let result = refresh_one_tick(&state, &registry_url);
    assert!(
        result.is_ok(),
        "refresh_one_tick must return Ok on HTTP failure: {result:?}"
    );

    // Original token should still be present (not cleared).
    let loaded = load_tokens(&state, &registry_url).expect("load_tokens");
    assert!(
        loaded.is_some(),
        "token row should still exist after failed refresh"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A persistent 401 from /portal/auth/refresh must drive backoff: the next
/// tick must NOT hit the endpoint again until next_refresh_attempt_at passes.
/// This protects against the 1-hot-loop-per-60s bug seen in production logs.
#[test]
fn refresh_one_tick_backs_off_after_401() {
    let _guard = KeychainOff::enable();
    let root = temp_root("backoff");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    let near_expiry_access = make_jwt_with_exp(now + 120);

    let mut server = Server::new();
    let registry_url = server.url();

    // The 401 mock must be hit exactly ONCE across both ticks. If backoff is
    // broken the second tick will hit it too and the expectation fails.
    let fail_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(401)
        .with_body(r#"{"error":"invalid_refresh_token"}"#)
        .expect(1)
        .create();

    save_tokens(&state, &registry_url, &near_expiry_access, "dead_refresh").expect("save_tokens");

    // First tick: hits the endpoint, gets 401, records failure + 60s backoff.
    refresh_one_tick(&state, &registry_url).expect("first tick");

    let after_first = load_tokens(&state, &registry_url)
        .expect("load")
        .expect("row");
    assert_eq!(after_first.refresh_failures, 1, "should record one failure");
    assert_eq!(
        after_first.last_refresh_status.as_deref(),
        Some("auth_failed")
    );
    assert!(
        after_first
            .next_refresh_attempt_at
            .map(|t| t > now as i64)
            .unwrap_or(false),
        "next_refresh_attempt_at must be in the future after a 401"
    );

    // Second tick (immediately): backoff window is still active, must skip
    // the HTTP call entirely. The mockito `expect(1)` assertion below
    // verifies the endpoint was hit only once total.
    refresh_one_tick(&state, &registry_url).expect("second tick");

    fail_mock.assert();

    let _ = std::fs::remove_dir_all(&root);
}

/// A successful refresh after a prior failure must reset the backoff
/// counters so future refreshes resume the normal cadence.
#[test]
fn refresh_one_tick_resets_backoff_on_success() {
    let _guard = KeychainOff::enable();
    let root = temp_root("reset");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    let near_expiry_access = make_jwt_with_exp(now + 120);

    let mut server = Server::new();
    let registry_url = server.url();

    let _ok_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(200)
        .with_body(
            serde_json::json!({
                "access_token": make_jwt_with_exp(now + 7200),
                "refresh_token": "fresh_refresh",
            })
            .to_string(),
        )
        .create();

    save_tokens(&state, &registry_url, &near_expiry_access, "old_refresh").expect("save_tokens");

    // Simulate a prior failure state on the row.
    vectorhawkd_core::auth::record_refresh_failure(
        &state,
        &registry_url,
        "auth_failed",
        Some((now - 1) as i64), // already expired backoff window
    )
    .expect("record");

    refresh_one_tick(&state, &registry_url).expect("tick");

    let after = load_tokens(&state, &registry_url)
        .expect("load")
        .expect("row");
    assert_eq!(after.refresh_failures, 0, "success must reset counter");
    assert_eq!(after.last_refresh_status.as_deref(), Some("ok"));
    assert!(after.next_refresh_attempt_at.is_none());
    assert_eq!(after.refresh_token, "fresh_refresh");

    let _ = std::fs::remove_dir_all(&root);
}

/// With no tokens stored, refresh_one_tick is a no-op.
#[test]
fn refresh_one_tick_with_no_tokens_is_noop() {
    let _guard = KeychainOff::enable();
    let root = temp_root("empty");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let result = refresh_one_tick(&state, "https://registry.vectorhawk.ai");
    assert!(
        result.is_ok(),
        "refresh_one_tick with empty table should succeed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// refresh_one_tick handles multiple token rows: rotates near-expiry ones,
/// leaves healthy ones unchanged.
#[test]
fn refresh_one_tick_handles_multiple_rows() {
    let _guard = KeychainOff::enable();
    let root = temp_root("multi");
    let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    let near_expiry = make_jwt_with_exp(now + 120);
    let healthy = make_jwt_with_exp(now + 600);

    let mut server = Server::new();
    let near_expiry_url = server.url();

    let _refresh_mock = server
        .mock("POST", "/portal/auth/refresh")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"access_token":"rotated","refresh_token":"rotated_ref","token_type":"bearer"}"#,
        )
        .create();

    // Save near-expiry token for the mock server's registry.
    save_tokens(&state, &near_expiry_url, &near_expiry, "ref1").expect("save near_expiry");

    // Save healthy token for a different (unreachable) registry.
    // It must NOT be refreshed, so its refresh URL should never be hit.
    let healthy_url = "http://127.0.0.1:1"; // unreachable — will error if called
    save_tokens(&state, healthy_url, &healthy, "ref2").expect("save healthy");

    refresh_one_tick(&state, &near_expiry_url).expect("refresh_one_tick should succeed");

    // Near-expiry token should be rotated.
    let rotated = load_tokens(&state, &near_expiry_url)
        .expect("load")
        .expect("row exists");
    assert_eq!(rotated.access_token, "rotated");

    // Healthy token should be untouched.
    let untouched = load_tokens(&state, healthy_url)
        .expect("load")
        .expect("row exists");
    assert_eq!(untouched.access_token, healthy);

    let _ = std::fs::remove_dir_all(&root);
}

/// `AuthClient::needs_refresh` boundary: exactly at the 300s threshold.
#[test]
fn needs_refresh_boundary_at_exactly_300s() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();

    // Exactly 300 seconds = not quite past threshold (exp - now = 300, not < 300).
    let token_300 = make_jwt_with_exp(now + 300);
    assert!(
        !AuthClient::needs_refresh(&token_300),
        "token expiring in exactly 300s should NOT need refresh (< 300 is the threshold)"
    );

    // 299 seconds = should refresh.
    let token_299 = make_jwt_with_exp(now + 299);
    assert!(
        AuthClient::needs_refresh(&token_299),
        "token expiring in 299s should need refresh"
    );
}
