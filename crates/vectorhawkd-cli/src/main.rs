//! `vectorhawk` — VectorHawk runner user CLI.
//!
//! Subcommand tree:
//!
//! ```text
//! vectorhawk doctor
//! vectorhawk skill list
//! vectorhawk skill install <skill-ref> [--registry-url <url>]
//! vectorhawk skill info <id>
//! vectorhawk skill run <id> --input <file> [--stub]
//! vectorhawk skill import <path>
//! vectorhawk plugin export <path> --format <fmt> [--output-dir <dir>]
//! vectorhawk plugin import <path> [--output-dir <dir>]
//! vectorhawk skill validate <path>
//! vectorhawk auth login [--registry-url <url>]
//! vectorhawk auth logout [--registry-url <url>]
//! vectorhawk auth status [--registry-url <url>]
//! vectorhawk mcp serve
//! vectorhawk mcp setup [--client <name>] [--dry-run]
//! vectorhawk mcp sync
//! vectorhawk mcp backends
//! vectorhawk daemon run [--foreground]
//! vectorhawk daemon install
//! vectorhawk daemon uninstall
//! ```
//!
//! `mcp serve` is the AI-client entry point — what `mcp setup` writes into
//! Claude Code / Cursor / etc. configs. The shim relays over `SocketBackend`
//! and, when the daemon is unreachable, returns a structured JSON-RPC error
//! containing install/restart instructions (M4 contract).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod install;

