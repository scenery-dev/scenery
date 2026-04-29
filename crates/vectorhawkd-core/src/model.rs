use anyhow::Result;

/// A request to generate text from a language model.
#[derive(Clone)]
pub struct ModelRequest {
    /// System prompt (instructions / persona).
    pub system_prompt: String,
    /// User-facing content (resolved step inputs).
    pub user_message: String,
    /// When `true` the model is asked to return valid JSON.
    pub json_output: bool,
    /// When `true`, the runtime tries a locally-running model first (Ollama),
    /// falling back to MCP sampling if the local call fails or no local model
    /// is available. When `false` (the default), the runtime uses MCP sampling
    /// directly — the AI client handles the generation.
    pub prefer_local: bool,
}

/// Identifies which backend produced a model response.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelSource {
    /// A locally-running Ollama instance. Contains the resolved model name.
    Local(String),
    /// The AI client handled the request via MCP `sampling/createMessage`.
    McpSampling,
}

/// The raw response returned by a model backend, including accounting data.
#[derive(Debug)]
pub struct ModelResponse {
    /// Raw text (or JSON string) produced by the model.
    pub text: String,
    /// Number of tokens in the prompt (0 when not reported by the backend).
    pub prompt_tokens: u64,
    /// Number of tokens in the completion (0 when not reported).
    pub completion_tokens: u64,
    /// Wall-clock time for the call in milliseconds.
    pub latency_ms: u64,
    /// Which backend produced this response.
    pub source: ModelSource,
}

/// Abstraction over any text-generation backend.
pub trait ModelClient: Send + Sync {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse>;
}

/// A mock model client that returns a configurable fixed response.
/// Useful for testing the LLM execution path without a real model backend.
pub struct MockModelClient {
    response_text: String,
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl MockModelClient {
    pub fn new(response_text: impl Into<String>) -> Self {
        Self {
            response_text: response_text.into(),
            prompt_tokens: 10,
            completion_tokens: 5,
        }
    }
}

impl ModelClient for MockModelClient {
    fn generate(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: self.response_text.clone(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            latency_ms: 1,
            source: ModelSource::Local("mock-model".to_string()),
        })
    }
}
