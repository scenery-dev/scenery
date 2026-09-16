//! AI client detection and `mcp setup` config writing.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;

/// Configuration for a detected AI client that supports MCP.
#[derive(Debug)]
pub struct ClientConfig {
    pub name: String,
    pub config_path: PathBuf,
    /// Top-level key in the client's config file that holds the MCP server
    /// map (e.g. `"mcpServers"` for Claude Code's JSON, `"mcp_servers"` for
    /// Codex's TOML).
    pub mcp_key: String,
    pub already_configured: bool,
    /// On-disk format of `config_path` — determines which of `write_mcp_entry`
    /// / `remove_mcp_entry` / staleness-check logic in this module applies.
    pub format: ConfigFormat,
}

/// The on-disk format of a client's MCP config file.
///
/// Every JSON client (Claude Code, Claude Desktop, Cursor, Windsurf, VS Code,
/// Gemini CLI) shares one JSON merge/read/remove implementation. Codex CLI's
/// `~/.codex/config.toml` is TOML, not JSON, so the handful of functions that
/// touch a config file branch on this field rather than assuming JSON
/// everywhere. Kept to exactly the two formats VectorHawk actually writes —
/// add a variant only when a third client needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Toml,
}

/// Path segment marking a versioned Homebrew Cellar install for `vectorhawk`
/// (`<prefix>/Cellar/vectorhawk/<ver>/bin/vectorhawk`). `brew upgrade` deletes
/// the old version's Cellar directory, so any command still pointing inside
/// one is either already dead or about to be on the next upgrade. Shared by
/// [`stable_command_from_exe`] (used when writing a fresh entry) and
/// [`command_needs_repair`] (used when checking an existing one).
const CELLAR: &str = "/Cellar/vectorhawk/";

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
        "command": resolve_mcp_command(),
        "args": MCP_ARGS,
    })
}

/// Resolve the absolute command path an AI client should spawn for the shim.
///
/// We write an absolute path (not bare `vectorhawk`) because AI clients launch
/// MCP servers with a non-login shell that often lacks Homebrew/Linuxbrew on
/// PATH. But `current_exe()` canonicalizes through symlinks to the *version-
/// pinned* Homebrew Cellar path (`…/Cellar/vectorhawk/<ver>/bin/vectorhawk`),
/// which `brew upgrade` deletes — leaving the client pointing at a dead binary.
/// So when a Cellar layout is detected, rewrite to the stable `<prefix>/bin`
/// symlink Homebrew keeps current across upgrades.
fn resolve_mcp_command() -> String {
    match std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
    {
        Some(exe) => stable_command_from_exe(&exe, |p| std::path::Path::new(p).exists()),
        None => MCP_COMMAND.to_string(),
    }
}

/// Rewrite a Homebrew Cellar exe path (`<prefix>/Cellar/vectorhawk/<ver>/bin/<bin>`)
/// to the version-independent `<prefix>/bin/<bin>` symlink when that symlink
/// exists; otherwise return `exe` unchanged. Pure, with an injectable existence
/// check so the rewrite is unit-testable without a real Homebrew install.
fn stable_command_from_exe(exe: &str, exists: impl Fn(&str) -> bool) -> String {
    if let Some(idx) = exe.find(CELLAR) {
        let stable = format!("{}/bin/{}", &exe[..idx], MCP_COMMAND);
        if exists(&stable) {
            return stable;
        }
    }
    exe.to_string()
}

/// Build the full `mcpServers` block suitable for merging into a client config.
pub fn build_mcp_servers_block() -> serde_json::Value {
    serde_json::json!({
        MCP_SERVER_NAME: build_mcp_entry()
    })
}

/// Build the MCP server config entry for a specific client, layering any
/// client-specific required fields on top of [`build_mcp_entry`]'s common
/// `command`/`args` shape.
///
/// VS Code's `mcp.json` schema requires an explicit `type` field per server
/// (`"stdio"` for a locally-spawned process) — see the field table in the
/// VS Code MCP configuration reference (`type` — Required — `"stdio"`):
/// <https://code.visualstudio.com/docs/agents/reference/mcp-configuration>.
/// Every other client's config format only needs `command`/`args`.
fn mcp_entry_for(config: &ClientConfig) -> serde_json::Value {
    let mut entry = build_mcp_entry();
    if config.name == "VS Code" {
        if let serde_json::Value::Object(ref mut map) = entry {
            map.insert(
                "type".to_string(),
                serde_json::Value::String("stdio".to_string()),
            );
        }
    }
    entry
}

/// Detect Claude Code installation and return config info.
///
/// Detected by `~/.claude`, `~/.claude.json`, or the presence of the app bundle
/// (so fresh machines without a prior Claude launch are still configured).
pub fn detect_claude_code() -> Option<ClientConfig> {
    let home = home_dir()?;
    let claude_config = home.join(".claude.json");
    let claude_dir = home.join(".claude");
    let app_present = claude_code_app_present(std::path::Path::new("/"));
    if !claude_dir.exists() && !claude_config.exists() && !app_present {
        return None;
    }
    let already = is_vectorhawk_configured(&claude_config, "mcpServers", ConfigFormat::Json);
    Some(ClientConfig {
        name: "Claude Code".to_string(),
        config_path: claude_config,
        mcp_key: "mcpServers".to_string(),
        already_configured: already,
        format: ConfigFormat::Json,
    })
}

/// Detect all supported AI clients: Claude Code, Claude Desktop, Cursor,
/// Windsurf, VS Code, Gemini CLI (6 clients always), plus Codex CLI when
/// built with the `daemon` feature (7 clients — see the Codex block in
/// [`detect_ai_clients_in`]).
pub fn detect_ai_clients() -> Vec<ClientConfig> {
    match home_dir() {
        Some(home) => detect_ai_clients_in(&home, std::path::Path::new("/")),
        None => Vec::new(),
    }
}

/// Inner detection that accepts an explicit home directory and system root.
///
/// Separated from `detect_ai_clients` so tests can supply temp-dir paths
/// without racing on the process-global HOME environment variable, and to
/// allow simulating app bundle presence in tests.
fn detect_ai_clients_in(
    home: &std::path::Path,
    system_root: &std::path::Path,
) -> Vec<ClientConfig> {
    let mut clients = Vec::new();

    // Claude Code — ~/.claude.json or ~/.claude dir, or app bundle present
    // (covers fresh machines where Claude has never been launched yet).
    let claude_config = home.join(".claude.json");
    let claude_dir = home.join(".claude");
    if claude_dir.exists() || claude_config.exists() || claude_code_app_present(system_root) {
        let already = is_vectorhawk_configured(&claude_config, "mcpServers", ConfigFormat::Json);
        clients.push(ClientConfig {
            name: "Claude Code".to_string(),
            config_path: claude_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
            format: ConfigFormat::Json,
        });
    }

    // Claude Desktop — platform-specific path
    if let Some(desktop_config) = claude_desktop_config_path(home) {
        let desktop_dir = desktop_config.parent().map(|p| p.to_path_buf());
        if desktop_dir.as_ref().map(|d| d.exists()).unwrap_or(false) || desktop_config.exists() {
            let already =
                is_vectorhawk_configured(&desktop_config, "mcpServers", ConfigFormat::Json);
            clients.push(ClientConfig {
                name: "Claude Desktop".to_string(),
                config_path: desktop_config,
                mcp_key: "mcpServers".to_string(),
                already_configured: already,
                format: ConfigFormat::Json,
            });
        }
    }

    // Cursor — ~/.cursor/mcp.json
    let cursor_dir = home.join(".cursor");
    if cursor_dir.exists() {
        let cursor_config = cursor_dir.join("mcp.json");
        let already = is_vectorhawk_configured(&cursor_config, "mcpServers", ConfigFormat::Json);
        clients.push(ClientConfig {
            name: "Cursor".to_string(),
            config_path: cursor_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
            format: ConfigFormat::Json,
        });
    }

    // Windsurf — ~/.codeium/windsurf/mcp_config.json
    let windsurf_dir = home.join(".codeium").join("windsurf");
    if windsurf_dir.exists() {
        let windsurf_config = windsurf_dir.join("mcp_config.json");
        let already = is_vectorhawk_configured(&windsurf_config, "mcpServers", ConfigFormat::Json);
        clients.push(ClientConfig {
            name: "Windsurf".to_string(),
            config_path: windsurf_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
            format: ConfigFormat::Json,
        });
    }

    // VS Code — user-level `Code/User/mcp.json`, top-level `servers` key.
    // NOT `settings.json`'s `mcpServers` — VS Code doesn't read that at all;
    // see `vscode_mcp_json_path` / `migrate_stale_vscode_settings_entry`.
    if let Some(vscode_config) = vscode_mcp_json_path(home) {
        let vscode_dir = vscode_config.parent().map(|p| p.to_path_buf());
        if vscode_dir.as_ref().map(|d| d.exists()).unwrap_or(false) || vscode_config.exists() {
            let already = is_vectorhawk_configured(&vscode_config, "servers", ConfigFormat::Json);
            clients.push(ClientConfig {
                name: "VS Code".to_string(),
                config_path: vscode_config,
                mcp_key: "servers".to_string(),
                already_configured: already,
                format: ConfigFormat::Json,
            });
        }
    }

    // Gemini CLI — ~/.gemini/settings.json
    let gemini_dir = home.join(".gemini");
    if gemini_dir.exists() {
        let gemini_config = gemini_dir.join("settings.json");
        let already = is_vectorhawk_configured(&gemini_config, "mcpServers", ConfigFormat::Json);
        clients.push(ClientConfig {
            name: "Gemini CLI".to_string(),
            config_path: gemini_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
            format: ConfigFormat::Json,
        });
    }

    // Codex CLI — ~/.codex/config.toml, TOML `[mcp_servers.<name>]` table
    // (confirmed against openai/codex: `codex-rs/config/src/mcp_edit.rs`
    // reads a top-level `mcp_servers` table into `McpServerConfig`, and the
    // `Stdio` variant in `codex-rs/config/src/mcp_types.rs` takes `command:
    // String` + `args: Vec<String>` — the same shape every other client's
    // entry uses, just TOML instead of JSON). `~/.codex` is Codex's default
    // config dir (`codex-rs/utils/home-dir/src/lib.rs`), overridable via
    // `CODEX_HOME` — same convention as `~/.claude`, `~/.cursor`, etc. here.
    //
    // Gated behind `daemon`: reading/writing TOML needs `toml_edit`, which
    // the shim (never runs `mcp setup` or `uninstall`) has no reason to link.
    #[cfg(feature = "daemon")]
    {
        let codex_dir = home.join(".codex");
        if codex_dir.exists() {
            let codex_config = codex_dir.join("config.toml");
            let already =
                is_vectorhawk_configured(&codex_config, "mcp_servers", ConfigFormat::Toml);
            clients.push(ClientConfig {
                name: "Codex".to_string(),
                config_path: codex_config,
                mcp_key: "mcp_servers".to_string(),
                already_configured: already,
                format: ConfigFormat::Toml,
            });
        }
    }

    clients
}