// ── CLI structure ─────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "vectorhawk",
    bin_name = "vectorhawk",
    version,
    about = "VectorHawk runner — governed AI platform"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print health status: daemon reachability, state directory, versions.
    Doctor {
        /// Registry URL to probe for reachability check.
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: Option<String>,
    },

    /// Skill management subcommands.
    #[command(subcommand)]
    Skill(SkillCommand),

    /// Authentication subcommands.
    #[command(subcommand)]
    Auth(AuthCommand),

    /// MCP subcommands (AI client integration).
    #[command(subcommand)]
    Mcp(McpCommand),

    /// Daemon lifecycle subcommands.
    #[command(subcommand)]
    Daemon(DaemonCommand),

    /// Plugin management subcommands (.mcpb / Claude Code plugin format).
    #[command(subcommand)]
    Plugin(PluginCommand),
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List all installed skills.
    List,

    /// Install a skill from a local bundle directory or a registry ID.
    ///
    /// SKILL_REF is treated as a local path if the path exists on disk;
    /// otherwise it is sent to the registry as an ID.
    Install {
        /// Local bundle directory path or registry skill ID.
        skill_ref: String,

        /// Symlink instead of copying the bundle (Unix only, dev mode).
        /// Ignored for registry installs.
        #[arg(long, default_value_t = false)]
        link: bool,

        /// Registry base URL (registry installs only).
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: Option<String>,
    },

    /// Show detailed information about an installed skill.
    Info {
        /// Skill ID to inspect.
        id: String,
    },

    /// Run an installed skill with the provided input.
    Run {
        /// Skill ID to run.
        id: String,

        /// Path to a JSON file containing the run input.
        #[arg(long, value_name = "FILE")]
        input: camino::Utf8PathBuf,

        /// Skip model calls; execute stub outputs only.
        #[arg(long, default_value_t = false)]
        stub: bool,
    },

    /// Import a SKILL.md file and scaffold a local bundle directory.
    Import {
        /// Path to the SKILL.md file.
        path: camino::Utf8PathBuf,

        /// Registry base URL used to call the scan endpoint.
        /// When omitted, scanning is skipped (offline / unauthenticated mode).
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: Option<String>,

        /// Bypass the scan warning and proceed even when the verdict is risky
        /// (Medium / High / Critical). Required when the scan flags a concern
        /// and you have reviewed the findings.
        #[arg(long, default_value_t = false)]
        confirm_risky: bool,
    },

    /// Validate a skill bundle directory.
    Validate {
        /// Path to the skill bundle directory.
        path: camino::Utf8PathBuf,
    },

    /// Scaffold a new SKILL.md-rooted skill.
    Init {
        /// Skill name (becomes the directory and the `name` frontmatter field).
        name: String,
        /// Target parent directory (default: current directory).
        #[arg(long)]
        output_dir: Option<camino::Utf8PathBuf>,
    },

    /// Publish a SKILL.md-rooted skill to the registry.
    Publish {
        /// Path to the skill directory (must contain SKILL.md).
        path: camino::Utf8PathBuf,
        /// Registry base URL.
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: String,
        /// Compile and validate without creating a registry entry.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Convert a legacy skill bundle to SKILL.md format.
    Convert {
        /// Path to the legacy bundle directory (must contain manifest.json).
        path: camino::Utf8PathBuf,
        /// Output directory for the new SKILL.md tree (default: <path>-skill-md/).
        #[arg(long)]
        output_dir: Option<camino::Utf8PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Log in to the VectorHawk registry using an OAuth browser flow.
    Login {
        /// Registry base URL.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },

    /// Log out of the VectorHawk registry.
    Logout {
        /// Registry base URL.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },

    /// Show current authentication status.
    Status {
        /// Registry base URL.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },

    /// Save a Personal Access Token for headless / CI environments.
    ///
    /// Creates a vh_pat_... token in the VectorHawk portal, then run:
    ///   vectorhawk auth token <vh_pat_...>
    ///
    /// Or set VECTORHAWK_TOKEN=<vh_pat_...> and restart the daemon.
    Token {
        /// The Personal Access Token to save (must start with vh_pat_).
        token: String,

        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Start the MCP relay shim — written into AI client configs by `mcp setup`.
    ///
    /// Connects to the vectorhawkd daemon over a Unix socket. If the daemon
    /// is unreachable, returns a JSON-RPC error containing install/restart
    /// instructions for every request — never a silent in-process fallback.
    Serve,

    /// Write the VectorHawk MCP entry into the specified AI client's config.
    Setup {
        /// Target AI client (currently only "claude-code" is supported in M0).
        #[arg(long)]
        client: Option<String>,

        /// Print the config entry that would be written without modifying any files.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Trigger an immediate registry sync via the daemon (M1.4).
    Sync,

    /// List registered backends and their health status (M1.4).
    Backends,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (debug / support repro only).
    Run {
        /// Keep the process in the foreground instead of daemonizing.
        #[arg(long, default_value_t = false)]
        foreground: bool,

        /// Override the registry URL for this daemon session.
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: Option<String>,

        /// Override the Ollama base URL for local LLM inference.
        #[arg(long, env = "VECTORHAWK_OLLAMA_URL")]
        ollama_url: Option<String>,

        /// Override the Ollama model tag to use for LLM steps.
        #[arg(long, env = "VECTORHAWK_OLLAMA_MODEL")]
        ollama_model: Option<String>,
    },

    /// Install the vectorhawkd LaunchAgent (macOS) or systemd user unit (Linux).
    Install,

    /// Remove the vectorhawkd LaunchAgent / systemd user unit.
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Export a VectorHawk plugin to Claude Code plugin or .mcpb Desktop Extension format.
    ///
    /// Use `--format mcpb` to produce a ZIP archive for Claude Desktop one-click install.
    /// Use `--format claude-code` to produce a Claude Code plugin directory.
    Export {
        /// Path to the VectorHawk plugin directory.
        path: camino::Utf8PathBuf,

        /// Export format: 'mcpb' for Desktop Extension archive, 'claude-code' for Claude Code plugin directory.
        #[arg(long, default_value = "mcpb")]
        format: String,

        /// Output directory where the exported artifact will be written (default: current directory).
        #[arg(long, value_name = "DIR")]
        output_dir: Option<camino::Utf8PathBuf>,
    },

    /// Import a Claude Code plugin directory or .mcpb Desktop Extension into VectorHawk plugin format.
    ///
    /// Auto-detects whether the input is a Claude Code plugin (directory with .claude-plugin/)
    /// or a .mcpb archive. Writes a VectorHawk plugin.json to the output directory.
    Import {
        /// Path to the Claude Code plugin directory or .mcpb file.
        path: camino::Utf8PathBuf,

        /// Output directory for the converted plugin (default: current directory).
        #[arg(long, value_name = "DIR")]
        output_dir: Option<camino::Utf8PathBuf>,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Structured logging to stderr. The AI client reads stdout (MCP JSON-RPC)
    // and ignores stderr, so tracing output is safe on all subcommands.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run(cli));

    if let Err(e) = result {
        eprintln!("vectorhawk: error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Doctor { registry_url } => cmd_doctor(registry_url.as_deref()).await,

        Command::Skill(SkillCommand::List) => cmd_skill_list().await,
        Command::Skill(SkillCommand::Install {
            skill_ref,
            link,
            registry_url,
        }) => cmd_skill_install(&skill_ref, link, registry_url.as_deref()).await,
        Command::Skill(SkillCommand::Info { id }) => cmd_skill_info(&id).await,
        Command::Skill(SkillCommand::Run { id, input, stub }) => {
            cmd_skill_run(&id, input, stub).await
        }
        Command::Skill(SkillCommand::Import {
            path,
            registry_url,
            confirm_risky,
        }) => cmd_skill_import(path, registry_url.as_deref(), confirm_risky).await,
        Command::Skill(SkillCommand::Validate { path }) => cmd_skill_validate(path).await,
        Command::Skill(SkillCommand::Init { name, output_dir }) => {
            cmd_skill_init(&name, output_dir.as_deref()).await
        }
        Command::Skill(SkillCommand::Publish {
            path,
            registry_url,
            dry_run,
        }) => cmd_skill_publish(path, &registry_url, dry_run).await,
        Command::Skill(SkillCommand::Convert { path, output_dir }) => {
            cmd_skill_convert(path, output_dir.as_deref()).await
        }

        Command::Auth(AuthCommand::Login { registry_url }) => cmd_auth_login(&registry_url).await,
        Command::Auth(AuthCommand::Logout { registry_url }) => cmd_auth_logout(&registry_url).await,
        Command::Auth(AuthCommand::Status { registry_url }) => cmd_auth_status(&registry_url).await,
        Command::Auth(AuthCommand::Token {
            token,
            registry_url,
        }) => cmd_auth_token(&token, &registry_url).await,

        Command::Mcp(McpCommand::Serve) => cmd_mcp_serve().await,
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            cmd_mcp_setup(client.as_deref(), dry_run).await
        }
        Command::Mcp(McpCommand::Sync) => cmd_mcp_sync().await,
        Command::Mcp(McpCommand::Backends) => cmd_mcp_backends().await,

        Command::Daemon(DaemonCommand::Run {
            foreground: _,
            registry_url,
            ollama_url,
            ollama_model,
        }) => cmd_daemon_run(registry_url, ollama_url, ollama_model).await,
        Command::Daemon(DaemonCommand::Install) => cmd_daemon_install().await,
        Command::Daemon(DaemonCommand::Uninstall) => cmd_daemon_uninstall().await,

        Command::Plugin(PluginCommand::Export {
            path,
            format,
            output_dir,
        }) => cmd_plugin_export(path, format, output_dir).await,
        Command::Plugin(PluginCommand::Import { path, output_dir }) => {
            cmd_plugin_import(path, output_dir).await
        }
    }
}

// ── doctor ────────────────────────────────────────────────────────────────────

async fn cmd_doctor(registry_url: Option<&str>) -> Result<()> {
    use vectorhawkd_core::app::VectorHawkApp;

    let version = env!("CARGO_PKG_VERSION");
    println!("VectorHawk runner version: {version}");

    // Bootstrap state to discover paths — do not fail doctor if this errors.
    match VectorHawkApp::bootstrap() {
        Ok(app) => {
            println!("State directory: {}", app.state.root_dir);
            println!("State database:  {}", app.state.db_path);

            let socket_path = app.state.socket_path();
            println!("Socket path:     {socket_path}");

            // Try to connect to the daemon socket with a 500 ms timeout.
            let daemon_status = probe_daemon_socket(socket_path.as_str()).await;
            println!("Daemon status:   {daemon_status}");

            // Sanity-check key workspace files.
            let db_exists = app.state.db_path.exists();
            println!(
                "state.db:        {}",
                if db_exists { "present" } else { "missing" }
            );

            // Extended doctor fields (M1.5): registry, audit queue, last sync.
            let effective_registry = registry_url.unwrap_or("https://app.vectorhawk.ai");
            let reachability = probe_registry(effective_registry).await;
            println!("Registry URL:    {effective_registry}");
            println!("Registry:        {reachability}");

            let audit_depth = query_audit_queue_depth(&app.state.db_path);
            println!("Audit queue:     {audit_depth}");

            let last_sync = query_last_sync_time(&app.state.db_path);
            println!("Last sync:       {last_sync}");

            // M2: daemon install status.
            match install::status() {
                Ok(install::InstallStatus::NotInstalled) => {
                    println!("Daemon install:  not installed");
                }
                Ok(install::InstallStatus::InstalledNotRunning { unit_path }) => {
                    println!("Daemon install:  installed but not running");
                    println!("  Unit path:     {unit_path}");
                }
                Ok(install::InstallStatus::InstalledAndRunning { unit_path }) => {
                    println!("Daemon install:  installed and running");
                    println!("  Unit path:     {unit_path}");
                }
                Err(e) => {
                    println!("Daemon install:  unknown ({e:#})");
                }
            }

            // M3: OAuth listener status — query the running daemon.
            let oauth_status = probe_oauth_listener_port(socket_path.as_str()).await;
            println!("OAuth listener:  {oauth_status}");

            // SEC3: scan endpoint reachability.
            let scan_status = probe_scan_endpoint(effective_registry).await;
            println!("Scan endpoint:   {scan_status}");
        }
        Err(e) => {
            eprintln!("warning: could not bootstrap state directory: {e:#}");
            println!("State directory: (unavailable)");
            println!("State database:  (unavailable)");
            println!("Socket path:     (unavailable)");
            println!("Daemon status:   unknown");
            println!("Registry:        unknown");
            println!("Audit queue:     unknown");
            println!("Last sync:       unknown");
            println!("OAuth listener:  not running");
            println!("Scan endpoint:   unknown");
        }
    }

    Ok(())
}

/// Attempt a Unix socket connection with a 500 ms deadline.
/// Returns a human-readable status string.
#[cfg(unix)]
async fn probe_daemon_socket(path: &str) -> String {
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    let path = path.to_string();
    match timeout(Duration::from_millis(500), UnixStream::connect(&path)).await {
        Ok(Ok(_)) => "running".to_string(),
        Ok(Err(e)) => format!("not running ({e})"),
        Err(_) => "not running (timeout)".to_string(),
    }
}

#[cfg(not(unix))]
async fn probe_daemon_socket(_path: &str) -> String {
    "unknown (daemon socket probe not supported on this platform)".to_string()
}

/// Query the running daemon for its OAuth callback listener port.
///
/// Issues `auth/get_oauth_listener_port` over the Unix socket (500 ms timeout).
/// Returns a human-readable status string:
/// - "running on port 39127"          — success
/// - "running on port 39134 (fallback)" — success on a non-default port
/// - "not running"                    — daemon unreachable or listener not bound
#[cfg(unix)]
async fn probe_oauth_listener_port(socket_path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    const OAUTH_PORT_BASE: u16 = 39127;

    let path = socket_path.to_string();
    let connected = match timeout(Duration::from_millis(500), UnixStream::connect(&path)).await {
        Ok(Ok(s)) => s,
        _ => return "not running".to_string(),
    };

    let (mut reader, mut writer) = connected.into_split();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "auth/get_oauth_listener_port",
        "params": {}
    });

    let body = match serde_json::to_vec(&request) {
        Ok(b) => b,
        Err(_) => return "not running".to_string(),
    };
    let len = body.len() as u32;

    if writer.write_all(&len.to_be_bytes()).await.is_err() {
        return "not running".to_string();
    }
    if writer.write_all(&body).await.is_err() {
        return "not running".to_string();
    }
    if writer.flush().await.is_err() {
        return "not running".to_string();
    }

    // Read response with a 500 ms timeout.
    let read_result = timeout(Duration::from_millis(500), async {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_body = vec![0u8; resp_len];
        reader.read_exact(&mut resp_body).await?;
        Ok::<Vec<u8>, tokio::io::Error>(resp_body)
    })
    .await;

    let resp_bytes = match read_result {
        Ok(Ok(b)) => b,
        _ => return "not running".to_string(),
    };

    let resp: serde_json::Value = match serde_json::from_slice(&resp_bytes) {
        Ok(v) => v,
        Err(_) => return "not running".to_string(),
    };

    if resp.get("error").is_some() {
        return "not running".to_string();
    }

    let port = match resp
        .get("result")
        .and_then(|r| r.get("port"))
        .and_then(|p| p.as_u64())
    {
        Some(p) => p as u16,
        None => return "not running".to_string(),
    };

    if port == OAUTH_PORT_BASE {
        format!("running on port {port}")
    } else {
        format!("running on port {port} (fallback)")
    }
}

#[cfg(not(unix))]
async fn probe_oauth_listener_port(_socket_path: &str) -> String {
    "not running (Unix sockets not supported on this platform)".to_string()
}

/// Probe the registry health endpoint with a 1 s timeout.
/// Returns a human-readable status string.
async fn probe_registry(base_url: &str) -> String {
    use tokio::time::{timeout, Duration};

    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("unavailable (client build error: {e})"),
    };

    match timeout(Duration::from_secs(2), client.get(&url).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => "reachable".to_string(),
        Ok(Ok(resp)) => format!("reachable (HTTP {})", resp.status()),
        Ok(Err(e)) => format!("unreachable ({e})"),
        Err(_) => "unreachable (timeout)".to_string(),
    }
}

/// Probe the registry's scan endpoint with a HEAD-style GET and a 2 s timeout.
///
/// The endpoint requires auth but we only need to confirm it's reachable;
/// any HTTP status other than 5xx or connection failure counts as reachable.
/// Returns a human-readable status string.
async fn probe_scan_endpoint(base_url: &str) -> String {
    use tokio::time::{timeout, Duration};

    let url = format!("{}/runner/scan", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("unreachable (client build error: {e})"),
    };

    match timeout(Duration::from_secs(2), client.get(&url).send()).await {
        Ok(Ok(resp)) => {
            let status = resp.status();
            if status.is_server_error() {
                format!("unreachable (scan endpoint returned HTTP {status})")
            } else {
                // 200, 401, 404, 405 etc all mean the server is listening.
                "reachable (verdict cache active)".to_string()
            }
        }
        Ok(Err(e)) => format!("unreachable ({e})"),
        Err(_) => "unreachable (scan warnings disabled — offline mode)".to_string(),
    }
}

/// Count unflushed audit events in the SQLite state DB.
/// Returns a display string — never panics on missing DB.
fn query_audit_queue_depth(db_path: &camino::Utf8PathBuf) -> String {
    use rusqlite::Connection;
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return "unknown (db open failed)".to_string(),
    };
    match conn.query_row(
        "SELECT COUNT(*) FROM audit_events WHERE uploaded = 0",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(n) => n.to_string(),
        Err(_) => "unknown (query failed)".to_string(),
    }
}

/// Return the most recent policy cache fetch timestamp as a human-readable string.
/// Falls back to "unknown" on any error.
fn query_last_sync_time(db_path: &camino::Utf8PathBuf) -> String {
    use rusqlite::Connection;
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return "unknown (db open failed)".to_string(),
    };
    match conn.query_row("SELECT MAX(fetched_at) FROM policy_cache", [], |row| {
        row.get::<_, Option<i64>>(0)
    }) {
        Ok(Some(ts)) => {
            // Convert unix timestamp to a human-readable UTC string if possible.
            format_unix_timestamp(ts)
        }
        Ok(None) => "never".to_string(),
        Err(_) => "unknown (query failed)".to_string(),
    }
}

/// Convert a Unix timestamp (seconds) to a human-readable UTC string.
fn format_unix_timestamp(ts: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let d = UNIX_EPOCH + Duration::from_secs(ts as u64);
    match d.duration_since(UNIX_EPOCH) {
        Ok(_) => {
            // chrono is in workspace deps
            use chrono::{DateTime, Utc};
            let dt = DateTime::<Utc>::from(d);
            dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()
        }
        Err(_) => ts.to_string(),
    }
}

// ── skill list ────────────────────────────────────────────────────────────────

async fn cmd_skill_list() -> Result<()> {
    use rusqlite::Connection;
    use vectorhawkd_core::state::AppState;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;

    let mut stmt = conn
        .prepare(
            "SELECT skill_id, active_version, current_status, installed_at \
             FROM installed_skills \
             ORDER BY skill_id",
        )
        .context("failed to prepare list query")?;

    struct Row {
        skill_id: String,
        active_version: String,
        current_status: String,
        installed_at: String,
    }

    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok(Row {
                skill_id: row.get(0)?,
                active_version: row.get(1)?,
                current_status: row.get(2)?,
                installed_at: row.get(3)?,
            })
        })
        .context("failed to execute list query")?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to collect skill rows")?;

    if rows.is_empty() {
        println!("No skills installed.");
        return Ok(());
    }

    println!(
        "{:<30} {:<12} {:<12} INSTALLED AT",
        "SKILL ID", "VERSION", "STATUS"
    );
    println!("{}", "-".repeat(75));
    for r in rows {
        println!(
            "{:<30} {:<12} {:<12} {}",
            r.skill_id, r.active_version, r.current_status, r.installed_at
        );
    }

    Ok(())
}

