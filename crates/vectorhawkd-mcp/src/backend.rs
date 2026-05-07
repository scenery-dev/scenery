//! The `Backend` trait and its three M0 implementations.
//!
//! # Architecture
//!
//! `Server<B: Backend>` is the MCP JSON-RPC dispatch loop. The `Backend` trait
//! is the seam between the loop and the actual work:
//!
//! ```text
//! AI client --stdio MCP--> Server<SocketBackend>   (shim normal path)
//!                          Server<EmbeddedBackend> (shim fallback path)
//! shim socket          --> Server<RealBackend>      (daemon)
//! ```
//!
//! # Implementations
//!
//! | Type | Who uses it | M0 status |
//! |---|---|---|
//! | `SocketBackend` | Shim (normal) | Scaffolded — relay loop lands in Stream 3 |
//! | `EmbeddedBackend` | Shim (fallback) | Functional — in-memory BackendRegistry |
//! | `RealBackend` | Daemon | Scaffolded — wired up fully in Stream 4 |
//!
//! # Local socket framing
//!
//! 4-byte big-endian length prefix followed by a UTF-8 JSON body.
//!
//! Rationale: length-prefixed framing is robust to embedded newlines in JSON
//! payloads (rare but possible in user-supplied strings). Newline-delimited
//! would be simpler but would require escaping any `\n` inside values.
//! The LSP-style Content-Length header is more complex than we need for a local
//! Unix socket. 4-byte prefix is the sweet spot: zero ambiguity, trivially
//! implemented in `tokio::io`, same convention used by many language servers
//! and wire protocols.

use crate::{
    aggregator::{BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility},
    protocol::{
        InitializeResult, ServerCapabilities, ServerInfo, ToolCallParams, ToolCallResult,
        ToolDefinition as ProtoToolDef, ToolsCapability, ToolsListResult,
    },
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(feature = "daemon")]
use crate::tools::UpdateCheckCache;

// ── Strict daemon response type ───────────────────────────────────────────────

/// A strict JSON-RPC response type used when deserializing daemon socket frames.
///
/// Unlike `JsonRpcResponse` (which tolerates unknown fields), this type uses
/// `#[serde(deny_unknown_fields)]` so that unexpected fields surface as a clear
/// deserialization error instead of silently mapping to `INTERNAL_ERROR`.
///
/// This catches daemon protocol drift early — if the daemon adds a required
/// field that the shim doesn't know about, the error message will include the
/// raw frame text, making the mismatch immediately obvious.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaemonResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonResponseError>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaemonResponseError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ── Backend trait ─────────────────────────────────────────────────────────────

/// The seam between the MCP dispatch loop and its work source.
///
/// The daemon installs `RealBackend`; the shim normally installs `SocketBackend`
/// and falls back to `EmbeddedBackend` when the socket is unreachable.
///
/// Methods receive and return fully-typed MCP params/results. The dispatch loop
/// handles JSON-RPC framing and error serialization around these calls.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Handle an `initialize` request. Returns server capabilities and metadata.
    async fn initialize(&self, params: Value) -> Result<InitializeResult>;

    /// Handle a `tools/list` request. Returns all currently available tools.
    async fn list_tools(&self, params: Value) -> Result<ToolsListResult>;

    /// Handle a `tools/call` request.
    async fn call_tool(&self, params: ToolCallParams) -> Result<ToolCallResult>;

    /// Optional: called when the server loop starts to allow the backend to
    /// perform any deferred initialization (e.g. connecting to the daemon
    /// socket, syncing the backend registry).
    async fn on_start(&self) -> Result<()> {
        Ok(())
    }

    /// Optional: called when the server loop exits cleanly to allow graceful
    /// shutdown (e.g. closing the socket, flushing audit events).
    async fn on_shutdown(&self) {}
}

// ── Framing helpers ───────────────────────────────────────────────────────────

/// Write a length-prefixed JSON frame to the stream.
///
/// Frame format: 4-byte big-endian length (bytes in body) + UTF-8 JSON body.
pub async fn write_framed<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