/// Write the VectorHawk MCP entry into a client config file.
///
/// Reads the existing JSON (if any), merges the entry under `mcp_key`, and
/// writes back. Creates the file (and parent directories) if they do not exist.
///
/// Before the very first edit to a given `config_path`, the pre-edit file is
/// backed up and a `restore-journal` entry (`op=config_edit`, `source=native`)
/// is appended, so `vectorhawk uninstall` can restore the client's config to
/// exactly what it was before VectorHawk touched it. Journal/backup failures
/// are logged and never block the actual config write — see
/// [`record_config_edit_journal`].
///
/// Dispatches on `config.format`: JSON clients merge via `serde_json`
/// ([`write_mcp_entry_json`]); Codex's TOML config merges via `toml_edit`
/// ([`write_mcp_entry_toml`]) so the user's formatting/comments survive.
pub fn write_mcp_entry(config: &ClientConfig) -> Result<()> {
    match config.format {
        ConfigFormat::Json => write_mcp_entry_json(config),
        ConfigFormat::Toml => write_mcp_entry_toml(config),
    }
}

fn write_mcp_entry_json(config: &ClientConfig) -> Result<()> {
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
        map.insert(MCP_SERVER_NAME.to_string(), mcp_entry_for(config));
    }

    if let Some(parent) = config.config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Journal + backup BEFORE overwriting the file, so the backup captures
    // the file exactly as it stood before this edit (or before any
    // VectorHawk edit at all, on repeat calls — see doc comment below).
    #[cfg(feature = "daemon")]
    record_config_edit_journal(config);

    let output = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    fs::write(&config.config_path, output)?;
    Ok(())
}

/// Write/merge the `[mcp_servers.vectorhawk]` table into a Codex-style TOML
/// config via `toml_edit`'s format-preserving document model, so any other
/// tables, keys, comments, or formatting in the user's `config.toml` survive
/// byte-for-byte. Mirrors [`write_mcp_entry_json`]'s merge-then-write shape
/// and journaling order.
///
/// Only compiled with the `daemon` feature — see the `daemon` feature note
/// on `toml_edit` in `Cargo.toml`. The shim never calls `write_mcp_entry` on
/// a TOML client (Codex detection itself is gated the same way), so this
/// fallback is unreachable in practice; it exists only so the match in
/// `write_mcp_entry` stays exhaustive without pulling `toml_edit` into
/// shim builds.
#[cfg(feature = "daemon")]
fn write_mcp_entry_toml(config: &ClientConfig) -> Result<()> {
    use anyhow::Context;
    use toml_edit::{value, Array, DocumentMut, Item, Table};

    let text = if config.config_path.exists() {
        fs::read_to_string(&config.config_path)?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse()
            .with_context(|| format!("failed to parse {} as TOML", config.config_path.display()))?
    };

    if doc.get(config.mcp_key.as_str()).is_none() {
        doc[config.mcp_key.as_str()] = Item::Table(Table::new());
    }
    // `as_table_like_mut` (not `as_table_mut`) so this also handles a user's
    // `mcp_servers = { ... }` written as a TOML *inline* table, not just a
    // standard `[mcp_servers]` header — both are valid TOML and Codex reads
    // either (see `mcp_types.rs`'s `try_into::<BTreeMap<...>>()`, which is
    // agnostic to the source table's syntax). Only a genuinely non-table
    // value (e.g. `mcp_servers = 5`) still hits the error below.
    let servers = doc[config.mcp_key.as_str()]
        .as_table_like_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} already has a top-level `{}` key that isn't a table",
                config.config_path.display(),
                config.mcp_key
            )
        })?;

    let mut entry = Table::new();
    entry["command"] = value(resolve_mcp_command());
    let mut args = Array::new();
    for arg in MCP_ARGS {
        args.push(*arg);
    }
    entry["args"] = value(args);
    // `TableLike::insert` on an inline table converts this `Item::Table` into
    // a nested inline table via `Item::into_value` (`Table::into_inline_table`
    // under the hood) rather than panicking, so the outer `mcp_servers = {
    // ... }` stays inline and the new `vectorhawk = { command = ..., args =
    // [...] }` entry matches its sibling entries' style; on a standard table
    // it inserts as `[mcp_servers.vectorhawk]` exactly as before.
    servers.insert(MCP_SERVER_NAME, Item::Table(entry));

    if let Some(parent) = config.config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Same journal-before-write ordering as the JSON path.
    record_config_edit_journal(config);

    fs::write(&config.config_path, doc.to_string())?;
    Ok(())
}

#[cfg(not(feature = "daemon"))]
fn write_mcp_entry_toml(_config: &ClientConfig) -> Result<()> {
    anyhow::bail!(
        "TOML client config writing requires the `daemon` feature (vectorhawkd-mcp built \
         without it, e.g. the shim, never calls this)"
    )
}

/// Record the restore-journal entry for a `write_mcp_entry` call.
///
/// Backs up `config.config_path` (if it exists) only the *first* time this
/// path is journaled — determined by scanning existing entries for a prior
/// `config_edit` against the same `target_path` and reusing its
/// `backup_path` — so a second `mcp setup` run never clobbers the pristine
/// pre-VectorHawk backup with an already-modified copy. If the file did not
/// exist before this edit, `backup_path` is left `None`: uninstall should
/// delete `target_path` rather than restore it in that case.
///
/// Best-effort: any failure (can't resolve the data dir, lock contention,
/// I/O error) is logged at WARN and swallowed. Losing a restore-journal
/// entry must never block the AI client from getting configured.
#[cfg(feature = "daemon")]
fn record_config_edit_journal(config: &ClientConfig) {
    use vectorhawkd_core::state::AppState;

    let root_dir = match AppState::resolve_root_dir() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "mcp setup: could not resolve data dir — skipping restore-journal entry");
            return;
        }
    };
    record_config_edit_journal_in(config, root_dir);
}

/// Core logic for [`record_config_edit_journal`], parameterised on `root_dir`
/// so tests can point it at a temp directory instead of the real platform
/// data dir.
#[cfg(feature = "daemon")]
fn record_config_edit_journal_in(config: &ClientConfig, root_dir: camino::Utf8PathBuf) {
    use vectorhawkd_core::restore_journal::{
        new_backup_ts, JournalEntry, JournalOp, JournalSource, RestoreJournal,
    };

    let journal = RestoreJournal::new(root_dir);
    let target_path = config.config_path.to_string_lossy().to_string();

    let prior_backup_path = journal.read_all().ok().and_then(|entries| {
        entries.into_iter().rev().find_map(|e| {
            if e.op == JournalOp::ConfigEdit && e.target_path == target_path {
                e.backup_path
            } else {
                None
            }
        })
    });

    let backup_path = match prior_backup_path {
        Some(p) => Some(p),
        None if config.config_path.exists() => {
            let Some(utf8_source) = camino::Utf8Path::from_path(config.config_path.as_path())
            else {
                tracing::warn!(
                    path = %config.config_path.display(),
                    "mcp setup: config path is not valid UTF-8 — skipping restore-journal backup"
                );
                return;
            };
            match journal.backup_path_for(&new_backup_ts(), utf8_source) {
                Ok(p) => Some(p.to_string()),
                Err(e) => {
                    tracing::warn!(error = %e, "mcp setup: failed to back up client config for restore journal");
                    None
                }
            }
        }
        None => None,
    };

    let mut entry = JournalEntry::new(JournalOp::ConfigEdit, JournalSource::Native, target_path)
        .with_slug(MCP_SERVER_NAME)
        .with_client(config.name.clone())
        .with_detail(serde_json::json!({
            "server_key": MCP_SERVER_NAME,
            "mcp_key": config.mcp_key,
        }));
    if let Some(bp) = backup_path {
        entry = entry.with_backup_path(bp);
    }

    if let Err(e) = journal.append(entry) {
        tracing::warn!(error = %e, "mcp setup: failed to append restore-journal entry (non-fatal)");
    }
}