// ── skill install ─────────────────────────────────────────────────────────────

async fn cmd_skill_install(skill_ref: &str, link: bool, registry_url: Option<&str>) -> Result<()> {
    use vectorhawkd_core::state::AppState;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    // Treat skill_ref as a local path when the path exists on disk.
    let path = camino::Utf8Path::new(skill_ref);
    if path.exists() {
        use vectorhawkd_core::installer::{install_unpacked_skill, InstallMode};
        use vectorhawkd_manifest::SkillPackage;

        let pkg = SkillPackage::load_from_dir(path)
            .with_context(|| format!("failed to load skill bundle at {skill_ref}"))?;

        let mode = if link {
            InstallMode::Symlink
        } else {
            InstallMode::Copy
        };

        install_unpacked_skill(&state, &pkg, mode)
            .with_context(|| format!("failed to install skill '{}'", pkg.manifest.id))?;

        println!(
            "Installed skill '{}' version {}.",
            pkg.manifest.id, pkg.manifest.version
        );
    } else {
        // Treat as a registry ID; download and install.
        use vectorhawkd_core::registry::RegistryClient;
        use vectorhawkd_core::updater::install_from_registry;

        let url = registry_url.unwrap_or("https://app.vectorhawk.ai");
        let registry = RegistryClient::new(url);

        // install_from_registry issues blocking HTTP + SQLite calls; run on a
        // blocking thread so we don't stall the tokio current-thread executor.
        let skill_id = skill_ref.to_string();
        let version = tokio::task::spawn_blocking(move || {
            install_from_registry(&state, &registry, &skill_id, None)
        })
        .await
        .context("install task panicked")?
        .with_context(|| format!("failed to install '{skill_ref}' from registry"))?;

        println!("Installed skill '{skill_ref}' version {version} from registry.");
    }

    Ok(())
}

