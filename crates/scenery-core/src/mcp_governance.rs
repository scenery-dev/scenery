//! MCP fleet governance: approved server fetching, caching, and audit buffering.

use crate::http::SendTrackedExt;
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
// base_url string and make blocking HTTP calls directly.
//
// M1.4 note: once `HttpRegistryClient` gains these methods on the trait,
// this module can delegate to the trait rather than duplicating HTTP code.

use reqwest::blocking::Client;

fn make_http_client() -> Result<Client> {
    crate::http::blocking_client_builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

/// Shown when the skills catalog is used without (valid) credentials.
pub const SKILLS_SIGN_IN_MESSAGE: &str =
    "Sign in to use the skills catalog (`scenery auth login`).";

/// Fetch the org's MCP server catalog from the registry.
///
/// Sends `auth_token` as a Bearer token when present; anonymous otherwise.
pub fn fetch_mcp_catalog(base_url: &str, auth_token: Option<&str>) -> Result<McpServersResponse> {
    let url = format!("{}/api/runner/mcp-servers", base_url.trim_end_matches('/'));
    debug!(url, "fetching MCP server catalog");

    let client = make_http_client()?;
    let mut req = client.get(&url);
    if let Some(token) = auth_token {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send_tracked()
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
        .send_tracked()
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
        .send_tracked()
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
        .send_tracked()
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
///
/// `duplicate_resolution` (and `fork_name` when forking) tell the backend how
/// to handle a detected duplicate: `use_existing` | `new_version` | `fork` |
/// `force`. When omitted and a high/exact duplicate is found, the backend
/// returns HTTP 409 with the similarity report; the caller can then re-submit
/// with a chosen resolution.
pub fn import_submit(
    base_url: &str,
    input: &str,
    auth_token: &str,
    duplicate_resolution: Option<&str>,
    fork_name: Option<&str>,
) -> Result<serde_json::Value> {
    let url = format!("{}/portal/import/submit", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({ "url": input });
    if let Some(res) = duplicate_resolution {
        body["duplicate_resolution"] = serde_json::Value::String(res.to_string());
    }
    if let Some(name) = fork_name {
        body["fork_name"] = serde_json::Value::String(name.to_string());
    }

    let client = make_http_client()?;
    let resp = client
        .post(&url)
        .bearer_auth(auth_token)
        .json(&body)
        .send_tracked()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        // Duplicate detected and no resolution supplied — surface the report
        // so the caller can prompt the user for a choice.
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("DUPLICATE_CONFLICT: {body}");
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("import submit failed (HTTP {status}): {body}");
    }

    resp.json()
        .context("failed to deserialize import submit response")
}

/// The skills service needs a (valid) sign-in. Callers downcast to this to
/// decide on a token refresh; its text is [`SKILLS_SIGN_IN_MESSAGE`].
#[derive(Debug)]
pub struct SignInRequired;

impl std::fmt::Display for SignInRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(SKILLS_SIGN_IN_MESSAGE)
    }
}

impl std::error::Error for SignInRequired {}

/// Call one of the backend's remote skills MCP tools (`search_skills`,
/// `load_skill`, `install_skill`) over its stateless `POST {base_url}/mcp`
/// JSON-RPC endpoint and return the MCP tool result object
/// (`{"content": [...], "isError"?: bool}`) verbatim.
///
/// The portal bearer is required; `device_id` is sent as `X-Device-ID` so the
/// backend can mark skills installed on this device and target installs.
/// A missing token or a 401 is reported as [`SKILLS_SIGN_IN_MESSAGE`].
pub fn call_remote_skills_tool(
    base_url: &str,
    auth_token: Option<&str>,
    device_id: Option<&str>,
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value> {
    let Some(token) = auth_token else {
        return Err(anyhow::Error::new(SignInRequired));
    };
    let url = format!("{}/mcp", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });

    let client = make_http_client()?;
    let mut req = client.post(&url).bearer_auth(token).json(&body);
    if let Some(id) = device_id.filter(|d| !d.is_empty()) {
        req = req.header("X-Device-ID", id);
    }
    let resp = req
        .send_tracked()
        .with_context(|| format!("failed to reach registry at {url}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(anyhow::Error::new(SignInRequired));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN && crate::auth::is_user_revoked_body(&text) {
            return Err(anyhow::Error::new(crate::auth::UserRevokedError));
        }
        anyhow::bail!("skills service failed (HTTP {status}): {text}");
    }

    let reply: serde_json::Value = resp
        .json()
        .context("failed to deserialize skills service response")?;
    if let Some(err) = reply.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("skills service error: {msg}");
    }
    reply
        .get("result")
        .cloned()
        .context("skills service response had no result")
}

