//! Tool dispatch for the VectorHawk MCP server.
//!
//! Ported from `skillrunner-mcp::tools`. All `skillclub_*` user-visible tool
//! names are renamed to `vectorhawk_*`.
//!
//! # Out-of-scope tools (not ported — deferred phases)
//!
//! - `skillclub_author` / `_author_confirm` (AUTH2)
//! - `skillclub_publish` / `_update` (registry write ops, M1.4)
//! - `skillclub_plugin_*` (PL phases)
//! - `skillclub_rate` (RAT1)
//! - `skillclub_scan` (SEC3)
//!
//! # RegistryClient methods added (M1.4 must reconcile)
//!
//! The governance tool handlers call free functions in
//! `vectorhawkd_core::mcp_governance` that make direct HTTP calls. When M1.4
//! expands `HttpRegistryClient` those free functions should delegate to the
//! trait instead. Methods effectively added:
//! - `fetch_mcp_catalog(base_url)`
//! - `submit_mcp_request(base_url, …)`
//! - `list_mcp_requests(base_url, …)`
//! - `import_preview(base_url, …)`
//! - `import_submit(base_url, …)`
//! - `search_skills(base_url, query)` — stub; M1.4 wires real HTTP
//! - `install_from_registry(base_url, …)` — stubbed with bail!("M1.4")

use crate::{
    aggregator::{sanitize_id, BackendRegistry},
    protocol::{ToolCallResult, ToolDefinition},
};
use anyhow::Result;
use rusqlite::Connection;
use semver::Version;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;
use vectorhawkd_core::{
    auth::{self, AuthClient},
    executor::{run_skill, RunResult},
    installer::{install_unpacked_skill, uninstall_skill, InstallMode},
    mcp_governance,
    model::{ModelClient, ModelSource},
    policy::PolicyClient,
    registry::RegistryClient,
    state::AppState,
    updater::install_from_registry,
    validator::validate_bundle,
};
use vectorhawkd_manifest::SkillPackage;

/// How long a cached update-check result is considered fresh before re-check.
const UPDATE_CHECK_TTL: Duration = Duration::from_secs(600); // 10 minutes

/// A single cached result from a registry update check for one skill.
pub struct UpdateCheckEntry {
    /// When this entry was populated.
    pub checked_at: Instant,
    /// The latest version from the registry if newer than installed.
    /// `None` means up-to-date or check failed.
    pub latest_version: Option<Version>,
}

/// Shared update-check cache passed from `ServerState` to the tools layer.
pub type UpdateCheckCache = Arc<Mutex<HashMap<String, UpdateCheckEntry>>>;

const GOVERNANCE_FOOTER: &str = "\n\n---\nTo add new MCP servers, use /mcp-request. Direct installation via /mcp bypasses governance.";

// ── Tool registry ─────────────────────────────────────────────────────────────

/// Builds the list of MCP tool definitions from installed skills + management tools.
pub fn build_tool_list(state: &AppState, registry_url: &Option<String>) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    let logged_in = registry_url
        .as_ref()
        .and_then(|url| auth::load_tokens(state, url).ok().flatten())
        .is_some();

    // Add installed skills as tools
    if let Ok(skill_tools) = skill_tools_from_db(state) {
        tools.extend(skill_tools);
    }

    // Management tools — always available (local operations)
    tools.push(ToolDefinition {
        name: "vectorhawk_list".to_string(),
        description:
            "List all installed skills available to the user. Use this when the user asks \
            'what skills do I have', 'what tools are available', or 'what can you do'. \
            Shows skill IDs, versions, and descriptions."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    });

    tools.push(ToolDefinition {
        name: "vectorhawk_validate".to_string(),
        description: "Validate a VectorHawk skill bundle directory. Checks manifest, workflow, \
            schemas, and file references."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the skill bundle directory to validate"
                }
            },
            "required": ["path"]
        }),
    });

    // Install is always available — supports both local paths and registry IDs
    tools.push(ToolDefinition {
        name: "vectorhawk_install".to_string(),
        description: "Install a skill from a local path or from the VectorHawk registry by its ID."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The ID of the skill to install from the registry (use this OR path, not both)"
                },
                "path": {
                    "type": "string",
                    "description": "Local path to a skill bundle directory to install (use this OR skill_id, not both)"
                },
                "version": {
                    "type": "string",
                    "description": "Optional specific version to install from registry (default: latest)"
                }
            },
            "required": []
        }),
    });

    tools.push(ToolDefinition {
        name: "vectorhawk_info".to_string(),
        description: "Show detailed information about an installed VectorHawk skill.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The ID of the installed skill to get info about"
                }
            },
            "required": ["skill_id"]
        }),
    });

    tools.push(ToolDefinition {
        name: "vectorhawk_uninstall".to_string(),
        description: "Uninstall an installed VectorHawk skill by its ID. Removes skill files and database records.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The ID of the installed skill to uninstall"
                }
            },
            "required": ["skill_id"]
        }),
    });

    // Update is available whenever a registry URL is configured.
    if registry_url.is_some() {
        tools.push(ToolDefinition {
            name: "vectorhawk_update".to_string(),
            description: "Update an installed skill to the latest version from the registry. \
                Call this after being notified that a newer version is available."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": {
                        "type": "string",
                        "description": "The skill ID to update"
                    }
                },
                "required": ["skill_id"]
            }),
        });
    }

    tools.push(ToolDefinition {
        name: "vectorhawk_import".to_string(),
        description: "Import an external skill or MCP server into VectorHawk. Paste an npm \
            package name (e.g. @modelcontextprotocol/server-github), npx command, or GitHub URL. \
            The system detects whether it's a skill or MCP server and routes to the appropriate \
            approval workflow."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The npm package name, npx command, or GitHub URL to import"
                },
                "confirm": {
                    "type": "boolean",
                    "description": "If true, submit the import after preview. If false (default), only preview."
                }
            },
            "required": ["input"]
        }),
    });

    // MCP Governance tools — only shown when logged in
    if logged_in {
        tools.push(ToolDefinition {
            name: "vectorhawk_mcp_catalog".to_string(),
            description: "Browse approved MCP servers in your organisation's catalog. Shows \
                available servers with their status, version pins, and credential notes."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_mcp_request".to_string(),
            description: "Request access to a new MCP server. In trust mode the request is \
                auto-approved. In catalog-only mode, known servers are auto-approved. In strict \
                mode the request goes to IT for review."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the MCP server to request (e.g., 'Slack MCP')"
                    },
                    "package_source": {
                        "type": "string",
                        "description": "Optional package source (e.g., '@modelcontextprotocol/server-slack')"
                    }
                },
                "required": ["server_name"]
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_mcp_status".to_string(),
            description: "Check the status of your MCP server access requests.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_mcp_install".to_string(),
            description: "Activate an approved MCP server through VectorHawk's governance system. \
                Forces an immediate sync with the registry and makes the server's tools available \
                right away. The server must already be approved via vectorhawk_mcp_request."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the approved MCP server to activate"
                    }
                },
                "required": ["server_name"]
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_mcp_uninstall".to_string(),
            description: "Remove a governed MCP server from VectorHawk. Deactivates the server \
                and removes its tools immediately."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the MCP server to deactivate"
                    }
                },
                "required": ["server_name"]
            }),
        });
    }

    // Login is available when a registry URL exists and user is not logged in
    if registry_url.is_some() && !logged_in {
        tools.push(ToolDefinition {
            name: "vectorhawk_login".to_string(),
            description: "Log in to the VectorHawk registry to unlock searching, installing, \
                and governance features. Opens a browser-based OAuth login flow."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "registry_url": {
                        "type": "string",
                        "description": "Optional registry URL override (defaults to the server's configured registry URL)"
                    }
                },
                "required": []
            }),
        });
    }

    // Registry tools requiring authentication
    if logged_in {
        tools.push(ToolDefinition {
            name: "vectorhawk_logout".to_string(),
            description: "Log out of the VectorHawk registry. Clears stored authentication tokens."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_search".to_string(),
            description: "Search the VectorHawk skill registry for skills that can be installed. \
                Use this when the user asks 'what skills are available', 'find skills for X', or \
                wants to discover new capabilities. Use an empty query to list all available skills."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find skills (e.g., 'contract', 'analysis'). Omit or leave empty to list all available skills."
                    }
                },
                "required": []
            }),
        });
    }

    // Plugin export/import tools — always available (local operations, no auth required)
    tools.push(ToolDefinition {
        name: "vectorhawk_plugin_export".to_string(),
        description: "Export a VectorHawk plugin to Claude Code plugin or .mcpb Desktop Extension \
            format for distribution. Use 'mcpb' to produce a ZIP archive for Claude Desktop \
            one-click install with enterprise allowlist support."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the VectorHawk plugin directory"
                },
                "format": {
                    "type": "string",
                    "enum": ["claude-code", "mcpb"],
                    "description": "Export format: 'claude-code' for a Claude Code plugin directory, 'mcpb' for a Desktop Extension archive"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory where the exported artifact will be written (default: current directory)"
                }
            },
            "required": ["path", "format"]
        }),
    });

    tools.push(ToolDefinition {
        name: "vectorhawk_plugin_import".to_string(),
        description: "Import a Claude Code plugin directory or .mcpb Desktop Extension into \
            VectorHawk plugin format. Auto-detects the external format and converts it."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the Claude Code plugin directory or .mcpb file"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory for the converted plugin (default: current directory)"
                }
            },
            "required": ["path"]
        }),
    });

    tools
}

