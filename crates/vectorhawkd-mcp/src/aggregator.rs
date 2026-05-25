//! MCP aggregator — merges tools from multiple backend MCP servers into a
//! single namespaced tool surface.
//!
//! # Tool namespacing
//!
//! All proxied tools are prefixed with `{server_id}__{tool_name}` using a
//! double-underscore separator to avoid collisions when two backends expose
//! tools with the same bare name.
//!
//! ```text
//! github__create_issue     <- GitHub MCP "create_issue"
//! sentry__search_issues    <- Sentry MCP "search_issues"
//! ```
//!
//! # Tool budget
//!
//! Cursor and Windsurf cap MCP tool counts at 100. `ToolBudget` tracks usage
//! and truncates lower-priority backend tools when the limit would be exceeded.
//! Priority order (highest to lowest):
//!
//! 1. Governance / management tools (handled outside this module, priority 100)
//! 2. Installed skill execution tools (handled outside this module, priority 90)
//! 3. Backend proxied tools, ordered by `BackendEntry::priority` descending
//!
//! # Health tracking
//!
//! Each backend tracks consecutive errors. After `UNHEALTHY_THRESHOLD` errors in
//! a row the backend is marked unhealthy and excluded from `tools/list` until a
//! successful call resets the error count to zero.
//!
//! # Transport variants
//!
//! - `Stub` — in-memory echo, always available, used for tests and the fallback.
//! - `Http` — async reqwest HTTP/2 call to a remote MCP server endpoint.
//!   Lazy: no connection is opened until the first tool call.
//! - `Stdio` — spawns a child process MCP server. Uses `tokio::task::spawn_blocking`
//!   to bridge the sync blocking I/O of `StdioProcess` into the async runtime.

use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    cmp::Reverse,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

use crate::stdio_process::StdioProcess;

// ── Tool budget ───────────────────────────────────────────────────────────────

/// Maximum number of MCP tools this aggregator will surface to the AI client.
pub const TOOL_BUDGET_TOTAL: usize = 100;

/// Slots reserved for governance / management / skill execution tools managed
/// outside this module.
pub const RESERVED_SLOTS: usize = 20;

/// The remaining budget available for proxied backend tools.
pub const BACKEND_TOOL_BUDGET: usize = TOOL_BUDGET_TOTAL - RESERVED_SLOTS;

/// Tracks tool-count usage across backend servers and enforces the budget cap.
#[derive(Debug, Default)]
pub struct ToolBudget {
    used: usize,
    truncated_servers: Vec<String>,
}

impl ToolBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Slots still available for proxied backend tools.
    pub fn remaining(&self) -> usize {
        BACKEND_TOOL_BUDGET.saturating_sub(self.used)
    }

    /// Returns `true` if there is budget for `n` more tools.
    pub fn has_room_for(&self, n: usize) -> bool {
        self.used + n <= BACKEND_TOOL_BUDGET
    }

    /// Consume `n` slots.
    pub fn consume(&mut self, n: usize) {
        self.used += n;
    }

    /// Record that a server's tools were partially or fully truncated.
    pub fn record_truncation(&mut self, server_id: &str) {
        if !self.truncated_servers.contains(&server_id.to_string()) {
            self.truncated_servers.push(server_id.to_string());
        }
    }

    /// Servers that had at least one tool truncated from the budget.
    pub fn truncated_servers(&self) -> &[String] {
        &self.truncated_servers
    }

    /// Reset for a full re-sync.
    pub fn reset(&mut self) {
        self.used = 0;
        self.truncated_servers.clear();
    }
}

// ── Tool definition ───────────────────────────────────────────────────────────

/// A single MCP tool definition as returned by a backend's `tools/list`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

// ── Backend entry ─────────────────────────────────────────────────────────────

/// Tool visibility policy for a backend.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolVisibility {
    /// Surface all tools from this backend.
    All,
    /// Surface only the listed tools.
    Curated(Vec<String>),
    /// Do not surface tools by default.
    OnDemand,
}

impl ToolVisibility {
    /// Filter a list of tool definitions according to this visibility policy.
    pub fn filter<'a>(&self, tools: &'a [ToolDefinition]) -> Vec<&'a ToolDefinition> {
        match self {
            ToolVisibility::All => tools.iter().collect(),
            ToolVisibility::Curated(allowed) => {
                tools.iter().filter(|t| allowed.contains(&t.name)).collect()
            }
            ToolVisibility::OnDemand => vec![],
        }
    }
}

