//! Background retry of the sync subsystem's startup.
//!
//! The daemon tries to start sync once at boot. If that fails (no readable
//! token because the macOS keychain ACL prompt was still blocking after a
//! `brew upgrade`, a registration hiccup, an unreachable registry) sync used
//! to stay down until the next daemon restart or an explicit auth action.
//! [`SyncController::run_start_retry`] closes that gap: while sync is not
//! running it retries on a bounded backoff (30 s doubling to 5 min), and
//! wakes immediately when the refresh loop reports a token.
//!
//! Keychain safety: a retry uses the access token the daemon already holds in
//! memory (set by the refresh loop) when it has one, so it does not touch the
//! keychain. Without one it makes exactly one read of the token store per
//! attempt, and attempts are at least one backoff interval apart, so a blocked
//! item cannot cause a prompt storm.
//!
//! Revocation: a revoked user (`user_revoked_at`) or device
//! (`device_revoked_at`) never triggers a start attempt.

use std::sync::Arc;
use std::time::Duration;

use scenery_core::auth::USER_REVOKED_KEY;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::SyncController;

/// Backoff bounds for [`SyncController::run_start_retry`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryConfig {
    /// First retry delay, and the delay after any success.
    pub initial: Duration,
    /// Ceiling the delay doubles up to.
    pub max: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(30),
            max: Duration::from_secs(300),
        }
    }
}

/// What one retry attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryOutcome {
    /// Sync was down and is now connected.
    Started,
    /// Sync is already running; nothing to do.
    AlreadyRunning,
    /// User or device is revoked; no attempt was made.
    Revoked,
    /// An attempt was made and did not produce a live connection.
    Failed,
}

impl SyncController {
    /// Record (or clear, with `None`) the in-memory access token the start
    /// retry may use instead of reading the token store.
    pub(crate) fn note_token(&self, token: Option<String>) {
        if let Ok(mut hint) = self.token_hint.lock() {
            *hint = token;
        }
    }

    /// Whether a sync subsystem has been spawned. Non-blocking: if a start is
    /// in progress right now, reports `true` so callers do not pile on.
    pub(crate) fn is_started(&self) -> bool {
        match self.handle.try_lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => true,
        }
    }

    fn is_revoked(&self) -> bool {
        self.state
            .get_sync_state(USER_REVOKED_KEY)
            .ok()
            .flatten()
            .is_some()
            || self
                .state
                .get_sync_state("device_revoked_at")
                .ok()
                .flatten()
                .is_some()
    }

    /// One start attempt, if sync is down and the session is not revoked.
    pub(crate) async fn retry_start_once(&self) -> RetryOutcome {
        let guard = self.handle.lock().await;
        if guard.is_some() {
            return RetryOutcome::AlreadyRunning;
        }
        if self.is_revoked() {
            self.note_token(None);
            return RetryOutcome::Revoked;
        }
        let hint = self.token_hint.lock().ok().and_then(|h| h.clone());
        if self.start_locked(guard, hint).await {
            RetryOutcome::Started
        } else {
            RetryOutcome::Failed
        }
    }

    /// Retry forever: wait `delay` (or until `kick` fires), attempt a start if
    /// sync is down, and back off from `cfg.initial` to `cfg.max` while
    /// attempts fail. Never returns.
    pub(crate) async fn run_start_retry(self: Arc<Self>, cfg: RetryConfig, kick: Arc<Notify>) {
        let mut delay = cfg.initial;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = kick.notified() => {}
            }
            match self.retry_start_once().await {
                RetryOutcome::Started => {
                    info!("sync subsystem started by background retry");
                    delay = cfg.max;
                }
                RetryOutcome::AlreadyRunning | RetryOutcome::Revoked => delay = cfg.max,
                RetryOutcome::Failed => {
                    debug!(
                        next_retry_secs = (delay * 2).min(cfg.max).as_secs(),
                        "sync start retry failed"
                    );
                    delay = (delay * 2).min(cfg.max);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "sync_retry_tests.rs"]
mod tests;