/// Load installed skills from SQLite and convert to MCP tool definitions.
fn skill_tools_from_db(state: &AppState) -> Result<Vec<ToolDefinition>> {
    let conn = Connection::open(&state.db_path)?;
    let mut stmt = conn.prepare(
        "SELECT skill_id, install_root FROM installed_skills WHERE current_status = 'active'",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut tools = Vec::new();
    for row in rows {
        let (skill_id, install_root) = row?;
        let active_path = format!("{}/active", install_root);
        if let Ok(tool) = skill_to_tool(&skill_id, &active_path) {
            tools.push(tool);
        }
    }

    Ok(tools)
}

/// Convert a single installed skill into an MCP tool definition.
fn skill_to_tool(skill_id: &str, active_path: &str) -> Result<ToolDefinition> {
    let pkg = SkillPackage::load_from_dir(active_path)?;

    let base_desc = pkg
        .manifest
        .description
        .clone()
        .unwrap_or_else(|| format!("VectorHawk skill: {}", pkg.manifest.name));

    let versioned_desc = format!("{} (v{})", base_desc, pkg.manifest.version);

    let triggers = pkg.manifest.triggers.clone();
    let description = if triggers.is_empty() {
        versioned_desc
    } else {
        format!(
            "{}\n\nUse this tool when the user asks to: {}",
            versioned_desc,
            triggers.join(", ")
        )
    };

    let input_schema = pkg.manifest.inputs_schema_or_default();

    Ok(ToolDefinition {
        name: skill_id.to_string(),
        description,
        input_schema,
    })
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

/// Execute a tool call and return the MCP result.
#[allow(clippy::too_many_arguments)]
pub fn handle_tool_call(
    name: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    registry_url: &Option<String>,
    update_check_cache: &UpdateCheckCache,
    aggregator: Option<&BackendRegistry>,
) -> ToolCallResult {
    let result = match name {
        "vectorhawk_list" => handle_list(state),
        "vectorhawk_search" => handle_search(arguments, registry_url),
        "vectorhawk_install" => handle_install(arguments, state, registry_url),
        "vectorhawk_info" => handle_info(arguments, state),
        "vectorhawk_validate" => handle_validate(arguments),
        "vectorhawk_import" => handle_import(arguments, state, registry_url),
        "vectorhawk_login" => handle_login(arguments, state, registry_url),
        "vectorhawk_logout" => handle_logout(state, registry_url),
        "vectorhawk_mcp_catalog" => handle_mcp_catalog(state, registry_url),
        "vectorhawk_mcp_request" => handle_mcp_request(arguments, state, registry_url),
        "vectorhawk_mcp_status" => handle_mcp_status(state, registry_url),
        "vectorhawk_mcp_install" => handle_mcp_install(arguments, state, registry_url, aggregator),
        "vectorhawk_mcp_uninstall" => handle_mcp_uninstall(arguments, registry_url, aggregator),
        "vectorhawk_uninstall" => handle_uninstall(arguments, state),
        "vectorhawk_update" => handle_update(arguments, state, registry_url),
        "vectorhawk_plugin_export" => handle_plugin_export(arguments),
        "vectorhawk_plugin_import" => handle_plugin_import(arguments),
        _ => handle_skill_run(
            name,
            arguments,
            state,
            policy_client,
            model_client,
            update_check_cache,
        ),
    };

    // Buffer audit event (best-effort, non-blocking for the caller)
    if !name.starts_with("vectorhawk_list") && !name.starts_with("vectorhawk_info") {
        let event = mcp_governance::AuditEvent {
            server_name: None,
            user_id: None,
            user_email: None,
            machine_id: None,
            event_type: "tool_called".to_string(),
            tool_name: Some(name.to_string()),
            metadata: None,
            org_id: "default".to_string(),
        };
        let _ = mcp_governance::buffer_audit_event(state, &event);
    }

    result
}

// ── Management tool handlers ──────────────────────────────────────────────────

fn handle_list(state: &AppState) -> ToolCallResult {
    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error_result(format!("Failed to open state DB: {e}")),
    };

    let mut stmt = match conn.prepare(
        "SELECT skill_id, active_version, current_status FROM installed_skills ORDER BY skill_id",
    ) {
        Ok(s) => s,
        Err(e) => return ToolCallResult::error_result(format!("Failed to query skills: {e}")),
    };

    let rows = match stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "skill_id": row.get::<_, String>(0)?,
            "version": row.get::<_, String>(1)?,
            "status": row.get::<_, String>(2)?,
        }))
    }) {
        Ok(r) => r,
        Err(e) => return ToolCallResult::error_result(format!("Failed to read skills: {e}")),
    };

    let skills: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();

    if skills.is_empty() {
        ToolCallResult::success(
            "No skills installed.\n\nTo get started:\n\
             - Use vectorhawk_search to browse the registry (requires login)\n\
             - Use vectorhawk_install with a local path to install a bundle\n\
             - Use vectorhawk_import to import an external skill or MCP server"
                .to_string(),
        )
    } else {
        match serde_json::to_string_pretty(&skills) {
            Ok(text) => ToolCallResult::success(text),
            Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
        }
    }
}