// ── skill info ────────────────────────────────────────────────────────────────

async fn cmd_skill_info(id: &str) -> Result<()> {
    use rusqlite::{Connection, OptionalExtension};
    use vectorhawkd_core::state::AppState;
    use vectorhawkd_manifest::SkillPackage;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;

    struct InstallRow {
        active_version: String,
        install_root: String,
        current_status: String,
        installed_at: String,
        channel: Option<String>,
    }

    let row: Option<InstallRow> = conn
        .query_row(
            "SELECT active_version, install_root, current_status, installed_at, channel \
             FROM installed_skills WHERE skill_id = ?1",
            [id],
            |row| {
                Ok(InstallRow {
                    active_version: row.get(0)?,
                    install_root: row.get(1)?,
                    current_status: row.get(2)?,
                    installed_at: row.get(3)?,
                    channel: row.get(4)?,
                })
            },
        )
        .optional()
        .context("failed to query installed_skills")?;

    let install = row.ok_or_else(|| anyhow::anyhow!("skill '{id}' is not installed"))?;

    println!("Skill ID:       {id}");
    println!("Version:        {}", install.active_version);
    println!("Status:         {}", install.current_status);
    println!(
        "Channel:        {}",
        install.channel.as_deref().unwrap_or("stable")
    );
    println!("Install root:   {}", install.install_root);
    println!("Installed at:   {}", install.installed_at);

    // Load manifest for additional details.
    let active_path = camino::Utf8PathBuf::from(&install.install_root).join("active");
    match SkillPackage::load_from_dir(&active_path) {
        Ok(pkg) => {
            println!(
                "Description:    {}",
                pkg.manifest.description.as_deref().unwrap_or("(none)")
            );
            println!("Publisher:      {}", pkg.manifest.publisher);
            println!("Workflow steps: {}", pkg.workflow.steps.len());
        }
        Err(e) => {
            eprintln!("warning: could not load skill manifest: {e:#}");
        }
    }

    // Show execution stats.
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), \
             COALESCE(AVG(latency_ms),0) \
             FROM execution_history WHERE skill_id = ?1",
        )
        .context("failed to prepare stats query")?;

    let (run_count, total_prompt, total_completion, avg_latency): (i64, i64, i64, f64) = stmt
        .query_row([id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .context("failed to query execution stats")?;

    println!("Run count:      {run_count}");
    println!("Total prompt tokens: {total_prompt}");
    println!("Total completion tokens: {total_completion}");
    println!("Avg latency (ms): {avg_latency:.0}");

    Ok(())
}

// ── skill run ─────────────────────────────────────────────────────────────────

async fn cmd_skill_run(id: &str, input_path: camino::Utf8PathBuf, stub: bool) -> Result<()> {
    use vectorhawkd_core::{
        executor::run_skill, ollama::OllamaClient, policy::MockPolicyClient, state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let policy = MockPolicyClient::default();

    let raw = std::fs::read_to_string(&input_path)
        .with_context(|| format!("failed to read {input_path}"))?;
    let input: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("input file {input_path} is not valid JSON"))?;

    let result = if stub {
        run_skill(&state, &policy, id, &input, None)
            .with_context(|| format!("failed to run skill '{id}' in stub mode"))?
    } else {
        let ollama_url = std::env::var("VECTORHAWK_OLLAMA_URL")
            .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let ollama_model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".to_string());
        let model = OllamaClient::new(ollama_url, ollama_model);
        run_skill(&state, &policy, id, &input, Some(&model))
            .with_context(|| format!("failed to run skill '{id}'"))?
    };

    println!("Skill:    {}", result.skill_id);
    println!("Version:  {}", result.version);
    println!("Steps:    {}", result.steps.len());
    println!("Latency:  {} ms", result.total_latency_ms);
    if result.total_prompt_tokens > 0 || result.total_completion_tokens > 0 {
        println!(
            "Tokens:   {} prompt / {} completion",
            result.total_prompt_tokens, result.total_completion_tokens
        );
    }
    println!();
    for step in &result.steps {
        let status = if step.output.is_some() { "ok" } else { "stub" };
        println!("[{}] {} ({})", status, step.id, step.step_type);
        if !step.note.is_empty() {
            println!("    {}", step.note);
        }
        if let Some(source) = &step.model_source {
            use vectorhawkd_core::model::ModelSource;
            let label = match source {
                ModelSource::Local(name) => format!("local Ollama ({name})"),
                ModelSource::Internal(name) => format!("internal VectorHawk model ({name})"),
                ModelSource::Provider(name) => format!("cloud provider ({name})"),
                ModelSource::McpSampling => "MCP sampling (delegated to AI client)".to_string(),
            };
            println!("    model:  {label}");
        }
        if let Some(out) = &step.output {
            println!(
                "    output: {}",
                serde_json::to_string(out).unwrap_or_default()
            );
        }
    }

    Ok(())
}

// ── skill import ──────────────────────────────────────────────────────────────

async fn cmd_skill_import(
    path: camino::Utf8PathBuf,
    registry_url: Option<&str>,
    confirm_risky: bool,
) -> Result<()> {
    use vectorhawkd_core::{
        auth::load_tokens,
        importer::import_local_skill_md_with_scan,
        scan::{HttpScanClient, NoOpScanClient, ScanClient},
        state::AppState,
    };

    // Attempt to build a scan client when we have both a registry URL and a
    // valid auth token. Fall back to the NoOp client (prints nothing, no HTTP
    // call) when either is missing — keeps the import path clean even offline.
    let effective_registry = registry_url.unwrap_or("https://app.vectorhawk.ai");
    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    let scan_client: Box<dyn ScanClient> =
        match load_tokens(&state, effective_registry).ok().flatten() {
            Some(tokens) if registry_url.is_some() => {
                Box::new(HttpScanClient::new(effective_registry, tokens.access_token))
            }
            _ => Box::new(NoOpScanClient),
        };

    let result = import_local_skill_md_with_scan(&path, Some(scan_client.as_ref()))
        .with_context(|| format!("failed to import SKILL.md at {path}"))?;

    // ── Display scan verdict badge ────────────────────────────────────────────

    if let Some(verdict) = &result.scan_verdict {
        let reset = "\x1b[0m";
        let color = verdict.verdict.ansi_color();
        let label = verdict.verdict.badge_label();
        println!("Scan: {color}{label}{reset}");

        if verdict.is_risky() {
            println!();
            let findings_text = verdict.format_findings();
            if !findings_text.is_empty() {
                print!("{findings_text}");
            }
        }

        if verdict.requires_confirmation() && !confirm_risky {
            anyhow::bail!(
                "Import blocked: verdict is {:?}. \
                 Review findings above and re-run with --confirm-risky to override.",
                verdict.verdict
            );
        }
    }

    // ── Display success ───────────────────────────────────────────────────────

    let bundle = &result.bundle;
    println!("Imported skill '{}'.", bundle.id);
    println!("Bundle created at: {}", bundle.output_dir);
    println!("Files written:");
    for f in &bundle.files {
        println!("  {f}");
    }

    Ok(())
}

// ── skill validate ────────────────────────────────────────────────────────────

async fn cmd_skill_validate(path: camino::Utf8PathBuf) -> Result<()> {
    use vectorhawkd_core::validator::validate_bundle;

    let report = validate_bundle(&path);

    let mut all_passed = true;
    for check in &report.checks {
        let status = if check.passed { "PASS" } else { "FAIL" };
        print!("  [{status}] {}", check.name);
        if let Some(detail) = &check.detail {
            print!(" — {detail}");
        }
        println!();
        if !check.passed {
            all_passed = false;
        }
    }

    if all_passed {
        println!("Validation passed.");
        Ok(())
    } else {
        anyhow::bail!("validation failed — see checks above")
    }
}

// ── skill init ────────────────────────────────────────────────────────────────

async fn cmd_skill_init(name: &str, output_dir: Option<&camino::Utf8Path>) -> Result<()> {
    use std::fs;

    let base = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| camino::Utf8PathBuf::from("."));
    let skill_dir = base.join(name);

    if skill_dir.exists() {
        anyhow::bail!("directory '{}' already exists", skill_dir);
    }

    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create directory '{skill_dir}'"))?;
    fs::create_dir(skill_dir.join("prompts")).context("failed to create prompts/ directory")?;

    let skill_md = format!(
        r#"---
name: {name}
description: "TODO: describe what this skill does"
license: Apache-2.0
vh_version: 0.1.0
vh_publisher: YOUR_PUBLISHER_ID
vh_permissions:
  network: none
  filesystem: none
  clipboard: none
vh_execution:
  timeout_ms: 30000
  memory_mb: 256
  sandbox: strict
---

# {name}

TODO: Write your system prompt here.

The user will provide: TODO

You should: TODO
"#
    );

    fs::write(skill_dir.join("SKILL.md"), &skill_md).context("failed to write SKILL.md")?;

    println!("Created skill at {skill_dir}/SKILL.md");
    println!();
    println!("Next steps:");
    println!("  1. Edit {skill_dir}/SKILL.md — fill in description, publisher, and prompt");
    println!("  2. vectorhawk skill validate {skill_dir}/");
    println!("  3. vectorhawk skill publish {skill_dir}/ --registry-url <url>");

    Ok(())
}