/// Remove the VectorHawk MCP entry from a client config file.
///
/// Returns `true` if the entry existed and was removed, `false` if it wasn't
/// present. The file is left unchanged when the entry is absent.
///
/// Dispatches on `config.format`, same as [`write_mcp_entry`].
pub fn remove_mcp_entry(config: &ClientConfig) -> Result<bool> {
    match config.format {
        ConfigFormat::Json => remove_mcp_entry_json(config),
        ConfigFormat::Toml => remove_mcp_entry_toml(config),
    }
}

fn remove_mcp_entry_json(config: &ClientConfig) -> Result<bool> {
    if !config.config_path.exists() {
        return Ok(false);
    }
    let text = fs::read_to_string(&config.config_path)?;
    let mut obj: serde_json::Map<String, serde_json::Value> =
        match serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or(serde_json::Value::Object(Default::default()))
        {
            serde_json::Value::Object(m) => m,
            _ => return Ok(false),
        };

    let removed = if let Some(serde_json::Value::Object(ref mut map)) = obj.get_mut(&config.mcp_key)
    {
        map.remove(MCP_SERVER_NAME).is_some()
    } else {
        false
    };

    if removed {
        let output = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
        fs::write(&config.config_path, output)?;
    }

    Ok(removed)
}

/// TOML counterpart of [`remove_mcp_entry_json`], via `toml_edit` so removing
/// the `vectorhawk` sub-table leaves every other table/comment/formatting in
/// the file untouched. Same `daemon`-only gating as [`write_mcp_entry_toml`].
#[cfg(feature = "daemon")]
fn remove_mcp_entry_toml(config: &ClientConfig) -> Result<bool> {
    use toml_edit::DocumentMut;

    if !config.config_path.exists() {
        return Ok(false);
    }
    let text = fs::read_to_string(&config.config_path)?;
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return Ok(false);
    };
    // Same `as_table_like_mut` reasoning as `write_mcp_entry_toml`: an inline
    // `mcp_servers = { vectorhawk = {...} }` must be removable too, not just
    // a standard `[mcp_servers]` table.
    let Some(servers) = doc
        .get_mut(config.mcp_key.as_str())
        .and_then(|item| item.as_table_like_mut())
    else {
        return Ok(false);
    };
    let removed = servers.remove(MCP_SERVER_NAME).is_some();

    if removed {
        fs::write(&config.config_path, doc.to_string())?;
    }
    Ok(removed)
}

#[cfg(not(feature = "daemon"))]
fn remove_mcp_entry_toml(_config: &ClientConfig) -> Result<bool> {
    anyhow::bail!(
        "TOML client config removal requires the `daemon` feature (vectorhawkd-mcp built \
         without it, e.g. the shim, never calls this)"
    )
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
Search for skills in the VectorHawk registry. Call the vectorhawk_search tool with the query from the arguments.

If no query was provided, ask the user what they're looking for before calling the tool.

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
Create a new skill. Call vectorhawk_author with the skill name and a system prompt from the arguments.

$ARGUMENTS

Walk the user through the result — show the generated SKILL.md and suggest next steps (validate, publish).
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
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    install_claude_skills_in(&home)
}

/// Remove VectorHawk slash command skill directories from `~/.claude/skills/`.
///
/// Returns the names of directories that were removed.
pub fn uninstall_claude_skills() -> Result<Vec<String>> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    uninstall_claude_skills_in(&home)
}

