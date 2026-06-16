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
use anyhow::{Context, Result};
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
    ratings::{
        has_existing_rating, increment_execution_count, record_rating, should_prompt_for_rating,
    },
    registry::RegistryClient,
    scan::{HttpScanClient, ScanClient},
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

/// Pending rating prompt: `Some((skill_id, version))` when the previous skill
/// execution appended a thumbs-up/down prompt that has not yet been answered.
/// Shared across tool calls within a single MCP session via interior mutability.
pub type RatingState = Arc<Mutex<Option<(String, String)>>>;

const RATING_PROMPT: &str = "\n\nWas this skill helpful? Reply 'thumbs up' or 'thumbs down'.";

const GOVERNANCE_FOOTER: &str = "\n\n---\nTo add new MCP servers, use /mcp-request. Direct installation via /mcp bypasses governance.";

// ── Tool registry ─────────────────────────────────────────────────────────────

/// Builds the list of MCP tool definitions from installed skills + management tools.
///
/// The vectorhawk_* management tools (search/install/uninstall/etc.) are
/// hidden by default because users now manage skills + MCP servers via the
/// portal — the management tools clutter the AI client's tool list. Set
/// `VECTORHAWK_EXPOSE_BUILTIN_TOOLS=1` to surface them for CLI-style
/// workflows.
pub fn build_tool_list(state: &AppState, registry_url: &Option<String>) -> Vec<ToolDefinition> {
    let expose = std::env::var("VECTORHAWK_EXPOSE_BUILTIN_TOOLS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    build_tool_list_inner(state, registry_url, expose)
}

/// Test-only entry point: same as `build_tool_list` but takes the
/// `expose_builtins` flag explicitly so tests don't depend on env vars.
#[cfg(test)]
pub(crate) fn build_tool_list_with_builtins(
    state: &AppState,
    registry_url: &Option<String>,
) -> Vec<ToolDefinition> {
    build_tool_list_inner(state, registry_url, true)
}

fn build_tool_list_inner(
    state: &AppState,
    registry_url: &Option<String>,
    expose_builtins: bool,
) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    let logged_in = registry_url
        .as_ref()
        .and_then(|url| auth::load_tokens(state, url).ok().flatten())
        .is_some();

    // Add installed skills as tools
    if let Ok(skill_tools) = skill_tools_from_db(state) {
        tools.extend(skill_tools);
    }

    if !expose_builtins {
        return tools;
    }

    // Management tools — opt-in via VECTORHAWK_EXPOSE_BUILTIN_TOOLS.
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
        description: "Import an external skill or MCP server into your organization's governed \
            catalog. Pass a GitHub URL, npm package name (e.g. '@modelcontextprotocol/server-slack'), \
            or marketplace URL. Use action='preview' first to see what will be imported, then \
            action='submit' to request approval."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "GitHub URL, npm package name (e.g. '@modelcontextprotocol/server-slack'), or marketplace URL to import"
                },
                "action": {
                    "type": "string",
                    "enum": ["preview", "submit"],
                    "description": "preview: see what will be imported without committing. submit: submit for approval (or auto-approve if policy allows)."
                }
            },
            "required": ["input", "action"]
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
    }

    // Search and scan — public endpoints, no auth required
    if registry_url.is_some() {
        tools.push(ToolDefinition {
            name: "vectorhawk_search".to_string(),
            description: "Search the VectorHawk skill registry for skills that can be installed. \
                Requires a search query — ask the user what they're looking for if none is provided."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (e.g., 'contract review', 'data analysis'). Must not be empty."
                    }
                },
                "required": ["query"]
            }),
        });

        tools.push(ToolDefinition {
            name: "vectorhawk_scan".to_string(),
            description: "Scan arbitrary content (SKILL.md, MCP JSON config, or package name) \
                for security threats using VectorHawk's AI scanner. Returns a verdict with \
                color-coded severity and findings. Use this before importing or installing \
                untrusted content."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Content to scan (SKILL.md text, JSON config, or package name/URL)"
                    },
                    "content_type": {
                        "type": "string",
                        "enum": ["skill_md", "mcp_json", "other"],
                        "description": "Hint about what the content is (default: other)"
                    }
                },
                "required": ["content"]
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

    // AUTH2c: skill authoring tools — always available (local operations)
    tools.push(ToolDefinition {
        name: "vectorhawk_author".to_string(),
        description: "Author a new VectorHawk skill from a name and system prompt. \
            In interactive mode (default) returns recommendations for you to review. \
            Pass mode='accept_suggestions' to apply recommendations automatically. \
            Pass mode='skip_metadata' to scaffold immediately with defaults."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (e.g. 'contract-compare')"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what the skill does"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "The system prompt text for the skill"
                },
                "triggers": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of trigger phrases for the skill"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory path (default: current directory)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["interactive", "accept_suggestions", "skip_metadata"],
                    "description": "Authoring mode (default: interactive)"
                }
            },
            "required": ["name", "system_prompt"]
        }),
    });

    tools.push(ToolDefinition {
        name: "vectorhawk_author_confirm".to_string(),
        description: "Confirm and scaffold a skill after reviewing recommendations from \
            vectorhawk_author. Provide the final values to create the SKILL.md."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The skill ID to create (e.g. 'contract-compare')"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "The system prompt text for the skill"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory path (default: current directory)"
                },
                "triggers": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Trigger phrases for the skill"
                },
                "permissions": {
                    "type": "object",
                    "properties": {
                        "network": {"type": "string"},
                        "filesystem": {"type": "string"},
                        "clipboard": {"type": "string"}
                    },
                    "description": "Permission settings (network, filesystem, clipboard)"
                },
                "model": {
                    "type": "object",
                    "properties": {
                        "min_params_b": {"type": "number"},
                        "recommended": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "fallback": {"type": "string"}
                    },
                    "description": "Model requirements"
                },
                "execution": {
                    "type": "object",
                    "properties": {
                        "timeout_ms": {"type": "integer"},
                        "memory_mb": {"type": "integer"},
                        "sandbox": {"type": "string"}
                    },
                    "description": "Execution constraints"
                }
            },
            "required": ["skill_id", "system_prompt"]
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