// ── skill publish ─────────────────────────────────────────────────────────────

async fn cmd_skill_publish(
    path: camino::Utf8PathBuf,
    registry_url: &str,
    dry_run: bool,
) -> Result<()> {
    use flate2::{write::GzEncoder, Compression};
    use tar::Builder;
    use vectorhawkd_core::{auth::load_tokens, registry::RegistryClient, state::AppState};

    if !path.join("SKILL.md").exists() {
        anyhow::bail!(
            "no SKILL.md found at '{}' — run 'vectorhawk skill init' to create one",
            path
        );
    }

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let tokens = load_tokens(&state, registry_url)
        .context("failed to load auth tokens")?
        .ok_or_else(|| anyhow::anyhow!("not authenticated — run 'vectorhawk auth login' first"))?;

    // Pack the skill directory into an in-memory tar.gz.
    let mut gz_buf: Vec<u8> = Vec::new();
    {
        let enc = GzEncoder::new(&mut gz_buf, Compression::default());
        let mut tar = Builder::new(enc);
        tar.append_dir_all(".", &path)
            .with_context(|| format!("failed to pack skill directory '{path}'"))?;
        tar.into_inner()
            .context("failed to finalize tar")?
            .finish()
            .context("failed to finalize gzip")?;
    }

    println!(
        "Packed {} ({} bytes) — uploading to {}...",
        path,
        gz_buf.len(),
        registry_url
    );

    if dry_run {
        println!("(dry run — skipping registry upload)");
        return Ok(());
    }

    let registry = RegistryClient::new(registry_url).with_auth(tokens.access_token);
    let resp = tokio::task::spawn_blocking(move || registry.compile_and_publish(gz_buf))
        .await
        .context("publish task panicked")?
        .context("publish failed")?;

    println!(
        "Published '{}' v{}",
        resp.frontmatter.name,
        resp.frontmatter.vh_version.as_deref().unwrap_or("?")
    );
    println!("Content hash: {}", resp.content_hash);
    if !resp.warnings.is_empty() {
        println!("Warnings:");
        for w in &resp.warnings {
            println!("  ! {w}");
        }
    }

    Ok(())
}

// ── skill convert ─────────────────────────────────────────────────────────────

