//! Daemon-internal pub/sub for OAuth callbacks, keyed by OAuth `state` parameter.
//!
//! The daemon holds one `OAuthState` instance for the process lifetime.
//! When a CLI invocation starts a login flow it calls `subscribe(state)` and
//! receives a `oneshot::Receiver`.  When the browser redirect hits the HTTP
//! listener, `notify(state, code)` fires the sender and the CLI's
//! `auth/wait_for_callback` JSON-RPC method completes.
//!
//! # Concurrency
//!
//! All mutations are guarded by a `tokio::sync::Mutex`.  The critical section
//! is brief — hash-map insert or remove plus one send — so lock contention is
//! negligible.
//!
//! # Shutdown
//!
//! `cancel_all()` is called from the daemon shutdown path.  It drops every
//! pending sender, which causes all waiting receivers to get a
//! `tokio::sync::oneshot::error::RecvError` (channel closed without a value).

use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::{oneshot, Mutex};
use tracing::warn;

/// Errors returned by `OAuthState` operations.
#[derive(Debug, Error)]
pub enum OAuthStateError {
    /// `subscribe` was called for a `state` value that already has a waiter.
    #[error("state '{0}' already has a subscriber — duplicate login in progress")]
    DuplicateSubscriber(String),

    /// `notify` was called but no subscriber was registered for this state.
    ///
    /// This is logged at WARN; the callback still returns 200 to the browser.
    #[error("state '{0}' has no subscriber — orphaned callback")]
    NoSubscriber(String),
}

/// Daemon-wide OAuth callback notification hub.
pub struct OAuthState {
    waiters: Mutex<HashMap<String, oneshot::Sender<(String, String)>>>,
}

impl OAuthState {
    /// Construct a new, empty hub.
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(HashMap::new()),
        }
    }

    /// Register a waiter for the given OAuth `state` value.
    ///
    /// Returns the receiving end of a oneshot channel.  The caller should
    /// await this receiver inside a timeout (the `auth/wait_for_callback`
    /// JSON-RPC handler supplies the timeout).
    ///
    /// # Errors
    ///
    /// Returns `OAuthStateError::DuplicateSubscriber` if a waiter is already
    /// registered for this state.  This indicates two concurrent logins using
    /// the same state value, which is a bug in the caller.
    pub async fn subscribe(
        &self,
        state: String,
    ) -> Result<oneshot::Receiver<(String, String)>, OAuthStateError> {
        let mut map = self.waiters.lock().await;
        if map.contains_key(&state) {
            return Err(OAuthStateError::DuplicateSubscriber(state));
        }
        let (tx, rx) = oneshot::channel();
        map.insert(state, tx);
        Ok(rx)
    }

    /// Notify the waiter for the given OAuth `state` that the callback arrived.
    ///
    /// Sends `(code, state)` to the registered receiver and removes the entry
    /// from the map.
    ///
    /// # Errors
    ///
    /// Returns `OAuthStateError::NoSubscriber` if no waiter is registered.
    /// The caller (`oauth_listener`) logs this at WARN and still returns 200
    /// to the browser.
    pub async fn notify(&self, state: String, code: String) -> Result<(), OAuthStateError> {
        let mut map = self.waiters.lock().await;
        match map.remove(&state) {
            Some(tx) => {
                // Receiver may have been dropped (timeout); ignore send errors.
                let _ = tx.send((code, state));
                Ok(())
            }
            None => {
                warn!(state = %state, "notify: no subscriber registered for state");
                Err(OAuthStateError::NoSubscriber(state))
            }
        }
    }

    /// Drop all pending senders.
    ///
    /// Each waiting receiver will observe `RecvError` (closed channel) and
    /// should propagate a "daemon shutting down" error to the CLI.
    pub async fn cancel_all(&self) {
        let mut map = self.waiters.lock().await;
        let count = map.len();
        map.clear();
        if count > 0 {
            warn!(
                count,
                "cancel_all: dropped pending OAuth waiters on shutdown"
            );
        }
    }
}

impl Default for OAuthState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "oauth_state_tests.rs"]
mod tests;
