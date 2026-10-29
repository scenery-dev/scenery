//! Trait for subscribing to OAuth callback notifications.
//!
//! Defined in the MCP crate so `tools.rs` can use it without a circular
//! dependency on the daemon crate. The daemon implements `OAuthSubscriber`
//! by wrapping its `OAuthState` pub/sub hub.

use std::{future::Future, pin::Pin, sync::Arc};

/// Await an OAuth authorization code delivered by the browser callback.
pub trait OAuthSubscriber: Send + Sync + 'static {
    /// Wait up to `timeout_secs` seconds for the browser callback carrying the
    /// given OAuth `state` value. Returns `Some(code)` on success, `None` on
    /// timeout or daemon shutdown.
    fn wait_for_code(
        &self,
        state: String,
        timeout_secs: u64,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>;
}

/// OAuth context threaded into tool handlers that need to complete the PKCE flow.
pub struct OAuthContext {
    /// TCP port the OAuth callback listener is bound to (daemon side).
    pub listener_port: u16,
    /// Pub/sub hub: subscribe to get the code when the browser calls back.
    pub subscriber: Arc<dyn OAuthSubscriber>,
}