async fn cmd_skill_convert(
    path: camino::Utf8PathBuf,
    output_dir: Option<&camino::Utf8Path>,
) -> Result<()> {
    use std::fs;
    use vectorhawkd_manifest::Manifest;

    let manifest_path = path.join("manifest.json");
    if !manifest_path.exists() {
        anyhow::bail!(
            "no manifest.json found at '{}' — this doesn't look like a legacy bundle",
            path
        );
    }

    let manifest_bytes = fs::read(&manifest_path).context("failed to read manifest.json")?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("failed to parse manifest.json")?;

    // Read system prompt from prompts/system.txt if it exists.
    let prompt_body = {
        let system_txt = path.join("prompts").join("system.txt");
        if system_txt.exists() {
            fs::read_to_string(&system_txt).context("failed to read prompts/system.txt")?
        } else {
            "TODO: write your system prompt here.\n".to_string()
        }
    };

    // Read workflow.yaml if it exists (for workflow_ref).
    let has_workflow_yaml = path.join("workflow.yaml").exists();

    // Build the SKILL.md frontmatter.
    let network = manifest.permissions.network.as_str();
    let filesystem = match manifest.permissions.filesystem {
        vectorhawkd_manifest::FilesystemAccess::None => "none",
        vectorhawkd_manifest::FilesystemAccess::ReadOnly => "read-only",
        vectorhawkd_manifest::FilesystemAccess::Full => "full",
    };
    let clipboard = match manifest.permissions.clipboard {
        vectorhawkd_manifest::ClipboardAccess::None => "none",
        vectorhawkd_manifest::ClipboardAccess::Read => "read",
        vectorhawkd_manifest::ClipboardAccess::Write => "write",
        vectorhawkd_manifest::ClipboardAccess::Full => "full",
    };
    let sandbox = match manifest.execution.sandbox {
        vectorhawkd_manifest::SandboxProfile::Strict => "strict",
        vectorhawkd_manifest::SandboxProfile::Relaxed => "relaxed",
        vectorhawkd_manifest::SandboxProfile::Unrestricted => "unrestricted",
    };
    let license = manifest.license.as_deref().unwrap_or("Apache-2.0");
    let description = manifest
        .description
        .as_deref()
        .unwrap_or("TODO: add description");

    // Escape any double-quotes in description before embedding in YAML string literal.
    let description_escaped = description.replace('"', "\\\"");
    let mut fm = format!(
        r#"---
name: {name}
description: "{description_escaped}"
license: {license}
vh_version: {version}
vh_publisher: {publisher}
vh_permissions:
  network: {network}
  filesystem: {filesystem}
  clipboard: {clipboard}
vh_execution:
  timeout_ms: {timeout_ms}
  memory_mb: {memory_mb}
  sandbox: {sandbox}
"#,
        name = manifest.name,
        description_escaped = description_escaped,
        license = license,
        version = manifest.version,
        publisher = manifest.publisher,
        network = network,
        filesystem = filesystem,
        clipboard = clipboard,
        timeout_ms = manifest.execution.timeout_ms,
        memory_mb = manifest.execution.memory_mb,
        sandbox = sandbox,
    );

    if has_workflow_yaml {
        fm.push_str("vh_workflow_ref: ./workflow.yaml\n");
    }

    fm.push_str("---\n\n");
    fm.push_str(&format!("# {}\n\n", manifest.name));
    fm.push_str(&prompt_body);

    // Strip any trailing slash before appending the suffix so that
    // `skill convert ./foo/` produces `./foo-skill-md/` not `./foo/-skill-md/`.
    let path_no_trailing = path.as_str().trim_end_matches('/');
    let dest = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| camino::Utf8PathBuf::from(format!("{path_no_trailing}-skill-md")));

    if dest.exists() {
        anyhow::bail!("output directory '{}' already exists", dest);
    }
    fs::create_dir_all(&dest)
        .with_context(|| format!("failed to create output directory '{dest}'"))?;

    fs::write(dest.join("SKILL.md"), &fm).context("failed to write SKILL.md")?;

    // Copy prompts/ if it exists.
    if path.join("prompts").exists() {
        let dest_prompts = dest.join("prompts");
        fs::create_dir_all(&dest_prompts).context("failed to create prompts/")?;
        for entry in fs::read_dir(path.join("prompts"))
            .context("failed to read prompts/")?
            .flatten()
        {
            let fname = entry.file_name();
            fs::copy(
                entry.path(),
                dest_prompts.join(fname.to_string_lossy().as_ref()),
            )
            .context("failed to copy prompt file")?;
        }
    }

    // Copy workflow.yaml if present.
    if has_workflow_yaml {
        fs::copy(path.join("workflow.yaml"), dest.join("workflow.yaml"))
            .context("failed to copy workflow.yaml")?;
    }

    println!("Converted to {dest}/SKILL.md");
    println!();
    println!("Next steps:");
    println!("  1. Review the generated SKILL.md");
    println!("  2. vectorhawk skill validate {dest}/");
    println!("  3. vectorhawk skill publish {dest}/ --registry-url <url>");

    Ok(())
}

// ── auth login ────────────────────────────────────────────────────────────────

/// Connect to the daemon Unix socket and perform a JSON-RPC auth login flow.
///
/// Steps:
///   1. Connect to the daemon socket (timeout 2 s). If unreachable, exit 2.
///   2. Issue `auth/get_oauth_listener_port` to learn the bound HTTP port.
///   3. Initiate PKCE flow with the loopback redirect URI.
///   4. Open browser to the authorization URL.
///   5. Issue `auth/wait_for_callback` and wait up to 300 s.
///   6. Exchange the code via `AuthClient::exchange_oauth_code`.
///   7. Save tokens and print user identity.
///
/// No stdin prompt remains. The paste-the-code path has been removed.
async fn cmd_auth_login(registry_url: &str) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use vectorhawkd_core::{
        auth::{save_tokens, AuthClient},
        state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let socket_path = state.socket_path();

    // Connect to the daemon socket with a 2-second timeout.
    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        UnixStream::connect(socket_path.as_std_path()),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(_)) | Err(_) => {
            eprintln!(
                "vectorhawk: auth login requires the running daemon — \
                 run `vectorhawk daemon install` first"
            );
            std::process::exit(2);
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // ── helpers: write/read one framed JSON-RPC call ──────────────────────────

    async fn send_rpc<W: AsyncWriteExt + Unpin>(
        writer: &mut W,
        request: &serde_json::Value,
    ) -> Result<()> {
        let body = serde_json::to_vec(request).context("failed to serialize JSON-RPC request")?;
        let len = body.len() as u32;
        writer
            .write_all(&len.to_be_bytes())
            .await
            .context("failed to write frame length")?;
        writer
            .write_all(&body)
            .await
            .context("failed to write frame body")?;
        writer.flush().await.context("failed to flush socket")?;
        Ok(())
    }

    async fn recv_rpc<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<serde_json::Value> {
        let mut len_buf = [0u8; 4];
        reader
            .read_exact(&mut len_buf)
            .await
            .context("failed to read frame length")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        reader
            .read_exact(&mut body)
            .await
            .context("failed to read frame body")?;
        serde_json::from_slice(&body).context("failed to parse JSON-RPC response")
    }

    // ── Step 2: get the OAuth listener port ──────────────────────────────────

    send_rpc(
        &mut writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "auth/get_oauth_listener_port",
            "params": {}
        }),
    )
    .await?;

    let port_resp = recv_rpc(&mut reader).await?;

    if let Some(err) = port_resp.get("error") {
        eprintln!(
            "vectorhawk: OAuth listener not running on the daemon: {}",
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
        );
        std::process::exit(2);
    }

    let port = port_resp
        .get("result")
        .and_then(|r| r.get("port"))
        .and_then(|p| p.as_u64())
        .context("auth/get_oauth_listener_port returned unexpected result shape")?
        as u16;

    // ── Step 3: initiate PKCE flow ────────────────────────────────────────────

    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/cli/callback");
    let client = AuthClient::new(registry_url);
    let init = client
        .initiate_oauth_flow_with_redirect(&redirect_uri)
        .context("failed to initiate OAuth PKCE flow")?;

    // ── Step 4: open browser ──────────────────────────────────────────────────

    println!("Opening browser for VectorHawk login...");
    println!();
    println!("If your browser does not open automatically, visit:");
    println!("  {}", init.auth_url);
    println!();

    open_browser(&init.auth_url);

    // ── Step 5: wait for callback via daemon ──────────────────────────────────

    println!("Waiting for authorization (up to 5 minutes)...");

    send_rpc(
        &mut writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "auth/wait_for_callback",
            "params": {
                "state": init.state,
                "timeout_secs": 300
            }
        }),
    )
    .await?;

    let callback_resp = recv_rpc(&mut reader).await?;

    if let Some(err) = callback_resp.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("auth/wait_for_callback failed: {msg}");
    }

    let code = callback_resp
        .get("result")
        .and_then(|r| r.get("code"))
        .and_then(|c| c.as_str())
        .context("auth/wait_for_callback returned unexpected result shape")?
        .to_string();

    // ── Step 6: exchange code for tokens ─────────────────────────────────────

    let tokens = client
        .exchange_oauth_code(&code, &init.code_verifier)
        .context("OAuth token exchange failed")?;

    // ── Step 7: save tokens and print identity ────────────────────────────────

    save_tokens(
        &state,
        registry_url,
        &tokens.access_token,
        &tokens.refresh_token,
    )
    .context("failed to save auth tokens")?;

    match client.me(&tokens.access_token) {
        Ok(user) => {
            println!("Logged in as {} ({}).", user.display_name, user.email);
        }
        Err(e) => {
            eprintln!("warning: could not fetch user info: {e:#}");
            println!("Login succeeded (token saved).");
        }
    }

    Ok(())
}