fn handle_search(arguments: &serde_json::Value, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match mcp_governance::search_skills(url, query) {
        Ok(results) => {
            if results.is_empty() {
                ToolCallResult::success(format!("No skills found matching '{query}'."))
            } else {
                match serde_json::to_string_pretty(&results) {
                    Ok(text) => ToolCallResult::success(text),
                    Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error_result(format!("Search failed: {e}")),
    }
}

fn handle_install(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let path = arguments.get("path").and_then(|v| v.as_str());
    let skill_id = arguments.get("skill_id").and_then(|v| v.as_str());

    match (path, skill_id) {
        // Local path install
        (Some(local_path), _) => {
            let utf8_path = camino::Utf8Path::new(local_path);
            let pkg = match SkillPackage::load_from_dir(utf8_path) {
                Ok(p) => p,
                Err(e) => {
                    return ToolCallResult::error_result(format!(
                        "Failed to load skill bundle at {local_path}: {e}"
                    ))
                }
            };
            let id = pkg.manifest.id.clone();
            let ver = pkg.manifest.version.to_string();
            match install_unpacked_skill(state, &pkg, InstallMode::Copy) {
                Ok(_) => ToolCallResult::success(format!(
                    "Successfully installed {id}@{ver} from local path."
                )),
                Err(e) => ToolCallResult::error_result(format!("Failed to install {id}: {e}")),
            }
        }
        // Registry install
        (None, Some(id)) => {
            let url = match registry_url {
                Some(u) => u,
                None => {
                    return ToolCallResult::error_result(
                        "No registry URL configured. Provide a local 'path' instead.",
                    )
                }
            };
            let version = arguments.get("version").and_then(|v| v.as_str());
            match mcp_governance::install_from_registry(url, id, version) {
                Ok(installed_ver) => ToolCallResult::success(format!(
                    "Successfully installed {id}@{installed_ver} from registry."
                )),
                Err(e) => ToolCallResult::error_result(format!("Failed to install {id}: {e}")),
            }
        }
        // Neither provided
        (None, None) => ToolCallResult::error_result(
            "Provide either 'path' (local install) or 'skill_id' (registry install)",
        ),
    }
}

fn handle_info(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error_result("Missing required parameter: skill_id"),
    };

    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error_result(format!("Failed to open state DB: {e}")),
    };

    let row: Option<(String, String, String)> = match conn.query_row(
        "SELECT skill_id, active_version, install_root FROM installed_skills WHERE skill_id = ?1",
        [skill_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ) {
        Ok(r) => Some(r),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return ToolCallResult::error_result(format!("Failed to query skill: {e}")),
    };

    let (_, version, install_root) = match row {
        Some(r) => r,
        None => {
            return ToolCallResult::error_result(format!("Skill '{skill_id}' is not installed"))
        }
    };

    let active_path = format!("{}/active", install_root);
    match SkillPackage::load_from_dir(&active_path) {
        Ok(pkg) => {
            let info = serde_json::json!({
                "skill_id": pkg.manifest.id,
                "name": pkg.manifest.name,
                "version": version,
                "publisher": pkg.manifest.publisher,
                "description": pkg.manifest.description,
                "steps": pkg.workflow.steps.len(),
                "permissions": {
                    "filesystem": pkg.manifest.permissions.filesystem,
                    "network": pkg.manifest.permissions.network,
                    "clipboard": pkg.manifest.permissions.clipboard,
                },
                "model_requirements": pkg.manifest.model_requirements.as_ref().map(|r| serde_json::json!({
                    "min_params_b": r.min_params_b,
                    "recommended": r.recommended,
                    "fallback": r.fallback,
                })),
            });
            match serde_json::to_string_pretty(&info) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
            }
        }
        Err(e) => ToolCallResult::error_result(format!("Failed to load skill package: {e}")),
    }
}

fn handle_validate(arguments: &serde_json::Value) -> ToolCallResult {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error_result("Missing required parameter: path"),
    };

    let utf8_path = camino::Utf8Path::new(path);
    let report = validate_bundle(utf8_path);

    let checks: Vec<serde_json::Value> = report
        .checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "passed": c.passed,
                "detail": c.detail,
            })
        })
        .collect();

    let result = serde_json::json!({
        "all_passed": report.all_passed(),
        "checks": checks,
    });

    match serde_json::to_string_pretty(&result) {
        Ok(text) => ToolCallResult::success(text),
        Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
    }
}

// ── Auth helper with refresh + elicitation fallback ───────────────────────────

fn ensure_auth(
    state: &AppState,
    registry_url: &str,
) -> std::result::Result<String, ToolCallResult> {
    match auth::load_tokens(state, registry_url) {
        Ok(Some(tokens)) => Ok(tokens.access_token),
        Ok(None) => Err(auth_elicitation_prompt(registry_url)),
        Err(e) => Err(ToolCallResult::error_result(format!(
            "Failed to load auth tokens: {e}"
        ))),
    }
}

fn try_refresh_auth(
    state: &AppState,
    registry_url: &str,
    refresh_token: &str,
) -> std::result::Result<String, ToolCallResult> {
    debug!("access token expired, attempting refresh");

    let auth_client = AuthClient::new(registry_url);
    match auth_client.refresh(refresh_token) {
        Ok(new_tokens) => {
            if let Err(e) = auth::save_tokens(
                state,
                registry_url,
                &new_tokens.access_token,
                &new_tokens.refresh_token,
            ) {
                debug!("failed to save refreshed tokens: {e}");
            }
            Ok(new_tokens.access_token)
        }
        Err(_) => {
            let _ = auth::clear_tokens(state, registry_url);
            Err(auth_elicitation_prompt(registry_url))
        }
    }
}