// ── Logging helpers ───────────────────────────────────────────────────────────

#[allow(dead_code)]
fn log_cache_hit() {
    info!("using cached MCP server list (offline mode)");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::auth;
    use mockito::Server;

    #[test]
    fn fetch_mcp_catalog_sends_bearer_when_token_given() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .match_header("authorization", "Bearer tok-123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"approval_mode":"allowlist","servers":[]}"#)
            .create();
        let resp = fetch_mcp_catalog(&server.url(), Some("tok-123")).unwrap();
        assert_eq!(resp.approval_mode, "allowlist");
        mock.assert();
    }

    #[test]
    fn fetch_mcp_catalog_stays_anonymous_without_token() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .match_header("authorization", mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"approval_mode":"allowlist","servers":[]}"#)
            .create();
        fetch_mcp_catalog(&server.url(), None).unwrap();
        mock.assert();
    }

    #[test]
    fn call_remote_skills_tool_sends_bearer_and_device_header() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/mcp")
            .match_header("authorization", "Bearer tok-123")
            .match_header("x-device-id", "dev-123")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "search_skills", "arguments": {"query": "pdf"}}
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"1 skill(s)"}]}}"#,
            )
            .create();
        let out = call_remote_skills_tool(
            &server.url(),
            Some("tok-123"),
            Some("dev-123"),
            "search_skills",
            &serde_json::json!({"query": "pdf"}),
        )
        .unwrap();
        assert_eq!(out["content"][0]["text"], "1 skill(s)");
        mock.assert();
    }

    #[test]
    fn call_remote_skills_tool_without_token_asks_to_sign_in() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", mockito::Matcher::Any)
            .expect(0)
            .create();
        let err = call_remote_skills_tool(
            &server.url(),
            None,
            None,
            "search_skills",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("scenery auth login"));
        mock.assert();
    }

    #[test]
    fn call_remote_skills_tool_maps_401_to_sign_in_message() {
        let mut server = Server::new();
        let _mock = server.mock("POST", "/mcp").with_status(401).create();
        let err = call_remote_skills_tool(
            &server.url(),
            Some("stale"),
            None,
            "load_skill",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("scenery auth login"));
    }

    #[test]
    fn call_remote_skills_tool_maps_403_user_revoked_to_typed_error() {
        let mut server = Server::new();
        let _mock = server
            .mock("POST", "/mcp")
            .with_status(403)
            .with_body(r#"{"detail":{"code":"user_revoked","message":"gone"}}"#)
            .create();
        let err = call_remote_skills_tool(
            &server.url(),
            Some("t"),
            None,
            "search_skills",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(auth::is_user_revoked_error(&err), "{err:#}");
    }

    #[test]
    fn call_remote_skills_tool_surfaces_jsonrpc_errors() {
        let mut server = Server::new();
        let _mock = server
            .mock("POST", "/mcp")
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"skill_id is required"}}"#)
            .create();
        let err = call_remote_skills_tool(
            &server.url(),
            Some("t"),
            None,
            "load_skill",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("skill_id is required"));
    }
}
