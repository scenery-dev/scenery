use crate::model::{ModelClient, ModelRequest, ModelResponse, ModelSource};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Calls a locally-running Ollama instance via its REST API.
pub struct OllamaClient {
    pub base_url: String,
    pub model: String,
    /// HTTP client for quick operations (health check, list models): 5s timeout.
    http_fast: reqwest::blocking::Client,
    /// HTTP client for generate calls: 30s timeout (configurable).
    http_generate: reqwest::blocking::Client,
}

/// Result of an Ollama health check.
#[derive(Debug)]
pub struct HealthStatus {
    pub reachable: bool,
    pub status_code: Option<u16>,
}

/// A model available in the local Ollama instance.
#[derive(Debug, Deserialize)]
pub struct OllamaModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_timeouts(
            base_url,
            model,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
    }

    pub fn with_timeouts(
        base_url: impl Into<String>,
        model: impl Into<String>,
        fast_timeout: Duration,
        generate_timeout: Duration,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            http_fast: reqwest::blocking::Client::builder()
                .timeout(fast_timeout)
                .build()
                .expect("failed to build HTTP client"),
            http_generate: reqwest::blocking::Client::builder()
                .timeout(generate_timeout)
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    /// Check if Ollama is reachable by hitting GET `/`.
    pub fn health_check(&self) -> HealthStatus {
        let url = format!("{}/", self.base_url.trim_end_matches('/'));
        match self.http_fast.get(&url).send() {
            Ok(resp) => HealthStatus {
                reachable: resp.status().is_success(),
                status_code: Some(resp.status().as_u16()),
            },
            Err(_) => HealthStatus {
                reachable: false,
                status_code: None,
            },
        }
    }

    /// List models available in the local Ollama instance via GET `/api/tags`.
    pub fn list_models(&self) -> Result<Vec<OllamaModel>> {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let resp = self
            .http_fast
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach Ollama at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }

        let tags: OllamaTagsResponse = resp
            .json()
            .context("failed to deserialize Ollama /api/tags response")?;

        Ok(tags.models)
    }
}

// ── Ollama wire types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    system: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a str>,
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
    #[serde(default)]
    prompt_eval_count: u64,
    #[serde(default)]
    eval_count: u64,
}

// ── ModelClient impl ──────────────────────────────────────────────────────────

impl ModelClient for OllamaClient {
    fn local_model_name(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn generate(&self, request: ModelRequest) -> Result<ModelResponse> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));

        let body = OllamaGenerateRequest {
            model: &self.model,
            prompt: &request.user_message,
            system: &request.system_prompt,
            stream: false,
            format: if request.json_output {
                Some("json")
            } else {
                None
            },
        };

        let start = Instant::now();
        let resp = self
            .http_generate
            .post(&url)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach Ollama at {url}"))?;

        let latency_ms = start.elapsed().as_millis() as u64;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }

        let ollama_resp: OllamaGenerateResponse = resp
            .json()
            .context("failed to deserialize Ollama response")?;

        Ok(ModelResponse {
            text: ollama_resp.response,
            prompt_tokens: ollama_resp.prompt_eval_count,
            completion_tokens: ollama_resp.eval_count,
            latency_ms,
            source: ModelSource::Local(self.model.clone()),
        })
    }
}

// ── Model resolution ──────────────────────────────────────────────────────────

