//! MCP fleet governance: approved server fetching, caching, and audit buffering.
//!
//! Ported from `skillrunner-core::mcp_governance`. Branding references updated
//! to VectorHawk; no semantic changes.

use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

// ── Wire types ────────────────────────────────────────────────────────────────

/// A single approved MCP server entry as returned by the registry.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    pub status: String,
    pub credential_note: Option<String>,
    pub server_config: Option<serde_json::Value>,

    // ── Aggregator-specific fields ────────────────────────────────────────
    /// Stable identifier for tool namespacing (e.g. `"github"` →
    /// tools become `github__create_issue`). Defaults to `name` when absent.
    #[serde(default)]
    pub server_id: Option<String>,

    /// Transport type: `"stdio"`, `"http"`, or `"gateway"`. Defaults to `"http"`.
    #[serde(default)]
    pub transport_type: Option<String>,

    /// For `gateway` transport: the upstream gateway URL.
    #[serde(default)]
    pub gateway_url: Option<String>,

    /// Tool visibility policy: `"all"`, `"curated"`, or `"on_demand"`.
    #[serde(default)]
    pub tool_visibility: Option<String>,

    /// For `tool_visibility = "curated"`: the list of tool names to surface.
    #[serde(default)]
    pub visible_tools: Option<Vec<String>>,

    /// Admin-assigned priority (higher = more important for tool budget).
    #[serde(default)]
    pub priority: Option<u8>,
}

/// Response from the approved-servers / MCP-servers registry endpoint.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpServersResponse {
    pub approval_mode: String,
    pub servers: Vec<McpServerEntry>,
}

// ── Audit buffer ──────────────────────────────────────────────────────────────

/// A single audit event to be buffered locally and batch-uploaded.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuditEvent {
    pub server_name: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub machine_id: Option<String>,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub org_id: String,
}

fn ensure_audit_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_audit_buffer (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_json TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )?;
    Ok(())
}

/// Buffer an audit event in local SQLite.
pub fn buffer_audit_event(state: &AppState, event: &AuditEvent) -> Result<()> {
    let conn = Connection::open(&state.db_path)?;
    ensure_audit_table(&conn)?;

    let json = serde_json::to_string(event)?;
    let now = unix_now();

    conn.execute(
        "INSERT INTO mcp_audit_buffer (event_json, created_at) VALUES (?1, ?2)",
        params![json, now as i64],
    )?;

    debug!(event_type = %event.event_type, "buffered audit event");
    Ok(())
}

// ── SQLite cache for approved-servers list ────────────────────────────────────

fn ensure_cache_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_config_cache (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            config_json TEXT NOT NULL,
            fetched_at INTEGER NOT NULL
        )",
    )?;
    Ok(())
}

fn cache_mcp_config(state: &AppState, response: &McpServersResponse) -> Result<()> {
    let conn = Connection::open(&state.db_path)?;
    ensure_cache_table(&conn)?;

    let json = serde_json::to_string(response)?;
    let now = unix_now();

    conn.execute(
        "INSERT INTO mcp_config_cache (id, config_json, fetched_at)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
             config_json = excluded.config_json,
             fetched_at = excluded.fetched_at",
        params![json, now as i64],
    )?;

    Ok(())
}

fn load_cached_mcp_config(state: &AppState) -> Result<Option<McpServersResponse>> {
    let conn = Connection::open(&state.db_path)?;
    ensure_cache_table(&conn)?;

    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT config_json, fetched_at FROM mcp_config_cache WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match row {
        Some((json, fetched_at)) => {
            const GRACE_SECONDS: u64 = 7 * 86400;
            let now = unix_now();
            if now > fetched_at as u64 + GRACE_SECONDS {
                warn!("cached MCP config is older than 7 days, ignoring");
                return Ok(None);
            }
            let resp: McpServersResponse =
                serde_json::from_str(&json).context("failed to deserialize cached MCP config")?;
            Ok(Some(resp))
        }
        None => Ok(None),
    }
}

/// Fetch the approved MCP server list, with SQLite cache fallback.
///
/// On network success: updates the SQLite cache and returns the fresh list.
/// On network failure: returns the cached list if within the 7-day offline
/// grace window, otherwise returns an error.
pub fn fetch_approved_servers_cached(
    state: &AppState,
    servers_response: McpServersResponse,
) -> Result<McpServersResponse> {
    cache_mcp_config(state, &servers_response)?;
    Ok(servers_response)
}

/// Load the cached MCP server list for offline use.
pub fn load_cached_servers(state: &AppState) -> Result<Option<McpServersResponse>> {
    load_cached_mcp_config(state)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
}

