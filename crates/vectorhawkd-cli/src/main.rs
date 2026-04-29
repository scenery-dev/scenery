//! `vectorhawk` — VectorHawk runner user CLI.
//!
//! Subcommand tree:
//!
//! ```text
//! vectorhawk doctor
//! vectorhawk mcp serve
//! vectorhawk mcp setup [--client <name>] [--dry-run]
//! vectorhawk daemon run [--foreground]
//! vectorhawk daemon install
//! vectorhawk daemon uninstall
//! ```
//!
//! `mcp serve` is the AI-client entry point — what `mcp setup` writes into
//! Claude Code / Cursor / etc. configs. On socket connect success it relays
//! over `SocketBackend`; on 2 s timeout it falls back to `EmbeddedBackend`.
//!
//! Commands deferred to M1 are stubbed: they print a notice and exit 2.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

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
    Doctor,

    /// MCP subcommands (AI client integration).
    #[command(subcommand)]
    Mcp(McpCommand),

    /// Daemon lifecycle subcommands.
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Start the MCP relay shim — written into AI client configs by `mcp setup`.
    ///
    /// Connects to the vectorhawkd daemon over a Unix socket. Falls back to
    /// an in-process `EmbeddedBackend` if the daemon is unreachable within 2 s.
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
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (debug / support repro only).
    Run {
        /// Keep the process in the foreground instead of daemonizing.
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },

    /// Install the vectorhawkd LaunchAgent (macOS) or systemd user unit (Linux).
    Install,

    /// Remove the vectorhawkd LaunchAgent / systemd user unit.
    Uninstall,
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
        Command::Doctor => cmd_doctor().await,
        Command::Mcp(McpCommand::Serve) => cmd_mcp_serve().await,
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            cmd_mcp_setup(client.as_deref(), dry_run).await
        }
        Command::Daemon(DaemonCommand::Run { foreground: _ }) => cmd_daemon_run().await,
        Command::Daemon(DaemonCommand::Install) => cmd_daemon_install().await,
        Command::Daemon(DaemonCommand::Uninstall) => cmd_daemon_uninstall().await,
    }
}

// ── doctor ────────────────────────────────────────────────────────────────────

async fn cmd_doctor() -> Result<()> {
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
        }
        Err(e) => {
            eprintln!("warning: could not bootstrap state directory: {e:#}");
            println!("State directory: (unavailable)");
            println!("State database:  (unavailable)");
            println!("Socket path:     (unavailable)");
            println!("Daemon status:   unknown");
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

// ── mcp serve ─────────────────────────────────────────────────────────────────

async fn cmd_mcp_serve() -> Result<()> {
    // Delegate entirely to the shim library, which owns the per-frame read-loop
    // and the mid-session daemon-kill fallback logic (AC4).
    vectorhawkd_shim::run_shim().await
}

// ── mcp setup ────────────────────────────────────────────────────────────────

async fn cmd_mcp_setup(client: Option<&str>, dry_run: bool) -> Result<()> {
    use vectorhawkd_mcp::setup::{
        build_mcp_entry, detect_claude_code, write_mcp_entry, MCP_SERVER_NAME,
    };

    // Resolve target client. M0: only claude-code supported.
    let target = client.unwrap_or("claude-code");

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
                        .unwrap_or_else(|| "~/.claude.json (Claude Code not detected)".to_string())
                );
                return Ok(());
            }

            // Real write path.
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
                return Ok(());
            }

            write_mcp_entry(&config)
                .with_context(|| format!("failed to write to {}", config.config_path.display()))?;

            println!(
                "Wrote vectorhawk MCP entry to {}.",
                config.config_path.display()
            );
        }
        other => {
            eprintln!(
                "vectorhawk mcp setup: client '{other}' is not yet supported (M0 supports only 'claude-code')"
            );
            std::process::exit(2);
        }
    }

    Ok(())
}

// ── daemon subcommands (M0 stubs) ─────────────────────────────────────────────

async fn cmd_daemon_run() -> Result<()> {
    // Stream 4 owns the daemon logic. Wire up after both streams merge.
    eprintln!("vectorhawk daemon run: not yet implemented (M1)");
    std::process::exit(2);
}

async fn cmd_daemon_install() -> Result<()> {
    eprintln!("vectorhawk daemon install: not yet implemented (M2)");
    std::process::exit(2);
}

async fn cmd_daemon_uninstall() -> Result<()> {
    eprintln!("vectorhawk daemon uninstall: not yet implemented (M2)");
    std::process::exit(2);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "commands_tests.rs"]
mod commands_tests;