/// Number of consecutive errors before a backend is marked unhealthy.
const UNHEALTHY_THRESHOLD: u32 = 3;

/// Transport variant for a backend MCP server.
#[derive(Debug)]
pub enum BackendTransport {
    /// In-memory stub — used for tests and the EmbeddedBackend fallback.
    Stub,
    /// HTTP MCP endpoint. Dispatches JSON-RPC via reqwest HTTP/2 (async).
    /// Lazy dial: no connection opened until first tool call.
    Http {
        url: String,
        /// Optional Bearer token for gateway-authenticated backends.
        auth_token: Option<String>,
    },
    /// Stdio child-process backend.
    ///
    /// The `process` field holds the live child process once lazily spawned.
    /// `Arc<Mutex<Option<...>>>` keeps `Clone` working while sharing one process
    /// across registry clones.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        /// Shared slot for the live child process; `None` until first use.
        process: Arc<Mutex<Option<StdioProcess>>>,
    },
}

// Manual impl: StdioProcess doesn't impl Clone, so we need special handling.
// Cloning a Stdio transport shares the same Arc (same child process), which is
// the correct behaviour — registry clones should reuse an already-spawned child.
impl Clone for BackendTransport {
    fn clone(&self) -> Self {
        match self {
            BackendTransport::Stub => BackendTransport::Stub,
            BackendTransport::Http { url, auth_token } => BackendTransport::Http {
                url: url.clone(),
                auth_token: auth_token.clone(),
            },
            BackendTransport::Stdio {
                command,
                args,
                env,
                process,
            } => BackendTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                process: process.clone(),
            },
        }
    }
}

/// A registered backend MCP server.
#[derive(Debug, Clone)]
pub struct BackendEntry {
    pub server_id: String,
    pub name: String,
    pub transport: BackendTransport,
    pub tools: Vec<ToolDefinition>,
    pub tool_visibility: ToolVisibility,
    pub priority: u8,
    /// Consecutive error count; reset to 0 on success.
    pub consecutive_errors: u32,
    /// Whether this backend has been marked unhealthy.
    pub unhealthy: bool,
}

impl BackendEntry {
    /// Iterate tools filtered by visibility policy.
    pub fn visible_tools(&self) -> Vec<&ToolDefinition> {
        if self.unhealthy {
            return vec![];
        }
        self.tool_visibility.filter(&self.tools)
    }

    /// Record a successful dispatch; reset error counter.
    pub fn record_success(&mut self) {
        self.consecutive_errors = 0;
        self.unhealthy = false;
    }

    /// Record a dispatch error. Returns `true` if this pushed the backend
    /// over the unhealthy threshold.
    pub fn record_error(&mut self) -> bool {
        self.consecutive_errors += 1;
        if self.consecutive_errors >= UNHEALTHY_THRESHOLD {
            if !self.unhealthy {
                warn!(
                    server_id = %self.server_id,
                    consecutive_errors = self.consecutive_errors,
                    "backend marked unhealthy after consecutive errors"
                );
                self.unhealthy = true;
            }
            return true;
        }
        false
    }
}

// ── Backend registry ──────────────────────────────────────────────────────────

/// Lightweight summary of a registered backend, returned by `BackendRegistry::list_backends`.
#[derive(Debug, Clone)]
pub struct BackendSummary {
    pub server_id: String,
    pub name: String,
    pub tool_count: usize,
    pub unhealthy: bool,
}

struct RegistryInner {
    backends: HashMap<String, BackendEntry>,
    budget: ToolBudget,
    last_synced: Option<Instant>,
}

/// The central MCP aggregator. Manages all backend connections and exposes a
/// merged, namespaced tool surface to `Server<B>`.
///
/// Clone is cheap — the inner state is behind an `Arc<Mutex<_>>`.
#[derive(Clone)]
pub struct BackendRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    /// Shared async HTTP client used for HTTP transport dispatch.
    http: reqwest::Client,
}

