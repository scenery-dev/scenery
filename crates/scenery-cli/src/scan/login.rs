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
#[derive(Clone)]
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
    // Detached: a slow or hung opener never delays the callback. Its answer
    // only decides whether the URL is printed.
    drop(spawn_blocking(move || {
        if !open_browser(&url) {
            eprintln!("Open this URL in your browser to sign in:\n  {url}\n");
            eprintln!(
                "Over SSH, first forward the callback port from your computer: \
                 ssh -L {port}:localhost:{port} <this machine>"
            );
        }
    }));
    eprintln!(
        "Waiting for you to sign in (up to {})...",
        wait_text(timeout)
    );
    let wait = async move { rx.await.map_err(|_| anyhow!("sign-in was cancelled")) };
    let outcome = tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| anyhow!(TIMED_OUT))??;
    let (code, _state) =
        outcome.map_err(|reason| anyhow!("Sign-in was denied or failed: {reason}"))?;
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

/// "5 minutes", "1 minute", or "30 seconds" for the wait message.
pub(crate) fn wait_text(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    match (secs / 60, secs % 60) {
        (1, 0) => "1 minute".to_string(),
        (m, 0) if m > 1 => format!("{m} minutes"),
        _ if secs == 1 => "1 second".to_string(),
        _ => format!("{secs} seconds"),
    }
}

/// Signal watchers that revoke the session if the process is interrupted
/// (SIGINT, SIGTERM, SIGHUP; Ctrl-C on Windows) and then exit 130. Call
/// [`SignalGuard::disarm`] after the normal [`sign_out`].
pub struct SignalGuard {
    tasks: Vec<JoinHandle<()>>,
}

impl SignalGuard {
    pub fn disarm(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

/// Install the watchers. The handlers are registered before this returns,
/// so a signal that arrives right after it is caught.
pub fn revoke_on_signal(base: &str, session: &LoginSession) -> Result<SignalGuard> {
    let mut tasks = Vec::new();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for kind in [
            SignalKind::interrupt(),
            SignalKind::terminate(),
            SignalKind::hangup(),
        ] {
            let mut sig = signal(kind).context("installing a signal handler")?;
            let base = base.to_string();
            let session = session.clone();
            tasks.push(tokio::spawn(async move {
                if sig.recv().await.is_some() {
                    revoke_and_exit(&base, &session).await;
                }
            }));
        }
    }
    #[cfg(not(unix))]
    {
        let base = base.to_string();
        let session = session.clone();
        tasks.push(tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                revoke_and_exit(&base, &session).await;
            }
        }));
    }
    Ok(SignalGuard { tasks })
}

async fn revoke_and_exit(base: &str, session: &LoginSession) {
    eprintln!("Interrupted; signing out the scan session.");
    sign_out(base, session).await;
    std::process::exit(130);
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