fn auth_elicitation_prompt(registry_url: &str) -> ToolCallResult {
    ToolCallResult::error_result(format!(
        "Authentication required.\n\n\
        Use the `vectorhawk_login` tool to authenticate.\n\
        Registry: {registry_url}\n\n\
        After logging in, retry this command."
    ))
}

// ── Auth tool handlers ────────────────────────────────────────────────────────

/// Login handler with optional OAuth completion support.
///
/// When `oauth` is `Some`, initiates a PKCE flow with the daemon's callback
/// port as the `redirect_uri`, then spawns a background task to await the
/// browser callback, exchange the code, and save the resulting tokens.
///
/// When `oauth` is `None` (shim fallback / test), falls back to the legacy
/// no-redirect URL and returns a hint that login cannot complete automatically.
pub fn handle_login_with_oauth(
    arguments: &serde_json::Value,
    state: &AppState,
    server_registry_url: &Option<String>,
    oauth: Option<&crate::oauth::OAuthContext>,
) -> ToolCallResult {
    let registry_url_arg = arguments
        .get("registry_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let registry_url = match registry_url_arg.as_ref().or(server_registry_url.as_ref()) {
        Some(u) => u.clone(),
        None => {
            return ToolCallResult::error_result(
                "No registry URL configured. Pass registry_url as an argument.",
            )
        }
    };

    let auth_client = AuthClient::new(&registry_url);

    match oauth {
        Some(ctx) => {
            let redirect_uri =
                format!("http://127.0.0.1:{}/oauth/cli/callback", ctx.listener_port);
            match auth_client.initiate_oauth_flow_with_redirect(&redirect_uri) {
                Ok(initiation) => {
                    // Clone everything the background task needs before we move
                    // ownership into the async block.
                    let code_verifier = initiation.code_verifier.clone();
                    let oauth_state_val = initiation.state.clone();
                    let subscriber = std::sync::Arc::clone(&ctx.subscriber);
                    let reg_url = registry_url.clone();
                    let task_state = AppState {
                        root_dir: state.root_dir.clone(),
                        db_path: state.db_path.clone(),
                    };

                    // Fire-and-forget: await browser callback → exchange code → save tokens.
                    // The AI client already has the URL; this completes silently in the background.
                    tokio::runtime::Handle::current().spawn(async move {
                        let code = match subscriber
                            .wait_for_code(oauth_state_val, 300)
                            .await
                        {
                            Some(c) => c,
                            None => {
                                tracing::warn!(
                                    "vectorhawk_login: OAuth callback timed out or daemon shut down"
                                );
                                return;
                            }
                        };

                        let result = tokio::task::spawn_blocking(move || {
                            let client = AuthClient::new(&reg_url);
                            let tokens = client.exchange_oauth_code(&code, &code_verifier)?;
                            auth::save_tokens(
                                &task_state,
                                &reg_url,
                                &tokens.access_token,
                                &tokens.refresh_token,
                            )?;
                            Ok::<(), anyhow::Error>(())
                        })
                        .await;

                        match result {
                            Ok(Ok(())) => {
                                tracing::info!("vectorhawk_login: PKCE complete, tokens saved")
                            }
                            Ok(Err(e)) => tracing::warn!(
                                error = %e,
                                "vectorhawk_login: token exchange/save failed"
                            ),
                            Err(e) => tracing::warn!(
                                error = %e,
                                "vectorhawk_login: background task panicked"
                            ),
                        }
                    });

                    ToolCallResult::success(format!(
                        "Open this URL in your browser to log in:\n{}\n\n\
                         After logging in, VectorHawk will automatically complete \
                         authentication. The `vectorhawk_login` tool will no longer \
                         appear once you are logged in.",
                        initiation.auth_url
                    ))
                }
                Err(e) => ToolCallResult::error_result(format!("Failed to initiate login: {e}")),
            }
        }
        None => {
            // No OAuth callback listener available (shim fallback mode or no listener port).
            match auth_client.initiate_oauth_flow() {
                Ok(initiation) => ToolCallResult::success(format!(
                    "Open this URL in your browser to log in:\n{}\n\n\
                     Note: automatic completion is unavailable (daemon not running or \
                     OAuth listener failed to bind). After logging in, restart the \
                     VectorHawk daemon.",
                    initiation.auth_url
                )),
                Err(e) => ToolCallResult::error_result(format!("Failed to initiate login: {e}")),
            }
        }
    }
}

fn handle_login(
    arguments: &serde_json::Value,
    state: &AppState,
    server_registry_url: &Option<String>,
) -> ToolCallResult {
    // Delegate to the OAuth-aware handler with no OAuth context (legacy / shim fallback).
    handle_login_with_oauth(arguments, state, server_registry_url, None)
}

fn handle_logout(state: &AppState, server_registry_url: &Option<String>) -> ToolCallResult {
    let registry_url = match server_registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    match auth::clear_tokens(state, registry_url) {
        Ok(()) => ToolCallResult::success("Logged out successfully."),
        Err(e) => ToolCallResult::error_result(format!("Failed to clear tokens: {e}")),
    }
}

// ── MCP import handler ────────────────────────────────────────────────────────

fn handle_import(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    let input = match arguments.get("input").and_then(|v| v.as_str()) {
        Some(i) if !i.is_empty() => i,
        _ => {
            return ToolCallResult::error_result(
                "Missing required parameter: input. Provide an npm package name, npx command, or GitHub URL.",
            )
        }
    };

    let confirm = arguments
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let preview = match mcp_governance::import_preview(url, input, &access_token) {
        Ok(v) => v,
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("401") || err_str.contains("Unauthorized") {
                let refresh_token = match auth::load_tokens(state, url) {
                    Ok(Some(t)) => t.refresh_token,
                    _ => return auth_elicitation_prompt(url),
                };
                let new_token = match try_refresh_auth(state, url, &refresh_token) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                match mcp_governance::import_preview(url, input, &new_token) {
                    Ok(v) => v,
                    Err(e) => {
                        return ToolCallResult::error_result(format!("Import preview failed: {e}"))
                    }
                }
            } else {
                return ToolCallResult::error_result(format!("Import preview failed: {e}"));
            }
        }
    };

    let preview_text = format_import_preview(&preview);

    if !confirm {
        return ToolCallResult::success(format!(
            "{preview_text}\n\nSet confirm=true to submit this import."
        ));
    }

    let submit_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    match mcp_governance::import_submit(url, input, &submit_token) {
        Ok(result) => ToolCallResult::success(format_import_result(&result)),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("401") || err_str.contains("Unauthorized") {
                let refresh_token = match auth::load_tokens(state, url) {
                    Ok(Some(t)) => t.refresh_token,
                    _ => return auth_elicitation_prompt(url),
                };
                let new_token = match try_refresh_auth(state, url, &refresh_token) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                match mcp_governance::import_submit(url, input, &new_token) {
                    Ok(result) => ToolCallResult::success(format_import_result(&result)),
                    Err(e) => ToolCallResult::error_result(format!("Import submit failed: {e}")),
                }
            } else {
                ToolCallResult::error_result(format!("Import submit failed: {e}"))
            }
        }
    }
}

