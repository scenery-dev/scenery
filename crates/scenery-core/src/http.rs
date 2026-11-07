//! Shared HTTP plumbing for every request the runner sends to the Scenery
//! backend (registry, auth, sync, downloads, gateway).
//!
//! Two jobs:
//!
//! 1. **Identify the runner.** Every client is built through
//!    [`async_client_builder`] / [`blocking_client_builder`], which install
//!    `X-Scenery-Runner-Version: <crate version>` as a default header. The
//!    backend uses it for its min-runner-version gate (HTTP 426) and to
//!    observe the field. Never call `reqwest::Client::builder()` directly;
//!    `http_tests::no_client_built_outside_this_module` fails if you do.
//! 2. **Surface HTTP 426.** [`SendTrackedExt`] / [`AsyncSendTrackedExt`] wrap
//!    `.send()` and, on a 426 `{"code":"runner_upgrade_required","min":"X.Y.Z"}`,
//!    record the requirement in process-global state (and log once). The CLI
//!    error printer, the daemon and the MCP tool results read it back through
//!    [`current_upgrade_required`]. The response is handed back to the caller
//!    unchanged (status, headers, body) so existing error paths keep working.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::StatusCode;
use tracing::warn;

/// Header carrying the runner's version on every backend request.
pub const RUNNER_VERSION_HEADER: &str = "X-Scenery-Runner-Version";

/// The runner version (workspace version, shared by every crate).
pub const RUNNER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Error `code` the backend puts in a 426 body.
pub const UPGRADE_REQUIRED_CODE: &str = "runner_upgrade_required";

/// How long a recorded 426 stays visible without being re-observed.
const UPGRADE_STATE_TTL: Duration = Duration::from_secs(15 * 60);

/// The backend refused this runner version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct UpgradeRequired {
    /// Minimum runner version the backend demands, when it told us.
    pub min: Option<String>,
}

impl std::fmt::Display for UpgradeRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.min {
            Some(min) => write!(
                f,
                "Scenery {min} or newer required; run `brew upgrade scenery`"
            ),
            None => write!(f, "A newer Scenery is required; run `brew upgrade scenery`"),
        }
    }
}

/// Default headers installed on every runner HTTP client.
pub fn default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-scenery-runner-version"),
        HeaderValue::from_static(RUNNER_VERSION),
    );
    headers
}

/// Async client builder with the runner-version header preinstalled.
pub fn async_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().default_headers(default_headers())
}

/// Blocking client builder with the runner-version header preinstalled.
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().default_headers(default_headers())
}

/// Async client with the default 5s connect / 30s total timeouts.
pub fn async_client() -> reqwest::Result<reqwest::Client> {
    async_client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
}

/// Blocking client with the default 5s connect / 30s total timeouts.
pub fn blocking_client() -> reqwest::Result<reqwest::blocking::Client> {
    blocking_client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
}

// ── Process-global 426 state ─────────────────────────────────────────────────

struct UpgradeState {
    min: Option<String>,
    observed: Instant,
}

fn upgrade_state() -> &'static Mutex<Option<UpgradeState>> {
    static STATE: OnceLock<Mutex<Option<UpgradeState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Extract `min` from a 426 body. `None` when absent or not our JSON shape.
pub fn parse_upgrade_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    // FastAPI may wrap custom errors in `detail`.
    let obj = value
        .get("detail")
        .filter(|d| d.is_object())
        .unwrap_or(&value);
    if obj.get("code").and_then(|c| c.as_str()) != Some(UPGRADE_REQUIRED_CODE) {
        return None;
    }
    obj.get("min").and_then(|m| m.as_str()).map(str::to_owned)
}

/// Record that the backend answered 426. Logs a warning the first time (or
/// when the required minimum changes), so a polling daemon does not spam.
pub fn record_upgrade_required(min: Option<String>) {
    let Ok(mut guard) = upgrade_state().lock() else {
        return;
    };
    let fresh = match guard.as_ref() {
        Some(s) => s.min != min || s.observed.elapsed() > UPGRADE_STATE_TTL,
        None => true,
    };
    if fresh {
        warn!(
            runner_version = RUNNER_VERSION,
            min = min.as_deref().unwrap_or("unknown"),
            "backend rejected this runner version (HTTP 426); run `brew upgrade scenery`"
        );
    }
    *guard = Some(UpgradeState {
        min,
        observed: Instant::now(),
    });
}

/// The most recent unexpired 426, if any.
pub fn current_upgrade_required() -> Option<UpgradeRequired> {
    let guard = upgrade_state().lock().ok()?;
    let state = guard.as_ref()?;
    if state.observed.elapsed() > UPGRADE_STATE_TTL {
        return None;
    }
    Some(UpgradeRequired {
        min: state.min.clone(),
    })
}

/// Forget any recorded 426 (tests, and after a successful self-upgrade).
pub fn clear_upgrade_required() {
    if let Ok(mut guard) = upgrade_state().lock() {
        *guard = None;
    }
}

/// Classify a response: returns the typed error for a 426, else `None`.
pub fn upgrade_required_from(status: StatusCode, body: &[u8]) -> Option<UpgradeRequired> {
    if status != StatusCode::UPGRADE_REQUIRED {
        return None;
    }
    Some(UpgradeRequired {
        min: parse_upgrade_body(body),
    })
}

fn rebuild(status: StatusCode, headers: HeaderMap, body: Vec<u8>) -> http::Response<Vec<u8>> {
    let mut out = http::Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

// ── send wrappers ────────────────────────────────────────────────────────────

/// Drop-in replacement for `.send()` on blocking requests that records 426s.
pub trait SendTrackedExt {
    fn send_tracked(self) -> reqwest::Result<reqwest::blocking::Response>;
}

impl SendTrackedExt for reqwest::blocking::RequestBuilder {
    fn send_tracked(self) -> reqwest::Result<reqwest::blocking::Response> {
        let resp = self.send()?;
        if resp.status() != StatusCode::UPGRADE_REQUIRED {
            return Ok(resp);
        }
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().map(|b| b.to_vec()).unwrap_or_default();
        record_upgrade_required(parse_upgrade_body(&body));
        Ok(reqwest::blocking::Response::from(rebuild(
            status, headers, body,
        )))
    }
}

/// Drop-in replacement for `.send()` on async requests that records 426s.
pub trait AsyncSendTrackedExt {
    fn send_tracked(
        self,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
}

impl AsyncSendTrackedExt for reqwest::RequestBuilder {
    async fn send_tracked(self) -> reqwest::Result<reqwest::Response> {
        let resp = self.send().await?;
        if resp.status() != StatusCode::UPGRADE_REQUIRED {
            return Ok(resp);
        }
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        record_upgrade_required(parse_upgrade_body(&body));
        Ok(reqwest::Response::from(rebuild(status, headers, body)))
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
