//! AI client detection and `mcp setup` config writing.
//!
//! # M0 scope
//!
//! Validates that the wire format for the Claude Code config entry is:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "vectorhawk": {
//!       "command": "vectorhawk",
//!       "args": ["mcp", "serve"]
//!     }
//!   }
//! }
//! ```
//!
//! This is the same shape as the skillrunner entry (was `skillrunner` → now
//! `vectorhawk`). `mcp setup` in M1 will write this to real config files.
//! For M0 we expose `build_claude_code_entry()` so the CLI can print/verify
//! the entry without writing anything.
//!
//! Cursor, Windsurf, VS Code, and Gemini CLI detection is scaffolded but
//! deferred to M1.

use anyhow::Result;
use std::path::PathBuf;

/// Configuration for a detected AI client that supports MCP.
#[derive(Debug)]
pub struct ClientConfig {
    pub name: String,
    pub config_path: PathBuf,
    /// Top-level JSON key in the client's config file that holds the MCP
    /// server map (e.g. `"mcpServers"` for Claude Code).
    pub mcp_key: String,
    pub already_configured: bool,
}

/// The name under which VectorHawk registers itself in AI client configs.
///
/// Must match what `vectorhawkd-shim` advertises and what `mcp setup` writes.
/// Changing this is a breaking change for all AI clients already configured.
pub const MCP_SERVER_NAME: &str = "vectorhawk";

/// The command the AI client runs to start the shim.
pub const MCP_COMMAND: &str = "vectorhawk";

/// Arguments passed to the shim command.
pub const MCP_ARGS: &[&str] = &["mcp", "serve"];

/// Build the JSON value for a single AI client's MCP server config entry.
///
/// For Claude Code this goes under `mcpServers.vectorhawk` in `~/.claude.json`.
/// The shape is identical to what skillrunner used — only the server name and
/// command change from `skillrunner` to `vectorhawk`.
///
/// # Example output
///
/// ```json
/// {
///   "command": "vectorhawk",
///   "args": ["mcp", "serve"]
/// }
/// ```
pub fn build_mcp_entry() -> serde_json::Value {
    serde_json::json!({
        "command": MCP_COMMAND,
        "args": MCP_ARGS,
    })
}

/// Build the full `mcpServers` block suitable for merging into a client config.
///
/// ```json
/// {
///   "mcpServers": {
///     "vectorhawk": {
///       "command": "vectorhawk",
///       "args": ["mcp", "serve"]
///     }
///   }
/// }
/// ```
pub fn build_mcp_servers_block() -> serde_json::Value {
    serde_json::json!({
        MCP_SERVER_NAME: build_mcp_entry()
    })
}

/// Detect Claude Code installation and return config info.
///
/// Claude Code is identified by the presence of `~/.claude` or `~/.claude.json`.
/// Returns `None` if neither exists.
pub fn detect_claude_code() -> Option<ClientConfig> {
    let home = dirs::home_dir()?;
    let claude_config = home.join(".claude.json");
    let claude_dir = home.join(".claude");
    if !claude_dir.exists() && !claude_config.exists() {
        return None;
    }
    let already = is_vectorhawk_configured(&claude_config, "mcpServers");
    Some(ClientConfig {
        name: "Claude Code".to_string(),
        config_path: claude_config,
        mcp_key: "mcpServers".to_string(),
        already_configured: already,
    })
}

/// Detect all supported AI clients.
///
/// M0: returns at most one entry (Claude Code). Other clients are deferred to M1.
pub fn detect_ai_clients() -> Vec<ClientConfig> {
    let mut clients = Vec::new();
    if let Some(c) = detect_claude_code() {
        clients.push(c);
    }
    // TODO(M1): Claude Desktop, Cursor, Windsurf, VS Code, Gemini CLI
    clients
}

/// Write the VectorHawk MCP entry into a client config file.
///
/// Reads the existing JSON (if any), merges the entry under `mcp_key`, and
/// writes back. Creates the file if it does not exist.
pub fn write_mcp_entry(config: &ClientConfig) -> Result<()> {
    let existing: serde_json::Value = if config.config_path.exists() {
        let text = std::fs::read_to_string(&config.config_path)?;
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let mut obj = match existing {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };

    // Merge under the mcp_key (e.g. "mcpServers")
    let servers = obj
        .entry(config.mcp_key.clone())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let serde_json::Value::Object(ref mut map) = servers {
        map.insert(MCP_SERVER_NAME.to_string(), build_mcp_entry());
    }

    let output = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;

    if let Some(parent) = config.config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config.config_path, output)?;
    Ok(())
}

/// Returns `true` if the config file at `path` already contains a
/// `vectorhawk` entry under `mcp_key`.
fn is_vectorhawk_configured(path: &std::path::Path, mcp_key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get(mcp_key)
        .and_then(|v| v.get(MCP_SERVER_NAME))
        .is_some()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_entry_has_correct_command_and_args() {
        let entry = build_mcp_entry();
        assert_eq!(entry["command"], "vectorhawk");
        let args = entry["args"].as_array().unwrap();
        assert_eq!(args[0], "mcp");
        assert_eq!(args[1], "serve");
    }

    #[test]
    fn mcp_servers_block_nests_correctly() {
        let block = build_mcp_servers_block();
        let entry = &block[MCP_SERVER_NAME];
        assert_eq!(entry["command"], "vectorhawk");
    }

    #[test]
    fn write_mcp_entry_round_trips() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("vh-setup-test-{nanos}.json"));

        let config = ClientConfig {
            name: "Test Client".to_string(),
            config_path: tmp.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).expect("write should succeed");

        let text = std::fs::read_to_string(&tmp).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");
        assert_eq!(json["mcpServers"]["vectorhawk"]["args"][0], "mcp");
        assert_eq!(json["mcpServers"]["vectorhawk"]["args"][1], "serve");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn write_mcp_entry_merges_with_existing() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("vh-setup-merge-{nanos}.json"));

        // Pre-existing config with another server
        let existing = serde_json::json!({
            "mcpServers": {
                "other-tool": {"command": "other", "args": []}
            }
        });
        std::fs::write(&tmp, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let config = ClientConfig {
            name: "Test Client".to_string(),
            config_path: tmp.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).expect("write should succeed");

        let text = std::fs::read_to_string(&tmp).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        // Both servers should exist
        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");
        assert_eq!(json["mcpServers"]["other-tool"]["command"], "other");

        let _ = std::fs::remove_file(&tmp);
    }
}
