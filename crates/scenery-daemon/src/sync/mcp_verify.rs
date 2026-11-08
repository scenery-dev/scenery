//! Verification of a backend-pushed MCP server definition (`mcp_server.v1`).
//!
//! The runner executes only what the verified payload says. The unsigned
//! fields next to the envelope are used only by debug builds with
//! `SCENERY_INSECURE_UNSIGNED=1` and no envelope.

use scenery_core::registry_url;
use scenery_core::signed::{self, Envelope};
use scenery_core::state::AppState;
use scenery_core::{trust, verifier};
use uuid::Uuid;

/// The definition fields the runner acts on.
#[derive(Debug, Clone)]
pub(crate) struct McpDefinition {
    pub mcp_server_name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    pub server_config: Option<serde_json::Value>,
    pub auth_type: String,
    pub gateway_server_id: Option<String>,
    pub gateway_url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct McpPayload {
    mcp_server_id: String,
    name: String,
    package_source: String,
    #[serde(default)]
    version_pin: Option<String>,
    #[serde(default)]
    server_config: Option<serde_json::Value>,
    auth_type: String,
    #[serde(default)]
    gateway_server_id: Option<String>,
    #[serde(default)]
    gateway_url: Option<String>,
}

/// A verified definition plus the envelope JSON to store with the row.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedMcp {
    pub def: McpDefinition,
    pub envelope_json: Option<String>,
}

/// L4: a gateway URL receives the user's portal JWT, so it must live on the
/// registry's own origin.
fn check_gateway_origin(def: &McpDefinition, registry_url: &str) -> Result<(), String> {
    match def.gateway_url.as_deref().filter(|u| !u.is_empty()) {
        Some(u) if !registry_url::same_origin(u, registry_url) => {
            Err("rejected: gateway_url is not on the registry origin".to_string())
        }
        _ => Ok(()),
    }
}

/// Verify `signed` for `mcp_server_id` and return the definition to act on.
/// The `Err` string is user-facing (it is PATCHed as the error message).
pub(crate) fn verify_mcp_definition(
    state: &AppState,
    registry_url: &str,
    mcp_server_id: &Uuid,
    unsigned: McpDefinition,
    signed_env: Option<&Envelope>,
) -> Result<VerifiedMcp, String> {
    let Some(env) = signed_env else {
        if trust::insecure_unsigned() {
            check_gateway_origin(&unsigned, registry_url)?;
            return Ok(VerifiedMcp {
                def: unsigned,
                envelope_json: None,
            });
        }
        return Err("rejected: definition is not signed".to_string());
    };
    let sub = format!("mcp_server:{mcp_server_id}");
    let v = verifier::verify_for_device(state, signed::TYP_MCP_SERVER, env, Some(&sub))
        .map_err(|e| format!("rejected: {e}"))?;
    if v.status != signed::STATUS_ACTIVE {
        return Err(format!("rejected: statement status is '{}'", v.status));
    }
    let p: McpPayload = v
        .payload_json()
        .map_err(|e| format!("rejected: {e}"))?;
    if p.mcp_server_id != mcp_server_id.to_string() {
        return Err("rejected: payload is for a different server".to_string());
    }
    let def = McpDefinition {
        mcp_server_name: p.name,
        package_source: p.package_source,
        version_pin: p.version_pin,
        server_config: p.server_config,
        auth_type: p.auth_type,
        gateway_server_id: p.gateway_server_id,
        gateway_url: p.gateway_url,
    };
    check_gateway_origin(&def, registry_url)?;
    Ok(VerifiedMcp {
        def,
        envelope_json: serde_json::to_string(env).ok(),
    })
}