/// Scan a tool call's arguments for a thumbs-up or thumbs-down rating reply.
///
/// Checks the `_rating_reply` key first, then all string values in the arguments
/// map. Returns `"up"` or `"down"` (the canonical SQLite values), or `None`.
fn extract_rating_reply(arguments: &serde_json::Value) -> Option<&'static str> {
    let check = |s: &str| -> Option<&'static str> {
        let lower = s.to_lowercase();
        if lower.contains("thumbs up") || lower.contains("thumb up") {
            Some("up")
        } else if lower.contains("thumbs down") || lower.contains("thumb down") {
            Some("down")
        } else {
            None
        }
    };

    if let Some(reply) = arguments.get("_rating_reply").and_then(|v| v.as_str()) {
        if let Some(r) = check(reply) {
            return Some(r);
        }
    }

    if let Some(map) = arguments.as_object() {
        for v in map.values() {
            if let Some(s) = v.as_str() {
                if let Some(r) = check(s) {
                    return Some(r);
                }
            }
        }
    }

    None
}

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
    rating_state: Option<&RatingState>,
) -> ToolCallResult {
    // Check if this tool call is a rating reply to a pending prompt.
    if let Some(rs) = rating_state {
        if let Some(reply) = extract_rating_reply(arguments) {
            if let Ok(mut guard) = rs.lock() {
                if let Some((skill_id, version)) = guard.take() {
                    if let Ok(conn) = Connection::open(&state.db_path) {
                        let _ = record_rating(&conn, &skill_id, &version, reply);
                        debug!(
                            skill_id,
                            version,
                            rating = reply,
                            "recorded rating from tool call reply"
                        );
                    }
                    let rating_word = if reply == "up" {
                        "thumbs up"
                    } else {
                        "thumbs down"
                    };
                    return ToolCallResult::success(format!(
                        "Got it — {rating_word} recorded for {skill_id}@{version}. Thanks!"
                    ));
                }
            }
        }
    }

    let result = match name {
        "vectorhawk_list" => handle_list(state),
        "vectorhawk_search" => handle_search(arguments, registry_url),
        "vectorhawk_install" => handle_install(arguments, state, registry_url),
        "vectorhawk_info" => handle_info(arguments, state),
        "vectorhawk_validate" => handle_validate(arguments),
        "vectorhawk_import" => handle_import(arguments, state, registry_url),
        "vectorhawk_scan" => handle_scan(arguments, state, registry_url),
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
        "vectorhawk_author" => handle_author(arguments, state, registry_url.as_deref()),
        "vectorhawk_author_confirm" => {
            handle_author_confirm(arguments, state, registry_url.as_deref())
        }
        _ => handle_skill_run(
            name,
            arguments,
            state,
            policy_client,
            model_client,
            update_check_cache,
            rating_state,
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
             - Use vectorhawk_search to browse the registry\n\
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

    if query.trim().is_empty() {
        return ToolCallResult::error_result(
            "A search query is required. Ask the user what they're looking for.",
        );
    }

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
            match mcp_governance::install_from_registry(state, url, id, version) {
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

    // If tokens already exist, return immediately — don't start a new OAuth flow.
    if auth::load_tokens(state, &registry_url)
        .ok()
        .flatten()
        .is_some()
    {
        return ToolCallResult::success("Already logged in to VectorHawk. No action needed.");
    }

    let auth_client = AuthClient::new(&registry_url);

    match oauth {
        Some(ctx) => {
            let redirect_uri = format!("http://127.0.0.1:{}/oauth/cli/callback", ctx.listener_port);
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
                        let code = match subscriber.wait_for_code(oauth_state_val, 300).await {
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

                    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
                    let port = ctx.listener_port;

                    // On Linux the daemon runs as a systemd service without a display.
                    // Always include SSH tunnel instructions — they're harmless on
                    // desktop and essential for headless/SSH use.
                    #[cfg(target_os = "linux")]
                    let ssh_note = {
                        let hostname = std::env::var("HOSTNAME")
                            .or_else(|_| {
                                std::fs::read_to_string("/etc/hostname")
                                    .map(|s| s.trim().to_string())
                            })
                            .unwrap_or_else(|_| "<this-machine>".to_string());
                        format!(
                            "\n\n**Option A — browser login (SSH tunnel required):**\n\
                             First run this in a new local terminal:\n\
                             ```\nssh -L {port}:localhost:{port} {hostname}\n```\
                             Then open the URL above. Keep the tunnel open until done.\n\n\
                             **Option B — Personal Access Token (easier for SSH):**\n\
                             1. Open https://app.vectorhawk.ai/portal/settings\n\
                             2. Create a token (starts with `vh_pat_`)\n\
                             3. Run: `vectorhawk auth token <vh_pat_...>`"
                        )
                    };
                    #[cfg(not(target_os = "linux"))]
                    let ssh_note = String::new();

                    ToolCallResult::success(format!(
                        "Open this URL in your browser to log in:\n{}{ssh_note}\n\n\
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

/// Import action variant parsed from the `action` field of a `vectorhawk_import` call.
#[derive(Debug, PartialEq, Eq)]
enum ImportAction {
    Preview,
    Submit,
}

/// Parse the `action` field from tool arguments.
///
/// Returns `Err(ToolCallResult)` with an actionable error message when the
/// field is missing or contains an unrecognised value.
fn parse_import_action(arguments: &serde_json::Value) -> Result<ImportAction, ToolCallResult> {
    match arguments.get("action").and_then(|v| v.as_str()) {
        Some("preview") => Ok(ImportAction::Preview),
        Some("submit") => Ok(ImportAction::Submit),
        Some(other) => Err(ToolCallResult::error_result(format!(
            "Invalid action '{other}'. Must be 'preview' or 'submit'."
        ))),
        None => Err(ToolCallResult::error_result(
            "Missing required parameter: action. Use 'preview' to inspect, then 'submit' to request approval.",
        )),
    }
}

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
                "Missing required parameter: input. Provide a GitHub URL, npm package name, or marketplace URL.",
            )
        }
    };

    let action = match parse_import_action(arguments) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    match action {
        ImportAction::Preview => handle_import_preview(url, input, &access_token, state),
        ImportAction::Submit => handle_import_submit(url, input, &access_token, state),
    }
}

/// Execute the preview step: call the registry and return a formatted preview.
///
/// Retries once with a refreshed token on 401.
fn handle_import_preview(
    url: &str,
    input: &str,
    access_token: &str,
    state: &AppState,
) -> ToolCallResult {
    match mcp_governance::import_preview(url, input, access_token) {
        Ok(preview) => ToolCallResult::success(format!(
            "{}\n\nUse action='submit' to request approval.",
            format_import_preview(&preview)
        )),
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
                    Ok(preview) => ToolCallResult::success(format!(
                        "{}\n\nUse action='submit' to request approval.",
                        format_import_preview(&preview)
                    )),
                    Err(e) => ToolCallResult::error_result(format!("Import preview failed: {e}")),
                }
            } else {
                ToolCallResult::error_result(format!("Import preview failed: {e}"))
            }
        }
    }
}

/// Execute the submit step: call the registry and return a formatted result.
///
/// Retries once with a refreshed token on 401.
fn handle_import_submit(
    url: &str,
    input: &str,
    access_token: &str,
    state: &AppState,
) -> ToolCallResult {
    match mcp_governance::import_submit(url, input, access_token) {
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

// ── vectorhawk_scan handler ───────────────────────────────────────────────────

fn handle_scan(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => {
            return ToolCallResult::error_result(
                "No registry URL configured — scan requires a registry connection.",
            )
        }
    };

    let content = match arguments.get("content").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c,
        _ => return ToolCallResult::error_result("Missing required parameter: content"),
    };

    let content_type = arguments
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("other");

    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let client = HttpScanClient::new(url, access_token);
    let verdict = match client.scan(content.as_bytes(), content_type) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error_result(format!("Scan failed unexpectedly: {e}")),
    };

    let label = verdict.verdict.badge_label();
    let cached_note = if verdict.cached { " (cached)" } else { "" };
    let version_note = verdict
        .scanner_version
        .as_deref()
        .map(|v| format!(" scanner v{v}"))
        .unwrap_or_default();

    let mut output = format!("Verdict: {label}{cached_note}{version_note}\n");

    let findings_text = verdict.format_findings();
    if !findings_text.is_empty() {
        output.push('\n');
        output.push_str(&findings_text);
    }

    if verdict.requires_confirmation() {
        output.push_str(
            "\nWARNING: This content has been flagged as risky. \
             Review the findings above before importing or installing.",
        );
    }

    ToolCallResult::success(output)
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
        Ok(None) => ToolCallResult::error_result(format!("Skill '{skill_id}' is not installed.")),
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
            return ToolCallResult::error_result("No registry configured — cannot update skills")
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
    rating_state: Option<&RatingState>,
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
            let base_text = match llm_summary {
                Some(summary) => format!("{summary}\n\n{output_text}"),
                None => output_text,
            };

            // RAT1-B: increment execution count and maybe append rating prompt.
            let rating_suffix =
                maybe_rating_prompt(state, &result.skill_id, &result.version, rating_state);
            let text = format!("{base_text}{rating_suffix}");

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

/// Increment the execution count for a successful skill run, check the rating
/// prompt schedule, and return the prompt suffix string if a prompt is due.
///
/// Returns an empty string when no prompt is needed. When a prompt is returned,
/// also sets `rating_state` so the next tool call can capture the reply.
fn maybe_rating_prompt(
    state: &AppState,
    skill_id: &str,
    version: &str,
    rating_state: Option<&RatingState>,
) -> String {
    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let count = match increment_execution_count(&conn, skill_id, version) {
        Ok(c) => c,
        Err(e) => {
            debug!(error = %e, "failed to increment execution count");
            return String::new();
        }
    };

    if !should_prompt_for_rating(count) {
        return String::new();
    }

    // Skip if a rating already exists for this version.
    if has_existing_rating(&conn, skill_id, version).unwrap_or(false) {
        return String::new();
    }

    // Only prompt if we have session state to track the reply. Without it there
    // is no mechanism to capture a thumbs-up/down, so the prompt would be noise.
    let rs = match rating_state {
        Some(rs) => rs,
        None => return String::new(),
    };

    if let Ok(mut guard) = rs.lock() {
        *guard = Some((skill_id.to_string(), version.to_string()));
    }

    RATING_PROMPT.to_string()
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

// ── skillclub_author / skillclub_author_confirm handlers ─────────────────────

/// Derive a skill directory name and display name from a raw skill ID or name.
///
/// Converts spaces to hyphens and lowercases the result so the directory
/// matches the convention used by `vectorhawk skill init`.
fn normalize_skill_id(raw: &str) -> String {
    raw.to_lowercase()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join("-")
        .replace('_', "-")
}

/// Parameters for `scaffold_skill_md`. Groups the SKILL.md fields to keep the
/// function signature within clippy's argument-count limit.
struct SkillMdParams<'a> {
    skill_id: &'a str,
    system_prompt: &'a str,
    output_dir: &'a str,
    publisher_id: &'a str,
    triggers: &'a [String],
    network: &'a str,
    filesystem: &'a str,
    clipboard: &'a str,
    min_params_b: f32,
    recommended_models: &'a [String],
    fallback: &'a str,
    timeout_ms: u32,
    memory_mb: u32,
    sandbox: &'a str,
}

/// Indent each line of `text` by `spaces` spaces, returning the result with a trailing newline.
/// Used to embed system prompts as YAML block scalars.
fn indent_block(text: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    let mut out = String::new();
    for line in text.trim().lines() {
        out.push_str(&prefix);
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str(&prefix);
        out.push('\n');
    }
    out
}

/// Scaffold a SKILL.md in `output_dir/<skill_id>/` with the provided values.
///
/// Returns the path that was written on success.
fn scaffold_skill_md(params: SkillMdParams<'_>) -> Result<String> {
    let SkillMdParams {
        skill_id,
        system_prompt,
        output_dir,
        publisher_id,
        triggers,
        network,
        filesystem,
        clipboard,
        min_params_b,
        recommended_models,
        fallback,
        timeout_ms,
        memory_mb,
        sandbox,
    } = params;
    use std::fs;

    let base = camino::Utf8PathBuf::from(output_dir);
    let skill_dir = base.join(skill_id);

    if skill_dir.exists() {
        anyhow::bail!("directory '{}' already exists", skill_dir);
    }

    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create directory '{skill_dir}'"))?;

    // Produce the triggers YAML block nested under metadata.vectorhawk.
    let triggers_yaml = if triggers.is_empty() {
        String::new()
    } else {
        let items = triggers
            .iter()
            .map(|t| format!("      - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("    triggers:\n{items}\n")
    };

    // Produce the model recommended list (6-space indent: inside metadata.vectorhawk.model).
    let recommended_yaml = recommended_models
        .iter()
        .map(|m| format!("      - {m}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Use skill_id as the name (the caller normalizes it from the user input).
    let body_block = indent_block(system_prompt, 12);
    let skill_md = format!(
        "---\nname: {skill_id}\ndescription: \"TODO: describe what this skill does\"\nlicense: MIT\n\
         metadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: {publisher_id}\n\
         {triggers_yaml}\
         \n    permissions:\n      network: {network}\n      filesystem: {filesystem}\n      clipboard: {clipboard}\n\
         \n    execution:\n      timeout_ms: {timeout_ms}\n      memory_mb: {memory_mb}\n      sandbox: {sandbox}\n\
         \n    model:\n      min_params_b: {min_params_b}\n      recommended:\n{recommended_yaml}\n      fallback: {fallback}\n\
         \n    workflow:\n      - id: run\n        type: llm\n        prompt:\n          kind: inline\n          body: |\n\
         {body_block}\
         \n        inputs:\n          text: input.text\n\
         \n    schemas:\n      inputs:\n        type: object\n        properties:\n          text:\n            type: string\n\
         \n        required:\n          - text\n---\n"
    );

    let skill_md_path = skill_dir.join("SKILL.md");
    fs::write(&skill_md_path, &skill_md)
        .with_context(|| format!("failed to write {skill_md_path}"))?;

    Ok(skill_dir.to_string())
}

/// Derive a publisher slug from a display name: lowercase + hyphenate.
fn derive_publisher_slug(display_name: &str) -> String {
    let slug: String = display_name
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    slug.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Try to look up the logged-in user's publisher slug from auth state.
///
/// Returns `None` silently on any failure.
fn try_infer_publisher_id(state: &AppState, registry_url: Option<&str>) -> Option<String> {
    use vectorhawkd_core::auth::{load_tokens, AuthClient};

    let url = registry_url.unwrap_or("https://app.vectorhawk.ai");
    let tokens = load_tokens(state, url).ok().flatten()?;
    let client = AuthClient::new(url);
    let user_info = client.me(&tokens.access_token).ok()?;
    let slug = derive_publisher_slug(&user_info.display_name);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// Format milliseconds as a human-readable duration string.
fn format_duration_ms(ms: u32) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{} min", ms / 60_000)
    }
}

fn handle_author(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: Option<&str>,
) -> ToolCallResult {
    use vectorhawkd_core::recommend::recommend_from_prompt;

    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n,
        _ => return ToolCallResult::error_result("Missing required parameter: name"),
    };

    let system_prompt = match arguments.get("system_prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return ToolCallResult::error_result("Missing required parameter: system_prompt"),
    };

    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mode = arguments
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("interactive");

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let skill_id = normalize_skill_id(name);
    let publisher_id = try_infer_publisher_id(state, registry_url)
        .unwrap_or_else(|| "YOUR_PUBLISHER_ID".to_string());

    match mode {
        "skip_metadata" => {
            // Scaffold immediately with hardcoded defaults.
            let explicit_triggers: Vec<String> = arguments
                .get("triggers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            match scaffold_skill_md(SkillMdParams {
                skill_id: &skill_id,
                system_prompt,
                output_dir,
                publisher_id: &publisher_id,
                triggers: &explicit_triggers,
                network: "none",
                filesystem: "none",
                clipboard: "none",
                min_params_b: 1.0,
                recommended_models: &["gemma3:2b".to_string()],
                fallback: "error",
                timeout_ms: 30000,
                memory_mb: 256,
                sandbox: "strict",
            }) {
                Ok(path) => ToolCallResult::success(format!(
                    "Skill '{skill_id}' created at {path}/SKILL.md\n\
                     Next: vectorhawk skill validate {path}/"
                )),
                Err(e) => ToolCallResult::error_result(format!("Failed to scaffold skill: {e}")),
            }
        }

        "accept_suggestions" => {
            // Run recommendations and scaffold immediately.
            let rec = recommend_from_prompt(name, description, system_prompt);

            match scaffold_skill_md(SkillMdParams {
                skill_id: &skill_id,
                system_prompt,
                output_dir,
                publisher_id: &publisher_id,
                triggers: &rec.triggers,
                network: rec.permissions.network,
                filesystem: rec.permissions.filesystem,
                clipboard: rec.permissions.clipboard,
                min_params_b: rec.model.min_params_b,
                recommended_models: &rec.model.recommended,
                fallback: rec.model.fallback,
                timeout_ms: rec.execution.timeout_ms,
                memory_mb: rec.execution.memory_mb,
                sandbox: rec.execution.sandbox,
            }) {
                Ok(path) => {
                    let confidence = format!("{:?}", rec.confidence).to_lowercase();
                    let model_primary = rec
                        .model
                        .recommended
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("gemma3:2b");
                    let fallback_note = if rec.model.fallback == "mcp_sampling" {
                        "falls back to AI client"
                    } else {
                        "no fallback"
                    };
                    let publisher_note = if publisher_id == "YOUR_PUBLISHER_ID" {
                        String::new()
                    } else {
                        format!("\n- Publisher: {publisher_id}")
                    };
                    ToolCallResult::success(format!(
                        "Skill '{skill_id}' created at {path}/SKILL.md\n\
                         Applied recommendations (confidence: {confidence}):\n\
                         - Network: {}\n\
                         - Filesystem: {}\n\
                         - Offline model: {model_primary} ({fallback_note})\n\
                         - Timeout: {}\n\
                         - Sandbox: {}{publisher_note}\n\
                         Next: vectorhawk skill validate {path}/",
                        rec.permissions.network,
                        rec.permissions.filesystem,
                        format_duration_ms(rec.execution.timeout_ms),
                        rec.execution.sandbox,
                    ))
                }
                Err(e) => ToolCallResult::error_result(format!("Failed to scaffold skill: {e}")),
            }
        }

        // "interactive" is the default — return structured recommendations without scaffolding.
        _ => {
            let rec = recommend_from_prompt(name, description, system_prompt);
            let confidence_str = format!("{:?}", rec.confidence).to_lowercase();

            let recommended_models: Vec<serde_json::Value> = rec
                .model
                .recommended
                .iter()
                .map(|m| serde_json::json!(m))
                .collect();

            let triggers_json: Vec<serde_json::Value> =
                rec.triggers.iter().map(|t| serde_json::json!(t)).collect();

            let model_primary = rec
                .model
                .recommended
                .first()
                .map(|s| s.as_str())
                .unwrap_or("gemma3:2b");
            let fallback_note = if rec.model.fallback == "mcp_sampling" {
                "falls back to your AI client if unavailable"
            } else {
                "returns an error if unavailable"
            };

            let payload = serde_json::json!({
                "status": "recommendations_ready",
                "skill_id": skill_id,
                "publisher_id": publisher_id,
                "summary": {
                    "network": rec.permissions.network,
                    "filesystem": rec.permissions.filesystem,
                    "offline_model": format!("{model_primary} (≥{}B params) — {fallback_note}", rec.model.min_params_b),
                    "timeout": format_duration_ms(rec.execution.timeout_ms),
                    "sandbox": rec.execution.sandbox,
                    "triggers": triggers_json,
                },
                "raw": {
                    "triggers": triggers_json,
                    "permissions": {
                        "network": rec.permissions.network,
                        "filesystem": rec.permissions.filesystem,
                        "clipboard": rec.permissions.clipboard
                    },
                    "model": {
                        "min_params_b": rec.model.min_params_b,
                        "recommended": recommended_models,
                        "fallback": rec.model.fallback
                    },
                    "execution": {
                        "timeout_ms": rec.execution.timeout_ms,
                        "memory_mb": rec.execution.memory_mb,
                        "sandbox": rec.execution.sandbox
                    }
                },
                "confidence": confidence_str,
                "message": "Recommendations ready. Review the summary above and call vectorhawk_author_confirm with your final values, or pass mode: accept_suggestions to apply these directly."
            });

            match serde_json::to_string_pretty(&payload) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error_result(format!("Failed to serialize: {e}")),
            }
        }
    }
}

fn handle_author_confirm(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: Option<&str>,
) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return ToolCallResult::error_result("Missing required parameter: skill_id"),
    };

    let system_prompt = match arguments.get("system_prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return ToolCallResult::error_result("Missing required parameter: system_prompt"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    // Extract triggers.
    let triggers: Vec<String> = arguments
        .get("triggers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Extract permissions with defaults.
    let perms = arguments.get("permissions");
    let network = perms
        .and_then(|p| p.get("network"))
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let filesystem = perms
        .and_then(|p| p.get("filesystem"))
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let clipboard = perms
        .and_then(|p| p.get("clipboard"))
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();

    // Extract model with defaults.
    let model = arguments.get("model");
    let min_params_b = model
        .and_then(|m| m.get("min_params_b"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0) as f32;
    let recommended_models: Vec<String> = model
        .and_then(|m| m.get("recommended"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(|| vec!["gemma3:2b".to_string()]);
    let fallback = model
        .and_then(|m| m.get("fallback"))
        .and_then(|v| v.as_str())
        .unwrap_or("error")
        .to_string();

    // Extract execution with defaults.
    let exec = arguments.get("execution");
    let timeout_ms = exec
        .and_then(|e| e.get("timeout_ms"))
        .and_then(|v| v.as_u64())
        .unwrap_or(30000) as u32;
    let memory_mb = exec
        .and_then(|e| e.get("memory_mb"))
        .and_then(|v| v.as_u64())
        .unwrap_or(256) as u32;
    let sandbox = exec
        .and_then(|e| e.get("sandbox"))
        .and_then(|v| v.as_str())
        .unwrap_or("strict")
        .to_string();

    let normalized_id = normalize_skill_id(skill_id);

    // Publisher ID: use explicit arg if provided, otherwise infer from auth.
    let publisher_id = arguments
        .get("publisher_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| try_infer_publisher_id(state, registry_url))
        .unwrap_or_else(|| "YOUR_PUBLISHER_ID".to_string());

    match scaffold_skill_md(SkillMdParams {
        skill_id: &normalized_id,
        system_prompt,
        output_dir,
        publisher_id: &publisher_id,
        triggers: &triggers,
        network: &network,
        filesystem: &filesystem,
        clipboard: &clipboard,
        min_params_b,
        recommended_models: &recommended_models,
        fallback: &fallback,
        timeout_ms,
        memory_mb,
        sandbox: &sandbox,
    }) {
        Ok(path) => ToolCallResult::success(format!(
            "Skill '{normalized_id}' created at {path}/SKILL.md\n\
             Next: vectorhawk skill validate {path}/"
        )),
        Err(e) => ToolCallResult::error_result(format!("Failed to scaffold skill: {e}")),
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
            "---\nname: Test Skill\ndescription: A test skill for MCP testing\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: vectorhawk\n    permissions:\n      network: none\n      filesystem: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n    schemas:\n      inputs:\n        type: object\n        properties:\n          query:\n            type: string\n        required:\n          - query\n      outputs:\n        type: object\n    workflow_ref: workflow.yaml\n---\n\nDo the thing.\n",
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

        let tools = build_tool_list_with_builtins(&state, &Some(url));
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

        let tools = build_tool_list_with_builtins(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"vectorhawk_list"));
        assert!(names.contains(&"vectorhawk_validate"));
        assert!(names.contains(&"vectorhawk_install"));
        assert!(!names.contains(&"vectorhawk_search"));
        assert!(!names.contains(&"vectorhawk_logout"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_search_available_without_login() {
        let state_root = temp_root("tool-list-search-no-login");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        // No fake_login — user is not authenticated

        let tools = build_tool_list_with_builtins(&state, &Some(url));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            names.contains(&"vectorhawk_search"),
            "search should appear without login"
        );
        assert!(
            names.contains(&"vectorhawk_login"),
            "login should appear when not logged in"
        );
        assert!(
            !names.contains(&"vectorhawk_logout"),
            "logout should not appear when not logged in"
        );

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

        let tools = build_tool_list_with_builtins(&state, &None);
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
    fn handle_install_from_registry_downloads_and_installs() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use mockito::Server;
        use sha2::{Digest, Sha256};

        let state_root = temp_root("handle-install-registry-ok");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Build a minimal skill bundle in memory as a tar.gz.
        let bundle_dir = temp_root("handle-install-registry-bundle");
        write_test_skill(&bundle_dir);
        let mut archive_bytes = Vec::new();
        {
            let enc = GzEncoder::new(&mut archive_bytes, Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.append_dir_all(".", bundle_dir.as_std_path()).unwrap();
            let gz = tar.into_inner().unwrap();
            gz.finish().unwrap();
        }
        let sha256 = hex::encode(Sha256::digest(&archive_bytes));

        let mut server = Server::new();
        let url = server.url();
        let download_path = "/download/test-skill-0.1.0.cskill";

        let _detail_mock = server
            .mock("GET", "/portal/skills/test-skill")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"skill_id":"test-skill","name":"Test Skill","latest_version":"0.1.0","publisher_name":"vectorhawk","description":"A test skill."}"#)
            .create();

        let _meta_mock = server
            .mock("GET", "/skills/test-skill/versions/0.1.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"skill_id":"test-skill","version":"0.1.0","download_url":"{url}{download_path}","sha256":"{sha256}","size_bytes":{}}}"#,
                archive_bytes.len()
            ))
            .create();

        let _download_mock = server
            .mock("GET", download_path)
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(archive_bytes)
            .create();

        let result = handle_install(
            &serde_json::json!({"skill_id": "test-skill"}),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success: {:?}",
            result.content
        );
        assert!(result.content[0].text.contains("test-skill"));
        assert!(result.content[0].text.contains("0.1.0"));

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&bundle_dir);
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
    fn handle_login_already_authenticated_returns_success_without_new_flow() {
        let state_root = temp_root("handle-login-already-authed");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let result = handle_login(&serde_json::json!({"registry_url": url}), &state, &None);
        // Should succeed without starting an OAuth flow
        assert_eq!(result.is_error, None);
        assert!(result.content[0].text.contains("Already logged in"));

        let _ = fs::remove_dir_all(&state_root);
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

    // ── parse_import_action unit tests ────────────────────────────────────────

    #[test]
    fn parse_import_action_accepts_preview() {
        let args = serde_json::json!({"action": "preview"});
        assert_eq!(parse_import_action(&args).unwrap(), ImportAction::Preview);
    }

    #[test]
    fn parse_import_action_accepts_submit() {
        let args = serde_json::json!({"action": "submit"});
        assert_eq!(parse_import_action(&args).unwrap(), ImportAction::Submit);
    }

    #[test]
    fn parse_import_action_rejects_unknown_value() {
        let args = serde_json::json!({"action": "confirm"});
        let err = parse_import_action(&args).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        assert!(err.content[0].text.contains("Invalid action"));
        assert!(err.content[0].text.contains("preview"));
        assert!(err.content[0].text.contains("submit"));
    }

    #[test]
    fn parse_import_action_errors_when_missing() {
        let args = serde_json::json!({"input": "some-package"});
        let err = parse_import_action(&args).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        assert!(err.content[0].text.contains("action"));
    }

    // ── handle_import integration tests ───────────────────────────────────────

    #[test]
    fn handle_import_requires_input() {
        let state_root = temp_root("import-no-input");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_import(
            &serde_json::json!({"action": "preview"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("input"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_requires_action() {
        let state_root = temp_root("import-no-action");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        fake_login(&state, "http://localhost:8000");
        let result = handle_import(
            &serde_json::json!({"input": "some-package"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("action"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_no_registry() {
        let state_root = temp_root("import-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let result = handle_import(
            &serde_json::json!({"input": "some-package", "action": "preview"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_preview_requires_auth() {
        let state_root = temp_root("import-preview-no-auth");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        // No fake_login — user is not authenticated
        let result = handle_import(
            &serde_json::json!({"input": "@modelcontextprotocol/server-slack", "action": "preview"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0].text.contains("login") || result.content[0].text.contains("auth"),
            "expected auth error, got: {}",
            result.content[0].text
        );
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_submit_requires_auth() {
        let state_root = temp_root("import-submit-no-auth");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        // No fake_login — user is not authenticated
        let result = handle_import(
            &serde_json::json!({"input": "@modelcontextprotocol/server-slack", "action": "submit"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0].text.contains("login") || result.content[0].text.contains("auth"),
            "expected auth error, got: {}",
            result.content[0].text
        );
        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_preview_skill_formats_output() {
        use mockito::Server;

        let state_root = temp_root("import-preview-skill-ok");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = Server::new();
        let url = server.url();
        fake_login(&state, &url);

        let _mock = server
            .mock("POST", "/portal/import/preview")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "skill",
                    "skill_id": "contract-compare",
                    "name": "Contract Compare",
                    "version": "0.3.0",
                    "publisher": "acme-corp",
                    "description": "Compare two contracts side by side.",
                    "license": "MIT"
                }"#,
            )
            .create();

        let result = handle_import(
            &serde_json::json!({
                "input": "https://github.com/acme/contract-compare",
                "action": "preview"
            }),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("Contract Compare"), "got: {text}");
        assert!(text.contains("0.3.0"), "got: {text}");
        assert!(text.contains("acme-corp"), "got: {text}");
        assert!(
            text.contains("submit"),
            "preview should suggest submit step, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_preview_mcp_server_formats_output() {
        use mockito::Server;

        let state_root = temp_root("import-preview-mcp-ok");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = Server::new();
        let url = server.url();
        fake_login(&state, &url);

        let _mock = server
            .mock("POST", "/portal/import/preview")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "mcp_server",
                    "package_name": "@modelcontextprotocol/server-slack",
                    "description": "Slack integration for MCP.",
                    "latest_version": "1.2.0",
                    "license": "Apache-2.0",
                    "keywords": ["slack", "messaging"],
                    "already_in_catalog": false,
                    "approval_mode": "strict"
                }"#,
            )
            .create();

        let result = handle_import(
            &serde_json::json!({
                "input": "@modelcontextprotocol/server-slack",
                "action": "preview"
            }),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(
            text.contains("@modelcontextprotocol/server-slack"),
            "got: {text}"
        );
        assert!(text.contains("strict"), "got: {text}");
        assert!(
            text.contains("submit"),
            "preview should suggest submit step, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_submit_mcp_server_approved() {
        use mockito::Server;

        let state_root = temp_root("import-submit-mcp-approved");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = Server::new();
        let url = server.url();
        fake_login(&state, &url);

        let _mock = server
            .mock("POST", "/portal/import/submit")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "mcp_server",
                    "request_id": "req-abc-123",
                    "server_name": "Slack MCP",
                    "package_source": "@modelcontextprotocol/server-slack",
                    "status": "approved",
                    "approval_mode": "trust"
                }"#,
            )
            .create();

        let result = handle_import(
            &serde_json::json!({
                "input": "@modelcontextprotocol/server-slack",
                "action": "submit"
            }),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("Slack MCP"), "got: {text}");
        assert!(text.contains("approved"), "got: {text}");
        assert!(
            text.contains("catalog"),
            "approved result should mention catalog, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_submit_mcp_server_pending() {
        use mockito::Server;

        let state_root = temp_root("import-submit-mcp-pending");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = Server::new();
        let url = server.url();
        fake_login(&state, &url);

        let _mock = server
            .mock("POST", "/portal/import/submit")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "mcp_server",
                    "request_id": "req-xyz-789",
                    "server_name": "Custom MCP",
                    "package_source": "my-org/custom-mcp",
                    "status": "pending",
                    "approval_mode": "strict"
                }"#,
            )
            .create();

        let result = handle_import(
            &serde_json::json!({
                "input": "my-org/custom-mcp",
                "action": "submit"
            }),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("pending"), "got: {text}");
        assert!(
            text.contains("review"),
            "pending result should mention review, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_import_submit_skill_ok() {
        use mockito::Server;

        let state_root = temp_root("import-submit-skill-ok");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = Server::new();
        let url = server.url();
        fake_login(&state, &url);

        let _mock = server
            .mock("POST", "/portal/import/submit")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "skill",
                    "skill_id": "contract-compare",
                    "version": "0.3.0",
                    "review_status": "approved",
                    "is_published": true,
                    "source_url": "https://github.com/acme/contract-compare"
                }"#,
            )
            .create();

        let result = handle_import(
            &serde_json::json!({
                "input": "https://github.com/acme/contract-compare",
                "action": "submit"
            }),
            &state,
            &Some(url),
        );

        assert_eq!(
            result.is_error, None,
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("contract-compare"), "got: {text}");
        assert!(text.contains("0.3.0"), "got: {text}");

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
            None,
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
        let result =
            handle_plugin_export(&serde_json::json!({"path": "/some/plugin", "format": "tarball"}));
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
        let result = handle_plugin_import(&serde_json::json!({"path": root.to_string()}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0]
            .text
            .contains("Could not detect plugin format"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_tool_list_includes_plugin_tools() {
        let state_root = temp_root("tool-list-plugin");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let tools = build_tool_list_with_builtins(&state, &None);
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

    // ── RAT1-B: rating prompt and capture ─────────────────────────────────────

    fn make_rating_state() -> RatingState {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn extract_rating_reply_detects_thumbs_up() {
        let args = serde_json::json!({"message": "thumbs up"});
        assert_eq!(extract_rating_reply(&args), Some("up"));
    }

    #[test]
    fn extract_rating_reply_detects_thumbs_down_case_insensitive() {
        let args = serde_json::json!({"message": "THUMBS DOWN please"});
        assert_eq!(extract_rating_reply(&args), Some("down"));
    }

    #[test]
    fn extract_rating_reply_checks_rating_reply_key_first() {
        let args = serde_json::json!({"_rating_reply": "thumbs up", "other": "thumbs down"});
        assert_eq!(extract_rating_reply(&args), Some("up"));
    }

    #[test]
    fn extract_rating_reply_returns_none_for_unrelated_args() {
        let args = serde_json::json!({"query": "summarize this document"});
        assert_eq!(extract_rating_reply(&args), None);
    }

    #[test]
    fn should_prompt_for_rating_triggers_at_3_then_every_5() {
        use vectorhawkd_core::ratings::should_prompt_for_rating;
        assert!(!should_prompt_for_rating(1));
        assert!(!should_prompt_for_rating(2));
        assert!(should_prompt_for_rating(3));
        assert!(!should_prompt_for_rating(4));
        assert!(!should_prompt_for_rating(7));
        assert!(should_prompt_for_rating(8));
        assert!(should_prompt_for_rating(13));
        assert!(should_prompt_for_rating(18));
    }

    #[test]
    fn rating_prompt_appended_on_3rd_successful_execution() {
        use rusqlite::Connection;
        use vectorhawkd_core::ratings::increment_execution_count;

        let state_root = temp_root("rat1b-prompt");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let rs = make_rating_state();

        // Pre-seed count to 2 so the next call makes it 3.
        let conn = Connection::open(&state.db_path).unwrap();
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        drop(conn);

        let suffix = maybe_rating_prompt(&state, "my-skill", "1.0.0", Some(&rs));
        assert!(
            suffix.contains("thumbs up") || suffix.contains("thumbs down"),
            "prompt suffix should contain rating text: {suffix:?}"
        );

        // Pending state should now be set.
        let guard = rs.lock().unwrap();
        assert_eq!(
            guard.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            Some(("my-skill", "1.0.0"))
        );
        drop(guard);

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn rating_not_appended_when_no_rating_state() {
        use rusqlite::Connection;
        use vectorhawkd_core::ratings::increment_execution_count;

        let state_root = temp_root("rat1b-no-rs");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let conn = Connection::open(&state.db_path).unwrap();
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        drop(conn);

        let suffix = maybe_rating_prompt(&state, "my-skill", "1.0.0", None);
        assert!(suffix.is_empty(), "no rating state → no prompt suffix");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn rating_reply_captured_and_state_cleared() {
        use rusqlite::Connection;
        use vectorhawkd_core::ratings::get_unsynced_ratings;

        let state_root = temp_root("rat1b-capture");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let rs = make_rating_state();

        // Set a pending rating prompt manually.
        {
            let mut guard = rs.lock().unwrap();
            *guard = Some(("cool-skill".to_string(), "2.0.0".to_string()));
        }

        let policy = vectorhawkd_core::policy::MockPolicyClient::new();
        let result = handle_tool_call(
            "vectorhawk_list",
            &serde_json::json!({"_rating_reply": "thumbs up"}),
            &state,
            &policy,
            None,
            &None,
            &empty_cache(),
            None,
            Some(&rs),
        );

        assert!(result.is_error.is_none() || result.is_error == Some(false));
        assert!(result.content[0].text.contains("thumbs up"));

        // Pending state should be cleared.
        let guard = rs.lock().unwrap();
        assert!(
            guard.is_none(),
            "pending rating should be cleared after capture"
        );
        drop(guard);

        // Rating should be recorded in SQLite.
        let conn = Connection::open(&state.db_path).unwrap();
        let ratings = get_unsynced_ratings(&conn).unwrap();
        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings[0].skill_id, "cool-skill");
        assert_eq!(ratings[0].version, "2.0.0");
        assert_eq!(ratings[0].rating, "up");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn rating_prompt_skipped_when_already_rated() {
        use rusqlite::Connection;
        use vectorhawkd_core::ratings::{increment_execution_count, record_rating};

        let state_root = temp_root("rat1b-skip");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let rs = make_rating_state();

        let conn = Connection::open(&state.db_path).unwrap();
        // Seed count to 2 and record an existing rating.
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        increment_execution_count(&conn, "my-skill", "1.0.0").unwrap();
        record_rating(&conn, "my-skill", "1.0.0", "up").unwrap();
        drop(conn);

        let suffix = maybe_rating_prompt(&state, "my-skill", "1.0.0", Some(&rs));
        assert!(
            suffix.is_empty(),
            "prompt should be skipped when already rated"
        );

        let guard = rs.lock().unwrap();
        assert!(
            guard.is_none(),
            "pending state should not be set when skipped"
        );

        let _ = fs::remove_dir_all(&state_root);
    }
}