/// Open a URL in the system default browser.
/// Best-effort: logs a warning on failure but does not return an error.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn();
    }
    // On unknown platforms the URL is already printed; the user can copy-paste.
}

// ── auth logout ───────────────────────────────────────────────────────────────

async fn cmd_auth_logout(registry_url: &str) -> Result<()> {
    use vectorhawkd_core::{auth::clear_tokens, state::AppState};

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    clear_tokens(&state, registry_url).context("failed to clear auth tokens")?;
    println!("Logged out from {registry_url}.");
    Ok(())
}

// ── auth status ───────────────────────────────────────────────────────────────

async fn cmd_auth_status(registry_url: &str) -> Result<()> {
    use vectorhawkd_core::{
        auth::{load_tokens, AuthClient},
        state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    let tokens = load_tokens(&state, registry_url).context("failed to load auth tokens")?;

    match tokens {
        None => {
            println!("Not logged in to {registry_url}.");
        }
        Some(stored) => {
            let client = AuthClient::new(registry_url);
            match client.me(&stored.access_token) {
                Ok(user) => {
                    println!("Logged in as {} ({}).", user.display_name, user.email);
                    println!("Registry: {registry_url}");
                }
                Err(e) => {
                    println!("Token present for {registry_url} but validation failed: {e:#}");
                    println!("Try `vectorhawk auth login` to refresh.");
                }
            }
        }
    }

    Ok(())
}

// ── auth token ────────────────────────────────────────────────────────────────

async fn cmd_auth_token(token: &str, registry_url: &str) -> Result<()> {
    use vectorhawkd_core::{
        auth::{save_tokens, AuthClient},
        state::AppState,
    };

    if !token.starts_with("vh_pat_") {
        anyhow::bail!(
            "token must start with 'vh_pat_'. \
             Create one in the VectorHawk portal under Settings → Access Tokens."
        );
    }

    let state = AppState::bootstrap().context("failed to bootstrap application state")?;

    let token_owned = token.to_string();
    let reg_url = registry_url.to_string();
    let user = tokio::task::spawn_blocking(move || AuthClient::new(&reg_url).me(&token_owned))
        .await
        .context("validation task panicked")?
        .context("token validation failed — check that the token is valid and not revoked")?;

    save_tokens(&state, registry_url, token, token)
        .context("failed to save token to local state")?;

    println!("Authenticated as {} ({}).", user.display_name, user.email);
    println!("Token saved for {}.", registry_url);
    Ok(())
}

// ── mcp serve ─────────────────────────────────────────────────────────────────

async fn cmd_mcp_serve() -> Result<()> {
    // Delegate entirely to the shim library, which owns the per-frame read-loop
    // and the mid-session daemon-kill fallback logic (AC4).
    vectorhawkd_shim::run_shim().await
}

// ── mcp setup ────────────────────────────────────────────────────────────────

async fn cmd_mcp_setup(client: Option<&str>, dry_run: bool) -> Result<()> {
    use vectorhawkd_mcp::setup::{
        build_mcp_entry, detect_ai_clients, detect_claude_code, install_claude_skills,
        write_mcp_entry, MCP_SERVER_NAME,
    };

    // Resolve target client.  When no --client flag is given, configure all
    // detected clients; when a specific client name is given, restrict to that
    // one (kept for backwards compat / scripted use).
    let target = client.unwrap_or("auto");

    if target == "auto" {
        let entry = build_mcp_entry();
        let block = serde_json::json!({
            "mcpServers": {
                MCP_SERVER_NAME: entry
            }
        });

        if dry_run {
            println!("-- dry run: config entry that would be written --");
            println!("{}", serde_json::to_string_pretty(&block)?);
            let clients = detect_ai_clients();
            if clients.is_empty() {
                println!("\nNo supported AI clients detected.");
            } else {
                println!("\nDetected clients:");
                for c in &clients {
                    println!("  {} → {}", c.name, c.config_path.display());
                }
            }
            return Ok(());
        }

        // M2: provision the daemon before writing AI client config.
        {
            let install_status = tokio::task::spawn_blocking(install::status)
                .await
                .context("install status task panicked")?
                .context("failed to check daemon install status")?;

            let was_not_running = !matches!(
                install_status,
                install::InstallStatus::InstalledAndRunning { .. }
            );

            if was_not_running {
                tokio::task::spawn_blocking(install::ensure_installed)
                    .await
                    .context("ensure_installed task panicked")?
                    .context("failed to provision daemon")?;

                wait_for_daemon_socket(5_000).await;
            }
        }

        let clients = detect_ai_clients();
        if clients.is_empty() {
            anyhow::bail!(
                "No supported AI clients detected. \
                 Install Claude Code, Cursor, Windsurf, VS Code, Gemini CLI, or \
                 Claude Desktop first, or use --dry-run to preview the entry."
            );
        }

        let mut wrote_claude_code = false;
        for config in &clients {
            if config.already_configured {
                println!("{}: vectorhawk already configured — skipped.", config.name);
                continue;
            }
            write_mcp_entry(config).with_context(|| {
                format!(
                    "failed to write {} config at {}",
                    config.name,
                    config.config_path.display()
                )
            })?;
            println!(
                "{}: wrote vectorhawk MCP entry to {}.",
                config.name,
                config.config_path.display()
            );
            if config.name == "Claude Code" {
                wrote_claude_code = true;
            }
        }

        // Install slash commands whenever Claude Code is present (configured or not).
        let has_claude_code = clients.iter().any(|c| c.name == "Claude Code");
        if has_claude_code || wrote_claude_code {
            match install_claude_skills() {
                Ok(installed) if !installed.is_empty() => {
                    println!(
                        "Installed {} VectorHawk slash command(s) to ~/.claude/skills/.",
                        installed.len()
                    );
                }
                Ok(_) => {
                    println!("VectorHawk slash commands already up to date.");
                }
                Err(e) => {
                    eprintln!("warning: failed to install slash commands: {e:#}");
                }
            }
        }

        return Ok(());
    }

    // ── Legacy single-client path (--client <name>) ───────────────────────────

    match target {
        "claude-code" => {
            let entry = build_mcp_entry();
            let block = serde_json::json!({
                "mcpServers": {
                    MCP_SERVER_NAME: entry
                }
            });

            if dry_run {
                println!("-- dry run: config entry that would be written --");
                println!("{}", serde_json::to_string_pretty(&block)?);
                println!(
                    "\nTarget: {}",
                    detect_claude_code()
                        .map(|c| c.config_path.display().to_string())
                        .unwrap_or_else(|| {
                            "~/.claude.json (Claude Code not detected)".to_string()
                        })
                );
                return Ok(());
            }

            // M2: provision the daemon before writing AI client config.
            {
                let install_status = tokio::task::spawn_blocking(install::status)
                    .await
                    .context("install status task panicked")?
                    .context("failed to check daemon install status")?;

                let was_not_running = !matches!(
                    install_status,
                    install::InstallStatus::InstalledAndRunning { .. }
                );

                if was_not_running {
                    tokio::task::spawn_blocking(install::ensure_installed)
                        .await
                        .context("ensure_installed task panicked")?
                        .context("failed to provision daemon")?;

                    wait_for_daemon_socket(5_000).await;
                }
            }

            let config = detect_claude_code().ok_or_else(|| {
                anyhow::anyhow!(
                    "Claude Code not detected (neither ~/.claude nor ~/.claude.json found). \
                     Install Claude Code first, or use --dry-run to preview the entry."
                )
            })?;

            if config.already_configured {
                println!(
                    "vectorhawk is already configured in {} — no changes made.",
                    config.config_path.display()
                );
            } else {
                write_mcp_entry(&config).with_context(|| {
                    format!("failed to write to {}", config.config_path.display())
                })?;
                println!(
                    "Wrote vectorhawk MCP entry to {}.",
                    config.config_path.display()
                );
            }

            match install_claude_skills() {
                Ok(installed) if !installed.is_empty() => {
                    println!(
                        "Installed {} VectorHawk slash command(s) to ~/.claude/skills/.",
                        installed.len()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("warning: failed to install slash commands: {e:#}");
                }
            }
        }
        other => {
            eprintln!(
                "vectorhawk mcp setup: client '{other}' is not yet supported. \
                 Omit --client to auto-detect all supported clients."
            );
            std::process::exit(2);
        }
    }

    Ok(())
}

/// Poll the daemon socket up to `timeout_ms` milliseconds, checking every
/// 250 ms. Prints a warning if the socket does not appear in time and returns
/// without error — the AI client config write proceeds regardless.
#[cfg(unix)]
async fn wait_for_daemon_socket(timeout_ms: u64) {
    use tokio::time::{sleep, Duration, Instant};

    let socket_path = install::daemon_socket_path();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    while Instant::now() < deadline {
        if install::socket_is_reachable(&socket_path, 200) {
            println!("Daemon is running and reachable on socket.");
            return;
        }
        sleep(Duration::from_millis(250)).await;
    }

    eprintln!(
        "warning: daemon socket not reachable within {timeout_ms} ms. \
         The config entry has been written; the daemon will be available after \
         your next login or once `vectorhawk daemon install` completes."
    );
}

#[cfg(not(unix))]
async fn wait_for_daemon_socket(_timeout_ms: u64) {
    // No-op on platforms without Unix sockets.
}

// ── plugin export / import ────────────────────────────────────────────────────

async fn cmd_plugin_export(
    path: camino::Utf8PathBuf,
    format: String,
    output_dir: Option<camino::Utf8PathBuf>,
) -> Result<()> {
    use vectorhawkd_core::plugin_export;

    let out = output_dir
        .as_deref()
        .unwrap_or_else(|| camino::Utf8Path::new("."));

    let result = match format.as_str() {
        "claude-code" => plugin_export::export_claude_code(&path, out)
            .with_context(|| format!("failed to export plugin at {path} as claude-code"))?,
        "mcpb" => plugin_export::export_mcpb(&path, out)
            .with_context(|| format!("failed to export plugin at {path} as mcpb"))?,
        other => {
            anyhow::bail!(
                "unsupported format '{}'. Use 'claude-code' or 'mcpb'",
                other
            );
        }
    };

    println!("Exported to {result}");
    Ok(())
}

async fn cmd_plugin_import(
    path: camino::Utf8PathBuf,
    output_dir: Option<camino::Utf8PathBuf>,
) -> Result<()> {
    use vectorhawkd_core::plugin_import;

    let out = output_dir
        .as_deref()
        .unwrap_or_else(|| camino::Utf8Path::new("."));

    let format = plugin_import::detect_plugin_format(&path).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not detect plugin format at '{}'. \
             Expected a Claude Code plugin directory (with .claude-plugin/) or a .mcpb file.",
            path
        )
    })?;

    let result = match format {
        plugin_import::ExternalPluginFormat::ClaudeCode => {
            plugin_import::import_claude_code_plugin(&path, out)
                .with_context(|| format!("failed to import Claude Code plugin at {path}"))?
        }
        plugin_import::ExternalPluginFormat::Mcpb => plugin_import::import_mcpb(&path, out)
            .with_context(|| format!("failed to import .mcpb at {path}"))?,
    };

    println!("Imported to {result}");
    println!("Next: vectorhawk skill validate {result}");
    Ok(())
}

