//! `search_skills`, `load_skill` and `install_skill`: the runner forwards these
//! three tools to the backend's remote skills MCP (`POST {registry}/mcp`).
//!
//! The backend owns the schemas, the visibility rules and the install queueing
//! (so a runner on a laptop and claude.ai see the same catalog). The runner only
//! adds its portal bearer and `X-Device-ID`, and returns the backend's
//! `content` / `isError` verbatim. Exposed whenever the user is signed in,
//! regardless of `SCENERY_EXPOSE_BUILTIN_TOOLS`.

use crate::protocol::{ToolCallResult, ToolDefinition, ToolResultContent};
use scenery_core::{
    auth::{self, AuthClient},
    mcp_governance,
    state::AppState,
};
use tracing::debug;

pub const TOOL_NAMES: [&str; 3] = ["search_skills", "load_skill", "install_skill"];

pub fn is_remote_skills_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// Tool definitions. Schemas match the backend's `/mcp` `tools/list`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "search_skills".to_string(),
            description: "Search the skills your organization has approved for you. With no \
                query, lists everything available. Returns each skill's id, name, version, \
                description, whether it can be loaded into this conversation, and whether it \
                is already installed on this device when known."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional search text; empty lists all."
                    }
                }
            }),
        },
        ToolDefinition {
            name: "load_skill".to_string(),
            description: "Load a skill's instructions (SKILL.md) into this conversation so you \
                can follow them now. Does not install anything. Some skills cannot be loaded; \
                install them instead."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": {
                        "type": "string",
                        "description": "The skill_id from search_skills."
                    }
                },
                "required": ["skill_id"]
            }),
        },
        ToolDefinition {
            name: "install_skill".to_string(),
            description: "Install one or more skills on the user's device so they are \
                available in future conversations. Ask the user before calling. If the user \
                has several devices the result lists them: ask which one, then call again \
                with device_id."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1
                    },
                    "device_id": {
                        "type": "string",
                        "description": "Target device id, when asked for."
                    }
                },
                "required": ["skill_ids"]
            }),
        },
    ]
}

/// Forward one of the three tools to the backend and shape its reply.
pub fn handle(
    name: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let Some(url) = registry_url else {
        return ToolCallResult::error_result("No registry URL configured");
    };
    let tokens = auth::load_tokens(state, url)
        .ok()
        .flatten()
        .filter(|t| !t.access_token.is_empty());
    let device_id = state.get_sync_state("device_id").ok().flatten();
    let call = |token: Option<&str>| {
        mcp_governance::call_remote_skills_tool(url, token, device_id.as_deref(), name, arguments)
    };

    let mut outcome = call(tokens.as_ref().map(|t| t.access_token.as_str()));
    if let (Err(e), Some(t)) = (&outcome, &tokens) {
        if e.to_string() == mcp_governance::SKILLS_SIGN_IN_MESSAGE {
            // The access token may just have expired: refresh once and retry.
            if let Some(fresh) = refresh(state, url, &t.refresh_token) {
                outcome = call(Some(&fresh));
            }
        }
    }
    match outcome {
        Ok(result) => to_tool_result(&result),
        Err(e) => ToolCallResult::error_result(e.to_string()),
    }
}

fn refresh(state: &AppState, url: &str, refresh_token: &str) -> Option<String> {
    debug!("skills MCP: access token rejected, attempting refresh");
    match AuthClient::new(url).refresh(refresh_token) {
        Ok(new) => {
            if let Err(e) = auth::save_tokens(state, url, &new.access_token, &new.refresh_token) {
                debug!("failed to save refreshed tokens: {e}");
            }
            Some(new.access_token)
        }
        Err(_) => {
            let _ = auth::clear_tokens(state, url);
            None
        }
    }
}

/// The backend's MCP result object, verbatim: its text `content` items and
/// `isError`.
fn to_tool_result(result: &serde_json::Value) -> ToolCallResult {
    let content: Vec<ToolResultContent> = result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .map(|text| ToolResultContent {
                    content_type: "text".to_string(),
                    text: text.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if content.is_empty() {
        return ToolCallResult::error_result("The skills service returned an empty result.");
    }
    let is_error = result
        .get("isError")
        .and_then(|e| e.as_bool())
        .filter(|e| *e);
    ToolCallResult { content, is_error }
}
