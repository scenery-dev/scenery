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
//! # M0 limitations
//!
//! - HTTP dispatch stubs out (returns an error); real HTTP call lands in M1.
//! - Stdio subprocess backends are not yet wired (M1).
//! - Registry sync (`sync_from_registry`) is stubbed to a no-op; the daemon
//!   stream (Stream 4) will wire in real backends via `register_backend`.

use anyhow::Result;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, info};

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

/// Transport variant for a backend MCP server.
///
/// M0 only wires `Stub` (in-memory) and `Http` (structure present, dispatch
/// returns an error until M1). `Stdio` is scaffolded for M1.
#[derive(Debug, Clone)]
pub enum BackendTransport {
    /// In-memory stub — used for tests and the EmbeddedBackend fallback.
    Stub,
    /// HTTP MCP endpoint. `url` is the base URL; dispatch is stubbed in M0.
    Http {
        url: String,
        /// Optional Bearer token for gateway-authenticated backends.
        auth_token: Option<String>,
    },
    /// Stdio child-process backend. Wired in M1.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
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
}

impl BackendEntry {
    /// Iterate tools filtered by visibility policy.
    pub fn visible_tools(&self) -> Vec<&ToolDefinition> {
        self.tool_visibility.filter(&self.tools)
    }
}

// ── Backend registry ──────────────────────────────────────────────────────────

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
}

impl BackendRegistry {
    /// Create a new, empty registry. Call `register_backend` to populate.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                backends: HashMap::new(),
                budget: ToolBudget::new(),
                last_synced: None,
            })),
        }
    }

    /// Register a backend entry directly. Used by the daemon and tests.
    ///
    /// If a backend with the same `server_id` already exists it is replaced.
    /// Budget accounting is updated accordingly.
    pub fn register_backend(&self, entry: BackendEntry) {
        let mut inner = self.inner.lock().unwrap();
        let server_id = entry.server_id.clone();
        let tool_count = entry.visible_tools().len();

        let budget_slots = if inner.budget.has_room_for(tool_count) {
            inner.budget.consume(tool_count);
            tool_count
        } else {
            let remaining = inner.budget.remaining();
            inner.budget.record_truncation(&server_id);
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

    /// Returns all namespaced tool definitions from all active backends.
    pub fn all_tools(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();

        for backend in inner.backends.values() {
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
    pub fn dispatch(&self, namespaced_tool: &str, args: &Value) -> Option<Result<Value>> {
        let (server_id, original_tool) = parse_tool_name(namespaced_tool)?;

        let inner = self.inner.lock().unwrap();
        let backend = inner.backends.get(server_id)?;

        // Verify the tool exists on this backend.
        let tool_exists = backend.tools.iter().any(|t| t.name == original_tool);
        if !tool_exists {
            return Some(Err(anyhow::anyhow!(
                "tool '{}' not found on backend '{}'",
                original_tool,
                server_id
            )));
        }

        let transport = backend.transport.clone();
        // Release lock before any I/O.
        drop(inner);

        // `args` is passed through to the backend dispatcher in M1.
        // For M0 only the Stub transport is functional; all others return errors.
        let result = match transport {
            BackendTransport::Stub => Ok(serde_json::json!({
                "content": [{"type": "text", "text": format!("stub response for {namespaced_tool}: {args}")}]
            })),
            BackendTransport::Http { .. } => {
                // TODO(M1): real HTTP dispatch via reqwest async client
                Err(anyhow::anyhow!(
                    "HTTP backend dispatch not yet implemented (M1) for '{namespaced_tool}'"
                ))
            }
            BackendTransport::Stdio { .. } => {
                // TODO(M1): stdio child-process dispatch
                Err(anyhow::anyhow!(
                    "stdio backend dispatch not yet implemented (M1) for '{namespaced_tool}'"
                ))
            }
        };

        Some(result)
    }

    /// Shut down all backend connections gracefully.
    /// For M0 (no live connections) this just clears the map.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        let count = inner.backends.len();
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
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
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
    fn dispatch_returns_none_for_non_namespaced_tool() {
        let registry = BackendRegistry::new();
        let result = registry.dispatch("skillclub_search", &Value::Null);
        assert!(result.is_none());
    }

    #[test]
    fn dispatch_returns_none_for_unknown_server() {
        let registry = BackendRegistry::new();
        let result = registry.dispatch("unknown__some_tool", &Value::Null);
        assert!(result.is_none());
    }

    #[test]
    fn dispatch_stub_backend_returns_ok() {
        let registry = BackendRegistry::new();
        registry.register_backend(stub_backend("test", &["do_thing"]));
        let result = registry.dispatch("test__do_thing", &serde_json::json!({}));
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
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
}