fn uninstall_claude_skills_in(home: &std::path::Path) -> Result<Vec<String>> {
    let skills_dir = home.join(".claude").join("skills");
    let mut removed = Vec::new();
    for (dir_name, _) in skill_definitions() {
        let skill_dir = skills_dir.join(dir_name);
        if skill_dir.exists() {
            fs::remove_dir_all(&skill_dir)?;
            removed.push(dir_name.to_string());
        }
    }
    Ok(removed)
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

/// Returns `true` if the Claude Code app is installed on this machine,
/// regardless of whether it has ever been launched (no `~/.claude` yet).
///
/// `system_root` is `/` in production; tests pass a temp dir so they can
/// create a fake app bundle without touching the real filesystem.
fn claude_code_app_present(system_root: &std::path::Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        system_root.join("Applications").join("Claude.app").exists()
    }
    #[cfg(target_os = "linux")]
    {
        // Claude Code on Linux is distributed as a .deb/.rpm package; the
        // binary lands at /usr/bin/claude or /usr/local/bin/claude.
        system_root.join("usr/local/bin/claude").exists()
            || system_root.join("usr/bin/claude").exists()
            || system_root.join("opt/Claude/claude").exists()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = system_root;
        false
    }
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

/// Return VS Code's per-user config directory (`Code/User/`) for the current
/// OS. Shared by [`vscode_mcp_json_path`] (the config file VS Code actually
/// reads) and [`vscode_settings_path`] (legacy location, kept only so
/// [`migrate_stale_vscode_settings_entry`] can clean up a stale entry an
/// older build left there).
fn vscode_user_dir(home: &std::path::Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(home.join(".config").join("Code").join("User"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Return VS Code's user-level MCP server config path: `Code/User/mcp.json`.
///
/// This is the file VS Code actually reads for user-profile MCP servers,
/// under a top-level `servers` key. See the VS Code MCP configuration
/// reference: <https://code.visualstudio.com/docs/agents/reference/mcp-configuration>
/// ("MCP server configuration is stored in the `mcp.json` JSON file. This
/// file can be in your workspace (`.vscode/mcp.json`) or in your user
/// profile."; `"servers": {}` — "an object that maps server names to their
/// configurations"). `settings.json`'s `mcpServers` key (the pre-fix
/// location this code used to write) is not read by VS Code at all.
fn vscode_mcp_json_path(home: &std::path::Path) -> Option<PathBuf> {
    vscode_user_dir(home).map(|d| d.join("mcp.json"))
}

/// Return the legacy VS Code user *settings* path (`Code/User/settings.json`).
///
/// No longer written to by `mcp setup` / the repair pass — kept only so
/// [`migrate_stale_vscode_settings_entry`] can find and remove a stale
/// `mcpServers.vectorhawk` key a pre-fix build left there. Gated behind
/// `daemon` because that's its only (non-test) caller — see the feature-gate
/// note on `migrate_stale_vscode_settings_entry` itself.
#[cfg(feature = "daemon")]
fn vscode_settings_path(home: &std::path::Path) -> Option<PathBuf> {
    vscode_user_dir(home).map(|d| d.join("settings.json"))
}

/// Returns `true` if the config file at `path` already contains a
/// `vectorhawk` entry under `mcp_key`, with a `command` that's current.
///
/// Format-agnostic: reads the entry via [`read_vectorhawk_command`], then
/// applies the same staleness rule regardless of whether it came from JSON
/// or TOML — see [`command_is_current`].
fn is_vectorhawk_configured(path: &std::path::Path, mcp_key: &str, format: ConfigFormat) -> bool {
    match read_vectorhawk_command(path, mcp_key, format) {
        Some(command) => command_is_current(&command),
        None => false,
    }
}

/// Returns `true` if `command` is a `vectorhawk` MCP entry's command that's
/// still current — i.e. does NOT need `mcp setup`/repair to rewrite it.
///
/// Treated as stale (returns `false`) if the command is:
///   - not an absolute path (bare command name), or
///   - the binary no longer exists (old Cellar removed after brew upgrade), or
///   - doesn't match the currently-running binary (stale Cellar path from a
///     previous brew upgrade where the old Cellar was still present during
///     post_install).
fn command_is_current(command: &str) -> bool {
    let p = std::path::Path::new(command);
    if !p.is_absolute() {
        return false;
    }
    match std::env::current_exe().ok() {
        Some(exe) => p == exe.as_path(),
        None => p.exists(),
    }
}

/// Read the `command` string of the `vectorhawk` entry (`{mcp_key}.vectorhawk.command`)
/// out of a client config file, whatever its on-disk format. Returns `None`
/// if the file is missing, unparsable, or the entry simply isn't present —
/// callers ([`is_vectorhawk_configured`], [`repair_stale_mcp_entries_in`])
/// treat that the same as "nothing to check/repair".
fn read_vectorhawk_command(
    path: &std::path::Path,
    mcp_key: &str,
    format: ConfigFormat,
) -> Option<String> {
    match format {
        ConfigFormat::Json => {
            let text = fs::read_to_string(path).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            json.get(mcp_key)?
                .get(MCP_SERVER_NAME)?
                .get("command")?
                .as_str()
                .map(str::to_string)
        }
        ConfigFormat::Toml => read_vectorhawk_command_toml(path, mcp_key),
    }
}

#[cfg(feature = "daemon")]
fn read_vectorhawk_command_toml(path: &std::path::Path, mcp_key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    // `Item::get` (unlike `as_table_mut`) already looks inside both a
    // standard `[mcp_servers]` table *and* an inline `mcp_servers = { ... }`
    // — `toml_edit`'s string `Index` impl matches `Item::Table` and
    // `Item::Value(Value::InlineTable)` alike — so this needs no
    // `as_table_like` dance the way the write/remove paths below do.
    // Covered by `codex_read_command_from_inline_table`.
    doc.get(mcp_key)?
        .get(MCP_SERVER_NAME)?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

#[cfg(not(feature = "daemon"))]
fn read_vectorhawk_command_toml(_path: &std::path::Path, _mcp_key: &str) -> Option<String> {
    None
}

// ── Unmanaged server detection (GAP-06) ───────────────────────────────────────

/// An MCP server entry found in a client's config file that is NOT managed by VectorHawk.
#[derive(Debug, Clone)]
pub struct UnmanagedServer {
    /// Name/key of the server in the config file (e.g. `"github-mcp"`).
    pub server_name: String,
    /// Path of the AI client config file that contained this entry.
    pub config_path: String,
    /// Which AI client (e.g. `"Claude Code"`, `"Cursor"`).
    pub client_name: String,
}

/// Scan all detected AI client config files and return MCP servers not managed by VectorHawk.
///
/// A server is considered managed if its key is `MCP_SERVER_NAME` (`"vectorhawk"`).
/// Every other server key is reported as unmanaged so IT admins can audit shadow
/// MCP installations via the `unmanaged_server_detected` audit event stream.
pub fn detect_unmanaged_servers() -> Vec<UnmanagedServer> {
    let clients = detect_ai_clients();
    let mut unmanaged = Vec::new();

    for client in &clients {
        if !client.config_path.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&client.config_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let config: serde_json::Value = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let servers = match config.get(&client.mcp_key).and_then(|v| v.as_object()) {
            Some(s) => s,
            None => continue,
        };

        for key in servers.keys() {
            if key == MCP_SERVER_NAME {
                continue;
            }
            unmanaged.push(UnmanagedServer {
                server_name: key.clone(),
                config_path: client.config_path.display().to_string(),
                client_name: client.name.clone(),
            });
        }
    }

    unmanaged
}

// ── Daemon-boot self-heal for stale MCP commands ───────────────────────────────

/// Returns `true` when `command` looks like a stale `vectorhawk` MCP command
/// that daemon-boot repair should rewrite: an absolute path that either (a)
/// no longer exists on disk — Homebrew deleted the old Cellar version on a
/// later upgrade — or (b) still points inside a versioned Cellar directory
/// (written by a pre-efb7fe1 `mcp setup`, still present but due to be pruned
/// on the next upgrade). A bare command name (resolved via `PATH`) is left
/// alone — it was never a Cellar path to begin with.
fn command_needs_repair(command: &str, exists: impl Fn(&str) -> bool) -> bool {
    let path = std::path::Path::new(command);
    if !path.is_absolute() {
        return false;
    }
    if !exists(command) {
        return true;
    }
    command.contains(CELLAR)
}

/// Scan all detected AI-client configs and rewrite any `vectorhawk` MCP
/// entry whose `command` is stale (see [`command_needs_repair`]) back to the
/// current stable path, via the same [`write_mcp_entry`] used by `mcp setup`
/// (so the write goes through the restore journal identically).
///
/// This is the daemon-boot half of the self-heal for GH board bug
/// "SSH/headless brew upgrade leaves stale versioned MCP command path":
/// `mcp setup` writes the stable Homebrew bin path since efb7fe1, but the
/// brew `post_install` hook that would normally re-run `mcp setup` on
/// upgrade doesn't execute headlessly over SSH (no D-Bus/desktop session),
/// so a config written by an older `mcp setup` — or one that's simply gone
/// stale after an upgrade removed its Cellar dir — never self-corrects on
/// its own. Running this on every daemon start converges the fleet without
/// requiring anyone to manually re-run `mcp setup`.
///
/// Idempotent: a config whose command already matches the current stable
/// path is left untouched. Only the `vectorhawk` entry is ever touched —
/// every other MCP server entry in the file is left exactly as-is.
///
/// Returns the names of clients whose entry was rewritten. Per-client I/O
/// failures are logged at WARN and skipped — never fatal to daemon startup.
pub fn repair_stale_mcp_entries() -> Vec<String> {
    match home_dir() {
        Some(home) => repair_stale_mcp_entries_in(&home, std::path::Path::new("/")),
        None => Vec::new(),
    }
}

/// Core logic for [`repair_stale_mcp_entries`], parameterised on `home` and
/// `system_root` so tests can point it at a temp directory instead of the
/// real filesystem.
fn repair_stale_mcp_entries_in(
    home: &std::path::Path,
    system_root: &std::path::Path,
) -> Vec<String> {
    let clients = detect_ai_clients_in(home, system_root);
    let mut repaired = Vec::new();

    for client in &clients {
        if !client.config_path.exists() {
            continue;
        }
        // Format-agnostic: covers both JSON clients and Codex's TOML config,
        // so a stale versioned `command` in `~/.codex/config.toml` gets
        // rewritten by the same daemon-boot self-heal as every other client.
        let Some(command) =
            read_vectorhawk_command(&client.config_path, &client.mcp_key, client.format)
        else {
            continue;
        };
        if !command_needs_repair(&command, |p| std::path::Path::new(p).exists()) {
            continue;
        }

        match write_mcp_entry(client) {
            Ok(()) => repaired.push(client.name.clone()),
            Err(e) => {
                tracing::warn!(
                    client = %client.name,
                    path = %client.config_path.display(),
                    error = %e,
                    "heal: failed to repair stale vectorhawk MCP command"
                );
            }
        }
    }

    // Legacy-location cleanup: a pre-fix build may have left a dead
    // `mcpServers.vectorhawk` key in Code/User/settings.json (the wrong
    // file/key — VS Code never read it). Clean it up here too, alongside the
    // per-client loop above, so it doesn't linger forever now that fresh
    // writes/repairs only ever touch the real `mcp.json` location.
    //
    // Gated behind `daemon` because it needs both the restore-journal
    // machinery and the `jsonc-parser` CST editor (see
    // [`migrate_stale_vscode_settings_entry`]), the same as
    // `record_config_edit_journal` already is.
    #[cfg(feature = "daemon")]
    if migrate_stale_vscode_settings_entry(home) {
        repaired.push("VS Code (settings.json cleanup)".to_string());
    }

    repaired
}

// ── VS Code settings.json → mcp.json migration ─────────────────────────────────

/// Remove a stale `mcpServers.vectorhawk` key left in the legacy
/// `Code/User/settings.json` by a pre-fix `mcp setup`/self-heal build. VS
/// Code never reads `mcpServers` out of `settings.json` — the real location
/// is `Code/User/mcp.json` → `servers` (see [`vscode_mcp_json_path`]) — so a
/// leftover key there is permanently dead weight once fresh writes/repairs
/// target the correct file.
///
/// `settings.json` is JSONC: VS Code allows comments and trailing commas in
/// it. Round-tripping it through `serde_json` (as this function used to)
/// would silently drop every comment on a successful parse, or — on a parse
/// failure, which is the *common* case for a real hand-edited settings.json
/// — skip the edit entirely, so the stale key would never actually get
/// cleaned up for most real users. Instead this uses `jsonc_parser`'s `cst`
/// (concrete syntax tree) editor: it parses the full token stream —
/// comments, whitespace, trailing commas and all — as real nodes, so
/// removing one property node leaves everything else (including comments
/// attached to neighboring properties) byte-for-byte as written.
/// `CstRootNode::parse` is lenient about JSONC syntax but still returns
/// `Err` for genuinely malformed JSON (e.g. unmatched braces); that case,
/// same as a non-object root or a missing `mcpServers`/`vectorhawk` key,
/// leaves the file completely untouched.
///
/// Returns `true` if the stale entry was found and removed.
#[cfg(feature = "daemon")]
fn migrate_stale_vscode_settings_entry(home: &std::path::Path) -> bool {
    use jsonc_parser::cst::CstRootNode;
    use jsonc_parser::ParseOptions;

    let Some(settings_path) = vscode_settings_path(home) else {
        return false;
    };
    if !settings_path.exists() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&settings_path) else {
        return false;
    };

    let Ok(root) = CstRootNode::parse(&text, &ParseOptions::default()) else {
        // Not even valid JSONC (e.g. unmatched braces) — leave it alone
        // rather than risk corrupting the user's file.
        return false;
    };
    let Some(root_obj) = root.object_value() else {
        return false;
    };
    let Some(mcp_servers_obj) = root_obj.object_value("mcpServers") else {
        return false;
    };
    let Some(vectorhawk_prop) = mcp_servers_obj.get(MCP_SERVER_NAME) else {
        return false;
    };

    // Journal this edit the same way `write_mcp_entry` journals a fresh
    // write, so `vectorhawk uninstall` still restores the user's true
    // pre-VectorHawk settings.json. `record_config_edit_journal` reuses the
    // *original* backup captured the first time VectorHawk ever touched
    // this file (see its backup-reuse doc comment) rather than backing up
    // the already-VectorHawk-modified content we're about to write below.
    // Runs before the write, same ordering `write_mcp_entry` uses.
    let legacy_config = ClientConfig {
        name: "VS Code".to_string(),
        config_path: settings_path.clone(),
        mcp_key: "mcpServers".to_string(),
        already_configured: false,
        format: ConfigFormat::Json,
    };
    record_config_edit_journal(&legacy_config);

    // Removing the property node preserves every comment, blank line, and
    // the rest of the file's formatting exactly as written — only the
    // `"vectorhawk": {...}` property (and its now-dangling comma, if any)
    // is excised.
    vectorhawk_prop.remove();
    fs::write(&settings_path, root.to_string()).is_ok()
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

    /// Serializes tests that call the public `write_mcp_entry` (which, with
    /// the `daemon` feature on, resolves the real platform data dir via
    /// `$HOME` to record a restore-journal entry). Redirecting `HOME` to a
    /// throwaway temp dir keeps that side effect out of the developer's real
    /// `~/Library/Application Support/VectorHawk`; the mutex prevents
    /// parallel tests from racing on the process-global env var.
    static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `HOME` pointed at `fake_home`, restoring the original
    /// value afterward even if `f` panics.
    fn with_fake_home<T>(fake_home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var_os("HOME");
        std::env::set_var("HOME", fake_home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match original {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match result {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    /// The command `build_mcp_entry` writes: the absolute path to the current
    /// executable, or the `MCP_COMMAND` fallback when it can't be resolved.
    /// (Absolute path so AI clients spawning a non-login shell still find it.)
    fn expected_command() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| MCP_COMMAND.to_string())
    }

    // ── build_mcp_entry / block ────────────────────────────────────────────────

    #[test]
    fn mcp_entry_has_correct_command_and_args() {
        let entry = build_mcp_entry();
        assert_eq!(entry["command"], expected_command());
        let args = entry["args"].as_array().unwrap();
        assert_eq!(args[0], "mcp");
        assert_eq!(args[1], "serve");
    }

    #[test]
    fn stable_command_rewrites_homebrew_cellar() {
        // Linuxbrew
        assert_eq!(
            stable_command_from_exe(
                "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk",
                |_| true,
            ),
            "/home/linuxbrew/.linuxbrew/bin/vectorhawk"
        );
        // macOS Apple-silicon Homebrew
        assert_eq!(
            stable_command_from_exe(
                "/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk",
                |_| true,
            ),
            "/opt/homebrew/bin/vectorhawk"
        );
    }

    #[test]
    fn stable_command_keeps_non_cellar_paths() {
        // A plain install symlink is already stable — leave it.
        let bin = "/usr/local/bin/vectorhawk";
        assert_eq!(stable_command_from_exe(bin, |_| true), bin);
        // A cargo dev build is not a Homebrew layout — leave it.
        let dev = "/home/dev/vectorhawkd/target/release/vectorhawk";
        assert_eq!(stable_command_from_exe(dev, |_| true), dev);
    }

    #[test]
    fn stable_command_falls_back_when_symlink_missing() {
        // If the <prefix>/bin symlink doesn't exist, keep the real Cellar path
        // rather than inventing a dead one.
        let cellar = "/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk";
        assert_eq!(stable_command_from_exe(cellar, |_| false), cellar);
    }

    #[test]
    fn mcp_servers_block_nests_correctly() {
        let block = build_mcp_servers_block();
        let entry = &block[MCP_SERVER_NAME];
        assert_eq!(entry["command"], expected_command());
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
            format: ConfigFormat::Json,
        };

        // write_mcp_entry's restore-journal side effect resolves the real
        // data dir via $HOME unless redirected — keep it inside `tmp`.
        with_fake_home(&tmp, || write_mcp_entry(&config)).expect("write should succeed");

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["vectorhawk"]["command"],
            expected_command()
        );
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
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let config = ClientConfig {
            name: "Test Client".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).expect("write should succeed");

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert_eq!(
            json["mcpServers"]["vectorhawk"]["command"],
            expected_command()
        );
        assert_eq!(json["mcpServers"]["other-tool"]["command"], "other");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── write_mcp_entry restore-journal integration ───────────────────────────
    // Uses record_config_edit_journal_in directly (root_dir injected) so these
    // tests never touch the real platform data dir.

    #[cfg(feature = "daemon")]
    fn journal_for(
        root_dir: &camino::Utf8Path,
    ) -> vectorhawkd_core::restore_journal::RestoreJournal {
        vectorhawkd_core::restore_journal::RestoreJournal::new(root_dir.to_owned())
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn record_config_edit_journal_backs_up_pre_existing_file_and_appends_entry() {
        let tmp = temp_root("journal-existing");
        let config_path = tmp.join("claude.json");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other":{"command":"other"}}}"#,
        )
        .unwrap();

        let root_dir = camino::Utf8PathBuf::from_path_buf(tmp.join("vh-root")).unwrap();

        let config = ClientConfig {
            name: "Claude Code".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        record_config_edit_journal_in(&config, root_dir.clone());

        let journal = journal_for(&root_dir);
        let entries = journal.read_all().unwrap();
        assert_eq!(entries.len(), 1, "one config_edit entry should be appended");

        let entry = &entries[0];
        assert_eq!(
            entry.op,
            vectorhawkd_core::restore_journal::JournalOp::ConfigEdit
        );
        assert_eq!(
            entry.source,
            vectorhawkd_core::restore_journal::JournalSource::Native
        );
        assert_eq!(entry.target_path, config_path.to_string_lossy());
        assert_eq!(entry.slug.as_deref(), Some(MCP_SERVER_NAME));
        assert_eq!(entry.client.as_deref(), Some("Claude Code"));
        assert_eq!(entry.detail["server_key"], MCP_SERVER_NAME);
        assert_eq!(entry.detail["mcp_key"], "mcpServers");

        let backup_path = entry
            .backup_path
            .as_ref()
            .expect("pre-existing file must be backed up");
        assert_eq!(
            fs::read_to_string(backup_path).unwrap(),
            r#"{"mcpServers":{"other":{"command":"other"}}}"#,
            "backup must capture the pre-edit content"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn record_config_edit_journal_omits_backup_when_file_did_not_exist() {
        let tmp = temp_root("journal-new-file");
        let config_path = tmp.join("cursor").join("mcp.json");
        fs::create_dir_all(&tmp).unwrap();
        // config_path deliberately does not exist yet.

        let root_dir = camino::Utf8PathBuf::from_path_buf(tmp.join("vh-root")).unwrap();
        let config = ClientConfig {
            name: "Cursor".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        record_config_edit_journal_in(&config, root_dir.clone());

        let entries = journal_for(&root_dir).read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].backup_path.is_none(),
            "no backup_path when the file did not exist before — uninstall should delete instead"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn record_config_edit_journal_second_call_reuses_original_backup() {
        let tmp = temp_root("journal-reuse");
        let config_path = tmp.join("claude.json");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(&config_path, "original content").unwrap();

        let root_dir = camino::Utf8PathBuf::from_path_buf(tmp.join("vh-root")).unwrap();
        let config = ClientConfig {
            name: "Claude Code".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        // First edit: backs up "original content".
        record_config_edit_journal_in(&config, root_dir.clone());
        // Simulate the actual write happening (file now VectorHawk-modified).
        fs::write(&config_path, "vectorhawk-modified content").unwrap();
        // Second edit (e.g. a later `mcp setup` re-run): must NOT re-back-up
        // the now-modified content over the pristine original.
        record_config_edit_journal_in(&config, root_dir.clone());

        let entries = journal_for(&root_dir).read_all().unwrap();
        assert_eq!(entries.len(), 2, "each call appends its own entry");
        let backup_1 = entries[0].backup_path.clone().unwrap();
        let backup_2 = entries[1].backup_path.clone().unwrap();
        assert_eq!(
            backup_1, backup_2,
            "second call must reuse the first backup path"
        );
        assert_eq!(
            fs::read_to_string(&backup_1).unwrap(),
            "original content",
            "backup must still hold the pristine pre-VectorHawk content"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── detect_ai_clients — 6-client matrix ───────────────────────────────────
    // Tests use detect_ai_clients_in(home) to avoid racing on HOME env var.

    #[test]
    fn detect_claude_code_when_dir_exists() {
        let tmp = temp_root("detect-cc");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let found = clients.iter().find(|c| c.name == "Claude Code");
        assert!(found.is_some(), "Claude Code should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn detect_claude_code_via_app_bundle_on_fresh_machine() {
        let tmp = temp_root("detect-cc-app");
        let home = tmp.join("home");
        let sys = tmp.join("sys");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(sys.join("Applications").join("Claude.app")).unwrap();

        // Home has no .claude dir or .claude.json — simulates a machine where
        // Claude is installed but has never been launched.
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".claude.json").exists());

        let clients = detect_ai_clients_in(&home, &sys);
        let found = clients.iter().find(|c| c.name == "Claude Code");
        assert!(
            found.is_some(),
            "Claude Code must be detected via app bundle"
        );
        assert_eq!(found.unwrap().config_path, home.join(".claude.json"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detect_claude_code_via_app_bundle_on_fresh_machine() {
        let tmp = temp_root("detect-cc-app");
        let home = tmp.join("home");
        let sys = tmp.join("sys");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(sys.join("usr/local/bin")).unwrap();
        fs::write(sys.join("usr/local/bin/claude"), b"").unwrap();

        assert!(!home.join(".claude").exists());
        assert!(!home.join(".claude.json").exists());

        let clients = detect_ai_clients_in(&home, &sys);
        let found = clients.iter().find(|c| c.name == "Claude Code");
        assert!(found.is_some(), "Claude Code must be detected via binary");
        assert_eq!(found.unwrap().config_path, home.join(".claude.json"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_cursor_when_dir_exists() {
        let tmp = temp_root("detect-cursor");
        fs::create_dir_all(tmp.join(".cursor")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let found = clients.iter().find(|c| c.name == "Cursor");
        assert!(found.is_some(), "Cursor should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_windsurf_when_dir_exists() {
        let tmp = temp_root("detect-windsurf");
        fs::create_dir_all(tmp.join(".codeium").join("windsurf")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
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

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let found = clients.iter().find(|c| c.name == "Claude Desktop");
        assert!(found.is_some(), "Claude Desktop should be detected");
        assert_eq!(found.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_vscode_uses_mcp_json_and_servers_key() {
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

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let found = clients.iter().find(|c| c.name == "VS Code");
        assert!(found.is_some(), "VS Code should be detected");
        let found = found.unwrap();
        assert_eq!(
            found.mcp_key, "servers",
            "VS Code's user-level MCP config uses a top-level `servers` key, not `mcpServers`"
        );
        assert_eq!(
            found.config_path,
            vscode_dir.join("mcp.json"),
            "VS Code's user-level MCP config lives in Code/User/mcp.json, not settings.json"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_gemini_cli_when_dir_exists() {
        let tmp = temp_root("detect-gemini");
        fs::create_dir_all(tmp.join(".gemini")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
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

        let clients = detect_ai_clients_in(&tmp, &tmp);
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
            format: ConfigFormat::Json,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["vectorhawk"]["command"],
            expected_command()
        );

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
            format: ConfigFormat::Json,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["vectorhawk"]["command"],
            expected_command()
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn vscode_write_has_correct_mcp_entry_shape() {
        let tmp = temp_root("vscode-write");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("mcp.json");

        let config = ClientConfig {
            name: "VS Code".to_string(),
            config_path: config_path.clone(),
            mcp_key: "servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["servers"]["vectorhawk"]["type"], "stdio",
            "VS Code's mcp.json requires an explicit `type` field per server"
        );
        assert_eq!(json["servers"]["vectorhawk"]["command"], expected_command());
        assert_eq!(json["servers"]["vectorhawk"]["args"][0], "mcp");
        assert_eq!(json["servers"]["vectorhawk"]["args"][1], "serve");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn other_clients_write_has_no_type_field() {
        // `type` is a VS Code-specific requirement; other clients' written
        // entries must stay exactly `{command, args}` — verified against the
        // actual file `write_mcp_entry` produces, not just `build_mcp_entry`
        // in isolation.
        let tmp = temp_root("cursor-no-type");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("mcp.json");

        let config = ClientConfig {
            name: "Cursor".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };
        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(json["mcpServers"]["vectorhawk"].get("type").is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── repair_stale_mcp_entries: VS Code targets mcp.json, not settings.json ──

    #[test]
    fn repair_rewrites_stale_vscode_entry_in_mcp_json_not_settings_json() {
        let tmp = temp_root("repair-vscode-mcp");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let mcp_json = vscode_mcp_json_path(&tmp).unwrap();
        fs::write(
            &mcp_json,
            r#"{"servers":{"vectorhawk":{"type":"stdio","command":"/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk","args":["mcp","serve"]}}}"#,
        )
        .unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            repaired.contains(&"VS Code".to_string()),
            "stale VS Code entry in mcp.json should be repaired, got: {repaired:?}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_json).unwrap()).unwrap();
        assert_eq!(json["servers"]["vectorhawk"]["command"], expected_command());
        assert_eq!(
            json["servers"]["vectorhawk"]["type"], "stdio",
            "repair goes through write_mcp_entry, which must keep the required `type` field"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── migrate_stale_vscode_settings_entry ───────────────────────────────────
    // Uses the real platform data dir via $HOME (redirected by `with_fake_home`)
    // to record a restore-journal entry, same as the `write_mcp_entry` tests —
    // hence `#[cfg(feature = "daemon")]` throughout, matching
    // `record_config_edit_journal`'s own gating.

    #[cfg(feature = "daemon")]
    #[test]
    fn migrate_removes_stale_vscode_settings_entry_when_json_is_strict() {
        let tmp = temp_root("migrate-strict");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let settings_path = vscode_settings_path(&tmp).unwrap();
        fs::write(
            &settings_path,
            r#"{"editor.fontSize": 14, "mcpServers": {"vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]}}}"#,
        )
        .unwrap();

        let removed = with_fake_home(&tmp, || migrate_stale_vscode_settings_entry(&tmp));
        assert!(removed, "stale entry should be found and removed");

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(
            json["mcpServers"].get("vectorhawk").is_none(),
            "vectorhawk key must be gone"
        );
        assert_eq!(
            json["editor.fontSize"], 14,
            "unrelated settings must survive"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn migrate_removes_stale_entry_from_jsonc_preserving_comments_and_trailing_commas() {
        // The common real-world case: a hand-edited settings.json with line
        // comments, a block comment, and a trailing comma. The card requires
        // the stale key be removed unconditionally here too — not skipped —
        // while every comment and the rest of the formatting survive intact.
        let tmp = temp_root("migrate-jsonc-preserve");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let settings_path = vscode_settings_path(&tmp).unwrap();
        let original = "{\n  // my favorite theme\n  \"workbench.colorTheme\": \"Dark+\",\n  /* block comment */\n  \"mcpServers\": {\"vectorhawk\": {\"command\": \"vectorhawk\"}},\n}\n";
        fs::write(&settings_path, original).unwrap();

        let removed = with_fake_home(&tmp, || migrate_stale_vscode_settings_entry(&tmp));
        assert!(
            removed,
            "stale entry must be removed even from a JSONC file with comments"
        );

        let after = fs::read_to_string(&settings_path).unwrap();
        assert!(
            after.contains("// my favorite theme"),
            "line comment must survive: {after:?}"
        );
        assert!(
            after.contains("/* block comment */"),
            "block comment must survive: {after:?}"
        );
        assert!(
            after.contains("\"workbench.colorTheme\": \"Dark+\""),
            "unrelated setting must survive untouched: {after:?}"
        );
        assert!(
            !after.contains("vectorhawk"),
            "stale vectorhawk entry must be gone: {after:?}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn migrate_leaves_genuinely_invalid_json_untouched() {
        // Not even valid JSONC — an unmatched brace. Even the lenient CST
        // parser must fail closed here rather than guess at a repair.
        let tmp = temp_root("migrate-invalid");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let settings_path = vscode_settings_path(&tmp).unwrap();
        let original = r#"{"mcpServers": {"vectorhawk": {"command": "vectorhawk"}}"#; // missing closing brace
        fs::write(&settings_path, original).unwrap();

        let removed = with_fake_home(&tmp, || migrate_stale_vscode_settings_entry(&tmp));
        assert!(!removed, "must not touch a file that isn't valid JSONC");

        let after = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(
            after, original,
            "invalid settings.json must be left byte-for-byte untouched"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn migrate_is_noop_when_no_stale_entry_present() {
        let tmp = temp_root("migrate-noop");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let settings_path = vscode_settings_path(&tmp).unwrap();
        fs::write(&settings_path, r#"{"editor.fontSize": 14}"#).unwrap();

        let removed = with_fake_home(&tmp, || migrate_stale_vscode_settings_entry(&tmp));
        assert!(!removed);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn migrate_is_noop_when_settings_file_does_not_exist() {
        let tmp = temp_root("migrate-missing");
        fs::create_dir_all(&tmp).unwrap();
        let removed = with_fake_home(&tmp, || migrate_stale_vscode_settings_entry(&tmp));
        assert!(!removed);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn repair_also_migrates_stale_vscode_settings_entry() {
        let tmp = temp_root("repair-migrate");
        let vscode_dir = vscode_user_dir(&tmp).unwrap();
        fs::create_dir_all(&vscode_dir).unwrap();
        let settings_path = vscode_settings_path(&tmp).unwrap();
        fs::write(
            &settings_path,
            r#"{"mcpServers": {"vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]}}}"#,
        )
        .unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            repaired.iter().any(|c| c.contains("VS Code")),
            "repair pass should report the settings.json migration, got: {repaired:?}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(json["mcpServers"].get("vectorhawk").is_none());

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

    // ── remove_mcp_entry ──────────────────────────────────────────────────────

    #[test]
    fn remove_mcp_entry_removes_existing_entry() {
        let tmp = temp_root("remove-entry");
        let config_path = tmp.join("config.json");
        fs::create_dir_all(&tmp).unwrap();

        let config = ClientConfig {
            name: "Test".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();
        assert!(is_vectorhawk_configured(
            &config_path,
            "mcpServers",
            ConfigFormat::Json
        ));

        let removed = remove_mcp_entry(&config).unwrap();
        assert!(removed, "should report entry was removed");
        assert!(
            !is_vectorhawk_configured(&config_path, "mcpServers", ConfigFormat::Json),
            "entry should be gone after remove"
        );

        // Other keys in the file should survive
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            json.get("mcpServers").is_some(),
            "mcpServers key should still exist (just empty)"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remove_mcp_entry_returns_false_when_absent() {
        let tmp = temp_root("remove-absent");
        let config_path = tmp.join("config.json");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            &config_path,
            r#"{"mcpServers": {"other-tool": {"command": "other"}}}"#,
        )
        .unwrap();

        let config = ClientConfig {
            name: "Test".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        let removed = remove_mcp_entry(&config).unwrap();
        assert!(!removed, "should report nothing was removed");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remove_mcp_entry_returns_false_for_missing_file() {
        let tmp = temp_root("remove-missing-file");
        let config = ClientConfig {
            name: "Test".to_string(),
            config_path: tmp.join("does-not-exist.json"),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };

        let removed = remove_mcp_entry(&config).unwrap();
        assert!(!removed);
    }

    #[test]
    fn remove_mcp_entry_preserves_other_mcp_entries() {
        let tmp = temp_root("remove-preserves");
        let config_path = tmp.join("config.json");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            &config_path,
            r#"{"mcpServers": {"vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]}, "other": {"command": "other"}}}"#,
        )
        .unwrap();

        let config = ClientConfig {
            name: "Test".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: true,
            format: ConfigFormat::Json,
        };

        let removed = remove_mcp_entry(&config).unwrap();
        assert!(removed);

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            json["mcpServers"].get("other").is_some(),
            "other MCP entry should be preserved"
        );
        assert!(
            json["mcpServers"].get("vectorhawk").is_none(),
            "vectorhawk entry should be removed"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── command_needs_repair ──────────────────────────────────────────────────

    #[test]
    fn command_needs_repair_when_path_does_not_exist() {
        assert!(command_needs_repair(
            "/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk",
            |_| false,
        ));
    }

    #[test]
    fn command_needs_repair_when_versioned_cellar_path_exists() {
        // Old Cellar dir hasn't been pruned yet (e.g. mid-upgrade), but it's
        // still a versioned path that must be rewritten to the stable one.
        assert!(command_needs_repair(
            "/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk",
            |_| true,
        ));
    }

    #[test]
    fn command_does_not_need_repair_when_stable_path_exists() {
        assert!(!command_needs_repair(
            "/home/linuxbrew/.linuxbrew/bin/vectorhawk",
            |_| true,
        ));
    }

    #[test]
    fn command_does_not_need_repair_for_bare_command() {
        // Not an absolute path — nothing for the repair pass to rewrite.
        assert!(!command_needs_repair("vectorhawk", |_| false));
    }

    // ── repair_stale_mcp_entries ──────────────────────────────────────────────

    #[test]
    fn repair_rewrites_stale_versioned_cellar_command() {
        let tmp = temp_root("repair-stale");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            r#"{"mcpServers":{"vectorhawk":{"command":"/home/linuxbrew/.linuxbrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk","args":["mcp","serve"]}}}"#,
        )
        .unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert_eq!(
            repaired,
            vec!["Claude Code".to_string()],
            "the stale Claude Code entry should be repaired"
        );

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["vectorhawk"]["command"],
            expected_command(),
            "command should be rewritten to the current stable path"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repair_leaves_current_command_untouched() {
        let tmp = temp_root("repair-current");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config_path = tmp.join(".claude.json");
        let existing = serde_json::json!({
            "mcpServers": {
                "vectorhawk": {"command": expected_command(), "args": ["mcp", "serve"]}
            }
        });
        fs::write(&config_path, serde_json::to_string(&existing).unwrap()).unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            repaired.is_empty(),
            "an already-current command must not be rewritten"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repair_is_idempotent() {
        let tmp = temp_root("repair-idempotent");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            r#"{"mcpServers":{"vectorhawk":{"command":"/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk","args":["mcp","serve"]}}}"#,
        )
        .unwrap();

        let first = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert_eq!(first, vec!["Claude Code".to_string()]);

        let second = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            second.is_empty(),
            "a second repair pass must be a no-op once the entry is stable"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repair_preserves_other_entries_and_only_touches_vectorhawk() {
        let tmp = temp_root("repair-preserves");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            r#"{"mcpServers":{"vectorhawk":{"command":"/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk","args":["mcp","serve"]},"other-tool":{"command":"/does/not/exist/other","args":[]}}}"#,
        )
        .unwrap();

        with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["other-tool"]["command"], "/does/not/exist/other",
            "unrelated MCP entries must never be touched by this repair pass"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repair_skips_missing_config_files() {
        let tmp = temp_root("repair-missing");
        fs::create_dir_all(&tmp).unwrap();
        // No client config files exist at all.
        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(repaired.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Codex CLI — TOML client ────────────────────────────────────────────────
    // Codex writes to `~/.codex/config.toml` under `[mcp_servers.<name>]`,
    // confirmed against openai/codex `codex-rs/config/src/{mcp_edit,mcp_types}.rs`
    // — a top-level `mcp_servers` table of `{command, args}` stdio entries.
    // All gated `#[cfg(feature = "daemon")]`: Codex detection itself only
    // exists under that feature (needs `toml_edit`), same as the VS Code
    // settings.json migration tests above.

    #[cfg(feature = "daemon")]
    #[test]
    fn detect_codex_when_dir_exists() {
        let tmp = temp_root("detect-codex");
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let found = clients.iter().find(|c| c.name == "Codex");
        assert!(found.is_some(), "Codex should be detected");
        let found = found.unwrap();
        assert_eq!(found.mcp_key, "mcp_servers");
        assert_eq!(found.format, ConfigFormat::Toml);
        assert_eq!(found.config_path, tmp.join(".codex").join("config.toml"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn detect_codex_absent_when_dir_missing() {
        let tmp = temp_root("detect-codex-absent");
        fs::create_dir_all(&tmp).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
        assert!(!clients.iter().any(|c| c.name == "Codex"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_write_creates_toml_entry_preserving_existing_content() {
        let tmp = temp_root("codex-write");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "model = \"gpt-5.1\"\n\n# Consider setting [mcp_servers] here!\n",
        )
        .unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).expect("write should succeed");

        let after = fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = after.parse().expect("output must be valid TOML");
        assert_eq!(
            doc["mcp_servers"]["vectorhawk"]["command"].as_str(),
            Some(expected_command().as_str())
        );
        let args = doc["mcp_servers"]["vectorhawk"]["args"]
            .as_array()
            .expect("args must be an array");
        assert_eq!(args.get(0).and_then(|v| v.as_str()), Some("mcp"));
        assert_eq!(args.get(1).and_then(|v| v.as_str()), Some("serve"));

        assert!(
            after.contains("model = \"gpt-5.1\""),
            "pre-existing top-level keys must survive: {after:?}"
        );
        assert!(
            after.contains("# Consider setting [mcp_servers] here!"),
            "comments must survive the format-preserving TOML edit: {after:?}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_write_into_fresh_file_creates_parent_dirs() {
        let tmp = temp_root("codex-write-fresh");
        let config_path = tmp.join(".codex").join("config.toml");
        // Deliberately don't create ~/.codex — write_mcp_entry must create it.

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config)).expect("write should succeed");

        let doc: toml_edit::DocumentMut = fs::read_to_string(&config_path)
            .unwrap()
            .parse()
            .expect("output must be valid TOML");
        assert_eq!(
            doc["mcp_servers"]["vectorhawk"]["command"].as_str(),
            Some(expected_command().as_str())
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_already_configured_becomes_true_after_write() {
        let tmp = temp_root("codex-idempotent");
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let before = with_fake_home(&tmp, || detect_ai_clients_in(&tmp, &tmp));
        let codex_before = before.iter().find(|c| c.name == "Codex").unwrap();
        assert!(
            !codex_before.already_configured,
            "must not be configured before the first write"
        );

        with_fake_home(&tmp, || write_mcp_entry(codex_before)).unwrap();

        let after = with_fake_home(&tmp, || detect_ai_clients_in(&tmp, &tmp));
        let codex_after = after.iter().find(|c| c.name == "Codex").unwrap();
        assert!(
            codex_after.already_configured,
            "a second `mcp setup` run must see Codex as already configured"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_remove_mcp_entry_removes_only_vectorhawk_table() {
        let tmp = temp_root("codex-remove");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "model = \"gpt-5.1\"\n\n[mcp_servers.other-tool]\ncommand = \"other\"\nargs = []\n\n[mcp_servers.vectorhawk]\ncommand = \"/opt/homebrew/bin/vectorhawk\"\nargs = [\"mcp\", \"serve\"]\n",
        )
        .unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: true,
            format: ConfigFormat::Toml,
        };

        let removed = remove_mcp_entry(&config).expect("remove should succeed");
        assert!(removed);

        let after = fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = after.parse().unwrap();
        assert!(
            doc["mcp_servers"].get("vectorhawk").is_none(),
            "vectorhawk table must be gone: {after:?}"
        );
        assert_eq!(
            doc["mcp_servers"]["other-tool"]["command"].as_str(),
            Some("other"),
            "the user's own server entry must survive"
        );
        assert!(
            after.contains("model = \"gpt-5.1\""),
            "unrelated top-level keys must survive"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_remove_mcp_entry_is_noop_when_absent() {
        let tmp = temp_root("codex-remove-noop");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(&config_path, "model = \"gpt-5.1\"\n").unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        let removed = remove_mcp_entry(&config).expect("remove should succeed");
        assert!(!removed);
        assert_eq!(
            fs::read_to_string(&config_path).unwrap(),
            "model = \"gpt-5.1\"\n"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Codex — inline-table `mcp_servers` (review finding, Fix Round 1) ──────
    // `mcp_servers = { foo = {...} }` is valid TOML and Codex reads it fine
    // (`mcp_types.rs`'s `try_into::<BTreeMap<...>>()` doesn't care about the
    // source table's syntax) — `as_table_mut()` alone doesn't see it (only
    // matches `Item::Table`, not `Item::Value(Value::InlineTable)`), so the
    // write/remove paths must use `as_table_like_mut()` instead.

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_write_into_inline_table_mcp_servers_preserves_style_and_siblings() {
        let tmp = temp_root("codex-write-inline");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "model = \"gpt-5.1\"\nmcp_servers = { other-tool = { command = \"other\", args = [] } }\n",
        )
        .unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        with_fake_home(&tmp, || write_mcp_entry(&config))
            .expect("write must succeed against an inline-table mcp_servers");

        let after = fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = after.parse().expect("output must be valid TOML");
        assert_eq!(
            doc["mcp_servers"]["vectorhawk"]["command"].as_str(),
            Some(expected_command().as_str())
        );
        assert_eq!(
            doc["mcp_servers"]["other-tool"]["command"].as_str(),
            Some("other"),
            "the user's other inline-table entry must survive: {after:?}"
        );
        assert!(
            after.contains("model = \"gpt-5.1\""),
            "unrelated top-level keys must survive: {after:?}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_already_configured_true_after_write_into_inline_table() {
        // Idempotency (review finding covers "detection ... agree" too): a
        // second `mcp setup` run against an inline-table config must see
        // Codex as already configured, exactly like the standard-table case.
        let tmp = temp_root("codex-inline-idempotent");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "mcp_servers = { other-tool = { command = \"other\" } }\n",
        )
        .unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };
        with_fake_home(&tmp, || write_mcp_entry(&config)).unwrap();

        assert!(
            is_vectorhawk_configured(&config_path, "mcp_servers", ConfigFormat::Toml),
            "must detect vectorhawk as configured inside an inline mcp_servers table"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_remove_mcp_entry_removes_from_inline_table_preserving_siblings() {
        let tmp = temp_root("codex-remove-inline");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "mcp_servers = { other-tool = { command = \"other\" }, vectorhawk = { command = \"/opt/homebrew/bin/vectorhawk\", args = [\"mcp\", \"serve\"] } }\n",
        )
        .unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: true,
            format: ConfigFormat::Toml,
        };

        let removed = remove_mcp_entry(&config).expect("remove must succeed against inline table");
        assert!(removed);

        let after = fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = after.parse().unwrap();
        assert!(
            doc["mcp_servers"].get("vectorhawk").is_none(),
            "vectorhawk entry must be gone: {after:?}"
        );
        assert_eq!(
            doc["mcp_servers"]["other-tool"]["command"].as_str(),
            Some("other"),
            "sibling inline-table entry must survive: {after:?}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn repair_rewrites_stale_codex_command_in_inline_table() {
        let tmp = temp_root("repair-codex-inline");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "mcp_servers = { vectorhawk = { command = \"/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk\", args = [\"mcp\", \"serve\"] } }\n",
        )
        .unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            repaired.contains(&"Codex".to_string()),
            "stale Codex command inside an inline mcp_servers table should be repaired, got: {repaired:?}"
        );

        let doc: toml_edit::DocumentMut =
            fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["vectorhawk"]["command"].as_str(),
            Some(expected_command().as_str())
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_read_command_from_inline_table() {
        // Documents/verifies that `read_vectorhawk_command`'s TOML path
        // already handles inline tables via `Item::get`'s generic `Index`
        // impl — no `as_table_like` needed on the read side, unlike write/
        // remove. See the doc comment on `read_vectorhawk_command_toml`.
        let tmp = temp_root("codex-read-inline");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("config.toml");
        fs::write(
            &config_path,
            "mcp_servers = { vectorhawk = { command = \"/opt/homebrew/bin/vectorhawk\" } }\n",
        )
        .unwrap();

        let command = read_vectorhawk_command(&config_path, "mcp_servers", ConfigFormat::Toml);
        assert_eq!(command.as_deref(), Some("/opt/homebrew/bin/vectorhawk"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn codex_write_fails_gracefully_when_mcp_servers_is_not_a_table() {
        // Genuinely non-table values must still fail — the fix widens what
        // counts as "a table" (inline tables too), it doesn't make every
        // value acceptable.
        let tmp = temp_root("codex-write-not-a-table");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(&config_path, "mcp_servers = 5\n").unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        let err = with_fake_home(&tmp, || write_mcp_entry(&config))
            .expect_err("a non-table mcp_servers value must still be rejected");
        assert!(err.to_string().contains("isn't a table"), "got: {err:#}");
        assert_eq!(
            fs::read_to_string(&config_path).unwrap(),
            "mcp_servers = 5\n",
            "file must be left untouched on this error path"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn repair_rewrites_stale_codex_command() {
        let tmp = temp_root("repair-codex");
        let codex_dir = tmp.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.vectorhawk]\ncommand = \"/opt/homebrew/Cellar/vectorhawk/1.0.65/bin/vectorhawk\"\nargs = [\"mcp\", \"serve\"]\n",
        )
        .unwrap();

        let repaired = with_fake_home(&tmp, || repair_stale_mcp_entries_in(&tmp, &tmp));
        assert!(
            repaired.contains(&"Codex".to_string()),
            "stale Codex TOML command should be repaired, got: {repaired:?}"
        );

        let doc: toml_edit::DocumentMut =
            fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["vectorhawk"]["command"].as_str(),
            Some(expected_command().as_str())
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn record_config_edit_journal_backs_up_codex_toml_and_appends_entry() {
        // Mirrors `record_config_edit_journal_backs_up_pre_existing_file_and_appends_entry`
        // above, but for a TOML (Codex) client, so the restore-journal path
        // that `vectorhawk uninstall` relies on is verified for TOML too.
        let tmp = temp_root("journal-codex");
        let config_path = tmp.join("config.toml");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(&config_path, "model = \"gpt-5.1\"\n").unwrap();

        let root_dir = camino::Utf8PathBuf::from_path_buf(tmp.join("vh-root")).unwrap();

        let config = ClientConfig {
            name: "Codex".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };

        record_config_edit_journal_in(&config, root_dir.clone());

        let journal = journal_for(&root_dir);
        let entries = journal.read_all().unwrap();
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.client.as_deref(), Some("Codex"));
        assert_eq!(entry.detail["mcp_key"], "mcp_servers");
        let backup_path = entry
            .backup_path
            .as_ref()
            .expect("pre-existing file must be backed up");
        assert_eq!(
            fs::read_to_string(backup_path).unwrap(),
            "model = \"gpt-5.1\"\n",
            "backup must capture the pre-edit TOML content"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn detect_all_clients_including_codex_when_all_dirs_exist() {
        let tmp = temp_root("detect-all-with-codex");
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::create_dir_all(tmp.join(".cursor")).unwrap();
        fs::create_dir_all(tmp.join(".codeium").join("windsurf")).unwrap();
        fs::create_dir_all(tmp.join(".gemini")).unwrap();
        fs::create_dir_all(tmp.join(".codex")).unwrap();

        let clients = detect_ai_clients_in(&tmp, &tmp);
        let names: Vec<&str> = clients.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Codex"), "got: {names:?}");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── uninstall_claude_skills ───────────────────────────────────────────────

    #[test]
    fn uninstall_claude_skills_removes_installed_dirs() {
        let tmp = temp_root("skills-uninstall");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        install_claude_skills_in(&tmp).unwrap();

        let removed = uninstall_claude_skills_in(&tmp).unwrap();
        assert_eq!(removed.len(), 11, "should remove all 11 skill dirs");

        for name in &removed {
            let skill_dir = tmp.join(".claude").join("skills").join(name);
            assert!(!skill_dir.exists(), "{name} dir should be gone");
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn uninstall_claude_skills_returns_empty_when_not_installed() {
        let tmp = temp_root("skills-uninstall-empty");
        fs::create_dir_all(tmp.join(".claude").join("skills")).unwrap();

        let removed = uninstall_claude_skills_in(&tmp).unwrap();
        assert!(
            removed.is_empty(),
            "should return empty list when nothing installed"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
