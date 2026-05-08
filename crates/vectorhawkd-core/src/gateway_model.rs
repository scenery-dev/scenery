//! HTTP-based model client that routes inference requests through the
//! VectorHawk gateway at `/gateway/v1/inference`.
//!
//! Auth flow:
//! 1. Look up the stored access token for `base_url` in SQLite via
//!    `auth::load_tokens`.
//! 2. If no token is present → return a user-friendly "not authenticated" error.
//! 3. POST to `{base_url}/gateway/v1/inference` with `Authorization: Bearer {token}`.
//! 4. On 401 → return an "auth token expired" error.
//! 5. On success → parse `vh_tier`, `vh_provider`, `vh_cost_usd`, `usage`, and
//!    `content[0].text` from the JSON response and map to `ModelResponse`.
//!
//! `GatewayModelClient` uses `reqwest::blocking`, so callers on a
//! current-thread Tokio executor **must** wrap `generate()` in
//! `tokio::task::spawn_blocking`. The daemon's `RealBackend::call_tool`
//! already does this for the entire tool-call hot path.

use crate::{
    auth::load_tokens,
    model::{ModelClient, ModelRequest, ModelResponse, ModelSource},
    state::AppState,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tracing::debug;

// ── Response types ────────────────────────────────────────────────────────────

/// One content block returned by the gateway.
#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
}

/// Token usage counters returned by the gateway.
#[derive(Debug, Deserialize)]
struct UsageBlock {
    input_tokens: u64,
    output_tokens: u64,
}

/// Top-level gateway inference response.
///
/// `#[serde(deny_unknown_fields)]` is intentionally omitted: the gateway may
/// add new vendor-extension fields, and we want to tolerate that gracefully.
#[derive(Debug, Deserialize)]
struct GatewayResponse {
    /// Routing tier — `"provider"`, `"internal"`, or `"local"`.
    vh_tier: String,
    /// Provider or model name (used to fill `ModelSource`).
    vh_provider: String,
    /// Cost of this call in USD. `0.0` for internal/local tiers.
    vh_cost_usd: f64,
    /// Token usage.
    usage: UsageBlock,
    /// One or more content blocks. We read `content[0].text`.
    content: Vec<ContentBlock>,
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Routes inference requests to the VectorHawk gateway over HTTPS.
///
/// Auth tokens are read from the daemon's local SQLite state on every call so
/// that token rotation by the background refresh loop is picked up
/// automatically without restarting the daemon.
pub struct GatewayModelClient {
    base_url: String,
    state: Arc<AppState>,
    http: reqwest::blocking::Client,
}

impl GatewayModelClient {
    /// Create a new client pointing at `base_url`.
    ///
    /// `base_url` should not have a trailing slash, e.g.
    /// `"https://app.vectorhawk.ai"`.
    pub fn new(base_url: impl Into<String>, state: Arc<AppState>) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest blocking client must build");

        Self {
            base_url: base_url.into(),
            state,
            http,
        }
    }
}

impl ModelClient for GatewayModelClient {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse> {
        use std::time::Instant;

        let stored = load_tokens(&self.state, &self.base_url)
            .context("failed to read auth tokens from state DB")?;

        let token = match stored {
            Some(t) => t.access_token,
            None => {
                anyhow::bail!(
                    "not authenticated — run `vectorhawk auth login` to connect to the gateway"
                );
            }
        };

        let url = format!(
            "{}/gateway/v1/inference",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "sending inference request to gateway");

        let body = serde_json::json!({
            "system_prompt": request.system_prompt,
            "user_message": request.user_message,
            "json_output": request.json_output,
        });

        let start = Instant::now();

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .with_context(|| format!("HTTP request to gateway failed: {url}"))?;

        let latency_ms = start.elapsed().as_millis() as u64;
        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "auth token expired — run `vectorhawk auth login` to refresh your credentials"
            );
        }

        if !status.is_success() {
            let body_text = resp.text().unwrap_or_default();
            anyhow::bail!("gateway returned HTTP {status}: {body_text}");
        }

        let gw: GatewayResponse = resp
            .json()
            .context("failed to parse gateway inference response")?;

        let text = gw
            .content
            .into_iter()
            .find(|b| b.block_type == "text")
            .map(|b| b.text)
            .unwrap_or_default();

        let source = map_tier_to_source(&gw.vh_tier, &gw.vh_provider);

        Ok(ModelResponse {
            text,
            prompt_tokens: gw.usage.input_tokens,
            completion_tokens: gw.usage.output_tokens,
            latency_ms,
            source,
            cost_usd: gw.vh_cost_usd,
        })
    }
}

/// Map the gateway's `vh_tier` + `vh_provider` fields to a `ModelSource` variant.
fn map_tier_to_source(tier: &str, provider: &str) -> ModelSource {
    match tier {
        "provider" => ModelSource::Provider(provider.to_string()),
        "internal" => ModelSource::Internal(provider.to_string()),
        "local" => ModelSource::Local(provider.to_string()),
        other => {
            tracing::warn!(
                tier = other,
                "unknown vh_tier from gateway; mapping to Provider"
            );
            ModelSource::Provider(provider.to_string())
        }
    }
}