// ── Registry method stubs (used by tools.rs) ──────────────────────────────────
//
// These are the registry API calls that tools.rs needs. The real HTTP
// implementations land in M1.4 when `HttpRegistryClient` is fully wired.
// For now, the methods are declared here as free functions that accept a
// base_url string and make blocking HTTP calls directly (matching the
// skillrunner pattern where RegistryClient was a concrete struct).
//
// M1.4 note: once `HttpRegistryClient` gains these methods on the trait,
// this module can delegate to the trait rather than duplicating HTTP code.

use reqwest::blocking::Client;

fn make_http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

/// Fetch the org's MCP server catalog from the registry.
pub fn fetch_mcp_catalog(base_url: &str) -> Result<McpServersResponse> {
    let url = format!("{}/api/runner/mcp-servers", base_url.trim_end_matches('/'));
    debug!(url, "fetching MCP server catalog");

    let client = make_http_client()?;
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        let preview = if body.len() > 200 {
            format!("{}...", &body[..200])
        } else {
            body
        };
        anyhow::bail!("registry returned HTTP {status} for MCP catalog: {preview}");
    }

    resp.json()
        .context("failed to deserialize MCP catalog response")
}

/// Submit an MCP server access request.
pub fn submit_mcp_request(
    base_url: &str,
    server_name: &str,
    package_source: Option<&str>,
    auth_token: &str,
) -> Result<serde_json::Value> {
    let url = format!("{}/portal/mcp/requests", base_url.trim_end_matches('/'));

    let mut body = serde_json::json!({ "server_name": server_name });
    if let Some(src) = package_source {
        body["package_source"] = serde_json::json!(src);
    }

    let client = make_http_client()?;
    let resp = client
        .post(&url)
        .bearer_auth(auth_token)
        .json(&body)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("MCP request submission failed (HTTP {status}): {body}");
    }

    resp.json()
        .context("failed to deserialize MCP request response")
}

/// List the current user's MCP server access requests.
pub fn list_mcp_requests(base_url: &str, auth_token: &str) -> Result<serde_json::Value> {
    let url = format!("{}/portal/mcp/requests", base_url.trim_end_matches('/'));

    let client = make_http_client()?;
    let resp = client
        .get(&url)
        .bearer_auth(auth_token)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("failed to fetch MCP requests (HTTP {status}): {body}");
    }

    resp.json()
        .context("failed to deserialize MCP requests response")
}

/// Preview an external import (skill or MCP server).
pub fn import_preview(base_url: &str, input: &str, auth_token: &str) -> Result<serde_json::Value> {
    let url = format!("{}/portal/import/preview", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "url": input });

    let client = make_http_client()?;
    let resp = client
        .post(&url)
        .bearer_auth(auth_token)
        .json(&body)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("import preview failed (HTTP {status}): {body}");
    }

    resp.json()
        .context("failed to deserialize import preview response")
}

/// Submit an external import for approval.
pub fn import_submit(base_url: &str, input: &str, auth_token: &str) -> Result<serde_json::Value> {
    let url = format!("{}/portal/import/submit", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "url": input });

    let client = make_http_client()?;
    let resp = client
        .post(&url)
        .bearer_auth(auth_token)
        .json(&body)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("import submit failed (HTTP {status}): {body}");
    }

    resp.json()
        .context("failed to deserialize import submit response")
}

/// Search the skill registry.
pub fn search_skills(base_url: &str, query: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "{}/api/skills/search?q={}",
        base_url.trim_end_matches('/'),
        urlencoding::encode(query)
    );

    let client = make_http_client()?;
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("skill search failed (HTTP {status}): {body}");
    }

    let results: serde_json::Value = resp
        .json()
        .context("failed to deserialize search results")?;

    Ok(results.as_array().cloned().unwrap_or_default())
}

/// Install a skill from the registry by ID and optional version.
/// Returns the installed version string.
pub fn install_from_registry(
    state: &AppState,
    base_url: &str,
    skill_id: &str,
    version: Option<&str>,
) -> Result<String> {
    use crate::auth;
    use crate::registry::RegistryClient;
    use crate::updater;

    let mut registry = RegistryClient::new(base_url);
    if let Ok(Some(tokens)) = auth::load_tokens(state, base_url) {
        registry.set_auth(&tokens.access_token);
    }
    updater::install_from_registry(state, &registry, skill_id, version)
}

// ── Logging helpers ───────────────────────────────────────────────────────────

#[allow(dead_code)]
fn log_cache_hit() {
    info!("using cached MCP server list (offline mode)");
}