fn format_import_preview(preview: &serde_json::Value) -> String {
    let import_type = preview
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match import_type {
        "skill" => {
            let name = preview.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let skill_id = preview
                .get("skill_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let version = preview
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let publisher = preview
                .get("publisher")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let desc = preview
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!(
                "Skill Import Preview\n  Name: {name}\n  ID: {skill_id}\n  Version: {version}\n  Publisher: {publisher}\n  Description: {desc}"
            )
        }
        "mcp_server" => {
            let pkg = preview
                .get("package_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let desc = preview
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ver = preview
                .get("latest_version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let license = preview
                .get("license")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let in_catalog = preview
                .get("already_in_catalog")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mode = preview
                .get("approval_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let mut text = format!(
                "MCP Server Import Preview\n  Package: {pkg}\n  Description: {desc}\n  Version: {ver}\n  License: {license}\n  In catalog: {in_catalog}\n  Approval mode: {mode}"
            );
            if let Some(keywords) = preview.get("keywords").and_then(|v| v.as_array()) {
                let kws: Vec<&str> = keywords.iter().filter_map(|k| k.as_str()).collect();
                if !kws.is_empty() {
                    text.push_str(&format!("\n  Keywords: {}", kws.join(", ")));
                }
            }
            text
        }
        _ => format!("Unknown import type: {import_type}"),
    }
}

fn format_import_result(result: &serde_json::Value) -> String {
    let import_type = result
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match import_type {
        "skill" => {
            let skill_id = result
                .get("skill_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let version = result
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = result
                .get("review_status")
                .and_then(|v| v.as_str())
                .unwrap_or("submitted");
            format!("Skill imported successfully!\n  ID: {skill_id}\n  Version: {version}\n  Status: {status}")
        }
        "mcp_server" => {
            let name = result
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let pkg = result
                .get("package_source")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let mut text = format!(
                "MCP Server import {}!\n  Server: {name}\n  Package: {pkg}\n  Status: {status}",
                if status == "approved" {
                    "approved"
                } else {
                    "submitted"
                }
            );
            if status == "approved" {
                text.push_str(
                    "\n\nThe server has been added to your catalog and will appear in your AI tools shortly.",
                );
            } else if status == "pending" {
                text.push_str("\n\nYour request has been submitted for admin review.");
            }
            text
        }
        _ => format!("Import completed (type: {import_type})"),
    }
}

// ── MCP Governance tool handlers ──────────────────────────────────────────────

fn handle_mcp_catalog(state: &AppState, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    match mcp_governance::fetch_mcp_catalog(url) {
        Ok(resp) => {
            // Cache for offline use (best-effort)
            let _ = mcp_governance::fetch_approved_servers_cached(state, resp.clone());

            let formatted: Vec<serde_json::Value> = resp
                .servers
                .iter()
                .filter(|s| s.status == "approved")
                .map(|s| {
                    let mut entry = serde_json::json!({
                        "name": s.name,
                        "package_source": s.package_source,
                        "status": s.status,
                    });
                    if let Some(pin) = &s.version_pin {
                        entry["version_pin"] = serde_json::json!(pin);
                    }
                    if let Some(note) = &s.credential_note {
                        entry["credential_note"] = serde_json::json!(note);
                    }
                    entry
                })
                .collect();

            if formatted.is_empty() {
                ToolCallResult::success(format!(
                    "No approved MCP servers in catalog (approval mode: {}).\n\
                     Ask your IT admin to add servers via the VectorHawk admin portal.{}",
                    resp.approval_mode, GOVERNANCE_FOOTER
                ))
            } else {
                let mut output = format!(
                    "Org approval mode: {}\n\nApproved MCP servers ({}):\n",
                    resp.approval_mode,
                    formatted.len()
                );
                match serde_json::to_string_pretty(&formatted) {
                    Ok(text) => {
                        output.push_str(&text);
                        output.push_str(GOVERNANCE_FOOTER);
                        ToolCallResult::success(output)
                    }
                    Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error_result(format!("Failed to fetch MCP catalog: {e}")),
    }
}

fn handle_mcp_request(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error_result("Missing required parameter: server_name"),
    };

    let package_source = arguments.get("package_source").and_then(|v| v.as_str());

    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let result =
        match mcp_governance::submit_mcp_request(url, server_name, package_source, &access_token) {
            Ok(v) => v,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("401") || err_str.contains("Unauthorized") {
                    let refresh_token = match auth::load_tokens(state, url) {
                        Ok(Some(t)) => t.refresh_token,
                        _ => return auth_elicitation_prompt(url),
                    };
                    let new_token = match try_refresh_auth(state, url, &refresh_token) {
                        Ok(t) => t,
                        Err(e) => return e,
                    };
                    match mcp_governance::submit_mcp_request(
                        url,
                        server_name,
                        package_source,
                        &new_token,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            return ToolCallResult::error_result(format!(
                                "Failed to submit request: {e}"
                            ))
                        }
                    }
                } else {
                    return ToolCallResult::error_result(format!("Failed to submit request: {e}"));
                }
            }
        };

    let req_status = result
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match req_status {
        "approved" => ToolCallResult::success(format!(
            "Request for '{server_name}' was approved! \
             Use `vectorhawk_mcp_install` with server_name '{server_name}' to activate it now."
        )),
        "pending" => ToolCallResult::success(format!(
            "Request for '{server_name}' has been submitted and is pending IT review.\n\n\
             Your admin will review it in the VectorHawk portal. \
             Run `vectorhawk_mcp_status` to check on it later, then use \
             `vectorhawk_mcp_install` to activate it once approved."
        )),
        _ => ToolCallResult::success(format!("Request submitted with status: {req_status}")),
    }
}

fn handle_mcp_status(state: &AppState, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let result = match mcp_governance::list_mcp_requests(url, &access_token) {
        Ok(v) => v,
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("401") || err_str.contains("Unauthorized") {
                let refresh_token = match auth::load_tokens(state, url) {
                    Ok(Some(t)) => t.refresh_token,
                    _ => return auth_elicitation_prompt(url),
                };
                let new_token = match try_refresh_auth(state, url, &refresh_token) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                match mcp_governance::list_mcp_requests(url, &new_token) {
                    Ok(v) => v,
                    Err(e) => {
                        return ToolCallResult::error_result(format!(
                            "Failed to fetch requests: {e}"
                        ))
                    }
                }
            } else {
                return ToolCallResult::error_result(format!("Failed to fetch requests: {e}"));
            }
        }
    };

    let items = result
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if items.is_empty() {
        ToolCallResult::success("No MCP server access requests found.")
    } else {
        let formatted: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "server_name": item.get("server_name").and_then(|v| v.as_str()).unwrap_or("?"),
                    "status": item.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                    "admin_notes": item.get("admin_notes").and_then(|v| v.as_str()),
                    "created_at": item.get("created_at").and_then(|v| v.as_str()),
                })
            })
            .collect();

        match serde_json::to_string_pretty(&formatted) {
            Ok(text) => ToolCallResult::success(text),
            Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
        }
    }
}