impl BackendRegistry {
    /// Create a new, empty registry. Call `register_backend` to populate.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            // HTTP/2 is used automatically when the server supports it via ALPN.
            // The reqwest 0.12 + rustls TLS backend handles protocol negotiation.
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                backends: HashMap::new(),
                budget: ToolBudget::new(),
                last_synced: None,
            })),
            http,
        }
    }

    /// Register a backend entry directly. Used by the daemon and tests.
    ///
    /// Backends are inserted in priority order during bulk loads; when called
    /// for a single entry the budget is updated accordingly. If a backend with
    /// the same `server_id` already exists it is replaced.
    pub fn register_backend(&self, entry: BackendEntry) {
        let mut inner = self.inner.lock().unwrap();
        let server_id = entry.server_id.clone();
        let tool_count = entry.visible_tools().len();

        let budget_slots = if inner.budget.has_room_for(tool_count) {
            inner.budget.consume(tool_count);
            tool_count
        } else {
            let remaining = inner.budget.remaining();
            if tool_count > 0 {
                inner.budget.record_truncation(&server_id);
            }
            inner.budget.consume(remaining);
            remaining
        };

        // Truncate tools to budget if necessary.
        let mut entry = entry;
        entry.tools.truncate(budget_slots);

        if inner.backends.contains_key(&server_id) {
            debug!(server_id = %server_id, tools = budget_slots, "refreshed backend");
        } else {
            info!(server_id = %server_id, tools = budget_slots, "registered backend");
        }
        inner.backends.insert(server_id, entry);
        inner.last_synced = Some(Instant::now());
    }

    /// Register multiple backends at once, allocating budget in priority order
    /// (highest priority first). This is the preferred bulk-load path.
    ///
    /// All existing backends are cleared and the budget is reset before loading.
    pub fn register_backends_with_priority(&self, mut entries: Vec<BackendEntry>) {
        // Sort descending by priority so highest-priority backends get budget first.
        entries.sort_by_key(|e| Reverse(e.priority));

        let mut inner = self.inner.lock().unwrap();
        inner.backends.clear();
        inner.budget.reset();

        for entry in entries {
            let server_id = entry.server_id.clone();
            let tool_count = entry.visible_tools().len();

            let budget_slots = if inner.budget.has_room_for(tool_count) {
                inner.budget.consume(tool_count);
                tool_count
            } else {
                let remaining = inner.budget.remaining();
                if tool_count > 0 {
                    inner.budget.record_truncation(&server_id);
                }
                inner.budget.consume(remaining);
                remaining
            };

            let mut entry = entry;
            entry.tools.truncate(budget_slots);

            info!(server_id = %server_id, tools = budget_slots, priority = entry.priority, "registered backend");
            inner.backends.insert(server_id, entry);
        }

        inner.last_synced = Some(Instant::now());
        info!(
            backends = inner.backends.len(),
            "bulk backend registration complete"
        );
    }

    /// Returns all namespaced tool definitions from all active (healthy) backends.
    pub fn all_tools(&self) -> Vec<Value> {
        self.all_tools_excluding(&std::collections::HashSet::new())
    }

    /// Like [`all_tools`] but skips any backend whose `server_id` is in
    /// `excluded_server_ids`.
    ///
    /// The daemon's MCP server passes the set of F2-pushed slugs here so that
    /// backends already surfaced as per-server entries in `~/.claude.json`
    /// don't get double-exposed under the `vectorhawk` aggregator namespace.
    /// Without this filter, Claude Code sees the same tool twice — once as
    /// `<slug>__tool` (native) and once as `vectorhawk__<slug>__tool` (nested).
    pub fn all_tools_excluding(
        &self,
        excluded_server_ids: &std::collections::HashSet<String>,
    ) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();

        for backend in inner.backends.values() {
            if backend.unhealthy {
                continue;
            }
            if excluded_server_ids.contains(&backend.server_id) {
                continue;
            }
            let server_id = &backend.server_id;
            for tool in backend.visible_tools() {
                let namespaced_name = namespace_tool(server_id, &tool.name);
                let mut tool_json = serde_json::json!({
                    "name": namespaced_name,
                });
                if let Some(desc) = &tool.description {
                    tool_json["description"] =
                        Value::String(format!("[{}] {}", backend.name, desc));
                }
                if let Some(schema) = &tool.input_schema {
                    tool_json["inputSchema"] = schema.clone();
                }
                out.push(tool_json);
            }
        }

        out
    }

    /// Dispatch a namespaced tool call to the appropriate backend.
    ///
    /// Returns `None` if the tool name does not match any registered backend
    /// (i.e. it should be handled by the skill/governance layer instead).
    ///
    /// For Stub and Http transports, the inner lock is released before I/O.
    /// For Stdio, the process Arc is cloned and the lock is released before
    /// calling `spawn_blocking`, so concurrent tool calls are not serialized.
    pub async fn dispatch(&self, namespaced_tool: &str, args: &Value) -> Option<Result<Value>> {
        let (server_id, original_tool) = parse_tool_name(namespaced_tool)?;

        // ── Extract dispatch target from locked state ─────────────────────────
        enum DispatchTarget {
            Stub {
                response: String,
            },
            Http {
                url: String,
                auth_token: Option<String>,
            },
            Stdio {
                process: Arc<Mutex<Option<StdioProcess>>>,
                command: String,
                args_list: Vec<String>,
                env: HashMap<String, String>,
            },
        }

        let (target, server_id_owned) = {
            let inner = self.inner.lock().unwrap();
            let backend = inner.backends.get(server_id)?;

            if backend.unhealthy {
                return Some(Err(anyhow::anyhow!(
                    "backend '{}' is unhealthy (too many consecutive errors)",
                    server_id
                )));
            }

            // Verify the tool exists on this backend.
            let tool_exists = backend.tools.iter().any(|t| t.name == original_tool);
            if !tool_exists {
                return Some(Err(anyhow::anyhow!(
                    "tool '{}' not found on backend '{}'",
                    original_tool,
                    server_id
                )));
            }

            let target = match &backend.transport {
                BackendTransport::Stub => DispatchTarget::Stub {
                    response: format!("stub response for {namespaced_tool}: {args}"),
                },
                BackendTransport::Http { url, auth_token } => DispatchTarget::Http {
                    url: url.clone(),
                    auth_token: auth_token.clone(),
                },
                BackendTransport::Stdio {
                    command,
                    args: args_list,
                    env,
                    process,
                } => DispatchTarget::Stdio {
                    process: process.clone(),
                    command: command.clone(),
                    args_list: args_list.clone(),
                    env: env.clone(),
                },
            };

            (target, server_id.to_string())
            // inner lock released here
        };

        let args_owned = args.clone();
        let tool_name_owned = original_tool.to_string();

        let result = match target {
            DispatchTarget::Stub { response } => {
                // Test hook: `VECTORHAWK_STUB_LATENCY_MS` injects a blocking
                // sleep wrapped in `spawn_blocking` to simulate a slow real
                // backend. Used by the M1.7 blocking-I/O stress test to
                // validate that independent calls aren't head-of-line blocked
                // when other calls are doing slow blocking work. No-op when
                // the env var is unset, so production code path is unchanged.
                if let Some(latency_ms) = std::env::var("VECTORHAWK_STUB_LATENCY_MS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    let _ = tokio::task::spawn_blocking(move || {
                        std::thread::sleep(std::time::Duration::from_millis(latency_ms));
                    })
                    .await;
                }
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": response}]
                }))
            }

            DispatchTarget::Http { url, auth_token } => {
                self.call_http_tool(&url, &tool_name_owned, &args_owned, auth_token.as_deref())
                    .await
            }

            DispatchTarget::Stdio {
                process,
                command,
                args_list,
                env,
            } => tokio::task::spawn_blocking(move || {
                call_stdio_tool_with_arc(
                    &process,
                    &command,
                    &args_list,
                    &env,
                    &tool_name_owned,
                    &args_owned,
                )
            })
            .await
            .context("spawn_blocking panicked in stdio dispatch")
            .and_then(|r| r),
        };

        // ── Update health state ───────────────────────────────────────────────
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(backend) = inner.backends.get_mut(&server_id_owned) {
                match &result {
                    Ok(_) => backend.record_success(),
                    Err(_) => {
                        backend.record_error();
                    }
                }
            }
        }

        Some(result)
    }

    /// Shut down all backend connections gracefully.
    /// For stdio backends this sends a kill signal to the child process.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        let count = inner.backends.len();

        for backend in inner.backends.values() {
            if let BackendTransport::Stdio { process, .. } = &backend.transport {
                if let Ok(mut guard) = process.lock() {
                    if let Some(proc) = guard.as_mut() {
                        let _ = proc.shutdown();
                    }
                }
            }
        }

        inner.backends.clear();
        inner.budget.reset();
        info!(backends = count, "aggregator shut down");
    }

    /// How long since the last successful sync, or `None` if never synced.
    pub fn time_since_sync(&self) -> Option<Duration> {
        self.inner.lock().unwrap().last_synced.map(|t| t.elapsed())
    }

    /// Number of currently active backends.
    pub fn backend_count(&self) -> usize {
        self.inner.lock().unwrap().backends.len()
    }

    /// Whether a backend with the given ID is active.
    pub fn has_backend(&self, server_id: &str) -> bool {
        self.inner.lock().unwrap().backends.contains_key(server_id)
    }

    /// Remove a single backend by ID. Returns `true` if one was removed.
    pub fn remove_backend(&self, server_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let removed = inner.backends.remove(server_id).is_some();
        if removed {
            info!(server_id = %server_id, "removed backend from aggregator");
        }
        removed
    }

    /// Get all namespaced tool names for a specific backend.
    pub fn backend_tools(&self, server_id: &str) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        match inner.backends.get(server_id) {
            Some(conn) => conn
                .tools
                .iter()
                .map(|t| format!("{server_id}__{}", t.name))
                .collect(),
            None => vec![],
        }
    }

    /// IDs of backends that had tools truncated due to the tool budget.
    pub fn truncated_backends(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .budget
            .truncated_servers()
            .to_vec()
    }

    /// IDs of backends currently marked unhealthy.
    pub fn unhealthy_backends(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .backends
            .values()
            .filter(|b| b.unhealthy)
            .map(|b| b.server_id.clone())
            .collect()
    }

    /// Snapshot of all registered backends, ordered by server_id.
    ///
    /// Returns lightweight summary rows — avoids cloning full tool lists.
    pub fn list_backends(&self) -> Vec<BackendSummary> {
        let inner = self.inner.lock().unwrap();
        let mut entries: Vec<BackendSummary> = inner
            .backends
            .values()
            .map(|b| BackendSummary {
                server_id: b.server_id.clone(),
                name: b.name.clone(),
                tool_count: b.tools.len(),
                unhealthy: b.unhealthy,
            })
            .collect();
        entries.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        entries
    }

    /// Mark a backend healthy again (resets consecutive error count).
    /// Called externally by the health-check loop if a probe succeeds.
    pub fn mark_healthy(&self, server_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(backend) = inner.backends.get_mut(server_id) {
            if backend.unhealthy {
                info!(server_id = %server_id, "backend recovered — marking healthy");
                backend.consecutive_errors = 0;
                backend.unhealthy = false;
            }
        }
    }

    // ── Private HTTP dispatch ─────────────────────────────────────────────────

    /// Call a tool on an HTTP backend using async reqwest.
    ///
    /// MCP Streamable HTTP transport: POST JSON-RPC to the endpoint URL.
    /// The method is in the request body, not appended to the path.
    async fn call_http_tool(
        &self,
        base_url: &str,
        tool_name: &str,
        args: &Value,
        auth_token: Option<&str>,
    ) -> Result<Value> {
        let url = base_url.trim_end_matches('/').to_string();
        debug!(url = %url, tool = %tool_name, "dispatching tool call to HTTP backend");

        let mut req = self.http.post(&url).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": args,
            }
        }));

        if let Some(token) = auth_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("failed to reach HTTP backend at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("HTTP backend tool call failed ({status}): {body}");
        }

        let body: Value = resp
            .json()
            .await
            .context("failed to parse HTTP backend tools/call response")?;

        if let Some(err) = body.get("error") {
            anyhow::bail!("HTTP backend returned JSON-RPC error: {err}");
        }

        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Fetch the tool list from an HTTP MCP backend. Used during registration.
    pub async fn fetch_tools_http(
        &self,
        url: &str,
        auth_token: Option<&str>,
    ) -> Result<Vec<ToolDefinition>> {
        let tools_url = url.trim_end_matches('/').to_string();
        debug!(url = %tools_url, "fetching tools from HTTP backend");

        let mut req = self.http.post(&tools_url).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }));

        if let Some(token) = auth_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("failed to reach HTTP backend at {tools_url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            anyhow::bail!("HTTP backend returned HTTP {status} on tools/list");
        }

        let body: Value = resp
            .json()
            .await
            .context("failed to parse tools/list response")?;

        let tools = body
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(tools)
    }

    /// Fetch the tool list from a stdio backend (blocking; wraps spawn_blocking).
    pub async fn fetch_tools_stdio(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Vec<ToolDefinition>> {
        let command = command.to_string();
        let args = args.to_vec();
        let env = env.clone();

        tokio::task::spawn_blocking(move || {
            let mut proc = StdioProcess::spawn(&command, &args, &env)
                .context("failed to spawn stdio backend for tool fetch")?;
            proc.list_tools()
        })
        .await
        .context("spawn_blocking panicked in fetch_tools_stdio")
        .and_then(|r| r)
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Stdio dispatch helpers ────────────────────────────────────────────────────

/// Spawn a new `StdioProcess` into `slot` if the slot is `None` or the
/// existing process is no longer alive.
///
/// On failure the slot is left as `None` and the error is returned.
fn spawn_if_needed(
    slot: &mut Option<StdioProcess>,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let needs_spawn = match slot.as_mut() {
        None => true,
        Some(p) => !p.is_alive(),
    };

    if !needs_spawn {
        return Ok(());
    }

    if slot.is_some() {
        warn!(command = %command, "stdio backend process died — attempting restart");
        *slot = None;
    }

    let proc = StdioProcess::spawn(command, args, env)
        .with_context(|| format!("failed to spawn stdio backend '{command}'"))?;

    *slot = Some(proc);
    debug!(command = %command, "stdio backend process spawned");
    Ok(())
}

/// Call a tool on a stdio backend, handling lazy spawn and one-shot restart.
///
/// Accepts the `Arc<Mutex<Option<StdioProcess>>>` so the outer registry lock
/// is not held during I/O.
///
/// This function is synchronous and must be called from within `spawn_blocking`.
fn call_stdio_tool_with_arc(
    proc_arc: &Arc<Mutex<Option<StdioProcess>>>,
    command: &str,
    args_list: &[String],
    env: &HashMap<String, String>,
    tool_name: &str,
    arguments: &Value,
) -> Result<Value> {
    let mut guard = proc_arc.lock().unwrap();
    spawn_if_needed(&mut guard, command, args_list, env)?;

    let proc = guard.as_mut().unwrap(); // guaranteed by spawn_if_needed
    proc.call_tool(tool_name, arguments)
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Namespace a tool name: `{server_id}__{tool_name}`.
///
/// The double-underscore separator is chosen because it cannot appear in
/// standard MCP tool names (which follow identifier conventions).
pub fn namespace_tool(server_id: &str, tool_name: &str) -> String {
    format!("{server_id}__{tool_name}")
}

/// Parse a namespaced tool name back into `(server_id, original_tool_name)`.
///
/// Returns `None` if the name does not contain the `__` separator (i.e. it is
/// a skill or governance tool, not a proxied backend tool).
pub fn parse_tool_name(namespaced: &str) -> Option<(&str, &str)> {
    let pos = namespaced.find("__")?;
    let server_id = &namespaced[..pos];
    let tool_name = &namespaced[pos + 2..];
    if server_id.is_empty() || tool_name.is_empty() {
        None
    } else {
        Some((server_id, tool_name))
    }
}

/// Convert a display name into a valid identifier (lowercase, dashes only).
pub fn sanitize_id(name: &str) -> String {
    let raw: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    raw.trim_matches('-').to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_backend(server_id: &str, tools: &[&str]) -> BackendEntry {
        BackendEntry {
            server_id: server_id.to_string(),
            name: server_id.to_string(),
            transport: BackendTransport::Stub,
            tools: tools
                .iter()
                .map(|n| ToolDefinition {
                    name: n.to_string(),
                    description: Some(format!("desc for {n}")),
                    input_schema: None,
                })
                .collect(),
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        }
    }

    // ── parse_tool_name ───────────────────────────────────────────────────────

    #[test]
    fn parse_tool_name_splits_on_double_underscore() {
        let (server, tool) = parse_tool_name("github__create_issue").unwrap();
        assert_eq!(server, "github");
        assert_eq!(tool, "create_issue");
    }

    #[test]
    fn parse_tool_name_returns_none_without_separator() {
        assert!(parse_tool_name("skillclub_search").is_none());
        assert!(parse_tool_name("create_issue").is_none());
        assert!(parse_tool_name("nodoubleunderscore").is_none());
    }

    #[test]
    fn parse_tool_name_returns_none_for_empty_parts() {
        assert!(parse_tool_name("__tool").is_none());
        assert!(parse_tool_name("server__").is_none());
        assert!(parse_tool_name("__").is_none());
    }

    // ── namespace_tool ────────────────────────────────────────────────────────

    #[test]
    fn namespace_tool_produces_double_underscore_prefix() {
        assert_eq!(
            namespace_tool("sentry", "search_issues"),
            "sentry__search_issues"
        );
        assert_eq!(namespace_tool("github", "create_pr"), "github__create_pr");
    }

    // ── ToolBudget ────────────────────────────────────────────────────────────

    #[test]
    fn tool_budget_tracks_remaining_correctly() {
        let mut budget = ToolBudget::new();
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET);
        budget.consume(10);
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET - 10);
    }

    #[test]
    fn tool_budget_has_room_for_works() {
        let mut budget = ToolBudget::new();
        assert!(budget.has_room_for(BACKEND_TOOL_BUDGET));
        budget.consume(BACKEND_TOOL_BUDGET);
        assert!(!budget.has_room_for(1));
    }

    #[test]
    fn tool_budget_records_truncation() {
        let mut budget = ToolBudget::new();
        budget.record_truncation("github");
        budget.record_truncation("sentry");
        budget.record_truncation("github"); // duplicate — should not double-count
        assert_eq!(budget.truncated_servers().len(), 2);
        assert!(budget.truncated_servers().contains(&"github".to_string()));
    }

    #[test]
    fn tool_budget_resets_cleanly() {
        let mut budget = ToolBudget::new();
        budget.consume(50);
        budget.record_truncation("github");
        budget.reset();
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET);
        assert!(budget.truncated_servers().is_empty());
    }

    // ── ToolVisibility ────────────────────────────────────────────────────────

    #[test]
    fn tool_visibility_all_passes_all_tools() {
        let tools = vec![
            ToolDefinition {
                name: "a".to_string(),
                description: None,
                input_schema: None,
            },
            ToolDefinition {
                name: "b".to_string(),
                description: None,
                input_schema: None,
            },
        ];
        let visible = ToolVisibility::All.filter(&tools);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn tool_visibility_curated_filters_to_allowed_list() {
        let tools = vec![
            ToolDefinition {
                name: "create_issue".to_string(),
                description: None,
                input_schema: None,
            },
            ToolDefinition {
                name: "delete_repo".to_string(),
                description: None,
                input_schema: None,
            },
            ToolDefinition {
                name: "list_prs".to_string(),
                description: None,
                input_schema: None,
            },
        ];
        let visible =
            ToolVisibility::Curated(vec!["create_issue".to_string(), "list_prs".to_string()])
                .filter(&tools);
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|t| t.name == "create_issue"));
        assert!(visible.iter().any(|t| t.name == "list_prs"));
        assert!(!visible.iter().any(|t| t.name == "delete_repo"));
    }

    #[test]
    fn tool_visibility_on_demand_returns_empty() {
        let tools = vec![ToolDefinition {
            name: "a".to_string(),
            description: None,
            input_schema: None,
        }];
        assert!(ToolVisibility::OnDemand.filter(&tools).is_empty());
    }

    // ── sanitize_id ───────────────────────────────────────────────────────────

    #[test]
    fn sanitize_id_lowercases_and_replaces_spaces() {
        assert_eq!(sanitize_id("GitHub MCP"), "github-mcp");
        assert_eq!(sanitize_id("Sentry.io"), "sentry-io");
        assert_eq!(sanitize_id("plain"), "plain");
    }

    // ── BackendEntry health ───────────────────────────────────────────────────

    #[test]
    fn backend_entry_becomes_unhealthy_after_threshold() {
        let mut entry = stub_backend("test", &["tool_a"]);
        for _ in 0..UNHEALTHY_THRESHOLD - 1 {
            let became_unhealthy = entry.record_error();
            assert!(!became_unhealthy);
        }
        let became_unhealthy = entry.record_error();
        assert!(became_unhealthy);
        assert!(entry.unhealthy);
    }

    #[test]
    fn backend_entry_recovers_on_success() {
        let mut entry = stub_backend("test", &["tool_a"]);
        for _ in 0..UNHEALTHY_THRESHOLD {
            entry.record_error();
        }
        assert!(entry.unhealthy);
        entry.record_success();
        assert!(!entry.unhealthy);
        assert_eq!(entry.consecutive_errors, 0);
    }

    #[test]
    fn unhealthy_backend_hides_tools() {
        let mut entry = stub_backend("test", &["tool_a", "tool_b"]);
        assert_eq!(entry.visible_tools().len(), 2);
        entry.unhealthy = true;
        assert_eq!(entry.visible_tools().len(), 0);
    }

    // ── BackendRegistry ───────────────────────────────────────────────────────

    #[test]
    fn all_tools_namespaces_correctly() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("github", &["create_issue"]));

        let tools = registry.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "github__create_issue");
        assert!(tools[0]["description"]
            .as_str()
            .unwrap_or("")
            .contains("github"));
    }

    #[test]
    fn all_tools_excludes_unhealthy_backends() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("healthy", &["tool_a"]));
        let mut sick = stub_backend("sick", &["tool_b"]);
        sick.unhealthy = true;
        registry.register_backend(sick);

        let tools = registry.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "healthy__tool_a");
    }

    #[test]
    fn all_tools_excluding_skips_named_backends() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("kept", &["tool_a"]));
        registry.register_backend(stub_backend("skipme", &["tool_b", "tool_c"]));

        // Without exclusion: all 3 tools across both backends.
        assert_eq!(registry.all_tools().len(), 3);

        // With "skipme" excluded: only the kept backend's single tool.
        let excluded: std::collections::HashSet<String> =
            std::iter::once("skipme".to_string()).collect();
        let tools = registry.all_tools_excluding(&excluded);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "kept__tool_a");
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_non_namespaced_tool() {
        let registry = BackendRegistry::new();
        let result = registry.dispatch("skillclub_search", &Value::Null).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_unknown_server() {
        let registry = BackendRegistry::new();
        let result = registry.dispatch("unknown__some_tool", &Value::Null).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn dispatch_stub_backend_returns_ok() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("test", &["do_thing"]));
        let result = registry
            .dispatch("test__do_thing", &serde_json::json!({}))
            .await;
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn dispatch_unhealthy_backend_returns_error() {
        let registry = BackendRegistry::new();
        let mut entry = stub_backend("sick", &["tool_a"]);
        entry.unhealthy = true;
        registry.register_backend(entry);

        let result = registry
            .dispatch("sick__tool_a", &serde_json::json!({}))
            .await;
        assert!(result.is_some());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn shutdown_clears_all_backends() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("test", &[]));
        assert_eq!(registry.backend_count(), 1);
        registry.shutdown();
        assert_eq!(registry.backend_count(), 0);
    }

    #[test]
    fn has_backend_returns_false_when_empty() {
        let registry = BackendRegistry::new();
        assert!(!registry.has_backend("test"));
    }

    #[test]
    fn has_backend_returns_true_for_existing() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("playwright", &[]));
        assert!(registry.has_backend("playwright"));
        assert!(!registry.has_backend("github"));
    }

    #[test]
    fn remove_backend_returns_false_when_not_found() {
        let registry = BackendRegistry::new();
        assert!(!registry.remove_backend("nonexistent"));
    }

    #[test]
    fn remove_backend_removes_and_returns_true() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("sentry", &[]));
        assert_eq!(registry.backend_count(), 1);
        assert!(registry.remove_backend("sentry"));
        assert_eq!(registry.backend_count(), 0);
    }

    #[test]
    fn backend_tools_returns_namespaced_names() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("github", &["create_issue", "list_repos"]));
        let tools = registry.backend_tools("github");
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"github__create_issue".to_string()));
        assert!(tools.contains(&"github__list_repos".to_string()));
    }

    #[test]
    fn backend_tools_returns_empty_for_unknown() {
        let registry = BackendRegistry::new();
        assert!(registry.backend_tools("unknown").is_empty());
    }

    // ── Priority-based budget truncation ──────────────────────────────────────

    #[test]
    fn register_backends_with_priority_high_priority_wins_budget() {
        let registry = BackendRegistry::new();

        // Fill most of the budget with a low-priority backend (50 tools = half of 80).
        let mut low = stub_backend("low", &[]);
        low.priority = 10;
        low.tools = (0..50)
            .map(|i| ToolDefinition {
                name: format!("tool_{i}"),
                description: None,
                input_schema: None,
            })
            .collect();

        // High-priority backend with 50 tools too.
        let mut high = stub_backend("high", &[]);
        high.priority = 90;
        high.tools = (0..50)
            .map(|i| ToolDefinition {
                name: format!("tool_{i}"),
                description: None,
                input_schema: None,
            })
            .collect();

        registry.register_backends_with_priority(vec![low, high]);

        // High priority should get all 50 tools; low priority should get only 30
        // (BACKEND_TOOL_BUDGET=80, 80-50=30).
        let high_tools = registry.backend_tools("high");
        let low_tools = registry.backend_tools("low");
        assert_eq!(
            high_tools.len(),
            50,
            "high priority should get full allocation"
        );
        assert_eq!(
            low_tools.len(),
            30,
            "low priority truncated to remaining budget"
        );
        assert!(registry.truncated_backends().contains(&"low".to_string()));
    }

    // ── mark_healthy ──────────────────────────────────────────────────────────

    #[test]
    fn mark_healthy_resets_unhealthy_backend() {
        let registry = BackendRegistry::new();
        let mut entry = stub_backend("test", &["tool_a"]);
        entry.unhealthy = true;
        registry.register_backend(entry);

        registry.mark_healthy("test");

        let inner = registry.inner.lock().unwrap();
        let backend = inner.backends.get("test").unwrap();
        assert!(!backend.unhealthy);
    }
}

// ── Stdio integration tests ───────────────────────────────────────────────────

#[cfg(test)]
#[path = "aggregator_stdio_tests.rs"]
mod stdio_tests;
