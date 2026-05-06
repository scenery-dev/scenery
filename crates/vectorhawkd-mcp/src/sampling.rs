use crate::protocol::{
    JsonRpcResponse, ModelPreferences, SamplingContent, SamplingCreateMessageParams,
    SamplingCreateMessageResult, SamplingMessage,
};
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use vectorhawkd_core::model::{ModelClient, ModelRequest, ModelResponse, ModelSource};
use vectorhawkd_manifest::ModelFallback;

/// Shared I/O handle used by both the MCP server loop and the sampling client.
///
/// The server loop reads JSON-RPC requests and writes responses. When an LLM
/// step triggers MCP sampling, the `McpSamplingClient` writes a
/// `sampling/createMessage` request and reads the client's response through the
/// same handle. Since the server loop is single-threaded and synchronous, there
/// is no concurrent access — the `Mutex` exists only to satisfy `Send + Sync`.
pub struct SharedIo {
    writer: Box<dyn Write + Send>,
    reader: Box<dyn BufRead + Send>,
}

impl SharedIo {
    pub fn new(writer: Box<dyn Write + Send>, reader: Box<dyn BufRead + Send>) -> Self {
        Self { writer, reader }
    }

    /// Read a single line from the input stream.
    pub fn read_line(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stdin closed",
            ));
        }
        Ok(line)
    }

    /// Write a JSON message followed by a newline, then flush.
    pub fn write_message(&mut self, json: &str) -> std::io::Result<()> {
        writeln!(self.writer, "{json}")?;
        self.writer.flush()
    }
}

/// A `ModelClient` that delegates LLM calls to the AI client via
/// `sampling/createMessage`.
///
/// This is used when Ollama is unavailable or when the local model cannot meet
/// the skill's model requirements. The MCP server sends a sampling request to
/// the connected AI client (Claude Code, Cursor, etc.) and waits for the
/// response over the same stdio channel.
pub struct McpSamplingClient {
    io: Arc<Mutex<SharedIo>>,
    next_id: Mutex<u64>,
}

impl McpSamplingClient {
    /// Create a sampling client with its own I/O handles (useful for tests).
    pub fn new(writer: Box<dyn Write + Send>, reader: Box<dyn BufRead + Send>) -> Self {
        Self {
            io: Arc::new(Mutex::new(SharedIo::new(writer, reader))),
            next_id: Mutex::new(1000),
        }
    }

    /// Create a sampling client that shares I/O with the server's main loop.
    pub fn from_shared(io: Arc<Mutex<SharedIo>>) -> Self {
        Self {
            io,
            next_id: Mutex::new(1000),
        }
    }

    /// Return a clone of the shared I/O handle (for the server loop to hold).
    pub fn shared_io(&self) -> Arc<Mutex<SharedIo>> {
        Arc::clone(&self.io)
    }

    fn next_request_id(&self) -> u64 {
        let mut id = self.next_id.lock().unwrap();
        let current = *id;
        *id += 1;
        current
    }
}

impl ModelClient for McpSamplingClient {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse> {
        let start = Instant::now();
        let request_id = self.next_request_id();

        let params = SamplingCreateMessageParams {
            messages: vec![SamplingMessage {
                role: "user".to_string(),
                content: SamplingContent {
                    content_type: "text".to_string(),
                    text: request.user_message,
                },
            }],
            system_prompt: Some(request.system_prompt),
            max_tokens: 4096,
            model_preferences: Some(ModelPreferences {
                hints: None,
                cost_priority: None,
                speed_priority: Some(0.5),
                intelligence_priority: Some(0.8),
            }),
        };