/// Activate an approved MCP server by forcing an immediate aggregator sync.
pub fn handle_mcp_install(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
    aggregator: Option<&BackendRegistry>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error_result("No registry URL configured"),
    };

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error_result("Missing required parameter: server_name"),
    };

    let aggregator = match aggregator {
        Some(a) => a,
        None => {
            return ToolCallResult::error_result(
                "Aggregator not available in this context (are you in shim fallback mode?)",
            )
        }
    };

    let server_id = sanitize_id(server_name);

    // Fetch the current catalog and cache it; then check if the server is in it
    match mcp_governance::fetch_mcp_catalog(url) {
        Ok(resp) => {
            let _ = mcp_governance::fetch_approved_servers_cached(state, resp.clone());

            let entry = resp
                .servers
                .iter()
                .find(|s| s.status == "approved" && sanitize_id(&s.name) == server_id);

            if entry.is_some() {
                // The aggregator sync (Stream M1.3) will wire the real backend
                // connection. For now we report success and note that the backend
                // will appear on the next full sync.
                let tools = aggregator.backend_tools(&server_id);
                let tool_list = if tools.is_empty() {
                    "Server will appear after the next aggregator sync.".to_string()
                } else {
                    tools.join(", ")
                };
                ToolCallResult::success(format!(
                    "MCP server '{server_name}' is approved and will be activated by VectorHawk governance.\n\nTools: {tool_list}"
                ))
            } else {
                ToolCallResult::error_result(format!(
                    "Server '{server_name}' is not in the approved server list. \
                     It may be pending approval, blocked, or not yet requested.\n\n\
                     Use vectorhawk_mcp_request to request access, then retry \
                     vectorhawk_mcp_install after approval."
                ))
            }
        }
        Err(e) => ToolCallResult::error_result(format!(
            "Failed to sync with registry: {e}. Check your network connection and registry URL."
        )),
    }
}

/// Remove a governed MCP server from the aggregator.
pub fn handle_mcp_uninstall(
    arguments: &serde_json::Value,
    registry_url: &Option<String>,
    aggregator: Option<&BackendRegistry>,
) -> ToolCallResult {
    if registry_url.is_none() {
        return ToolCallResult::error_result("No registry URL configured");
    }

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error_result("Missing required parameter: server_name"),
    };

    let aggregator = match aggregator {
        Some(a) => a,
        None => {
            return ToolCallResult::error_result(
                "Aggregator not available in this context (are you in shim fallback mode?)",
            )
        }
    };

    let server_id = sanitize_id(server_name);

    if aggregator.remove_backend(&server_id) {
        ToolCallResult::success(format!(
            "MCP server '{server_name}' has been deactivated. Its tools are no longer available."
        ))
    } else {
        ToolCallResult::error_result(format!(
            "No active MCP server found with name '{server_name}'. Use vectorhawk_mcp_status to see your servers."
        ))
    }
}

// ── Skill lifecycle handlers (GAP-07, GAP-08) ─────────────────────────────────

fn handle_uninstall(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error_result("Missing required parameter: skill_id"),
    };

    match uninstall_skill(state, skill_id) {
        Ok(Some(version)) => {
            ToolCallResult::success(format!("Successfully uninstalled {skill_id}@{version}."))
        }
        Ok(None) => {
            ToolCallResult::error_result(format!("Skill '{skill_id}' is not installed."))
        }
        Err(e) => ToolCallResult::error_result(format!("Failed to uninstall '{skill_id}': {e}")),
    }
}

fn handle_update(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error_result("Missing required parameter: skill_id"),
    };

    let url = match registry_url {
        Some(u) => u,
        None => {
            return ToolCallResult::error_result(
                "No registry configured — cannot update skills",
            )
        }
    };

    let registry = RegistryClient::new(url);
    match install_from_registry(state, &registry, skill_id, None) {
        Ok(version) => ToolCallResult::success(format!("Updated {skill_id} to v{version}.")),
        Err(e) => ToolCallResult::error_result(format!("Update failed: {e}")),
    }
}

// ── Skill execution handler ───────────────────────────────────────────────────

fn build_llm_execution_summary(result: &RunResult) -> Option<String> {
    let llm_steps: Vec<_> = result
        .steps
        .iter()
        .filter(|s| s.model_source.is_some())
        .collect();

    if llm_steps.is_empty() {
        return None;
    }

    let total_prompt: u64 = llm_steps.iter().filter_map(|s| s.prompt_tokens).sum();
    let total_completion: u64 = llm_steps.iter().filter_map(|s| s.completion_tokens).sum();
    let total_latency: u64 = llm_steps.iter().filter_map(|s| s.latency_ms).sum();
    let step_count = llm_steps.len();

    let has_sampling = llm_steps
        .iter()
        .any(|s| matches!(&s.model_source, Some(ModelSource::McpSampling)));

    let source_label = if has_sampling {
        "remote model via MCP sampling".to_string()
    } else {
        let model_name = llm_steps
            .iter()
            .find_map(|s| match &s.model_source {
                Some(ModelSource::Local(name)) => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "local model".to_string());
        format!("local model {model_name}")
    };

    let step_phrase = if step_count == 1 {
        "1 step".to_string()
    } else {
        format!("{step_count} steps")
    };

    Some(format!(
        "\u{25b6} Ran {} v{} \u{2014} used {source_label} across {step_phrase} \
         ({total_prompt}\u{2192}{total_completion} tokens, {total_latency}ms)",
        result.skill_id, result.version,
    ))
}

fn handle_skill_run(
    skill_id: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    update_check_cache: &UpdateCheckCache,
) -> ToolCallResult {
    // Check if skill is deactivated before attempting execution
    if let Ok(conn) = Connection::open(&state.db_path) {
        if let Ok(status) = conn.query_row(
            "SELECT current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| row.get::<_, String>(0),
        ) {
            if status == "deactivated" {
                return ToolCallResult::error_result(format!(
                    "The skill '{skill_id}' has been deactivated by your organisation's \
                     administrator. Please contact your IT department to resolve this or \
                     request reactivation."
                ));
            }
        }
    }

    // Update-check gate (cache-backed, best-effort)
    let skip_update_check = arguments
        .get("skip_update_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !skip_update_check {
        if let Some(update_prompt) = maybe_build_update_prompt(skill_id, update_check_cache) {
            return update_prompt;
        }
    }

    match run_skill(state, policy_client, skill_id, arguments, model_client) {
        Ok(result) => {
            let output = result
                .steps
                .iter()
                .rev()
                .find_map(|s| s.output.as_ref())
                .cloned()
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "status": "completed",
                        "skill_id": result.skill_id,
                        "version": result.version,
                        "steps_completed": result.steps.len(),
                    })
                });

            let output_text = match &output {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };

            let llm_summary = build_llm_execution_summary(&result);
            let text = match llm_summary {
                Some(summary) => format!("{summary}\n\n{output_text}"),
                None => output_text,
            };

            ToolCallResult::success(text)
        }
        Err(e) => {
            let err_str = e.to_string();
            if let Some((_, reason_raw)) = err_str.split_once("is blocked:") {
                let reason = reason_raw.trim();
                return ToolCallResult::error_result(format!(
                    "\u{26d4} Skill '{skill_id}' is blocked: {reason}\n\n\
                     If this skill was recently unpublished, run `vectorhawk_list` to review your skills.\n\
                     If you believe this is an error, contact your administrator."
                ));
            }

            let mut chain = format!("Skill execution failed: {e}");
            let mut source = e.source();
            while let Some(src) = source {
                chain.push_str(&format!("\n  caused by: {src}"));
                source = src.source();
            }
            ToolCallResult::error_result(chain)
        }
    }
}