/// Resolve the model name to use for an Ollama request.
///
/// Priority order:
/// 1. If `requested` matches a model name in Ollama's available list → use it
/// 2. If Ollama has exactly one model available → use it (unambiguous fallback)
/// 3. If Ollama has multiple models but `requested` is not in the list → use the
///    first available model and emit a warning
/// 4. If Ollama has no models → return an error
pub fn resolve_model<T: AsRef<str>>(client: &OllamaClient, requested: T) -> Result<String> {
    let requested = requested.as_ref();
    let models = client
        .list_models()
        .context("failed to list Ollama models for model resolution")?;

    if models.is_empty() {
        anyhow::bail!("Ollama has no models installed — run `ollama pull <model>` to install one");
    }

    let available: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();

    if !requested.is_empty() {
        if available.contains(&requested) {
            return Ok(requested.to_string());
        }
        tracing::warn!(
            requested_model = requested,
            available = ?available,
            "requested model is not available in Ollama; falling back to first available model"
        );
    }

    Ok(models
        .into_iter()
        .next()
        .map(|m| m.name)
        .expect("non-empty list checked above"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_check_returns_unreachable_for_bad_url() {
        let client = OllamaClient::with_timeouts(
            "http://127.0.0.1:19999",
            "test",
            Duration::from_millis(200),
            Duration::from_secs(1),
        );
        let status = client.health_check();
        assert!(!status.reachable);
        assert!(status.status_code.is_none());
    }

    #[test]
    fn list_models_errors_for_unreachable_server() {
        let client = OllamaClient::with_timeouts(
            "http://127.0.0.1:19999",
            "test",
            Duration::from_millis(200),
            Duration::from_secs(1),
        );
        let err = client.list_models().expect_err("should fail");
        assert!(
            err.to_string().contains("failed to reach Ollama"),
            "got: {err}"
        );
    }

    #[test]
    fn health_check_returns_reachable_when_server_responds_200() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_body("Ollama is running")
            .create();

        let client = OllamaClient::new(server.url(), "test-model");
        let status = client.health_check();
        assert!(status.reachable);
        assert_eq!(status.status_code, Some(200));
    }

    #[test]
    fn list_models_parses_tags_response() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/tags")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"models":[{"name":"llama3.2:latest","size":2000000000},{"name":"mistral:latest","size":4000000000}]}"#,
            )
            .create();

        let client = OllamaClient::new(server.url(), "test-model");
        let models = client.list_models().expect("list models");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "llama3.2:latest");
        assert_eq!(models[1].name, "mistral:latest");
    }

    #[test]
    fn generate_sends_correct_request_and_parses_response() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/api/generate")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"model":"test-model","stream":false}"#.to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"response":"Hello world","prompt_eval_count":10,"eval_count":5}"#)
            .create();

        let client = OllamaClient::new(server.url(), "test-model");
        let resp = client
            .generate(ModelRequest {
                system_prompt: "You are helpful.".to_string(),
                user_message: "Say hello".to_string(),
                json_output: false,
                prefer_local: false,
                ..Default::default()
            })
            .expect("generate");

        assert_eq!(resp.text, "Hello world");
        assert_eq!(resp.prompt_tokens, 10);
        assert_eq!(resp.completion_tokens, 5);
        assert!(resp.latency_ms < 5000);
    }

    #[test]
    fn generate_returns_error_on_http_failure() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(500)
            .with_body("internal server error")
            .create();

        let client = OllamaClient::new(server.url(), "test-model");
        let err = client
            .generate(ModelRequest {
                system_prompt: "test".to_string(),
                user_message: "test".to_string(),
                json_output: false,
                prefer_local: false,
                ..Default::default()
            })
            .expect_err("should fail on 500");

        assert!(err.to_string().contains("500"), "got: {err}");
    }

    #[test]
    fn resolve_model_returns_requested_when_present() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/tags")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"models":[{"name":"llama3.2:latest","size":0},{"name":"deepseek-coder-v2:latest","size":0}]}"#,
            )
            .create();
        let client = OllamaClient::new(server.url(), "");
        let result = resolve_model(&client, "deepseek-coder-v2:latest").expect("resolve");
        assert_eq!(result, "deepseek-coder-v2:latest");
    }

    #[test]
    fn resolve_model_errors_when_no_models_available() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/tags")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"models":[]}"#)
            .create();
        let client = OllamaClient::new(server.url(), "");
        let err = resolve_model(&client, "llama3.2").expect_err("should fail with empty list");
        assert!(
            err.to_string().contains("no models installed"),
            "got: {err}"
        );
    }
}