        let json_rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "sampling/createMessage",
            "params": params,
        });

        let mut io = self.io.lock().unwrap();

        let request_str = serde_json::to_string(&json_rpc_request)
            .context("failed to serialize sampling request")?;
        io.write_message(&request_str)
            .context("failed to write sampling request")?;

        let line = io.read_line().context("failed to read sampling response")?;

        let latency_ms = start.elapsed().as_millis() as u64;

        let response: JsonRpcResponse =
            serde_json::from_str(line.trim()).context("failed to parse sampling response")?;

        if let Some(error) = response.error {
            anyhow::bail!(
                "sampling/createMessage failed: {} (code {})",
                error.message,
                error.code
            );
        }

        let result_value = response
            .result
            .ok_or_else(|| anyhow::anyhow!("sampling response has no result"))?;

        let result: SamplingCreateMessageResult =
            serde_json::from_value(result_value).context("failed to parse sampling result")?;

        Ok(ModelResponse {
            text: result.content.text,
            prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms,
            source: ModelSource::McpSampling,
        })
    }
}

/// A `ModelClient` that tries the local Ollama model first and falls back to
/// MCP sampling delegation if Ollama is unavailable or fails.
pub struct HybridModelClient<'a> {
    ollama: Option<&'a dyn ModelClient>,
    sampling: &'a McpSamplingClient,
}

impl<'a> HybridModelClient<'a> {
    pub fn new(ollama: Option<&'a dyn ModelClient>, sampling: &'a McpSamplingClient) -> Self {
        Self { ollama, sampling }
    }
}

impl ModelClient for HybridModelClient<'_> {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse> {
        // prefer_local=true:  try Ollama → fall back to MCP sampling
        // prefer_local=false: try MCP sampling → fall back to Ollama
        if request.prefer_local {
            if let Some(ollama) = self.ollama {
                if local_model_compatible(ollama, &request.recommended_models) {
                    match ollama.generate(request.clone()) {
                        Ok(response) => return Ok(response),
                        Err(e) => {
                            tracing::warn!("local model failed: {e}");
                            if matches!(request.fallback, Some(ModelFallback::Error)) {
                                return Err(e);
                            }
                        }
                    }
                } else {
                    tracing::warn!(
                        "local model {:?} not in recommended list {:?}; honoring fallback",
                        ollama.local_model_name(),
                        request.recommended_models,
                    );
                    if matches!(request.fallback, Some(ModelFallback::Error)) {
                        anyhow::bail!(
                            "local model {:?} does not match any of the skill's recommended models {:?}, and fallback=error",
                            ollama.local_model_name().unwrap_or("(unknown)"),
                            request.recommended_models,
                        );
                    }
                }
            } else if matches!(request.fallback, Some(ModelFallback::Error)) {
                anyhow::bail!(
                    "local model preferred but no Ollama backend is configured, and fallback=error"
                );
            }
            tracing::debug!("delegating LLM call to AI client via MCP sampling");
            return self.sampling.generate(request);
        }

        tracing::debug!("delegating LLM call to AI client via MCP sampling");
        match self.sampling.generate(request.clone()) {
            Ok(response) => Ok(response),
            Err(sampling_err) => {
                if let Some(ollama) = self.ollama {
                    tracing::warn!(
                        "MCP sampling unavailable, falling back to local model: {sampling_err}"
                    );
                    return ollama.generate(request);
                }
                Err(sampling_err)
            }
        }
    }
}