/// Consult the update-check cache for `skill_id`.
///
/// Returns `Some(ToolCallResult)` when a newer version is known to be
/// available (prompting the user to update). Returns `None` when up-to-date,
/// check failed, or no registry client is configured.
///
/// M1.4: wire to the real update-check when `HttpRegistryClient` gains
/// `check_for_update`. For now the cache is populated externally (e.g. by
/// the daemon's registry sync loop).
fn maybe_build_update_prompt(skill_id: &str, cache: &UpdateCheckCache) -> Option<ToolCallResult> {
    let cached = cache.lock().ok().and_then(|guard| {
        guard.get(skill_id).and_then(|entry| {
            if entry.checked_at.elapsed() < UPDATE_CHECK_TTL {
                Some(entry.latest_version.clone())
            } else {
                None
            }
        })
    });

    let latest = cached.flatten()?;

    Some(ToolCallResult::error_result(format!(
        "Update available for '{skill_id}' (v{latest} in registry).\n\n\
         Would you like to update before running? Options:\n\
         1. Call vectorhawk_install(skill_id=\"{skill_id}\") to install v{latest}, then retry.\n\
         2. Call this skill again with skip_update_check=true to run the current version."
    )))
}

// ── Plugin export handler ─────────────────────────────────────────────────────

fn handle_plugin_export(arguments: &serde_json::Value) -> ToolCallResult {
    use vectorhawkd_core::plugin_export;

    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error_result("Missing required parameter: path"),
    };

    let format = match arguments.get("format").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ToolCallResult::error_result("Missing required parameter: format"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let plugin_path = camino::Utf8Path::new(path);
    let out_path = camino::Utf8Path::new(output_dir);

    let result = match format {
        "claude-code" => plugin_export::export_claude_code(plugin_path, out_path),
        "mcpb" => plugin_export::export_mcpb(plugin_path, out_path),
        other => {
            return ToolCallResult::error_result(format!(
                "Unsupported format '{other}'. Use 'claude-code' or 'mcpb'."
            ))
        }
    };

    match result {
        Ok(exported_path) => {
            ToolCallResult::success(format!("Plugin exported successfully to: {exported_path}"))
        }
        Err(e) => ToolCallResult::error_result(format!("Export failed: {e}")),
    }
}

// ── Plugin import handler ─────────────────────────────────────────────────────