// ── mcp sync ──────────────────────────────────────────────────────────────────

/// Trigger one registry sync tick in-process (Route A).
///
/// Equivalent to one period of the daemon's background sync loop, but runs
/// directly in the CLI process against the same state directory and registry.
/// This does NOT communicate with a running daemon — it operates on the shared
/// SQLite state independently.
async fn cmd_mcp_sync() -> Result<()> {
    use std::sync::Arc;
    use vectorhawkd_core::{audit::SqliteAuditBuffer, registry::RegistryClient, state::AppState};
    use vectorhawkd_daemon::run_sync_tick;
    use vectorhawkd_mcp::tools::UpdateCheckCache;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let registry_url = std::env::var("VECTORHAWK_REGISTRY_URL")
        .unwrap_or_else(|_| "https://app.vectorhawk.ai".to_string());

    let registry = Arc::new(RegistryClient::new(&registry_url));
    let audit = Arc::new(SqliteAuditBuffer::new(Arc::clone(&registry), &state));
    let db_path = state.db_path.clone();
    let root_dir = state.root_dir.clone();
    let update_cache: UpdateCheckCache =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    tokio::task::spawn_blocking(move || {
        run_sync_tick(&registry, &audit, &db_path, &root_dir, &update_cache)
    })
    .await
    .context("sync task panicked")?
    .context("registry sync failed")?;

    println!("Registry sync complete.");
    Ok(())
}

// ── mcp backends ──────────────────────────────────────────────────────────────

/// List registered backends from the stub registry.
///
/// Builds the same stub registry the daemon uses and prints each backend's
/// ID, name, tool count, and health status. Real HTTP backends arrive in M1.3;
/// until then this shows the M0 stub entries.
async fn cmd_mcp_backends() -> Result<()> {
    use vectorhawkd_daemon::build_stub_registry;

    let registry = build_stub_registry();
    let backends = registry.list_backends();

    if backends.is_empty() {
        println!("No backends registered.");
        return Ok(());
    }

    println!("{:<20} {:<20} {:<8} STATUS", "SERVER ID", "NAME", "TOOLS");
    println!("{}", "-".repeat(60));
    for b in &backends {
        let status = if b.unhealthy { "unhealthy" } else { "healthy" };
        println!(
            "{:<20} {:<20} {:<8} {status}",
            b.server_id, b.name, b.tool_count
        );
    }

    Ok(())
}

// ── daemon subcommands ────────────────────────────────────────────────────────

async fn cmd_daemon_run(
    registry_url: Option<String>,
    ollama_url: Option<String>,
    ollama_model: Option<String>,
) -> Result<()> {
    use vectorhawkd_daemon::{run_daemon, DaemonOpts};

    let opts = DaemonOpts {
        registry_url,
        socket_path_override: None,
        ollama_url,
        ollama_model,
    };

    run_daemon(opts).await
}

async fn cmd_daemon_install() -> Result<()> {
    tokio::task::spawn_blocking(install::install)
        .await
        .context("install task panicked")?
        .context("daemon install failed")
}

async fn cmd_daemon_uninstall() -> Result<()> {
    tokio::task::spawn_blocking(install::uninstall)
        .await
        .context("uninstall task panicked")?
        .context("daemon uninstall failed")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "commands_tests.rs"]
mod commands_tests;
