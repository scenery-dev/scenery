//! Unit tests for `oauth_listener`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use crate::oauth_state::OAuthState;

use super::start_listener;

/// Spawn the listener, hit the callback URL with reqwest, assert 200 HTML
/// response and that OAuthState::notify was called (verified by receiving the
/// code on the subscriber channel).
#[tokio::test]
async fn callback_returns_200_html_and_notifies_oauth_state() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind on a free port in the test environment");

    // Subscribe before hitting the callback so the sender is registered.
    let rx = oauth_state
        .subscribe("test-state-1".to_string())
        .await
        .unwrap();

    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?code=test-code&state=test-state-1",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();

    assert_eq!(resp.status(), 200, "callback should return 200");

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("VectorHawk login complete"),
        "body should contain success message; got: {body}"
    );
    assert!(
        body.contains("window.close()"),
        "body should contain auto-close script; got: {body}"
    );

    // The notification should arrive promptly.
    let (code, state) = tokio::time::timeout(std::time::Duration::from_millis(500), rx)
        .await
        .expect("notify should arrive within 500 ms")
        .expect("channel should not be closed");

    assert_eq!(code, "test-code");
    assert_eq!(state, "test-state-1");

    handle.abort();
}

#[tokio::test]
async fn callback_missing_code_returns_400() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind");

    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?state=only-state",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 400, "missing code should yield 400");

    handle.abort();
}

#[tokio::test]
async fn callback_missing_state_returns_400() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind");

    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?code=only-code",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 400, "missing state should yield 400");

    handle.abort();
}

#[tokio::test]
async fn callback_no_subscriber_still_returns_200() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind");

    // Intentionally no subscribe call.
    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?code=c&state=orphan",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();
    // Orphaned callback must not expose the error to the browser.
    assert_eq!(
        resp.status(),
        200,
        "orphaned callback should still return 200"
    );

    handle.abort();
}

#[tokio::test]
async fn callback_oauth_error_param_returns_200_with_error_page() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind");

    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?error=access_denied&state=s",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("access_denied"),
        "error page should include the error value; got: {body}"
    );

    handle.abort();
}

/// The `error` query parameter is attacker-controlled (any web page can
/// navigate the browser to the loopback callback), so it must be rendered as
/// escaped text, never as markup.
#[tokio::test]
async fn callback_oauth_error_param_is_html_escaped() {
    let oauth_state = Arc::new(OAuthState::new());

    let (addr, handle) = start_listener(Arc::clone(&oauth_state))
        .await
        .unwrap()
        .expect("listener should bind");

    // error=<script>alert('x')</script>&"
    let url = format!(
        "http://127.0.0.1:{}/oauth/cli/callback?error=%3Cscript%3Ealert(%27x%27)%3C%2Fscript%3E%26%22",
        addr.port()
    );

    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("<script>"),
        "error value must not be injected as markup; got: {body}"
    );
    assert!(
        body.contains("&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;&amp;&quot;"),
        "error value should be rendered HTML-escaped; got: {body}"
    );

    handle.abort();
}
