use anyhow::Result;
use vectorhawkd_manifest::ModelFallback;

/// A request to generate text from a language model.
#[derive(Clone, Default)]
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
    /// Manifest-declared list of recommended local models (Ollama tags).
    /// When non-empty, the routing layer requires the configured Ollama
    /// model to match one of these names; otherwise it skips local execution.
    pub recommended_models: Vec<String>,
    /// What to do when local execution is unavailable or rejected.
    /// `None` is treated as `McpSampling` (delegate to AI client).
    pub fallback: Option<ModelFallback>,
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

    /// Returns the configured local model name, if this backend runs locally
    /// (Ollama). Used by `HybridModelClient` to validate compatibility against
    /// a skill's `recommended_models` list before routing to local execution.
    fn local_model_name(&self) -> Option<&str> {
        None
    }
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

    /// Override the token counts returned by this mock.
    pub fn with_tokens(mut self, prompt: u64, completion: u64) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
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