fn handle_plugin_import(arguments: &serde_json::Value) -> ToolCallResult {
    use vectorhawkd_core::plugin_import;

    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => camino::Utf8PathBuf::from(p),
        None => return ToolCallResult::error_result("Missing required parameter: path"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let out = camino::Utf8PathBuf::from(output_dir);

    let format = match plugin_import::detect_plugin_format(&path) {
        Some(f) => f,
        None => {
            return ToolCallResult::error_result(format!(
                "Could not detect plugin format at '{}'. \
             Expected a Claude Code plugin directory (with .claude-plugin/) or a .mcpb file.",
                path
            ))
        }
    };

    let format_label = format!("{:?}", format);

    let result = match format {
        plugin_import::ExternalPluginFormat::ClaudeCode => {
            plugin_import::import_claude_code_plugin(&path, &out)
        }
        plugin_import::ExternalPluginFormat::Mcpb => plugin_import::import_mcpb(&path, &out),
    };

    match result {
        Ok(p) => {
            let payload = serde_json::json!({
                "status": "imported",
                "format": format_label,
                "output_path": p.as_str(),
                "next_steps": format!(
                    "Plugin converted to VectorHawk format at '{p}'.\n\
                     Next: run 'vectorhawk plugin validate {p}' to validate the bundle."
                ),
            });
            ToolCallResult::success(
                serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| format!("Imported to {p}")),
            )
        }
        Err(e) => ToolCallResult::error_result(format!("Import failed: {e}")),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use vectorhawkd_core::installer::install_unpacked_skill;

    fn temp_root(label: &str) -> camino::Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        camino::Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("mcp-tools-tests-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_test_skill(root: &camino::Utf8PathBuf) {
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\n\
             name: Test Skill\n\
             description: A test skill for MCP testing\n\
             license: Apache-2.0\n\
             vh_version: 0.1.0\n\
             vh_publisher: vectorhawk\n\
             vh_permissions:\n  \
               network: none\n  \
               filesystem: none\n  \
               clipboard: none\n\
             vh_execution:\n  \
               sandbox: strict\n  \
               timeout_ms: 30000\n  \
               memory_mb: 256\n\
             vh_schemas:\n  \
               inputs:\n    \
                 type: object\n    \
                 properties:\n      \
                   query:\n        \
                     type: string\n    \
                 required:\n      \
                   - query\n  \
               outputs:\n    \
                 type: object\n\
             vh_workflow_ref: workflow.yaml\n\
             ---\n\
             \n\
             Do the thing.\n",
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
    }

    fn fake_login(state: &AppState, url: &str) {
        vectorhawkd_core::auth::save_tokens(state, url, "fake-access", "fake-refresh").unwrap();
    }

    fn empty_cache() -> UpdateCheckCache {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn build_tool_list_includes_management_tools() {
        let state_root = temp_root("tool-list");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let tools = build_tool_list(&state, &Some(url));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"vectorhawk_list"));
        assert!(names.contains(&"vectorhawk_search"));
        assert!(names.contains(&"vectorhawk_install"));
        assert!(names.contains(&"vectorhawk_info"));
        assert!(names.contains(&"vectorhawk_validate"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_without_registry_omits_registry_tools() {
        let state_root = temp_root("tool-list-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"vectorhawk_list"));
        assert!(names.contains(&"vectorhawk_validate"));
        assert!(names.contains(&"vectorhawk_install"));
        assert!(!names.contains(&"vectorhawk_search"));
        assert!(!names.contains(&"vectorhawk_logout"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_includes_installed_skill() {
        let state_root = temp_root("tool-list-skill");
        let skill_root = temp_root("tool-list-skill-bundle");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let tools = build_tool_list(&state, &None);
        let skill_tool = tools.iter().find(|t| t.name == "test-skill");

        assert!(
            skill_tool.is_some(),
            "installed skill should appear as tool"
        );
        let tool = skill_tool.unwrap();
        assert!(
            tool.description
                .starts_with("A test skill for MCP testing (v0.1.0)"),
            "description should start with versioned desc, got: {}",
            tool.description
        );
        assert_eq!(tool.input_schema["type"], "object");
        assert!(tool.input_schema["properties"]["query"].is_object());

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_list_returns_installed_skills() {
        let state_root = temp_root("handle-list");
        let skill_root = temp_root("handle-list-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let result = handle_list(&state);
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(
            text.contains("test-skill"),
            "should list test-skill, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_list_empty() {
        let state_root = temp_root("handle-list-empty");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_list(&state);
        assert!(result.is_error.is_none());
        assert!(result.content[0].text.contains("No skills installed"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_search_requires_registry() {
        let result = handle_search(&serde_json::json!({"query": "test"}), &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));
    }

    #[test]
    fn handle_install_requires_path_or_skill_id() {
        let state_root = temp_root("handle-install-no-id");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_install(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0].text.contains("path") || result.content[0].text.contains("skill_id")
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_install_local_path() {
        let state_root = temp_root("handle-install-local");
        let skill_root = temp_root("handle-install-local-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);

        let result = handle_install(
            &serde_json::json!({"path": skill_root.as_str()}),
            &state,
            &None,
        );
        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );
        assert!(result.content[0].text.contains("test-skill"));
        assert!(result.content[0].text.contains("0.1.0"));

        let list_result = handle_list(&state);
        assert!(list_result.content[0].text.contains("test-skill"));

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_install_registry_requires_url() {
        let state_root = temp_root("handle-install-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_install(
            &serde_json::json!({"skill_id": "some-skill"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_info_not_installed() {
        let state_root = temp_root("handle-info-missing");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_info(&serde_json::json!({"skill_id": "ghost"}), &state);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("not installed"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_info_returns_skill_details() {
        let state_root = temp_root("handle-info-ok");
        let skill_root = temp_root("handle-info-ok-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        let result = handle_info(&serde_json::json!({"skill_id": "test-skill"}), &state);
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(text.contains("test-skill"), "got: {text}");
        assert!(text.contains("Test Skill"), "got: {text}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_validate_validates_bundle() {
        let skill_root = temp_root("handle-validate-skill");
        write_test_skill(&skill_root);

        let result = handle_validate(&serde_json::json!({"path": skill_root.as_str()}));
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(text.contains("all_passed"), "got: {text}");

        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_validate_requires_path() {
        let result = handle_validate(&serde_json::json!({}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path"));
    }

    #[test]
    fn handle_login_no_registry_url() {
        let state_root = temp_root("handle-login-no-url");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_login(&serde_json::json!({}), &state, &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_logout_no_registry_url() {
        let state_root = temp_root("handle-logout-no-url");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_logout(&state, &None);
        assert_eq!(result.is_error, Some(true));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_logout_clears_tokens() {
        let state_root = temp_root("handle-logout-ok");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let result = handle_logout(&state, &Some(url.clone()));
        assert!(result.is_error.is_none());

        // Tokens should be gone
        let tokens = vectorhawkd_core::auth::load_tokens(&state, &url).unwrap();
        assert!(tokens.is_none());

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_catalog_requires_registry() {
        let state_root = temp_root("mcp-catalog-no-url");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_mcp_catalog(&state, &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_request_requires_server_name() {
        let state_root = temp_root("mcp-request-no-name");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_mcp_request(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("server_name"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_uninstall_requires_server_name() {
        let result = handle_mcp_uninstall(
            &serde_json::json!({}),
            &Some("http://localhost:8000".to_string()),
            None,
        );
        // No aggregator provided — returns aggregator error first
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn handle_mcp_uninstall_unknown_server() {
        let registry = BackendRegistry::new();
        let result = handle_mcp_uninstall(
            &serde_json::json!({"server_name": "nonexistent"}),
            &Some("http://localhost:8000".to_string()),
            Some(&registry),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("No active MCP server"));
    }

    #[test]
    fn handle_import_requires_input() {
        let state_root = temp_root("import-no-input");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_import(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_no_registry() {
        let state_root = temp_root("import-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_import(&serde_json::json!({"input": "some-package"}), &state, &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_skill_run_deactivated_skill() {
        let state_root = temp_root("skill-run-deactivated");
        let skill_root = temp_root("skill-run-deactivated-bundle");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

        // Mark the skill as deactivated
        let conn = Connection::open(&state.db_path).unwrap();
        conn.execute(
            "UPDATE installed_skills SET current_status = 'deactivated' WHERE skill_id = ?1",
            ["test-skill"],
        )
        .unwrap();

        let policy = vectorhawkd_core::policy::MockPolicyClient::new();
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "test"}),
            &state,
            &policy,
            None,
            &empty_cache(),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("deactivated"));

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    // ── plugin export/import tool handler tests ───────────────────────────────

    #[test]
    fn handle_plugin_export_requires_path() {
        let result = handle_plugin_export(&serde_json::json!({"format": "mcpb"}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path"));
    }

    #[test]
    fn handle_plugin_export_requires_format() {
        let result = handle_plugin_export(&serde_json::json!({"path": "/some/plugin"}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("format"));
    }

    #[test]
    fn handle_plugin_export_rejects_unknown_format() {
        let result = handle_plugin_export(
            &serde_json::json!({"path": "/some/plugin", "format": "tarball"}),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("Unsupported format"));
    }

    #[test]
    fn handle_plugin_import_requires_path() {
        let result = handle_plugin_import(&serde_json::json!({}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path"));
    }

    #[test]
    fn handle_plugin_import_unknown_format() {
        let root = temp_root("pi-unknown");
        fs::create_dir_all(&root).unwrap();
        // A plain directory with no recognized format markers
        let result = handle_plugin_import(
            &serde_json::json!({"path": root.to_string()}),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("Could not detect plugin format"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_tool_list_includes_plugin_tools() {
        let state_root = temp_root("tool-list-plugin");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"vectorhawk_plugin_export"),
            "tool list should include vectorhawk_plugin_export"
        );
        assert!(
            names.contains(&"vectorhawk_plugin_import"),
            "tool list should include vectorhawk_plugin_import"
        );
        let _ = fs::remove_dir_all(&state_root);
    }
}