/// Returns true if the configured local model satisfies the skill's
/// `recommended_models` list. An empty list means no constraint — any local
/// model is acceptable.
fn local_model_compatible(client: &dyn ModelClient, recommended: &[String]) -> bool {
    if recommended.is_empty() {
        return true;
    }
    let Some(name) = client.local_model_name() else {
        return false;
    };
    recommended.iter().any(|r| r == name)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use vectorhawkd_core::model::MockModelClient;

    #[test]
    fn sampling_client_sends_request_and_parses_response() {
        let response_json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1000,
            "result": {
                "role": "assistant",
                "content": {"type": "text", "text": "Hello from the AI client!"},
                "model": "claude-sonnet-4-5"
            }
        });
        let response_line = format!("{}\n", serde_json::to_string(&response_json).unwrap());

        let reader = Box::new(Cursor::new(response_line.into_bytes()));
        let writer = Box::new(Vec::<u8>::new());

        let client = McpSamplingClient::new(writer, reader);
        let result = client
            .generate(ModelRequest {
                system_prompt: "You are helpful.".to_string(),
                user_message: "Say hello".to_string(),
                json_output: false,
                prefer_local: false,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "Hello from the AI client!");
        assert!(result.latency_ms < 5000);
    }

    #[test]
    fn sampling_client_handles_error_response() {
        let response_json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1000,
            "error": {"code": -32600, "message": "Sampling not supported"}
        });
        let response_line = format!("{}\n", serde_json::to_string(&response_json).unwrap());

        let reader = Box::new(Cursor::new(response_line.into_bytes()));
        let writer = Box::new(Vec::<u8>::new());

        let client = McpSamplingClient::new(writer, reader);
        let err = client
            .generate(ModelRequest {
                system_prompt: "test".to_string(),
                user_message: "test".to_string(),
                json_output: false,
                prefer_local: false,
                ..Default::default()
            })
            .expect_err("should fail on error response");

        assert!(
            err.to_string().contains("Sampling not supported"),
            "got: {err}"
        );
    }

    #[test]
    fn hybrid_client_uses_local_when_prefer_local_true() {
        let mock = MockModelClient::new("local response");
        let response_json = serde_json::json!({
            "jsonrpc": "2.0", "id": 1000,
            "result": {"role": "assistant", "content": {"type": "text", "text": "remote"}}
        });
        let reader = Box::new(Cursor::new(
            format!("{}\n", serde_json::to_string(&response_json).unwrap()).into_bytes(),
        ));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&mock), &sampling);
        let result = hybrid
            .generate(ModelRequest {
                system_prompt: "test".to_string(),
                user_message: "test".to_string(),
                json_output: false,
                prefer_local: true,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "local response");
    }

    #[test]
    fn hybrid_client_delegates_when_prefer_local_false() {
        let mock = MockModelClient::new("local response");
        let response_json = serde_json::json!({
            "jsonrpc": "2.0", "id": 1000,
            "result": {"role": "assistant", "content": {"type": "text", "text": "remote response"}}
        });
        let reader = Box::new(Cursor::new(
            format!("{}\n", serde_json::to_string(&response_json).unwrap()).into_bytes(),
        ));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&mock), &sampling);
        let result = hybrid
            .generate(ModelRequest {
                system_prompt: "test".to_string(),
                user_message: "test".to_string(),
                json_output: false,
                prefer_local: false,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "remote response");
    }

    #[test]
    fn hybrid_client_falls_back_on_local_failure() {
        struct FailingClient;
        impl ModelClient for FailingClient {
            fn generate(&self, _request: ModelRequest) -> Result<ModelResponse> {
                anyhow::bail!("Ollama connection refused")
            }
        }

        let failing = FailingClient;
        let response_json = serde_json::json!({
            "jsonrpc": "2.0", "id": 1000,
            "result": {"role": "assistant", "content": {"type": "text", "text": "fallback response"}}
        });
        let reader = Box::new(Cursor::new(
            format!("{}\n", serde_json::to_string(&response_json).unwrap()).into_bytes(),
        ));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&failing), &sampling);
        let result = hybrid
            .generate(ModelRequest {
                system_prompt: "test".to_string(),
                user_message: "test".to_string(),
                json_output: false,
                prefer_local: true,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "fallback response");
    }

    /// A MockModelClient that reports a configurable local model name so we
    /// can exercise `recommended_models` compatibility checks.
    struct NamedLocalClient {
        name: String,
        response: String,
    }

    impl ModelClient for NamedLocalClient {
        fn local_model_name(&self) -> Option<&str> {
            Some(&self.name)
        }
        fn generate(&self, _request: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse {
                text: self.response.clone(),
                prompt_tokens: 0,
                completion_tokens: 0,
                latency_ms: 1,
                source: ModelSource::Local(self.name.clone()),
            })
        }
    }

    #[test]
    fn hybrid_uses_local_when_recommended_model_matches() {
        let local = NamedLocalClient {
            name: "llama3:8b".to_string(),
            response: "from local llama3".to_string(),
        };
        let response_json = serde_json::json!({
            "jsonrpc": "2.0", "id": 1000,
            "result": {"role": "assistant", "content": {"type": "text", "text": "from sampling"}}
        });
        let reader = Box::new(Cursor::new(
            format!("{}\n", serde_json::to_string(&response_json).unwrap()).into_bytes(),
        ));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&local), &sampling);
        let result = hybrid
            .generate(ModelRequest {
                prefer_local: true,
                recommended_models: vec!["llama3:8b".to_string()],
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "from local llama3");
    }

    #[test]
    fn hybrid_skips_local_when_recommended_model_doesnt_match() {
        // Local Ollama is configured with `mistral`, but the skill recommends
        // only `llama3:8b`. Routing must skip Ollama and use sampling.
        let local = NamedLocalClient {
            name: "mistral".to_string(),
            response: "should not be used".to_string(),
        };
        let response_json = serde_json::json!({
            "jsonrpc": "2.0", "id": 1000,
            "result": {"role": "assistant", "content": {"type": "text", "text": "from sampling"}}
        });
        let reader = Box::new(Cursor::new(
            format!("{}\n", serde_json::to_string(&response_json).unwrap()).into_bytes(),
        ));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&local), &sampling);
        let result = hybrid
            .generate(ModelRequest {
                prefer_local: true,
                recommended_models: vec!["llama3:8b".to_string()],
                fallback: Some(ModelFallback::McpSampling),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.text, "from sampling");
        assert!(matches!(result.source, ModelSource::McpSampling));
    }

    #[test]
    fn hybrid_errors_when_recommended_mismatch_and_fallback_error() {
        let local = NamedLocalClient {
            name: "mistral".to_string(),
            response: "ignored".to_string(),
        };
        // Sampling reader is unused — we expect an early error before sampling.
        let reader = Box::new(Cursor::new(Vec::<u8>::new()));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(Some(&local), &sampling);
        let err = hybrid
            .generate(ModelRequest {
                prefer_local: true,
                recommended_models: vec!["llama3:8b".to_string()],
                fallback: Some(ModelFallback::Error),
                ..Default::default()
            })
            .expect_err("should error when fallback=Error and local mismatch");

        assert!(err.to_string().contains("does not match"), "got: {err}");
    }

    #[test]
    fn hybrid_errors_when_no_local_and_fallback_error() {
        // No Ollama backend at all + prefer_local + fallback=Error → error.
        let reader = Box::new(Cursor::new(Vec::<u8>::new()));
        let writer = Box::new(Vec::<u8>::new());
        let sampling = McpSamplingClient::new(writer, reader);

        let hybrid = HybridModelClient::new(None, &sampling);
        let err = hybrid
            .generate(ModelRequest {
                prefer_local: true,
                fallback: Some(ModelFallback::Error),
                ..Default::default()
            })
            .expect_err("should error with no Ollama and fallback=Error");

        assert!(err.to_string().contains("no Ollama backend"), "got: {err}");
    }

    #[test]
    fn shared_io_read_and_write() {
        let input = b"hello world\n";
        let reader = Box::new(Cursor::new(input.to_vec()));
        let writer: Box<Vec<u8>> = Box::default();

        let io = Arc::new(Mutex::new(SharedIo::new(writer, reader)));

        {
            let mut handle = io.lock().unwrap();
            handle.write_message(r#"{"test":true}"#).unwrap();
        }

        {
            let mut handle = io.lock().unwrap();
            let line = handle.read_line().unwrap();
            assert_eq!(line.trim(), "hello world");
        }
    }
}