/// Read a length-prefixed JSON frame from the stream.
///
/// Returns `None` on clean EOF (peer closed connection).
pub async fn read_framed<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

// ── EmbeddedBackend ───────────────────────────────────────────────────────────

/// In-process backend that runs dispatch logic directly, with no daemon socket.
///
/// The shim uses this when the daemon socket is unreachable (>2 s timeout). It
/// holds its own `BackendRegistry` and serves the session from memory. No audit
/// upload, no registry sync, no OAuth callback — those require the long-lived
/// daemon process.
///
/// This is also the canonical reference implementation: every other stream can
/// use it as the integration test target before the daemon or socket relay is
/// ready.
pub struct EmbeddedBackend {
    registry: Arc<BackendRegistry>,
    server_name: String,
    server_version: String,
}

impl EmbeddedBackend {
    /// Create an embedded backend with a pre-populated registry.
    pub fn new(registry: Arc<BackendRegistry>) -> Self {
        Self {
            registry,
            server_name: "vectorhawkd".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Create an embedded backend with a single stub backend pre-registered.
    ///
    /// Convenience constructor for tests and the M0 shim fallback.
    pub fn with_stub_backend(server_id: &str, tools: &[(&str, &str)]) -> Self {
        let registry = Arc::new(BackendRegistry::new());
        let tool_defs: Vec<ToolDefinition> = tools
            .iter()
            .map(|(name, desc)| ToolDefinition {
                name: name.to_string(),
                description: Some(desc.to_string()),
                input_schema: None,
            })
            .collect();
        registry.register_backend(BackendEntry {
            server_id: server_id.to_string(),
            name: server_id.to_string(),
            transport: BackendTransport::Stub,
            tools: tool_defs,
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        });
        Self::new(registry)
    }
}

#[async_trait]
impl Backend for EmbeddedBackend {
    async fn initialize(&self, _params: Value) -> Result<InitializeResult> {
        Ok(InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: true }),
            },
            server_info: ServerInfo {
                name: self.server_name.clone(),
                version: self.server_version.clone(),
            },
            instructions: Some(
                "VectorHawk runner — governed AI platform. \
                 Running in in-process fallback mode (daemon unreachable). \
                 Audit, registry sync, and OAuth are unavailable in this session."
                    .to_string(),
            ),
        })
    }

    async fn list_tools(&self, _params: Value) -> Result<ToolsListResult> {
        let backend_tools = self.registry.all_tools();
        let tools: Vec<ProtoToolDef> = backend_tools
            .iter()
            .filter_map(|bt| {
                let name = bt["name"].as_str()?.to_string();
                let description = bt["description"].as_str().unwrap_or("").to_string();
                let input_schema = bt
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
                Some(ProtoToolDef {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect();
        Ok(ToolsListResult { tools })
    }

    async fn call_tool(&self, params: ToolCallParams) -> Result<ToolCallResult> {
        match self
            .registry
            .dispatch(&params.name, &params.arguments)
            .await
        {
            Some(Ok(value)) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                Ok(ToolCallResult::success(text))
            }
            Some(Err(e)) => Ok(ToolCallResult::error_result(format!("backend error: {e}"))),
            None => Ok(ToolCallResult::error_result(format!(
                "unknown tool: {}",
                params.name
            ))),
        }
    }

    async fn on_shutdown(&self) {
        self.registry.shutdown();
    }
}

// ── SocketBackend ─────────────────────────────────────────────────────────────
//
// Unix-only (macOS + Linux). Windows support deferred to M2/M3.

/// Backend that relays all MCP calls to the daemon over a Unix domain socket.
///
/// Each MCP method call is serialized as a JSON-RPC request, sent over the
/// socket with length-prefix framing, and the response is awaited and
/// deserialized. The daemon sees the shim as a client and dispatches the call
/// to `RealBackend`.
///
/// The shim instantiates this first; if the socket is unreachable within 2 s
/// it falls back to `EmbeddedBackend`.
///
/// # M0 status
///
/// Scaffolded — the struct and framing helpers exist; the actual relay loop
/// (`vectorhawkd-shim`) is completed in Stream 3.
#[cfg(unix)]
pub struct SocketBackend {
    socket_path: std::path::PathBuf,
    /// Live connection to the daemon, established during `on_start`.
    stream: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
}

#[cfg(unix)]
impl SocketBackend {
    pub fn new(socket_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            stream: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Connect to the daemon socket with a 2 s timeout.
    pub async fn connect(&self) -> Result<()> {
        let timeout = std::time::Duration::from_secs(2);
        let stream = tokio::time::timeout(timeout, UnixStream::connect(&self.socket_path))
            .await
            .context("daemon socket connect timed out (>2 s)")?
            .context("failed to connect to daemon socket")?;

        *self.stream.lock().await = Some(stream);
        Ok(())
    }

    /// Returns `true` if there is a live socket connection.
    pub async fn is_connected(&self) -> bool {
        self.stream.lock().await.is_some()
    }

    /// Send a JSON-RPC request over the socket and await the result value.
    ///
    /// Uses `DaemonResponse` (which applies `deny_unknown_fields`) so that
    /// unexpected fields in the daemon response surface as a clear
    /// deserialization error rather than silently mapping to INTERNAL_ERROR.
    async fn relay(&self, method: &str, params: Value) -> Result<Value> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_vec(&request).context("failed to serialize relay request")?;

        let mut guard = self.stream.lock().await;
        let stream = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("socket not connected — call on_start() first"))?;

        let (mut reader, mut writer) = stream.split();

        write_framed(&mut writer, &body)
            .await
            .context("failed to write framed request")?;

        let frame = read_framed(&mut reader)
            .await
            .context("failed to read framed response")?
            .ok_or_else(|| anyhow::anyhow!("daemon closed socket unexpectedly"))?;

        // Use the strict response type: unknown fields are surfaced as a
        // deserialization error rather than silently swallowed.
        let response: DaemonResponse = serde_json::from_slice(&frame).with_context(|| {
            // Include the raw frame text (truncated) to aid debugging.
            let preview: String = String::from_utf8_lossy(&frame).chars().take(200).collect();
            format!(
                "daemon response failed schema validation (unknown or missing fields): {}",
                preview
            )
        })?;

        if let Some(err) = response.error {
            anyhow::bail!("daemon returned error {}: {}", err.code, err.message);
        }

        response
            .result
            .ok_or_else(|| anyhow::anyhow!("daemon response has no result"))
    }
}

#[cfg(unix)]
#[async_trait]
impl Backend for SocketBackend {
    async fn on_start(&self) -> Result<()> {
        self.connect().await
    }

    async fn initialize(&self, params: Value) -> Result<InitializeResult> {
        let resp = self.relay("initialize", params).await?;
        serde_json::from_value(resp).context("failed to deserialize initialize result")
    }

    async fn list_tools(&self, params: Value) -> Result<ToolsListResult> {
        let resp = self.relay("tools/list", params).await?;
        serde_json::from_value(resp).context("failed to deserialize tools/list result")
    }

    async fn call_tool(&self, params: ToolCallParams) -> Result<ToolCallResult> {
        let params_value =
            serde_json::to_value(&params).context("failed to serialize tool call params")?;
        let resp = self.relay("tools/call", params_value).await?;
        serde_json::from_value(resp).context("failed to deserialize tools/call result")
    }
}

// ── RealBackend ───────────────────────────────────────────────────────────────

/// The daemon's backend implementation.
///
/// Holds the `BackendRegistry` (live HTTP/2 connections to approved backend MCP
/// servers), dispatches tool calls to the correct backend, and emits audit
/// events. The daemon instantiates `Server<RealBackend>` which listens on the
/// Unix socket and handles each shim connection in a separate task.
pub struct RealBackend {
    registry: Arc<BackendRegistry>,
    server_name: String,
    server_version: String,
    /// Optional audit sink. When `Some`, every successful or failed `call_tool`
    /// invocation records an `AuditEvent` via `spawn_blocking` so the SQLite
    /// write does not stall the current-thread async runtime.
    #[cfg(feature = "daemon")]
    audit: Option<Arc<dyn vectorhawkd_core::audit::AuditBuffer>>,
    /// Managed-deployment config when the runner is enrolled (`managed.json`
    /// present and `managed=true`). Drives the `initialize` instructions copy.
    #[cfg(feature = "daemon")]
    managed: Option<vectorhawkd_core::managed::ManagedConfig>,
    /// Application state (SQLite path, root dir). Required for management tool
    /// dispatch via `tools::handle_tool_call`.
    #[cfg(feature = "daemon")]
    state: Option<Arc<vectorhawkd_core::state::AppState>>,
    /// Registry URL forwarded to management tool handlers (search, install, etc.).
    #[cfg(feature = "daemon")]
    registry_url: Option<String>,
    /// Policy client used when executing skill steps through management tools.
    /// `MockPolicyClient` (allow-all) is the default until a registry-backed
    /// policy client is wired in a future stream.
    // TODO: replace with HttpPolicyClient once M1.4 wires it into the daemon.
    #[cfg(feature = "daemon")]
    policy_client: Option<Arc<dyn vectorhawkd_core::policy::PolicyClient + Send + Sync>>,
    /// Optional model client (Ollama or HybridModelClient). When `None`, skill
    /// steps that require a model will surface the existing "no model client"
    /// error message. A future stream will wire the model client here.
    #[cfg(feature = "daemon")]
    model_client: Option<Arc<dyn vectorhawkd_core::model::ModelClient>>,
    /// Per-skill update-check cache shared between all tool call invocations.
    #[cfg(feature = "daemon")]
    update_check_cache: Option<UpdateCheckCache>,
    /// Port the OAuth callback HTTP listener is bound to. `None` means no
    /// listener is running (ports exhausted or unsupported context).
    #[cfg(feature = "daemon")]
    oauth_listener_port: Option<u16>,
    /// Pub/sub hub for receiving authorization codes from the browser callback.
    #[cfg(feature = "daemon")]
    oauth_subscriber: Option<std::sync::Arc<dyn crate::oauth::OAuthSubscriber>>,
}

impl RealBackend {
    pub fn new(registry: Arc<BackendRegistry>) -> Self {
        Self {
            registry,
            server_name: "vectorhawkd".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            #[cfg(feature = "daemon")]
            audit: None,
            #[cfg(feature = "daemon")]
            managed: None,
            #[cfg(feature = "daemon")]
            state: None,
            #[cfg(feature = "daemon")]
            registry_url: None,
            #[cfg(feature = "daemon")]
            policy_client: None,
            #[cfg(feature = "daemon")]
            model_client: None,
            #[cfg(feature = "daemon")]
            update_check_cache: None,
            #[cfg(feature = "daemon")]
            oauth_listener_port: None,
            #[cfg(feature = "daemon")]
            oauth_subscriber: None,
        }
    }

    /// Construct a `RealBackend` with a backing audit buffer. Every `call_tool`
    /// emits a `tool_called` audit event via `tokio::task::spawn_blocking`.
    #[cfg(feature = "daemon")]
    pub fn with_audit(
        registry: Arc<BackendRegistry>,
        audit: Arc<dyn vectorhawkd_core::audit::AuditBuffer>,
    ) -> Self {
        Self {
            registry,
            server_name: "vectorhawkd".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            audit: Some(audit),
            managed: None,
            state: None,
            registry_url: None,
            policy_client: None,
            model_client: None,
            update_check_cache: None,
            oauth_listener_port: None,
            oauth_subscriber: None,
        }
    }

    /// Construct a `RealBackend` with both an audit buffer and managed-mode
    /// config. The managed config is used to render dynamic `initialize`
    /// instructions (BL1).
    #[cfg(feature = "daemon")]
    pub fn with_audit_and_managed(
        registry: Arc<BackendRegistry>,
        audit: Arc<dyn vectorhawkd_core::audit::AuditBuffer>,
        managed: Option<vectorhawkd_core::managed::ManagedConfig>,
    ) -> Self {
        Self {
            registry,
            server_name: "vectorhawkd".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            audit: Some(audit),
            managed,
            state: None,
            registry_url: None,
            policy_client: None,
            model_client: None,
            update_check_cache: None,
            oauth_listener_port: None,
            oauth_subscriber: None,
        }
    }

    /// Construct a `RealBackend` with full management-tool context.
    ///
    /// This is the constructor used by the daemon for production. It wires in
    /// all context needed for `tools::handle_tool_call` and
    /// `tools::build_tool_list` so that management tools (`vectorhawk_list`,
    /// `vectorhawk_install`, `vectorhawk_search`, etc.) are reachable from
    /// `list_tools` / `call_tool`.
    #[cfg(feature = "daemon")]
    #[allow(clippy::too_many_arguments)]
    pub fn with_full_context(
        registry: Arc<BackendRegistry>,
        audit: Arc<dyn vectorhawkd_core::audit::AuditBuffer>,
        managed: Option<vectorhawkd_core::managed::ManagedConfig>,
        state: Arc<vectorhawkd_core::state::AppState>,
        registry_url: Option<String>,
        policy_client: Arc<dyn vectorhawkd_core::policy::PolicyClient + Send + Sync>,
        model_client: Option<Arc<dyn vectorhawkd_core::model::ModelClient>>,
        update_check_cache: UpdateCheckCache,
    ) -> Self {
        Self {
            registry,
            server_name: "vectorhawkd".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            audit: Some(audit),
            managed,
            state: Some(state),
            registry_url,
            policy_client: Some(policy_client),
            model_client,
            update_check_cache: Some(update_check_cache),
            oauth_listener_port: None,
            oauth_subscriber: None,
        }
    }

    /// Attach the OAuth callback port and subscriber to this backend.
    ///
    /// Call this after any of the constructors above. Enables `vectorhawk_login`
    /// to complete the PKCE flow automatically after the browser redirects.
    ///
    /// When not called (or when the listener failed to bind), `vectorhawk_login`
    /// falls back to the legacy no-redirect URL and instructs the user to restart
    /// the daemon manually.
    #[cfg(feature = "daemon")]
    pub fn with_oauth(
        mut self,
        listener_port: u16,
        subscriber: std::sync::Arc<dyn crate::oauth::OAuthSubscriber>,
    ) -> Self {
        self.oauth_listener_port = Some(listener_port);
        self.oauth_subscriber = Some(subscriber);
        self
    }
}

#[async_trait]
impl Backend for RealBackend {
    async fn initialize(&self, _params: Value) -> Result<InitializeResult> {
        #[cfg(feature = "daemon")]
        let instructions = crate::instructions::build_instructions(self.managed.as_ref(), "daemon");
        #[cfg(not(feature = "daemon"))]
        let instructions = "VectorHawk runner — governed AI platform.".to_string();

        Ok(InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: true }),
            },
            server_info: ServerInfo {
                name: self.server_name.clone(),
                version: self.server_version.clone(),
            },
            instructions: Some(instructions),
        })
    }

    async fn list_tools(&self, _params: Value) -> Result<ToolsListResult> {
        // Management tools (vectorhawk_*) from the installed tools layer.
        // Only available when the full context has been wired (daemon path).
        #[cfg(feature = "daemon")]
        let mut tools: Vec<ProtoToolDef> = if let Some(state) = &self.state {
            crate::tools::build_tool_list(state, &self.registry_url)
                .into_iter()
                .map(|td| ProtoToolDef {
                    name: td.name,
                    description: td.description,
                    input_schema: td.input_schema,
                })
                .collect()
        } else {
            Vec::new()
        };

        #[cfg(not(feature = "daemon"))]
        let mut tools: Vec<ProtoToolDef> = Vec::new();

        // Merge in namespaced backend tools from the BackendRegistry.
        // Names are already namespaced with `__` so they cannot collide with
        // `vectorhawk_*` management tools.
        let backend_tools = self.registry.all_tools();
        for bt in &backend_tools {
            let name = match bt["name"].as_str() {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            let description = bt["description"].as_str().unwrap_or("").to_string();
            let input_schema = bt
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
            tools.push(ProtoToolDef {
                name,
                description,
                input_schema,
            });
        }

        Ok(ToolsListResult { tools })
    }

    async fn call_tool(&self, params: ToolCallParams) -> Result<ToolCallResult> {
        let started = std::time::Instant::now();

        // Route vectorhawk_* management tools and installed skill IDs through
        // the tools layer. Namespaced backend tools (containing `__`) go
        // straight to the BackendRegistry.
        #[cfg(feature = "daemon")]
        if let Some(state) = &self.state {
            // Intercept vectorhawk_login first so it receives an OAuthContext
            // when the daemon's callback listener is running. Falls through to
            // handle_tool_call (which calls the legacy handle_login) only when
            // state is absent — which cannot happen here.
            if params.name == "vectorhawk_login" {
                let oauth_ctx = match (self.oauth_listener_port, self.oauth_subscriber.as_ref()) {
                    (Some(port), Some(sub)) => Some(crate::oauth::OAuthContext {
                        listener_port: port,
                        subscriber: std::sync::Arc::clone(sub),
                    }),
                    _ => None,
                };
                let tool_result = crate::tools::handle_login_with_oauth(
                    &params.arguments,
                    state,
                    &self.registry_url,
                    oauth_ctx.as_ref(),
                );

                let latency_ms = started.elapsed().as_millis() as u64;
                let status = if tool_result.is_error == Some(true) {
                    "tool_error"
                } else {
                    "ok"
                };

                if let Some(audit) = &self.audit {
                    let audit = Arc::clone(audit);
                    let event = vectorhawkd_core::audit::AuditEvent {
                        event_type: "tool_called".to_string(),
                        payload: serde_json::json!({
                            "tool": params.name,
                            "status": status,
                            "latency_ms": latency_ms,
                        }),
                    };
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = audit.record(&event) {
                            tracing::warn!(error = %e, "failed to record audit event");
                        }
                    });
                }
                let _ = latency_ms;
                let _ = status;

                return Ok(tool_result);
            }

            let is_management_tool = params.name.starts_with("vectorhawk_");
            let is_backend_tool = params.name.contains("__");

            if is_management_tool || !is_backend_tool {
                use vectorhawkd_core::policy::MockPolicyClient;
                let empty_cache = std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                ));
                let cache = self
                    .update_check_cache
                    .as_ref()
                    .unwrap_or(&empty_cache);

                let tool_result = if let Some(pc) = &self.policy_client {
                    crate::tools::handle_tool_call(
                        &params.name,
                        &params.arguments,
                        state,
                        pc.as_ref(),
                        self.model_client.as_deref(),
                        &self.registry_url,
                        cache,
                        Some(&*self.registry),
                    )
                } else {
                    let mock_policy = MockPolicyClient::new();
                    crate::tools::handle_tool_call(
                        &params.name,
                        &params.arguments,
                        state,
                        &mock_policy,
                        self.model_client.as_deref(),
                        &self.registry_url,
                        cache,
                        Some(&*self.registry),
                    )
                };

                let latency_ms = started.elapsed().as_millis() as u64;
                let status = if tool_result.is_error == Some(true) {
                    "tool_error"
                } else {
                    "ok"
                };

                #[cfg(feature = "daemon")]
                if let Some(audit) = &self.audit {
                    let audit = Arc::clone(audit);
                    let event = vectorhawkd_core::audit::AuditEvent {
                        event_type: "tool_called".to_string(),
                        payload: serde_json::json!({
                            "tool": params.name,
                            "status": status,
                            "latency_ms": latency_ms,
                        }),
                    };
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = audit.record(&event) {
                            tracing::warn!(error = %e, "failed to record audit event");
                        }
                    });
                }
                let _ = status;
                let _ = latency_ms;

                return Ok(tool_result);
            }
        }

        let dispatch_outcome = self
            .registry
            .dispatch(&params.name, &params.arguments)
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let (result, status) = match dispatch_outcome {
            Some(Ok(value)) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                (Ok(ToolCallResult::success(text)), "ok")
            }
            Some(Err(e)) => (
                Ok(ToolCallResult::error_result(format!(
                    "backend dispatch error: {e}"
                ))),
                "backend_error",
            ),
            None => (
                Ok(ToolCallResult::error_result(format!(
                    "unknown tool: {}",
                    params.name
                ))),
                "unknown_tool",
            ),
        };

        // Best-effort audit emission. Failures here must not affect the AI
        // client's response, only log a warning. SQLite write goes through
        // spawn_blocking so the current-thread runtime is not stalled.
        #[cfg(feature = "daemon")]
        if let Some(audit) = &self.audit {
            let audit = Arc::clone(audit);
            let event = vectorhawkd_core::audit::AuditEvent {
                event_type: "tool_called".to_string(),
                payload: serde_json::json!({
                    "tool": params.name,
                    "status": status,
                    "latency_ms": latency_ms,
                    "args_size": params.arguments.to_string().len(),
                }),
            };
            tokio::task::spawn_blocking(move || {
                if let Err(e) = audit.record(&event) {
                    tracing::warn!(error = %e, "failed to record audit event");
                }
            });
        }
        let _ = status; // silence unused on no-default-feature builds
        let _ = latency_ms;

        result
    }

    async fn on_shutdown(&self) {
        self.registry.shutdown();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn make_embedded() -> EmbeddedBackend {
        EmbeddedBackend::with_stub_backend(
            "test",
            &[("do_thing", "Does a thing"), ("other_tool", "Another tool")],
        )
    }

    #[tokio::test]
    async fn embedded_initialize_returns_correct_protocol() {
        let backend = make_embedded().await;
        let result = backend.initialize(serde_json::json!({})).await.unwrap();
        assert_eq!(result.protocol_version, "2024-11-05");
        assert_eq!(result.server_info.name, "vectorhawkd");
        assert!(result.capabilities.tools.is_some());
    }

    #[tokio::test]
    async fn embedded_list_tools_returns_namespaced_tools() {
        let backend = make_embedded().await;
        let result = backend.list_tools(serde_json::json!({})).await.unwrap();
        let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"test__do_thing"),
            "expected test__do_thing in {names:?}"
        );
        assert!(
            names.contains(&"test__other_tool"),
            "expected test__other_tool in {names:?}"
        );
    }

    #[tokio::test]
    async fn embedded_call_tool_stub_returns_ok() {
        let backend = make_embedded().await;
        let result = backend
            .call_tool(ToolCallParams {
                name: "test__do_thing".to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert!(
            result.is_error.is_none(),
            "stub call should not be an error"
        );
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn embedded_call_unknown_tool_returns_error_result() {
        let backend = make_embedded().await;
        let result = backend
            .call_tool(ToolCallParams {
                name: "unknown__tool".to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn framing_write_produces_correct_byte_layout() {
        let payload = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let mut buf = Vec::new();
        write_framed(&mut buf, payload).await.unwrap();

        // First 4 bytes: big-endian length.
        let len_bytes: [u8; 4] = buf[..4].try_into().unwrap();
        let len = u32::from_be_bytes(len_bytes) as usize;
        assert_eq!(len, payload.len());
        // Remaining bytes: the payload verbatim.
        assert_eq!(&buf[4..], payload);
    }

    #[tokio::test]
    async fn framing_round_trip() {
        let payload = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let mut written = Vec::new();
        write_framed(&mut written, payload).await.unwrap();

        // Read back via an in-memory channel (avoids std::io::Cursor AsyncRead issue).
        let (mut server_half, mut client_half) = tokio::io::duplex(1024);
        client_half.write_all(&written).await.unwrap();
        drop(client_half); // close write side so EOF is clean

        let frame = read_framed(&mut server_half).await.unwrap().unwrap();
        assert_eq!(frame, payload);
    }
}
