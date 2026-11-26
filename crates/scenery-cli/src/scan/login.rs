//! `scenery scan --once --login`: an in-process browser sign-in.
//!
//! The session lives only in memory. Nothing is saved: no keychain entry,
//! no state dir, no token file. The caller revokes it with [`sign_out`]
//! before the command exits, on success and on failure.
//!
//! This reuses the runner's CLI OAuth flow (PKCE, loopback callback on the
//! first free port in 39127-39136), but runs the callback listener inside
//! this process instead of asking the daemon for it.

use anyhow::{anyhow, Context, Result};
use scenery_core::auth::AuthClient;
use scenery_daemon::oauth_listener::start_listener;
use scenery_daemon::OAuthState;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::{spawn_blocking, JoinHandle};

/// How long the user has to finish signing in.
pub(crate) const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const TIMED_OUT: &str = "Sign-in timed out.";

/// A signed-in session, held in memory only.
pub struct LoginSession {
    pub access_token: String,
    pub refresh_token: String,
}

// Never derive Debug: it would print the tokens.
impl fmt::Debug for LoginSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginSession(..)")
    }
}

/// Sign in through the browser. `open_browser` gets the authorize URL and
/// returns whether a browser opened; when it did not, the URL is printed.
/// The callback listener is stopped (its port released) before this returns.
pub async fn sign_in<F>(base: &str, open_browser: F, timeout: Duration) -> Result<LoginSession>
where
    F: Fn(&str) -> bool + Send + 'static,
{
    let hub = Arc::new(OAuthState::new());
    let (addr, listener) = start_listener(Arc::clone(&hub))
        .await
        .context("could not start the sign-in callback listener")?
        .ok_or_else(|| {
            anyhow!("all sign-in callback ports (39127-39136) are in use; close other sign-ins and retry")
        })?;
    let result = flow(base, &hub, addr.port(), open_browser, timeout).await;
    hub.cancel_all().await;
    stop(listener).await;
    result
}

/// Abort the listener task and wait for it, so its socket is closed.
async fn stop(listener: JoinHandle<()>) {
    listener.abort();
    // The join error is the cancellation we just asked for.
    let _cancelled = listener.await;
}

async fn flow<F>(
    base: &str,
    hub: &OAuthState,
    port: u16,
    open_browser: F,
    timeout: Duration,
) -> Result<LoginSession>
where
    F: Fn(&str) -> bool + Send + 'static,
{
    let redirect = format!("http://127.0.0.1:{port}/oauth/cli/callback");
    let owned = base.to_string();
    // AuthClient wraps a blocking reqwest client: build and use it off the runtime.
    let init =
        spawn_blocking(move || AuthClient::new(owned).initiate_oauth_flow_with_redirect(&redirect))
            .await
            .context("sign-in task failed")??;
    let rx = hub.subscribe(init.state.clone()).await?;
    let url = init.auth_url.clone();
    let wait = async move {
        let shown = url.clone();
        let opened = spawn_blocking(move || open_browser(&url))
            .await
            .unwrap_or(false);
        if !opened {
            eprintln!("Open this URL in your browser to sign in:\n  {shown}\n");
            eprintln!(
                "Over SSH, first forward the callback port from your computer: \
                 ssh -L {port}:localhost:{port} <this machine>"
            );
        }
        eprintln!("Waiting for you to sign in (up to 5 minutes)...");
        rx.await.map_err(|_| anyhow!("sign-in was cancelled"))
    };
    let (code, _state) = tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| anyhow!(TIMED_OUT))??;
    let owned = base.to_string();
    let verifier = init.code_verifier;
    let tokens =
        spawn_blocking(move || AuthClient::new(owned).exchange_oauth_code(&code, &verifier))
            .await
            .context("sign-in task failed")?
            .context("could not finish signing in")?;
    Ok(LoginSession {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
    })
}

/// Revoke the session (`POST /portal/auth/revoke`, scope `family`: this
/// session's refresh token, its access tokens and every rotation of them).
/// Best effort: a failure prints one warning line.
pub async fn sign_out(base: &str, session: &LoginSession) {
    let owned = base.to_string();
    let refresh = session.refresh_token.clone();
    let result = spawn_blocking(move || AuthClient::new(owned).revoke(&refresh, "family")).await;
    let err = match result {
        Ok(Ok(())) => return,
        Ok(Err(e)) => format!("{e:#}"),
        Err(e) => e.to_string(),
    };
    eprintln!("warning: could not sign out the scan session: {err}");
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod login_tests;
