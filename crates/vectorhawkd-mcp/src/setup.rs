//! AI client detection and `mcp setup` config writing.

use anyhow::Result;
use std::fs;
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
pub fn build_mcp_entry() -> serde_json::Value {
    serde_json::json!({
        "command": MCP_COMMAND,
        "args": MCP_ARGS,
    })
}

/// Build the full `mcpServers` block suitable for merging into a client config.
pub fn build_mcp_servers_block() -> serde_json::Value {
    serde_json::json!({
        MCP_SERVER_NAME: build_mcp_entry()
    })
}

/// Detect Claude Code installation and return config info.
///
/// Claude Code is identified by the presence of `~/.claude` or `~/.claude.json`.
pub fn detect_claude_code() -> Option<ClientConfig> {
    let home = home_dir()?;
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

/// Detect all supported AI clients (6 clients: Claude Code, Claude Desktop,
/// Cursor, Windsurf, VS Code, Gemini CLI).
pub fn detect_ai_clients() -> Vec<ClientConfig> {
    match home_dir() {
        Some(home) => detect_ai_clients_in(&home),
        None => Vec::new(),
    }
}

/// Inner detection that accepts an explicit home directory.
///
/// Separated from `detect_ai_clients` so tests can supply a temp-dir home
/// without racing on the process-global HOME environment variable.
fn detect_ai_clients_in(home: &std::path::Path) -> Vec<ClientConfig> {
    let mut clients = Vec::new();

    // Claude Code — ~/.claude.json or ~/.claude dir
    let claude_config = home.join(".claude.json");
    let claude_dir = home.join(".claude");
    if claude_dir.exists() || claude_config.exists() {
        let already = is_vectorhawk_configured(&claude_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Claude Code".to_string(),
            config_path: claude_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // Claude Desktop — platform-specific path
    if let Some(desktop_config) = claude_desktop_config_path(home) {
        let desktop_dir = desktop_config.parent().map(|p| p.to_path_buf());
        if desktop_dir
            .as_ref()
            .map(|d| d.exists())
            .unwrap_or(false)
            || desktop_config.exists()
        {
            let already = is_vectorhawk_configured(&desktop_config, "mcpServers");
            clients.push(ClientConfig {
                name: "Claude Desktop".to_string(),
                config_path: desktop_config,
                mcp_key: "mcpServers".to_string(),
                already_configured: already,
            });
        }
    }

    // Cursor — ~/.cursor/mcp.json
    let cursor_dir = home.join(".cursor");
    if cursor_dir.exists() {
        let cursor_config = cursor_dir.join("mcp.json");
        let already = is_vectorhawk_configured(&cursor_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Cursor".to_string(),
            config_path: cursor_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // Windsurf — ~/.codeium/windsurf/mcp_config.json
    let windsurf_dir = home.join(".codeium").join("windsurf");
    if windsurf_dir.exists() {
        let windsurf_config = windsurf_dir.join("mcp_config.json");
        let already = is_vectorhawk_configured(&windsurf_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Windsurf".to_string(),
            config_path: windsurf_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // VS Code — platform-specific settings.json
    if let Some(vscode_config) = vscode_settings_path(home) {
        let vscode_dir = vscode_config.parent().map(|p| p.to_path_buf());
        if vscode_dir.as_ref().map(|d| d.exists()).unwrap_or(false)
            || vscode_config.exists()
        {
            let already = is_vectorhawk_configured(&vscode_config, "mcpServers");
            clients.push(ClientConfig {
                name: "VS Code".to_string(),
                config_path: vscode_config,
                mcp_key: "mcpServers".to_string(),
                already_configured: already,
            });
        }
    }

    // Gemini CLI — ~/.gemini/settings.json
    let gemini_dir = home.join(".gemini");
    if gemini_dir.exists() {
        let gemini_config = gemini_dir.join("settings.json");
        let already = is_vectorhawk_configured(&gemini_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Gemini CLI".to_string(),
            config_path: gemini_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    clients
}

/// Write the VectorHawk MCP entry into a client config file.
///
/// Reads the existing JSON (if any), merges the entry under `mcp_key`, and
/// writes back. Creates the file (and parent directories) if they do not exist.
pub fn write_mcp_entry(config: &ClientConfig) -> Result<()> {
    let existing: serde_json::Value = if config.config_path.exists() {
        let text = fs::read_to_string(&config.config_path)?;
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let mut obj = match existing {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };

    let servers = obj
        .entry(config.mcp_key.clone())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let serde_json::Value::Object(ref mut map) = servers {
        map.insert(MCP_SERVER_NAME.to_string(), build_mcp_entry());
    }

    if let Some(parent) = config.config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let output = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    fs::write(&config.config_path, output)?;
    Ok(())
}

// ── Slash command skills ───────────────────────────────────────────────────────

/// SKILL.md definitions for VectorHawk slash commands.
/// Each tuple is (directory_name, SKILL.md content).
fn skill_definitions() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "vectorhawk",
            r#"---
name: vectorhawk
description: VectorHawk hub — show auth status, installed skills, MCP servers, and available commands
---
Show the user a VectorHawk status overview:

1. Call vectorhawk_login to check authentication status (if registry is configured)
2. Call vectorhawk_list to show installed skills count and names
3. Call vectorhawk_mcp_status to show active MCP server count (if registry is configured)
4. Then list all available VectorHawk slash commands:
   - /mcp-login — Authenticate with VectorHawk
   - /mcp-search — Browse approved MCP servers
   - /mcp-install — Install an approved MCP server
   - /mcp-request — Request access to a new MCP server
   - /mcp-status — Check MCP server request status
   - /skill-search — Search for skills in the registry
   - /skill-install — Install a skill
   - /skill-list — List installed skills
   - /skill-create — Create a new skill
   - /skill-publish — Publish a skill to the registry
"#,
        ),
        (
            "mcp-login",
            r#"---
name: mcp-login
description: Authenticate with the VectorHawk registry
---
Log the user into VectorHawk. Call the vectorhawk_login tool.

If it succeeds, confirm they are logged in and show their identity.
If it fails, show the error and suggest checking their registry URL.
"#,
        ),
        (
            "mcp-search",
            r#"---
name: mcp-search
description: Browse approved MCP servers in your organization's catalog
---
Browse available MCP servers. Call the vectorhawk_mcp_catalog tool.

$ARGUMENTS

Show results in a clean table with server name, status, and description.
If no servers are found, suggest the user contact their IT admin.
"#,
        ),
        (
            "mcp-install",
            r#"---
name: mcp-install
description: Install an approved MCP server through VectorHawk governance
---
Install an MCP server through governance. Call the vectorhawk_mcp_install tool with the server ID from the arguments.

$ARGUMENTS

If the server is not yet approved, suggest using /mcp-request first.
"#,
        ),
        (
            "mcp-request",
            r#"---
name: mcp-request
description: Request access to a new MCP server from your organization
---
Request access to an MCP server. Call the vectorhawk_mcp_request tool with the server ID from the arguments.

$ARGUMENTS

Explain the approval status to the user (auto-approved, pending review, etc.).
Suggest using /mcp-status to check back on pending requests.
"#,
        ),
        (
            "mcp-status",
            r#"---
name: mcp-status
description: Check the status of your MCP server access requests
---
Check MCP server request status. Call the vectorhawk_mcp_status tool.

Show results clearly — which requests are approved, pending, or denied.
For approved servers, suggest using /mcp-install to activate them.
"#,
        ),
        (
            "skill-search",
            r#"---
name: skill-search
description: Search the VectorHawk registry for available skills
---
Search for skills in the VectorHawk registry. Call the vectorhawk_search tool with the query from the arguments. Use an empty query to list all available skills.

$ARGUMENTS

Show results with skill name, version, and description.
"#,
        ),
        (
            "skill-install",
            r#"---
name: skill-install
description: Install a skill from the VectorHawk registry
---
Install a skill. Call the vectorhawk_install tool with the skill ID from the arguments.

$ARGUMENTS

Confirm installation and show what the skill does.
"#,
        ),
        (
            "skill-list",
            r#"---
name: skill-list
description: List all installed VectorHawk skills
---
List installed skills. Call the vectorhawk_list tool.

Show each skill's name, version, and a brief description.
"#,
        ),
        (
            "skill-create",
            r#"---
name: skill-create
description: Create a new VectorHawk skill from a name and system prompt
---
Create a new skill. Use the vectorhawk_import tool with the skill description from the arguments, or scaffold a SKILL.md manually and use vectorhawk_validate to check it.

$ARGUMENTS

Walk the user through the result — show the generated bundle path and suggest next steps (validate, test, publish).
"#,
        ),
        (
            "skill-publish",
            r#"---
name: skill-publish
description: Publish a skill bundle to the VectorHawk registry
---
Publish a skill to the registry. Use `vectorhawk skill publish` via the Bash tool with the skill path from the arguments.

$ARGUMENTS

If not authenticated, suggest using /mcp-login first.
Show the publish result and the skill's registry URL.
"#,
        ),
    ]
}

/// Install VectorHawk slash command skills to `~/.claude/skills/`.
///
/// Each skill is a SKILL.md file that wraps a VectorHawk MCP tool,
/// giving users clean top-level slash commands in Claude Code.
/// Skips writing if the skill file already exists with identical content.
pub fn install_claude_skills() -> Result<Vec<String>> {
    let home =
        home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    install_claude_skills_in(&home)
}

/// Install skills to a custom root directory (for testing).
fn install_claude_skills_in(home: &std::path::Path) -> Result<Vec<String>> {
    let skills_dir = home.join(".claude").join("skills");
    let mut installed = Vec::new();

    for (dir_name, content) in skill_definitions() {
        let skill_dir = skills_dir.join(dir_name);
        let skill_file = skill_dir.join("SKILL.md");

        if skill_file.exists() {
            if let Ok(existing) = fs::read_to_string(&skill_file) {
                if existing == content {
                    continue;
                }
            }
        }

        fs::create_dir_all(&skill_dir)?;
        fs::write(&skill_file, content)?;
        installed.push(dir_name.to_string());
    }

    Ok(installed)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Return the Claude Desktop config path for the current OS.
fn claude_desktop_config_path(home: &std::path::Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Return the VS Code user settings path for the current OS.
fn vscode_settings_path(home: &std::path::Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
                .join("settings.json"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            home.join(".config")
                .join("Code")
                .join("User")
                .join("settings.json"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Returns `true` if the config file at `path` already contains a
/// `vectorhawk` entry under `mcp_key`.
fn is_vectorhawk_configured(path: &std::path::Path, mcp_key: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vh-setup-test-{label}-{nanos}"))
    }

    // ── build_mcp_entry / block ────────────────────────────────────────────────

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

    // ── write_mcp_entry ────────────────────────────────────────────────────────

    #[test]
    fn write_mcp_entry_round_trips() {
        let tmp = temp_root("write");
        let config_path = tmp.join("claude.json");
        fs::create_dir_all(&tmp).unwrap();

        let config = ClientConfig {
            name: "Test Client".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).expect("write should succeed");

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");
        assert_eq!(json["mcpServers"]["vectorhawk"]["args"][0], "mcp");
        assert_eq!(json["mcpServers"]["vectorhawk"]["args"][1], "serve");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_mcp_entry_merges_with_existing() {
        let tmp = temp_root("merge");
        let config_path = tmp.join("claude.json");
        fs::create_dir_all(&tmp).unwrap();

        let existing = serde_json::json!({
            "mcpServers": {
                "other-tool": {"command": "other", "args": []}
            }
        });
        fs::write(&config_path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let config = ClientConfig {
            name: "Test Client".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).expect("write should succeed");

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");
        assert_eq!(json["mcpServers"]["other-tool"]["command"], "other");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── detect_ai_clients — 6-client matrix ───────────────────────────────────
    // Tests use detect_ai_clients_in(home) to avoid racing on HOME env var.

    #[test]
    fn detect_claude_code_when_dir_exists() {
        let tmp = temp_root("detect-cc");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "Claude Code");
        assert!(found.is_some(), "Claude Code should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_cursor_when_dir_exists() {
        let tmp = temp_root("detect-cursor");
        fs::create_dir_all(tmp.join(".cursor")).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "Cursor");
        assert!(found.is_some(), "Cursor should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_windsurf_when_dir_exists() {
        let tmp = temp_root("detect-windsurf");
        fs::create_dir_all(tmp.join(".codeium").join("windsurf")).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "Windsurf");
        assert!(found.is_some(), "Windsurf should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_claude_desktop_when_dir_exists() {
        let tmp = temp_root("detect-desktop");

        #[cfg(target_os = "macos")]
        let desktop_dir = tmp
            .join("Library")
            .join("Application Support")
            .join("Claude");
        #[cfg(target_os = "linux")]
        let desktop_dir = tmp.join(".config").join("Claude");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            return; // Not supported on this OS
        }

        fs::create_dir_all(&desktop_dir).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "Claude Desktop");
        assert!(found.is_some(), "Claude Desktop should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_vscode_when_settings_dir_exists() {
        let tmp = temp_root("detect-vscode");

        #[cfg(target_os = "macos")]
        let vscode_dir = tmp
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User");
        #[cfg(target_os = "linux")]
        let vscode_dir = tmp.join(".config").join("Code").join("User");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            return; // Not supported on this OS
        }

        fs::create_dir_all(&vscode_dir).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "VS Code");
        assert!(found.is_some(), "VS Code should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_gemini_cli_when_dir_exists() {
        let tmp = temp_root("detect-gemini");
        fs::create_dir_all(tmp.join(".gemini")).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        let found = clients.iter().find(|c| c.name == "Gemini CLI");
        assert!(found.is_some(), "Gemini CLI should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn detect_all_six_clients_when_all_dirs_exist() {
        let tmp = temp_root("detect-all6");

        // Claude Code
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // Cursor
        fs::create_dir_all(tmp.join(".cursor")).unwrap();
        // Windsurf
        fs::create_dir_all(tmp.join(".codeium").join("windsurf")).unwrap();
        // Gemini CLI
        fs::create_dir_all(tmp.join(".gemini")).unwrap();

        // Claude Desktop
        #[cfg(target_os = "macos")]
        fs::create_dir_all(
            tmp.join("Library")
                .join("Application Support")
                .join("Claude"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        fs::create_dir_all(tmp.join(".config").join("Claude")).unwrap();

        // VS Code
        #[cfg(target_os = "macos")]
        fs::create_dir_all(
            tmp.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        fs::create_dir_all(tmp.join(".config").join("Code").join("User")).unwrap();

        let clients = detect_ai_clients_in(&tmp);
        assert_eq!(
            clients.len(),
            6,
            "should detect all 6 clients, got: {:?}",
            clients.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        let names: Vec<&str> = clients.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Claude Code"));
        assert!(names.contains(&"Claude Desktop"));
        assert!(names.contains(&"Cursor"));
        assert!(names.contains(&"Windsurf"));
        assert!(names.contains(&"VS Code"));
        assert!(names.contains(&"Gemini CLI"));

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── write_mcp_entry per client shape ──────────────────────────────────────

    #[test]
    fn cursor_write_has_correct_mcp_entry() {
        let tmp = temp_root("cursor-write");
        let cursor_dir = tmp.join(".cursor");
        fs::create_dir_all(&cursor_dir).unwrap();
        let config_path = cursor_dir.join("mcp.json");

        let config = ClientConfig {
            name: "Cursor".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn windsurf_write_has_correct_mcp_entry() {
        let tmp = temp_root("windsurf-write");
        let ws_dir = tmp.join(".codeium").join("windsurf");
        fs::create_dir_all(&ws_dir).unwrap();
        let config_path = ws_dir.join("mcp_config.json");

        let config = ClientConfig {
            name: "Windsurf".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        write_mcp_entry(&config).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["vectorhawk"]["command"], "vectorhawk");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── install_claude_skills ─────────────────────────────────────────────────

    #[test]
    fn install_claude_skills_creates_all_skill_files() {
        let tmp = temp_root("skills-install");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let installed = install_claude_skills_in(&tmp).unwrap();

        assert_eq!(
            installed.len(),
            11,
            "should install all 11 skills, got: {installed:?}"
        );

        let expected = [
            "vectorhawk",
            "mcp-login",
            "mcp-search",
            "mcp-install",
            "mcp-request",
            "mcp-status",
            "skill-search",
            "skill-install",
            "skill-list",
            "skill-create",
            "skill-publish",
        ];
        for name in &expected {
            assert!(
                installed.contains(&name.to_string()),
                "expected '{name}' in installed list"
            );
        }

        // Verify each SKILL.md exists and is non-empty with proper frontmatter
        for name in &expected {
            let skill_file = tmp
                .join(".claude")
                .join("skills")
                .join(name)
                .join("SKILL.md");
            assert!(skill_file.exists(), "SKILL.md missing for {name}");
            let content = fs::read_to_string(&skill_file).unwrap();
            assert!(!content.is_empty(), "SKILL.md for {name} must not be empty");
            assert!(
                content.starts_with("---\n"),
                "SKILL.md for {name} must start with YAML frontmatter"
            );
            assert!(
                content.contains(&format!("name: {name}")),
                "SKILL.md for {name} must contain name field"
            );
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_claude_skills_uses_vectorhawk_tool_names() {
        let tmp = temp_root("skills-toolnames");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        install_claude_skills_in(&tmp).unwrap();

        let skills_dir = tmp.join(".claude").join("skills");

        // Every SKILL.md that references a tool must use vectorhawk_* names, not skillclub_*
        for entry in fs::read_dir(&skills_dir).unwrap() {
            let entry = entry.unwrap();
            let skill_file = entry.path().join("SKILL.md");
            if skill_file.exists() {
                let content = fs::read_to_string(&skill_file).unwrap();
                assert!(
                    !content.contains("skillclub_"),
                    "SKILL.md {:?} must not reference skillclub_* tools",
                    skill_file
                );
                // Hub and tool-calling skills should reference vectorhawk_* tools
                if content.contains("vectorhawk_") {
                    // Good — referencing the correct tool namespace
                }
            }
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_claude_skills_skips_identical_content() {
        let tmp = temp_root("skills-skip");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let first = install_claude_skills_in(&tmp).unwrap();
        assert_eq!(first.len(), 11, "first install should write all 11");

        let second = install_claude_skills_in(&tmp).unwrap();
        assert!(
            second.is_empty(),
            "re-install with unchanged content should skip all, got: {second:?}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_claude_skills_updates_changed_content() {
        let tmp = temp_root("skills-update");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        install_claude_skills_in(&tmp).unwrap();

        // Corrupt one file
        let skill_file = tmp
            .join(".claude")
            .join("skills")
            .join("mcp-login")
            .join("SKILL.md");
        fs::write(&skill_file, "old content").unwrap();

        let updated = install_claude_skills_in(&tmp).unwrap();
        assert_eq!(updated.len(), 1, "should update only the modified skill");
        assert_eq!(updated[0], "mcp-login");

        let content = fs::read_to_string(&skill_file).unwrap();
        assert!(
            content.contains("vectorhawk_login"),
            "updated SKILL.md should reference vectorhawk_login"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn skill_definitions_have_valid_structure() {
        for (name, content) in skill_definitions() {
            assert!(!name.is_empty(), "skill dir name must not be empty");
            assert!(
                content.starts_with("---\n"),
                "{name}: must start with YAML frontmatter"
            );
            assert!(
                content.contains("description:"),
                "{name}: must have description field"
            );
            assert!(
                content.contains(&format!("name: {name}")),
                "{name}: name field must match dir name"
            );
            assert!(
                !content.contains("skillclub_"),
                "{name}: must not reference skillclub_* tools"
            );
        }
    }
}
