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
//! vectorhawk migrate list-backups
//! vectorhawk migrate rollback --ts <ts> [--slug <slug>] [--yes]
//! ```
//!
//! `mcp serve` is the AI-client entry point — what `mcp setup` writes into
//! Claude Code / Cursor / etc. configs. The shim relays over `SocketBackend`
//! and, when the daemon is unreachable, returns a structured JSON-RPC error
//! containing install/restart instructions (M4 contract).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod commands_migrate;
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

    /// Sync status subcommands — inspect the daemon reconciler state.
    #[command(subcommand)]
    Sync(SyncCommand),

    /// Managed-paths backup and rollback.
    #[command(subcommand)]
    Migrate(commands_migrate::MigrateCommand),
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

    /// Search the skill registry for available skills.
    Search {
        /// Search query (keyword or partial skill name).
        query: String,

        /// Registry base URL.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
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

        /// Model name to use (overrides OLLAMA_MODEL env var and manifest recommendations).
        #[arg(long)]
        model: Option<String>,
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

        /// Automatically apply recommended metadata (vh_triggers, vh_model) when
        /// any recommended fields are missing, without prompting.
        #[arg(long, default_value_t = false)]
        accept_suggestions: bool,

        /// Skip the recommendation prompt entirely; do not add missing metadata.
        #[arg(long, default_value_t = false)]
        skip_metadata: bool,
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

    /// Uninstall (remove) a locally installed skill.
    ///
    /// Removes the skill's versioned install directory, the active/ symlink,
    /// and the installed_skills / skill_versions DB rows.  Execution counts
    /// and ratings are preserved for historical audit purposes.
    ///
    /// If the skill was installed via a managed backend desired-state record,
    /// the backend installation is also deactivated so the reconciler will not
    /// reinstall it on the next sync.
    #[command(alias = "remove")]
    Uninstall {
        /// Skill ID to remove.
        id: String,

        /// Registry base URL (used to deactivate managed installs).
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: Option<String>,
    },

    /// Update an installed skill to the latest registry version.
    ///
    /// With <id>: check the registry for the latest version of that skill
    /// and install it if newer than what is currently installed.
    ///
    /// With --all: do the same for every skill in the state DB with status 'active'.
    ///
    /// With neither: print usage.
    Update {
        /// Skill ID to update. Mutually exclusive with --all.
        id: Option<String>,

        /// Update all active installed skills.
        #[arg(long, default_value_t = false)]
        all: bool,

        /// Registry base URL.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: Option<String>,
    },

    /// Author a new skill interactively, with heuristic recommendations.
    ///
    /// Prompts for skill name and system prompt if not provided as flags.
    /// Runs the heuristic engine and presents recommendations for permissions,
    /// model sizing, execution constraints, and trigger phrases.
    Author {
        /// Skill name (becomes directory name and frontmatter `name` field).
        #[arg(long)]
        name: Option<String>,

        /// Path to a file containing the system prompt text.
        #[arg(long, value_name = "FILE")]
        prompt_file: Option<camino::Utf8PathBuf>,

        /// Automatically apply all recommendations without interactive prompts.
        #[arg(long, default_value_t = false)]
        accept_suggestions: bool,

        /// Skip all metadata recommendations; scaffold with hardcoded defaults.
        #[arg(long, default_value_t = false)]
        skip_metadata: bool,

        /// Target parent directory (default: current directory).
        #[arg(long)]
        output_dir: Option<camino::Utf8PathBuf>,

        /// Registry URL used to look up your publisher ID (defaults to production).
        #[arg(long, env = "VECTORHAWK_REGISTRY_URL")]
        registry_url: Option<String>,
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

    /// Pair this device using a code from the VectorHawk portal.
    ///
    /// In the portal, open the catalog page — if this device isn't registered
    /// yet you'll see a setup screen with a pairing code.  Then run:
    ///   vectorhawk auth pair <code>
    ///
    /// This registers the device and authenticates the CLI in one step,
    /// without a separate browser login.
    Pair {
        /// The pairing code shown in the portal (e.g. VH-7X4K-9M2P).
        code: String,

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
    ///
    /// When `--server <slug>` is supplied the shim filters the daemon's
    /// tool list to only the tools belonging to that one backend, stripping
    /// the `<slug>__` prefix on the way out and re-adding it on `tools/call`.
    /// This is what the per-server `~/.claude.json` entries written by F2 use.
    Serve {
        /// Filter the aggregator to only this one backend server.
        /// When set, tools/list only returns tools from this server,
        /// with the `<slug>__` namespace prefix stripped for the AI client.
        #[arg(long, value_name = "SLUG")]
        server: Option<String>,
    },

    /// Write the VectorHawk MCP entry into the specified AI client's config.
    Setup {
        /// Target AI client (currently only "claude-code" is supported in M0).
        #[arg(long)]
        client: Option<String>,

        /// Print the config entry that would be written without modifying any files.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Remove the VectorHawk MCP entry from all AI client configs and delete
    /// VectorHawk slash command skill directories from ~/.claude/skills/.
    ///
    /// Run this before `brew uninstall vectorhawk` to leave AI client configs clean.
    Remove,

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

    /// Stop and start the daemon in place. Picks up a refreshed auth token
    /// or environment changes without re-running install.
    Restart,
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

    /// Install a governed plugin from the registry.
    ///
    /// Requests the install via the portal; the daemon then registers it as a
    /// self-contained Claude Code plugin (it appears in `/plugin list`, bundling
    /// its skills). Requires `vectorhawk auth login`.
    Install {
        /// Plugin slug to install (as shown in the catalog).
        slug: String,

        /// Specific version to install; omit for the latest published version.
        #[arg(long)]
        version: Option<String>,

        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },

    /// Uninstall (remove) a locally installed plugin.
    ///
    /// Removes the plugin from the local Claude Code marketplace (marketplace
    /// source dir, install cache, installed_plugins.json, settings.json
    /// enabledPlugins entry) and deactivates the backend installation so the
    /// reconciler does not reinstall it on the next sync.
    #[command(alias = "remove")]
    Uninstall {
        /// Plugin slug to remove (as shown in `claude plugin list`).
        slug: String,

        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// List all installation records from local SQLite with their current state.
    ///
    /// Shows skill_id, version, source (registry/local), installation_id, and
    /// deactivated status.  Useful for debugging reconcile failures reported by
    /// `vectorhawk doctor`.
    Status,
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
        Command::Skill(SkillCommand::Search {
            query,
            registry_url,
        }) => cmd_skill_search(&query, registry_url.as_deref()).await,
        Command::Skill(SkillCommand::Info { id }) => cmd_skill_info(&id).await,
        Command::Skill(SkillCommand::Run {
            id,
            input,
            stub,
            model,
        }) => cmd_skill_run(&id, input, stub, model.as_deref()).await,
        Command::Skill(SkillCommand::Import {
            path,
            registry_url,
            confirm_risky,
            accept_suggestions,
            skip_metadata,
        }) => {
            cmd_skill_import(
                path,
                registry_url.as_deref(),
                confirm_risky,
                accept_suggestions,
                skip_metadata,
            )
            .await
        }
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
        Command::Skill(SkillCommand::Uninstall { id, registry_url }) => {
            cmd_skill_uninstall(&id, registry_url.as_deref()).await
        }
        Command::Skill(SkillCommand::Update {
            id,
            all,
            registry_url,
        }) => cmd_skill_update(id.as_deref(), all, registry_url.as_deref()).await,
        Command::Skill(SkillCommand::Author {
            name,
            prompt_file,
            accept_suggestions,
            skip_metadata,
            output_dir,
            registry_url,
        }) => {
            cmd_skill_author(
                name.as_deref(),
                prompt_file.as_deref(),
                accept_suggestions,
                skip_metadata,
                output_dir.as_deref(),
                registry_url.as_deref(),
            )
            .await
        }

        Command::Auth(AuthCommand::Login { registry_url }) => cmd_auth_login(&registry_url).await,
        Command::Auth(AuthCommand::Logout { registry_url }) => cmd_auth_logout(&registry_url).await,
        Command::Auth(AuthCommand::Status { registry_url }) => cmd_auth_status(&registry_url).await,
        Command::Auth(AuthCommand::Pair { code, registry_url }) => {
            cmd_auth_pair(&code, &registry_url).await
        }
        Command::Auth(AuthCommand::Token {
            token,
            registry_url,
        }) => cmd_auth_token(&token, &registry_url).await,

        Command::Mcp(McpCommand::Serve { server }) => cmd_mcp_serve(server).await,
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            cmd_mcp_setup(client.as_deref(), dry_run).await
        }
        Command::Mcp(McpCommand::Remove) => cmd_mcp_remove().await,
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
        Command::Daemon(DaemonCommand::Restart) => cmd_daemon_restart().await,

        Command::Plugin(PluginCommand::Export {
            path,
            format,
            output_dir,
        }) => cmd_plugin_export(path, format, output_dir).await,
        Command::Plugin(PluginCommand::Install {
            slug,
            version,
            registry_url,
        }) => cmd_plugin_install(&slug, version.as_deref(), &registry_url).await,
        Command::Plugin(PluginCommand::Uninstall { slug, registry_url }) => {
            cmd_plugin_uninstall(&slug, registry_url.as_deref()).await
        }
        Command::Plugin(PluginCommand::Import { path, output_dir }) => {
            cmd_plugin_import(path, output_dir).await
        }

        Command::Sync(SyncCommand::Status) => cmd_sync_status().await,

        Command::Migrate(args) => commands_migrate::run(args).await,
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

            // RUN2: SSE sync + reconciler status.
            let sync_status = query_sync_status(&app.state.db_path);
            println!("SSE sync:        {sync_status}");

            let reconcile_status = query_reconcile_status(&app.state.db_path);
            println!("Reconcile:       {reconcile_status}");
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
            println!("SSE sync:        unknown");
            println!("Reconcile:       unknown");
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

/// Result of asking a running daemon to reload credentials and start syncing.
enum DaemonReload {
    /// Daemon reloaded and the SSE sync subsystem is now active.
    SyncActive,
    /// Daemon reachable but sync did not start (e.g. device registration failed).
    SyncInactive,
    /// Daemon is not running / unreachable.
    DaemonDown,
}

/// Ask a running daemon to pick up freshly-saved credentials: register this
/// device and start the SSE sync subsystem immediately, with no daemon restart.
///
/// Best-effort with a short deadline — the caller has already persisted tokens,
/// so a down daemon is not an error here; it will sync on its next start.
#[cfg(unix)]
async fn daemon_auth_reload(socket_path: &str) -> DaemonReload {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    let connected = match timeout(
        Duration::from_secs(2),
        UnixStream::connect(socket_path.to_string()),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return DaemonReload::DaemonDown,
    };

    let (mut reader, mut writer) = connected.into_split();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "auth/reload",
        "params": {}
    });

    let body = match serde_json::to_vec(&request) {
        Ok(b) => b,
        Err(_) => return DaemonReload::DaemonDown,
    };
    let len = body.len() as u32;
    if writer.write_all(&len.to_be_bytes()).await.is_err()
        || writer.write_all(&body).await.is_err()
        || writer.flush().await.is_err()
    {
        return DaemonReload::DaemonDown;
    }

    // Device registration involves a network round-trip on the daemon side;
    // give it up to 15 s before giving up.
    let read_result = timeout(Duration::from_secs(15), async {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_body = vec![0u8; resp_len];
        reader.read_exact(&mut resp_body).await?;
        Ok::<Vec<u8>, tokio::io::Error>(resp_body)
    })
    .await;

    let resp: serde_json::Value = match read_result {
        Ok(Ok(b)) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(_) => return DaemonReload::DaemonDown,
        },
        _ => return DaemonReload::DaemonDown,
    };

    match resp
        .get("result")
        .and_then(|r| r.get("sync_active"))
        .and_then(|a| a.as_bool())
    {
        Some(true) => DaemonReload::SyncActive,
        _ => DaemonReload::SyncInactive,
    }
}

#[cfg(not(unix))]
async fn daemon_auth_reload(_socket_path: &str) -> DaemonReload {
    DaemonReload::DaemonDown
}

/// Persist tokens, then nudge a running daemon to start syncing immediately and
/// print a status line tailored to the outcome.  Shared by `auth login`,
/// `auth pair`, and `auth token`.
async fn finish_auth(state: &vectorhawkd_core::state::AppState) {
    let socket_path = state.socket_path();
    match daemon_auth_reload(socket_path.as_str()).await {
        DaemonReload::SyncActive => {
            println!("Daemon is syncing your governed skills now.");
        }
        DaemonReload::SyncInactive => {
            println!(
                "Saved, but the daemon could not start syncing yet. \
                 Run `vectorhawk doctor` to diagnose."
            );
        }
        DaemonReload::DaemonDown => {
            println!(
                "Saved. Start the daemon with `vectorhawk daemon install` and \
                 your skills will sync automatically."
            );
        }
    }
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
    match conn.query_row(
        "SELECT value FROM meta WHERE key = 'last_sync_at'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(val) => match val.parse::<i64>() {
            Ok(ts) => format_unix_timestamp(ts),
            Err(_) => "unknown (bad timestamp)".to_string(),
        },
        Err(_) => "never".to_string(),
    }
}

/// Return SSE sync status from `sync_state` table.
///
/// Reads the `last_event_id` key; if it's present, reports the device_id
/// and last event.  If the table or keys are absent, reports "no sync state".
fn query_sync_status(db_path: &camino::Utf8PathBuf) -> String {
    use rusqlite::Connection;
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return "unknown (db open failed)".to_string(),
    };

    let device_id: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = 'device_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    let last_event_id: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = 'last_event_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    match (device_id, last_event_id) {
        (Some(did), Some(eid)) => format!("device {did} — last event id: {eid}"),
        (Some(did), None) => format!("device {did} — no events received yet"),
        (None, _) => "not registered — run 'vectorhawk auth login' to enable sync".to_string(),
    }
}

/// Return a reconciler summary from local SQLite.
///
/// Counts installed (active), deactivated, and error states.
fn query_reconcile_status(db_path: &camino::Utf8PathBuf) -> String {
    use rusqlite::Connection;
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return "unknown (db open failed)".to_string(),
    };

    // Count active (non-deactivated) registry-sourced skills.
    let installed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE deactivated = 0 AND source = 'registry'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let deactivated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE deactivated = 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // "Errors" here means skills with current_status = 'error' (set by reconciler on failure).
    let errors: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM installed_skills WHERE current_status = 'error'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if errors > 0 {
        format!(
            "{installed} installed, {deactivated} deactivated, {errors} errors — run 'vectorhawk sync status'"
        )
    } else {
        format!("{installed} installed, {deactivated} deactivated, 0 errors")
    }
}

/// Truncate a string to at most `max_chars` Unicode scalar values.
/// Appends "…" when truncation occurs.
fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
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

    // Attempt registry update check — skip silently if offline.
    // We use the default registry URL; for now `skill list` doesn't take --registry-url.
    let update_hints = fetch_update_hints_for_list(&state, "https://app.vectorhawk.ai").await;

    println!(
        "{:<30} {:<12} {:<12} INSTALLED AT",
        "SKILL ID", "VERSION", "STATUS"
    );
    println!("{}", "-".repeat(75));
    for r in &rows {
        let update_suffix = update_hints
            .get(r.skill_id.as_str())
            .map(|latest| format!("  (update available: {latest})"))
            .unwrap_or_default();
        println!(
            "{:<30} {:<12} {:<12} {}{}",
            r.skill_id, r.active_version, r.current_status, r.installed_at, update_suffix
        );
    }

    Ok(())
}

/// Query the registry for the latest version of each installed skill and return
/// a map of skill_id → latest_version for skills that have an available update.
///
/// Returns an empty map on any error (offline, registry unreachable, etc.).
async fn fetch_update_hints_for_list(
    state: &vectorhawkd_core::state::AppState,
    registry_url: &str,
) -> std::collections::HashMap<String, String> {
    use semver::Version;
    use vectorhawkd_core::registry::RegistryClient;

    let db_path = state.db_path.clone();
    let registry_url = registry_url.to_string();

    tokio::task::spawn_blocking(move || {
        let registry = RegistryClient::new(&registry_url);
        let conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return std::collections::HashMap::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT skill_id, active_version FROM installed_skills WHERE current_status = 'active'",
        ) {
            Ok(s) => s,
            Err(_) => return std::collections::HashMap::new(),
        };
        let pairs: Vec<(String, String)> = match stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .and_then(|iter| iter.collect::<rusqlite::Result<Vec<_>>>())
        {
            Ok(v) => v,
            Err(_) => return std::collections::HashMap::new(),
        };

        let mut hints = std::collections::HashMap::new();
        for (skill_id, installed_str) in pairs {
            let installed = match Version::parse(&installed_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let detail = match registry.fetch_skill_detail(&skill_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let latest_str = match detail.latest_version {
                Some(v) => v,
                None => continue,
            };
            let latest = match Version::parse(&latest_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if latest > installed {
                hints.insert(skill_id, latest_str);
            }
        }
        hints
    })
    .await
    .unwrap_or_default()
}

// ── skill search ─────────────────────────────────────────────────────────────

async fn cmd_skill_search(query: &str, registry_url: Option<&str>) -> Result<()> {
    use vectorhawkd_core::registry::RegistryClient;

    let url = registry_url.unwrap_or("https://app.vectorhawk.ai");
    let registry = RegistryClient::new(url);
    let q = query.to_string();

    let results = tokio::task::spawn_blocking(move || registry.search_skills(&q))
        .await
        .context("search task panicked")?
        .context("skill search failed")?;

    if results.is_empty() {
        println!("No skills matched \"{query}\".");
        return Ok(());
    }

    // Column caps — content is truncated to these, headers are not.
    const ID_CAP: usize = 32;
    const VER_CAP: usize = 10; // semver fits easily
    const PUB_CAP: usize = 20;
    const DESC_CAP: usize = 60;

    // Compute column widths from actual data, bounded by caps and header minimums.
    let id_w = results
        .iter()
        .map(|r| r.skill_id.len().min(ID_CAP))
        .max()
        .unwrap_or(8)
        .max("SKILL_ID".len());
    let ver_w = results
        .iter()
        .map(|r| {
            r.latest_version
                .as_deref()
                .unwrap_or("-")
                .len()
                .min(VER_CAP)
        })
        .max()
        .unwrap_or(6)
        .max("LATEST".len());
    let pub_w = results
        .iter()
        .map(|r| {
            r.publisher_name
                .as_deref()
                .unwrap_or("-")
                .len()
                .min(PUB_CAP)
        })
        .max()
        .unwrap_or(9)
        .max("PUBLISHER".len());

    // Header + separator — style matches `skill update`.
    println!(
        "{:<id_w$}  {:<ver_w$}  {:<pub_w$}  DESCRIPTION",
        "SKILL_ID", "LATEST", "PUBLISHER",
    );
    println!("{}", "-".repeat(id_w + ver_w + pub_w + DESC_CAP + 8));

    for r in &results {
        let id = truncate_str(&r.skill_id, ID_CAP);
        let version = truncate_str(r.latest_version.as_deref().unwrap_or("-"), VER_CAP);
        let publisher = truncate_str(r.publisher_name.as_deref().unwrap_or("-"), PUB_CAP);
        let desc = truncate_str(r.description.as_deref().unwrap_or(""), DESC_CAP);
        println!(
            "{:<id_w$}  {:<ver_w$}  {:<pub_w$}  {desc}",
            id, version, publisher
        );
    }

    let count = results.len();
    println!("\n{count} result(s). Run: vectorhawk skill install <id>");
    Ok(())
}

// ── skill install ─────────────────────────────────────────────────────────────

/// Maximum time to poll local SQLite for the reconciler to confirm install.
///
/// The reconciler retries failed installs once after a 30s delay (see
/// `RETRY_DELAY_SECS` in `vectorhawkd-daemon::sync::reconciler`). Keep this
/// comfortably above 30s so a transient failure on the first attempt does
/// not surface to the user as a timeout when the second attempt would have
/// succeeded.
const REGISTRY_INSTALL_POLL_TIMEOUT_SECS: u64 = 120;
/// SQLite poll interval while waiting for reconciler confirmation.
const REGISTRY_INSTALL_POLL_INTERVAL_MS: u64 = 500;

async fn cmd_skill_install(skill_ref: &str, link: bool, registry_url: Option<&str>) -> Result<()> {
    use vectorhawkd_core::{
        audit::{write_audit_event_direct, AuditEvent},
        state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    // Treat skill_ref as a local path when it exists on disk or the --link
    // flag is set (--link only makes sense with local paths).
    let path = camino::Utf8Path::new(skill_ref);
    if path.exists() {
        // ── Local / offline install ───────────────────────────────────────────
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

        // Audit: skill installed from a local directory (fail-open: never block install).
        let mode_str = if link { "symlink" } else { "copy" };
        let ts = chrono::Utc::now().to_rfc3339();
        write_audit_event_direct(
            &state.db_path,
            &AuditEvent {
                event_type: "skill_installed".to_string(),
                payload: serde_json::json!({
                    "skill_id": pkg.manifest.id,
                    "version":  pkg.manifest.version.to_string(),
                    "source":   "cli-local",
                    "mode":     mode_str,
                    "ts":       ts,
                }),
            },
        );

        println!(
            "Installed skill '{}' version {} (local).",
            pkg.manifest.id, pkg.manifest.version
        );
    } else {
        // ── Registry install ─────────────────────────────────────────────────
        // Audit is emitted inside the helpers below so the correct source
        // label is recorded regardless of which path fires (desired-state,
        // cli-registry direct, or desired-state's inner direct fallback).
        let url = registry_url.unwrap_or("https://app.vectorhawk.ai");

        // Check if the daemon has an auth token.  If not, fall through to
        // direct install so the CLI remains usable without the daemon.
        let has_token = state.get_sync_state("device_id").ok().flatten().is_some();

        if has_token {
            install_via_desired_state(skill_ref, url, &state).await?;
        } else {
            // No daemon registration → direct install path (pre-RUN2 behaviour).
            direct_registry_install(skill_ref, url, state.db_path.clone()).await?;
        }
    }

    Ok(())
}

/// Call `POST /api/installations` and poll SQLite for the reconciler to confirm.
async fn install_via_desired_state(
    skill_id: &str,
    registry_url: &str,
    state: &vectorhawkd_core::state::AppState,
) -> Result<()> {
    use rusqlite::Connection;
    use rusqlite::OptionalExtension;

    let url = format!("{}/api/installations", registry_url.trim_end_matches('/'));

    // Load the Bearer token.
    let token = {
        let rows =
            vectorhawkd_core::auth::load_all_tokens(state).context("failed to load auth tokens")?;
        rows.into_iter()
            .find(|r| r.registry_url == registry_url)
            .map(|r| r.access_token)
    };

    let token = match token {
        Some(t) => t,
        None => {
            // No token → fall back to direct install.  The direct install
            // helper emits its own audit event with source="cli-registry".
            eprintln!(
                "note: not logged in — installing directly from registry (run \
                 'vectorhawk auth login' to enable portal-driven installs)"
            );
            return direct_registry_install(skill_id, registry_url, state.db_path.clone()).await;
        }
    };

    let payload = serde_json::json!({
        "skill_id": skill_id,
        "source": "cli",
    });

    eprint!("Requesting install of '{skill_id}' from backend");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("failed to call {url}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("session expired — run 'vectorhawk auth login' to refresh");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("install request failed (HTTP {status}): {body}");
    }

    // Poll SQLite for the reconciler to confirm 'installed' state (max 30 s).
    let db_path = state.db_path.clone();
    let skill_id_str = skill_id.to_string();
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(REGISTRY_INSTALL_POLL_TIMEOUT_SECS);

    loop {
        if std::time::Instant::now() >= deadline {
            eprintln!();
            anyhow::bail!(
                "timed out waiting for '{skill_id}' to install — the daemon may not be \
                 running. Run 'vectorhawk daemon install' to enable background sync, \
                 or 'vectorhawk sync status' to check for errors."
            );
        }

        eprint!(".");

        let installed = tokio::task::spawn_blocking({
            let db = db_path.clone();
            let id = skill_id_str.clone();
            move || {
                let conn = Connection::open(&db)?;
                let row: Option<(String, i64)> = conn
                    .query_row(
                        "SELECT current_status, COALESCE(deactivated, 0) \
                         FROM installed_skills WHERE skill_id = ?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                anyhow::Ok(row)
            }
        })
        .await
        .context("poll task panicked")?
        .context("failed to poll installed_skills")?;

        match installed {
            Some((status, 0)) if status == "active" => {
                eprintln!();
                println!("Installed skill '{skill_id}' from registry.");
                // Audit: daemon confirmed the desired-state install succeeded.
                // Fail-open: never block on audit write failure.
                let ts = chrono::Utc::now().to_rfc3339();
                vectorhawkd_core::audit::write_audit_event_direct(
                    &state.db_path,
                    &vectorhawkd_core::audit::AuditEvent {
                        event_type: "skill_installed".to_string(),
                        payload: serde_json::json!({
                            "skill_id": skill_id,
                            "source":   "desired-state",
                            "ts":       ts,
                        }),
                    },
                );
                return Ok(());
            }
            Some((status, _)) if status == "error" => {
                eprintln!();
                anyhow::bail!(
                    "install of '{skill_id}' encountered an error — run \
                     'vectorhawk sync status' for details"
                );
            }
            _ => {
                // Not yet installed; wait and retry.
                tokio::time::sleep(std::time::Duration::from_millis(
                    REGISTRY_INSTALL_POLL_INTERVAL_MS,
                ))
                .await;
            }
        }
    }
}

/// Direct registry install — pre-RUN2 behaviour, used when the daemon is not
/// running or the user is not logged in.
///
/// Emits a `skill_installed` audit event with `source = "cli-registry"` on
/// success.  On failure the install error propagates; the audit event is only
/// recorded for successful installs.
async fn direct_registry_install(
    skill_ref: &str,
    registry_url: &str,
    db_path: camino::Utf8PathBuf,
) -> Result<()> {
    use vectorhawkd_core::{
        audit::{write_audit_event_direct, AuditEvent},
        registry::RegistryClient,
        state::AppState,
        updater::install_from_registry,
    };

    let url = registry_url.to_string();
    let registry = RegistryClient::new(&url);
    let skill_id = skill_ref.to_string();
    // Reconstruct the minimal AppState from db_path — AppState::bootstrap()
    // was already called by the outer command; we just need the struct fields.
    let root_dir = db_path
        .parent()
        .map(|p| p.to_owned())
        .ok_or_else(|| anyhow::anyhow!("db_path has no parent directory"))?;
    let state = AppState {
        root_dir,
        db_path: db_path.clone(),
    };

    let version = tokio::task::spawn_blocking(move || {
        install_from_registry(&state, &registry, &skill_id, None)
    })
    .await
    .context("install task panicked")?
    .with_context(|| format!("failed to install '{skill_ref}' from registry"))?;

    // Audit: successful direct-registry install (fail-open).
    let ts = chrono::Utc::now().to_rfc3339();
    write_audit_event_direct(
        &db_path,
        &AuditEvent {
            event_type: "skill_installed".to_string(),
            payload: serde_json::json!({
                "skill_id": skill_ref,
                "version":  version,
                "source":   "cli-registry",
                "mode":     "copy",
                "ts":       ts,
            }),
        },
    );

    println!("Installed skill '{skill_ref}' version {version} from registry.");
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

    // Show execution stats. Sourced from skill_execution_counts (the
    // aggregate counter the daemon syncs to the registry) — the per-run
    // execution_history table was retired in the local-DB shrink.
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(SUM(total_runs), 0), COALESCE(SUM(successful_runs), 0) \
             FROM skill_execution_counts WHERE skill_id = ?1",
        )
        .context("failed to prepare stats query")?;

    let (total_runs, successful_runs): (i64, i64) = stmt
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("failed to query execution stats")?;

    println!("Run count:      {total_runs}");
    println!("Successful:     {successful_runs}");

    Ok(())
}

// ── skill run ─────────────────────────────────────────────────────────────────

/// Select the best available Ollama model for a given skill.
///
/// Priority order:
/// 1. `--model` CLI flag (passed as `explicit_model`)
/// 2. `OLLAMA_MODEL` env var
/// 3. Manifest `vh_model.recommended` list matched against locally available models
/// 4. First available Ollama model (if none of the recommended are present)
/// 5. First recommended model name (if Ollama is unreachable)
fn select_model_for_skill(
    ollama_url: &str,
    skill_id: &str,
    state: &vectorhawkd_core::state::AppState,
    explicit_model: Option<&str>,
) -> String {
    use vectorhawkd_core::ollama::OllamaClient;
    use vectorhawkd_manifest::SkillPackage;

    // Priority 1: explicit CLI flag.
    if let Some(m) = explicit_model {
        return m.to_string();
    }

    // Priority 2: OLLAMA_MODEL env var.
    if let Ok(m) = std::env::var("OLLAMA_MODEL") {
        if !m.is_empty() {
            return m;
        }
    }

    // Priority 3+: load manifest recommended models, then query Ollama.
    let install_root = state.root_dir.join("skills").join(skill_id);
    let active_path = install_root.join("active");
    let recommended: Vec<String> = SkillPackage::load_from_dir(&active_path)
        .ok()
        .and_then(|pkg| pkg.manifest.model_requirements)
        .map(|reqs| reqs.recommended)
        .unwrap_or_default();

    let client = OllamaClient::new(ollama_url, "");
    let available: Vec<String> = client
        .list_models()
        .ok()
        .map(|models| models.into_iter().map(|m| m.name).collect())
        .unwrap_or_default();

    if available.is_empty() {
        // Ollama unreachable — fall back to first recommended or hard default.
        return recommended
            .into_iter()
            .next()
            .unwrap_or_else(|| "llama3".to_string());
    }

    if recommended.is_empty() {
        return available
            .into_iter()
            .next()
            .unwrap_or_else(|| "llama3".to_string());
    }

    // Find first recommended model that is available (prefix match).
    for rec in &recommended {
        if let Some(found) = available
            .iter()
            .find(|a| *a == rec || a.starts_with(rec.as_str()))
        {
            return found.clone();
        }
    }

    // None of the recommended are available — use first available.
    available
        .into_iter()
        .next()
        .unwrap_or_else(|| recommended[0].clone())
}

async fn cmd_skill_run(
    id: &str,
    input_path: camino::Utf8PathBuf,
    stub: bool,
    explicit_model: Option<&str>,
) -> Result<()> {
    use vectorhawkd_core::{
        audit::{write_audit_event_direct, AuditEvent},
        executor::run_skill,
        ollama::OllamaClient,
        policy::MockPolicyClient,
        state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let policy = MockPolicyClient::default();

    let raw = std::fs::read_to_string(&input_path)
        .with_context(|| format!("failed to read {input_path}"))?;
    let input: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("input file {input_path} is not valid JSON"))?;

    let run_outcome = if stub {
        run_skill(&state, &policy, id, &input, None)
    } else {
        let ollama_url = std::env::var("VECTORHAWK_OLLAMA_URL")
            .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let ollama_model = select_model_for_skill(&ollama_url, id, &state, explicit_model);
        let model = OllamaClient::new(ollama_url, ollama_model);
        run_skill(&state, &policy, id, &input, Some(&model))
    };

    // Audit and execution counter: always emit, whether the run succeeded or
    // failed.  Fail-open — a broken audit write must not abort the run result.
    let (run_status, run_version) = match &run_outcome {
        Ok(r) => ("ok", r.version.clone()),
        Err(_) => ("error", String::new()),
    };

    let ts = chrono::Utc::now().to_rfc3339();
    write_audit_event_direct(
        &state.db_path,
        &AuditEvent {
            event_type: "skill_run".to_string(),
            payload: serde_json::json!({
                "skill_id": id,
                "version":  run_version,
                "status":   run_status,
                "invoker":  "cli",
                "ts":       ts,
            }),
        },
    );

    // Increment execution counter for parity with the MCP path.
    // Only on success, matching the semantics of increment_execution_count.
    if let Ok(ref r) = run_outcome {
        if let Ok(conn) = rusqlite::Connection::open(&state.db_path) {
            if let Err(e) =
                vectorhawkd_core::ratings::increment_execution_count(&conn, id, &r.version)
            {
                tracing::warn!(error = %e, "failed to increment execution count on CLI run");
            }
        }
    }

    let result = run_outcome.with_context(|| format!("failed to run skill '{id}'"))?;

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
    accept_suggestions: bool,
    skip_metadata: bool,
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

    // ── AUTH2d: recommendation enrichment for missing vh_triggers / vh_model ──

    if !skip_metadata {
        let skill_md_path = bundle.output_dir.join("SKILL.md");
        maybe_enrich_skill_md(&skill_md_path, accept_suggestions)?;
    }

    Ok(())
}

/// Read a SKILL.md back from disk after import and, if `vh_triggers` or
/// `vh_model` are absent, offer to add them via the recommendation engine.
///
/// Runs interactively unless `accept_suggestions` is true.
fn maybe_enrich_skill_md(
    skill_md_path: &camino::Utf8PathBuf,
    accept_suggestions: bool,
) -> Result<()> {
    use std::io::{self, Write};
    use vectorhawkd_core::recommend::recommend_from_prompt;

    let content = match std::fs::read_to_string(skill_md_path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // best-effort — do not fail import
    };

    // Detect whether vh_triggers and/or vh_model are present in the frontmatter.
    let missing_triggers = !content.contains("vh_triggers:");
    let missing_model = !content.contains("vh_model:");

    if !missing_triggers && !missing_model {
        return Ok(());
    }

    // Report which fields are missing.
    let mut missing_fields: Vec<&str> = Vec::new();
    if missing_triggers {
        missing_fields.push("vh_triggers");
    }
    if missing_model {
        missing_fields.push("vh_model");
    }
    let fields_str = missing_fields.join(", ");
    println!("Missing recommended fields detected: {fields_str}");

    let should_apply = if accept_suggestions {
        true
    } else {
        print!("Run recommendation engine to fill these? [Y/n]: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok();
        let answer = answer.trim().to_lowercase();
        answer.is_empty() || answer == "y" || answer == "yes"
    };

    if !should_apply {
        return Ok(());
    }

    // Extract the body (system prompt) from the SKILL.md.
    let body = extract_skill_md_body(&content).unwrap_or_default();
    let rec = recommend_from_prompt("", "", &body);

    // Build the YAML lines to inject before the closing `---`.
    let mut additions = String::new();

    if missing_model {
        let recommended_yaml = rec
            .model
            .recommended
            .iter()
            .map(|m| format!("  - {m}"))
            .collect::<Vec<_>>()
            .join("\n");
        additions.push_str(&format!(
            "vh_model:\n  min_params_b: {}\n  recommended:\n{recommended_yaml}\n  fallback: {}\n",
            rec.model.min_params_b, rec.model.fallback
        ));
    }

    if missing_triggers && !rec.triggers.is_empty() {
        let items = rec
            .triggers
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        additions.push_str(&format!("vh_triggers:\n{items}\n"));
    }

    if additions.is_empty() {
        return Ok(());
    }

    // Insert the additions before the closing `---` line of the frontmatter.
    let enriched = insert_before_closing_fence(&content, &additions);
    std::fs::write(skill_md_path, enriched)
        .with_context(|| format!("failed to write enriched SKILL.md at {skill_md_path}"))?;

    let mut added: Vec<&str> = Vec::new();
    if missing_model {
        added.push("vh_model");
    }
    if missing_triggers && !rec.triggers.is_empty() {
        added.push("vh_triggers");
    }
    println!("Added: {}", added.join(", "));

    Ok(())
}

/// Extract the Markdown body (after the closing `---` frontmatter fence).
fn extract_skill_md_body(content: &str) -> Option<String> {
    let after_open = content.strip_prefix("---\n")?;
    let close = after_open.find("\n---\n")?;
    let body = &after_open[close + 5..];
    Some(body.trim().to_string())
}

/// Insert `additions` text immediately before the closing `\n---\n` frontmatter fence.
///
/// Returns the original content unchanged if no frontmatter fence is found.
fn insert_before_closing_fence(content: &str, additions: &str) -> String {
    let after_open = match content.strip_prefix("---\n") {
        Some(s) => s,
        None => return content.to_string(),
    };
    let close_offset = match after_open.find("\n---\n") {
        Some(o) => o,
        None => return content.to_string(),
    };
    // close_offset is byte offset in after_open where `\n---\n` starts.
    // In original content that is 4 (len of "---\n") + close_offset.
    let split_at = 4 + close_offset + 1; // +1 to include the '\n' that precedes ---
    let (front, back) = content.split_at(split_at);
    format!("{front}{additions}{back}")
}

// ── skill validate ────────────────────────────────────────────────────────────

/// A single row in the validation output table.
struct ValidateRow {
    check: String,
    status: String,
    detail: String,
}

async fn cmd_skill_validate(path: camino::Utf8PathBuf) -> Result<()> {
    use vectorhawkd_core::validator::{validate_bundle, CheckLevel};

    // Try to detect whether the Ollama model check should run.
    let ollama_status = check_ollama_for_validate(&path).await;

    let report = validate_bundle(&path);

    // Build table rows from the core validation report.
    let mut rows: Vec<ValidateRow> = report
        .checks
        .iter()
        .map(|c| ValidateRow {
            check: c.name.clone(),
            status: match c.level {
                CheckLevel::Pass => "PASS".to_string(),
                CheckLevel::Warn => "WARN".to_string(),
                CheckLevel::Fail => "FAIL".to_string(),
            },
            detail: c.detail.clone().unwrap_or_default(),
        })
        .collect();

    // Append Ollama row when the bundle declares a model requirement.
    // None means no model requirement — skip the row entirely.
    if let Some(ref ollama_line) = ollama_status {
        let is_warn = ollama_line.starts_with("not");
        rows.push(ValidateRow {
            check: "ollama_model".to_string(),
            status: if is_warn { "WARN" } else { "PASS" }.to_string(),
            detail: ollama_line.clone(),
        });
    }

    // Compute dynamic column widths (minimum widths match the header labels).
    let check_w = rows.iter().map(|r| r.check.len()).max().unwrap_or(5).max(5); // "CHECK"
    let status_w: usize = 6; // "STATUS" / "PASS  " / "WARN  " / "FAIL  "

    // Print header.
    println!("Validating {path}/");
    println!();
    println!("{:<check_w$}  {:<status_w$}  DETAIL", "CHECK", "STATUS",);
    println!("{}", "-".repeat(check_w + status_w + 4 + 6)); // 4 = two "  " gaps, 6 = "DETAIL"

    // Print rows.
    for row in &rows {
        println!(
            "{:<check_w$}  {:<status_w$}  {}",
            row.check, row.status, row.detail,
        );
    }

    println!();

    let fail_count = report.fail_count();
    let warn_count = report.warn_count();
    let pass_count = report.checks.len().saturating_sub(fail_count + warn_count);

    // Ollama WARN row does not affect the core counts — only core checks determine exit code.
    println!("{pass_count} passed, {warn_count} warn, {fail_count} fail");

    if fail_count == 0 {
        println!("Run: vectorhawk skill install {path}/");
        Ok(())
    } else {
        anyhow::bail!("validation failed — fix FAIL rows above")
    }
}

/// Probe Ollama for model availability, returning a display string or `None` when
/// the bundle doesn't declare model requirements (nothing useful to show).
async fn check_ollama_for_validate(path: &camino::Utf8Path) -> Option<String> {
    use vectorhawkd_core::ollama::OllamaClient;
    use vectorhawkd_manifest::SkillPackage;

    let pkg = SkillPackage::load_from_dir(path).ok()?;
    let recommended = pkg
        .manifest
        .model_requirements
        .map(|r| r.recommended)
        .unwrap_or_default();

    let ollama_url = std::env::var("VECTORHAWK_OLLAMA_URL")
        .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
        .unwrap_or_else(|_| "http://localhost:11434".to_string());

    let client = OllamaClient::new(&ollama_url, "");

    // Run the blocking Ollama calls off the async executor.
    let status = tokio::task::spawn_blocking(move || {
        if !client.health_check().reachable {
            return "not running — will use MCP sampling fallback".to_string();
        }
        if recommended.is_empty() {
            return "Ollama running (no model preference declared)".to_string();
        }
        let available: Vec<String> = client
            .list_models()
            .ok()
            .map(|models| models.into_iter().map(|m| m.name).collect())
            .unwrap_or_default();

        for rec in &recommended {
            if let Some(found) = available
                .iter()
                .find(|a| *a == rec || a.starts_with(rec.as_str()))
            {
                return format!("{found} available in local Ollama");
            }
        }
        let first = recommended[0].clone();
        format!("{first} not found in local Ollama — pull it or rely on MCP sampling")
    })
    .await
    .ok()?;

    Some(status)
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
version: 0.1.0
publisher: YOUR_PUBLISHER_ID
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

// ── skill author ─────────────────────────────────────────────────────────────

/// Format milliseconds as a human-readable duration string.
fn format_duration_ms(ms: u32) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{} min", ms / 60_000)
    }
}

/// Derive a publisher slug from a display name: lowercase, spaces → hyphens, strip non-alnum.
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

/// Return true if the SKILL.md frontmatter has no `publisher:` line or uses the placeholder.
fn needs_publisher(skill_md: &str) -> bool {
    let placeholder = "YOUR_PUBLISHER_ID";
    for line in skill_md.lines() {
        if let Some(rest) = line.strip_prefix("publisher:") {
            let val = rest.trim();
            return val.is_empty() || val == placeholder;
        }
    }
    true // no publisher line at all
}

/// Replace (or insert) the `publisher:` frontmatter line with `publisher: <slug>`.
fn inject_publisher_field(skill_md: &str, slug: &str) -> String {
    let placeholder = "YOUR_PUBLISHER_ID";
    // Replace existing placeholder line.
    if skill_md.contains(&format!("publisher: {placeholder}")) || skill_md.contains("publisher: ") {
        return skill_md
            .lines()
            .map(|line| {
                if let Some(rest) = line.strip_prefix("publisher:") {
                    let val = rest.trim();
                    if val.is_empty() || val == placeholder {
                        return format!("publisher: {slug}");
                    }
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + if skill_md.ends_with('\n') { "\n" } else { "" };
    }
    // No publisher line — insert after `description:` or after the opening `---`.
    let mut lines: Vec<String> = skill_md.lines().map(|l| l.to_string()).collect();
    let insert_after = lines
        .iter()
        .position(|l| l.starts_with("description:"))
        .or_else(|| lines.iter().position(|l| l == "---"))
        .map(|i| i + 1)
        .unwrap_or(1);
    lines.insert(insert_after, format!("publisher: {slug}"));
    lines.join("\n") + if skill_md.ends_with('\n') { "\n" } else { "" }
}

/// Try to look up the logged-in user's publisher slug from auth state.
///
/// Returns `None` silently on any failure (no tokens, network error, etc.).
async fn try_infer_publisher_id(registry_url: &str) -> Option<String> {
    use vectorhawkd_core::{
        auth::{load_tokens, AuthClient},
        state::AppState,
    };

    let state = AppState::bootstrap().ok()?;
    let tokens = load_tokens(&state, registry_url).ok().flatten()?;
    let client = AuthClient::new(registry_url);
    let user_info = tokio::task::spawn_blocking(move || client.me(&tokens.access_token))
        .await
        .ok()?
        .ok()?;
    let slug = derive_publisher_slug(&user_info.display_name);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

async fn cmd_skill_author(
    name: Option<&str>,
    prompt_file: Option<&camino::Utf8Path>,
    accept_suggestions: bool,
    skip_metadata: bool,
    output_dir: Option<&camino::Utf8Path>,
    registry_url: Option<&str>,
) -> Result<()> {
    use std::fs;
    use std::io::{self, BufRead, Write};
    use vectorhawkd_core::recommend::recommend_from_prompt;

    // ── Step 1: Resolve skill name ────────────────────────────────────────────

    let skill_name = match name {
        Some(n) => n.to_string(),
        None => {
            print!("Skill name: ");
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin()
                .lock()
                .read_line(&mut line)
                .context("failed to read skill name from stdin")?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("skill name must not be empty");
            }
            trimmed
        }
    };

    // ── Step 2: Resolve system prompt ─────────────────────────────────────────

    let prompt_text = match prompt_file {
        Some(p) => {
            fs::read_to_string(p).with_context(|| format!("failed to read prompt file '{p}'"))?
        }
        None => {
            print!("System prompt file path: ");
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin()
                .lock()
                .read_line(&mut line)
                .context("failed to read prompt file path from stdin")?;
            let file_path = line.trim();
            if file_path.is_empty() {
                anyhow::bail!("prompt file path must not be empty");
            }
            fs::read_to_string(file_path)
                .with_context(|| format!("failed to read prompt file '{file_path}'"))?
        }
    };

    // ── Step 3: Run recommendation engine ────────────────────────────────────

    let mut rec = recommend_from_prompt(&skill_name, "", &prompt_text);

    // ── Step 3b: Detect locally available Ollama models (Fix 2E) ─────────────
    //
    // If Ollama is reachable, override the recommended model list with a model
    // that is actually installed locally. This prevents the common DX failure
    // where the scaffold recommends a model the user hasn't pulled yet.

    let ollama_url_for_detect = std::env::var("VECTORHAWK_OLLAMA_URL")
        .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
        .unwrap_or_else(|_| "http://localhost:11434".to_string());

    let available_ollama: Vec<String> = {
        use vectorhawkd_core::ollama::OllamaClient;
        let client = OllamaClient::new(&ollama_url_for_detect, "");
        if client.health_check().reachable {
            client
                .list_models()
                .ok()
                .map(|models| models.into_iter().map(|m| m.name).collect())
                .unwrap_or_default()
        } else {
            vec![]
        }
    };

    if !available_ollama.is_empty() {
        // Find the first recommended model present locally (prefix match).
        let found = rec.model.recommended.iter().find(|rec_model| {
            available_ollama
                .iter()
                .any(|a| a == *rec_model || a.starts_with(rec_model.as_str()))
        });
        if let Some(matched) = found {
            let matched = matched.clone();
            rec.model.recommended = vec![matched];
        } else {
            // None of the recommended models are available — use first available.
            rec.model.recommended = vec![available_ollama[0].clone()];
        }
    }

    // ── Step 4: Determine final metadata values ───────────────────────────────

    // Normalize the skill ID from the name.
    let skill_id = skill_name
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join("-")
        .replace('_', "-");

    let base = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| camino::Utf8PathBuf::from("."));

    // Try to infer publisher from logged-in account.
    let effective_registry = registry_url.unwrap_or("https://app.vectorhawk.ai");
    let publisher_id = try_infer_publisher_id(effective_registry)
        .await
        .unwrap_or_else(|| "YOUR_PUBLISHER_ID".to_string());

    if skip_metadata {
        // Scaffold with hardcoded defaults.
        scaffold_with_defaults(&skill_name, &skill_id, &prompt_text, &base, &publisher_id)?;
        println!("Created skill at {}/{}/SKILL.md", base, skill_id);
        println!();
        println!("Next steps:");
        let publisher_note = if publisher_id == "YOUR_PUBLISHER_ID" {
            " and publisher ID".to_string()
        } else {
            format!(" (publisher: {publisher_id} — verify at app.vectorhawk.ai/portal)")
        };
        println!(
            "  1. Edit {}/{}/SKILL.md — fill in description{}",
            base, skill_id, publisher_note
        );
        println!("  2. vectorhawk skill validate {}/{}/", base, skill_id);
        return Ok(());
    }

    if accept_suggestions {
        // Scaffold with recommendations applied.
        scaffold_with_recommendations(
            &skill_name,
            &skill_id,
            &prompt_text,
            &base,
            &rec,
            &publisher_id,
        )?;
        let confidence_str = format!("{:?}", rec.confidence).to_lowercase();
        println!("Created skill at {}/{}/SKILL.md", base, skill_id);
        println!("Applied recommendations (confidence: {confidence_str}):");
        println!("  Network: {}", rec.permissions.network);
        println!("  Filesystem: {}", rec.permissions.filesystem);
        let model_primary = rec
            .model
            .recommended
            .first()
            .map(|s| s.as_str())
            .unwrap_or("gemma3:2b");
        let model_status = if available_ollama
            .iter()
            .any(|a| a == model_primary || a.starts_with(model_primary))
        {
            format!("{model_primary} (installed locally)")
        } else {
            format!("{model_primary} (not installed — run: ollama pull {model_primary})")
        };
        println!("  Offline model: {model_status}");
        println!(
            "  Timeout: {}",
            format_duration_ms(rec.execution.timeout_ms)
        );
        println!("  Sandbox: {}", rec.execution.sandbox);
        println!("  Triggers: {}", rec.triggers.join(", "));
        if publisher_id != "YOUR_PUBLISHER_ID" {
            println!("  Publisher: {publisher_id}");
        }
        println!();
        println!("Next: vectorhawk skill validate {}/{}/", base, skill_id);
        return Ok(());
    }

    // ── Interactive mode: show each group and prompt [Y/n/edit] ───────────────

    let confidence_str = format!("{:?}", rec.confidence).to_lowercase();
    println!();
    println!("Recommendations (confidence: {confidence_str}):");
    println!();

    // Permissions group.
    println!(
        "Network: {}  |  Filesystem: {}  |  Clipboard: {}",
        rec.permissions.network, rec.permissions.filesystem, rec.permissions.clipboard
    );
    let (net, fs_perm, clip) = prompt_field_group_permissions(
        rec.permissions.network,
        rec.permissions.filesystem,
        rec.permissions.clipboard,
    )?;

    // Model group.
    let model_primary = rec
        .model
        .recommended
        .first()
        .map(|s| s.as_str())
        .unwrap_or("gemma3:2b");
    let fallback_desc = if rec.model.fallback == "mcp_sampling" {
        "falls back to your AI client if unavailable"
    } else {
        "returns an error if unavailable"
    };
    let model_install_note = if available_ollama
        .iter()
        .any(|a| a == model_primary || a.starts_with(model_primary))
    {
        format!("{model_primary} (installed locally)")
    } else {
        format!("{model_primary} (not installed — run: ollama pull {model_primary})")
    };
    println!(
        "Offline model: {} (needs ≥{}B params) — {}",
        model_install_note, rec.model.min_params_b, fallback_desc
    );
    let (min_params_b, recommended_models, fallback) = prompt_field_group_model(
        rec.model.min_params_b,
        &rec.model.recommended,
        rec.model.fallback,
    )?;

    // Execution group.
    println!(
        "Timeout: {}  |  Memory: {} MB  |  Sandbox: {}",
        format_duration_ms(rec.execution.timeout_ms),
        rec.execution.memory_mb,
        rec.execution.sandbox
    );
    let (timeout_ms, memory_mb, sandbox) = prompt_field_group_execution(
        rec.execution.timeout_ms,
        rec.execution.memory_mb,
        rec.execution.sandbox,
    )?;

    // Triggers group.
    println!("Suggested triggers: {}", rec.triggers.join(", "));
    let triggers = prompt_field_group_triggers(&rec.triggers)?;

    // ── Scaffold with confirmed values ────────────────────────────────────────

    let skill_dir = base.join(&skill_id);
    if skill_dir.exists() {
        anyhow::bail!("directory '{}' already exists", skill_dir);
    }
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create directory '{skill_dir}'"))?;

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

    let recommended_yaml = recommended_models
        .iter()
        .map(|m| format!("      - {m}"))
        .collect::<Vec<_>>()
        .join("\n");

    let body_block = indent_block(&prompt_text, 12);
    let skill_md = format!(
        "---\nname: {skill_name}\ndescription: \"TODO: describe what this skill does\"\nlicense: MIT\n\
         metadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: {publisher_id}\n\
         {triggers_yaml}\
         \n    permissions:\n      network: {net}\n      filesystem: {fs_perm}\n      clipboard: {clip}\n\
         \n    execution:\n      timeout_ms: {timeout_ms}\n      memory_mb: {memory_mb}\n      sandbox: {sandbox}\n\
         \n    model:\n      min_params_b: {min_params_b}\n      recommended:\n{recommended_yaml}\n      fallback: {fallback}\n\
         \n    workflow:\n      - id: run\n        type: llm\n        prompt:\n          kind: inline\n          body: |\n\
         {body_block}\
         \n        inputs:\n          text: input.text\n\
         \n    schemas:\n      inputs:\n        type: object\n        properties:\n          text:\n            type: string\n\
         \n        required:\n          - text\n---\n"
    );

    fs::write(skill_dir.join("SKILL.md"), &skill_md).context("failed to write SKILL.md")?;

    println!();
    println!("Created skill at {skill_dir}/SKILL.md");
    println!();
    println!("Next steps:");
    let publisher_note = if publisher_id == "YOUR_PUBLISHER_ID" {
        " — fill in description and publisher ID".to_string()
    } else {
        format!(" — fill in description (publisher: {publisher_id})")
    };
    println!("  1. Edit {skill_dir}/SKILL.md{publisher_note}");
    println!("  2. vectorhawk skill validate {skill_dir}/");

    Ok(())
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

/// Scaffold a skill with hardcoded minimal defaults (no recommendations).
fn scaffold_with_defaults(
    skill_name: &str,
    skill_id: &str,
    prompt_text: &str,
    base: &camino::Utf8Path,
    publisher_id: &str,
) -> Result<()> {
    use std::fs;

    let skill_dir = base.join(skill_id);
    if skill_dir.exists() {
        anyhow::bail!("directory '{}' already exists", skill_dir);
    }
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create directory '{skill_dir}'"))?;

    let body_block = indent_block(prompt_text, 12);
    let skill_md = format!(
        "---\nname: {skill_name}\ndescription: \"TODO: describe what this skill does\"\nlicense: MIT\n\
         metadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: {publisher_id}\n\
         \n    permissions:\n      network: none\n      filesystem: none\n      clipboard: none\n\
         \n    execution:\n      timeout_ms: 30000\n      memory_mb: 256\n      sandbox: strict\n\
         \n    workflow:\n      - id: run\n        type: llm\n        prompt:\n          kind: inline\n          body: |\n\
         {body_block}\
         \n        inputs:\n          text: input.text\n\
         \n    schemas:\n      inputs:\n        type: object\n        properties:\n          text:\n            type: string\n\
         \n        required:\n          - text\n---\n"
    );

    fs::write(skill_dir.join("SKILL.md"), &skill_md).context("failed to write SKILL.md")
}

/// Scaffold a skill using the provided recommendation output.
fn scaffold_with_recommendations(
    skill_name: &str,
    skill_id: &str,
    prompt_text: &str,
    base: &camino::Utf8Path,
    rec: &vectorhawkd_core::recommend::Recommendations,
    publisher_id: &str,
) -> Result<()> {
    use std::fs;

    let skill_dir = base.join(skill_id);
    if skill_dir.exists() {
        anyhow::bail!("directory '{}' already exists", skill_dir);
    }
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("failed to create directory '{skill_dir}'"))?;

    let triggers_yaml = if rec.triggers.is_empty() {
        String::new()
    } else {
        let items = rec
            .triggers
            .iter()
            .map(|t| format!("      - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("    triggers:\n{items}\n")
    };

    let recommended_yaml = rec
        .model
        .recommended
        .iter()
        .map(|m| format!("      - {m}"))
        .collect::<Vec<_>>()
        .join("\n");

    let body_block = indent_block(prompt_text, 12);
    let skill_md = format!(
        "---\nname: {skill_name}\ndescription: \"TODO: describe what this skill does\"\nlicense: MIT\n\
         metadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: {publisher_id}\n\
         {triggers_yaml}\
         \n    permissions:\n      network: {net}\n      filesystem: {fs_perm}\n      clipboard: {clip}\n\
         \n    execution:\n      timeout_ms: {timeout_ms}\n      memory_mb: {memory_mb}\n      sandbox: {sandbox}\n\
         \n    model:\n      min_params_b: {min_params_b}\n      recommended:\n{recommended_yaml}\n      fallback: {fallback}\n\
         \n    workflow:\n      - id: run\n        type: llm\n        prompt:\n          kind: inline\n          body: |\n\
         {body_block}\
         \n        inputs:\n          text: input.text\n\
         \n    schemas:\n      inputs:\n        type: object\n        properties:\n          text:\n            type: string\n\
         \n        required:\n          - text\n---\n",
        net = rec.permissions.network,
        fs_perm = rec.permissions.filesystem,
        clip = rec.permissions.clipboard,
        timeout_ms = rec.execution.timeout_ms,
        memory_mb = rec.execution.memory_mb,
        sandbox = rec.execution.sandbox,
        min_params_b = rec.model.min_params_b,
        fallback = rec.model.fallback,
    );

    fs::write(skill_dir.join("SKILL.md"), &skill_md).context("failed to write SKILL.md")
}

// ── Interactive field-group prompt helpers ────────────────────────────────────

/// Prompt for the permissions field group. Returns (network, filesystem, clipboard).
fn prompt_field_group_permissions(
    net_rec: &str,
    fs_rec: &str,
    clip_rec: &str,
) -> Result<(String, String, String)> {
    use std::io::{self, BufRead, Write};

    print!("[Y/n/edit]: ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok();
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        return Ok((
            net_rec.to_string(),
            fs_rec.to_string(),
            clip_rec.to_string(),
        ));
    }

    if answer == "n" || answer == "no" {
        return Ok(("none".to_string(), "none".to_string(), "none".to_string()));
    }

    // Edit mode.
    print!("  network [{net_rec}]: ");
    io::stdout().flush().ok();
    let mut net = String::new();
    io::stdin().lock().read_line(&mut net).ok();
    let net = net.trim();
    let net = if net.is_empty() { net_rec } else { net };

    print!("  filesystem [{fs_rec}]: ");
    io::stdout().flush().ok();
    let mut fs_val = String::new();
    io::stdin().lock().read_line(&mut fs_val).ok();
    let fs_val = fs_val.trim();
    let fs_val = if fs_val.is_empty() { fs_rec } else { fs_val };

    print!("  clipboard [{clip_rec}]: ");
    io::stdout().flush().ok();
    let mut clip = String::new();
    io::stdin().lock().read_line(&mut clip).ok();
    let clip = clip.trim();
    let clip = if clip.is_empty() { clip_rec } else { clip };

    Ok((net.to_string(), fs_val.to_string(), clip.to_string()))
}

/// Prompt for the model field group. Returns (min_params_b, recommended, fallback).
fn prompt_field_group_model(
    min_b_rec: f32,
    models_rec: &[String],
    fallback_rec: &str,
) -> Result<(f32, Vec<String>, String)> {
    use std::io::{self, BufRead, Write};

    print!("[Y/n/edit]: ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok();
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        return Ok((min_b_rec, models_rec.to_vec(), fallback_rec.to_string()));
    }

    if answer == "n" || answer == "no" {
        return Ok((1.0, vec!["gemma3:2b".to_string()], "error".to_string()));
    }

    // Edit mode.
    let models_str = models_rec.join(",");
    print!("  min_params_b [{min_b_rec}]: ");
    io::stdout().flush().ok();
    let mut min_b = String::new();
    io::stdin().lock().read_line(&mut min_b).ok();
    let min_b = min_b.trim();
    let min_b: f32 = if min_b.is_empty() {
        min_b_rec
    } else {
        min_b.parse().unwrap_or(min_b_rec)
    };

    print!("  recommended [{models_str}]: ");
    io::stdout().flush().ok();
    let mut models_input = String::new();
    io::stdin().lock().read_line(&mut models_input).ok();
    let models_input = models_input.trim();
    let models: Vec<String> = if models_input.is_empty() {
        models_rec.to_vec()
    } else {
        models_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    print!("  fallback [{fallback_rec}]: ");
    io::stdout().flush().ok();
    let mut fallback = String::new();
    io::stdin().lock().read_line(&mut fallback).ok();
    let fallback = fallback.trim();
    let fallback = if fallback.is_empty() {
        fallback_rec
    } else {
        fallback
    };

    Ok((min_b, models, fallback.to_string()))
}

/// Prompt for the execution field group. Returns (timeout_ms, memory_mb, sandbox).
fn prompt_field_group_execution(
    timeout_rec: u32,
    memory_rec: u32,
    sandbox_rec: &str,
) -> Result<(u32, u32, String)> {
    use std::io::{self, BufRead, Write};

    print!("[Y/n/edit]: ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok();
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        return Ok((timeout_rec, memory_rec, sandbox_rec.to_string()));
    }

    if answer == "n" || answer == "no" {
        return Ok((30000, 256, "strict".to_string()));
    }

    // Edit mode.
    print!("  timeout_ms [{timeout_rec}]: ");
    io::stdout().flush().ok();
    let mut timeout = String::new();
    io::stdin().lock().read_line(&mut timeout).ok();
    let timeout = timeout.trim();
    let timeout: u32 = if timeout.is_empty() {
        timeout_rec
    } else {
        timeout.parse().unwrap_or(timeout_rec)
    };

    print!("  memory_mb [{memory_rec}]: ");
    io::stdout().flush().ok();
    let mut memory = String::new();
    io::stdin().lock().read_line(&mut memory).ok();
    let memory = memory.trim();
    let memory: u32 = if memory.is_empty() {
        memory_rec
    } else {
        memory.parse().unwrap_or(memory_rec)
    };

    print!("  sandbox [{sandbox_rec}]: ");
    io::stdout().flush().ok();
    let mut sandbox = String::new();
    io::stdin().lock().read_line(&mut sandbox).ok();
    let sandbox = sandbox.trim();
    let sandbox = if sandbox.is_empty() {
        sandbox_rec
    } else {
        sandbox
    };

    Ok((timeout, memory, sandbox.to_string()))
}

/// Prompt for the triggers field group. Returns the chosen trigger list.
fn prompt_field_group_triggers(triggers_rec: &[String]) -> Result<Vec<String>> {
    use std::io::{self, BufRead, Write};

    print!("[Y/n/edit]: ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok();
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        return Ok(triggers_rec.to_vec());
    }

    if answer == "n" || answer == "no" {
        return Ok(Vec::new());
    }

    // Edit mode: show current and read new comma-separated list.
    let current = triggers_rec.join(", ");
    print!("  triggers [{current}]: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input).ok();
    let input = input.trim();
    if input.is_empty() {
        return Ok(triggers_rec.to_vec());
    }

    let triggers: Vec<String> = input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 3)
        .collect();

    Ok(triggers)
}

// ── skill publish ─────────────────────────────────────────────────────────────

async fn cmd_skill_publish(
    path: camino::Utf8PathBuf,
    registry_url: &str,
    dry_run: bool,
) -> Result<()> {
    use flate2::{write::GzEncoder, Compression};
    use std::fs;
    use tar::Builder;
    use vectorhawkd_core::{
        auth::{load_tokens, AuthClient},
        registry::RegistryClient,
        state::AppState,
    };

    if !path.join("SKILL.md").exists() {
        anyhow::bail!(
            "no SKILL.md found at '{}' — run 'vectorhawk skill init' to create one",
            path
        );
    }

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let tokens = load_tokens(&state, registry_url)
        .context("failed to load auth tokens")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not authenticated — run 'vectorhawk auth login' first, then retry publish"
            )
        })?;

    // Read SKILL.md and auto-inject publisher if it is missing or is the placeholder.
    let skill_md_path = path.join("SKILL.md");
    let skill_md_text = fs::read_to_string(&skill_md_path)
        .with_context(|| format!("failed to read {skill_md_path}"))?;

    let (skill_md_to_pack, injected_publisher) = if needs_publisher(&skill_md_text) {
        let client = AuthClient::new(registry_url);
        let user = client
            .me(&tokens.access_token)
            .context("failed to look up your publisher ID — try 'vectorhawk auth login'")?;
        let slug = derive_publisher_slug(&user.display_name);
        let patched = inject_publisher_field(&skill_md_text, &slug);
        (patched, Some(slug))
    } else {
        (skill_md_text, None)
    };

    if let Some(ref slug) = injected_publisher {
        println!("Publisher not set — using '{slug}' from your logged-in account.");
    }

    // Pack the skill directory into an in-memory tar.gz.
    // Build the tar manually so we can substitute the (possibly patched) SKILL.md.
    let mut gz_buf: Vec<u8> = Vec::new();
    {
        let enc = GzEncoder::new(&mut gz_buf, Compression::default());
        let mut tar = Builder::new(enc);

        // Add all directory contents except SKILL.md.
        tar.append_dir_all(".", &path)
            .with_context(|| format!("failed to pack skill directory '{path}'"))?;

        // Override SKILL.md with the (possibly patched) content.
        let skill_md_bytes = skill_md_to_pack.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(skill_md_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "SKILL.md", skill_md_bytes)
            .context("failed to append patched SKILL.md to archive")?;

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
    // CLI publish is not discovery-driven; pass None so the backend does not
    // attempt to back-fill frontmatter from a catalog stub.
    let resp = tokio::task::spawn_blocking(move || registry.compile_and_publish(gz_buf, None))
        .await
        .context("publish task panicked")?
        .context("publish failed")?;

    println!(
        "Published '{}' v{}",
        resp.frontmatter.name,
        resp.frontmatter
            .version
            .as_deref()
            .or(resp.frontmatter.vh_version.as_deref())
            .unwrap_or("?")
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
version: {version}
publisher: {publisher}
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

// ── skill update ─────────────────────────────────────────────────────────────

/// The outcome of comparing an installed version against the registry latest.
///
/// Pure, testable — no I/O.  Produced by [`decide_update_action`] and consumed
/// by [`cmd_skill_update`] to drive the install loop and the summary table.
#[derive(Debug, PartialEq)]
pub enum UpdateAction {
    /// Registry advertises a newer version; install it.
    Upgrade { latest: semver::Version },
    /// Installed version is already at or ahead of registry latest.
    AlreadyUpToDate,
    /// Registry has no published versions for this skill.
    NoVersions,
}

/// Decide what action to take for a single skill based on installed vs. latest.
///
/// `latest` is `None` when the registry returned no published version for the
/// skill.  This function is pure and has no side effects, making it unit-testable
/// without a DB or HTTP server.
pub fn decide_update_action(
    installed: &semver::Version,
    latest: Option<semver::Version>,
) -> UpdateAction {
    match latest {
        None => UpdateAction::NoVersions,
        Some(v) if v > *installed => UpdateAction::Upgrade { latest: v },
        Some(_) => UpdateAction::AlreadyUpToDate,
    }
}

/// A single row in the update summary table.
struct UpdateRow {
    skill_id: String,
    current: String,
    latest: String,
    status: String,
}

/// Update one or all active skills from the registry.
///
/// Fetches the latest registry version for each targeted skill, installs it
/// when strictly newer than installed, then prints a one-line-per-skill summary
/// table with columns: SKILL_ID | CURRENT | LATEST | STATUS.
async fn cmd_skill_update(id: Option<&str>, all: bool, registry_url: Option<&str>) -> Result<()> {
    use rusqlite::{Connection, OptionalExtension};
    use semver::Version;
    use vectorhawkd_core::{
        registry::RegistryClient, state::AppState, updater::install_from_registry,
    };

    if id.is_none() && !all {
        eprintln!("Usage: vectorhawk skill update <id>  |  vectorhawk skill update --all");
        std::process::exit(2);
    }

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let url = registry_url.unwrap_or("https://app.vectorhawk.ai");

    // Collect the list of (skill_id, installed_version) pairs to process.
    let targets: Vec<(String, String)> = if all {
        // Open connection and collect into a Vec immediately so conn/stmt drop
        // before leaving this block — rusqlite MappedRows borrows both.
        let rows: Vec<(String, String)> = {
            let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
            let mut stmt = conn
                .prepare(
                    "SELECT skill_id, active_version FROM installed_skills \
                     WHERE current_status = 'active' ORDER BY skill_id",
                )
                .context("failed to prepare update query")?;
            let collected = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .context("failed to execute update query")?
                .collect::<Result<Vec<_>, _>>()
                .context("failed to collect skill rows")?;
            collected
        };
        rows
    } else {
        // Single skill — look up its installed version.
        let skill_id = id.expect("id is Some when !all");
        let installed_ver: Option<String> = {
            let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
            conn.query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
                [skill_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to query installed_skills")?
        };
        let ver =
            installed_ver.ok_or_else(|| anyhow::anyhow!("skill '{skill_id}' is not installed"))?;
        vec![(skill_id.to_string(), ver)]
    };

    if targets.is_empty() {
        println!("No active skills installed.");
        return Ok(());
    }

    let registry_url_owned = url.to_string();

    // ── Process each skill; collect summary rows ──────────────────────────────

    let mut summary: Vec<UpdateRow> = Vec::with_capacity(targets.len());
    let mut had_error = false;

    for (skill_id, installed_str) in targets {
        let installed = match Version::parse(&installed_str) {
            Ok(v) => v,
            Err(e) => {
                summary.push(UpdateRow {
                    skill_id: skill_id.clone(),
                    current: installed_str.clone(),
                    latest: "-".to_string(),
                    status: format!("error: invalid semver ({e})"),
                });
                had_error = true;
                continue;
            }
        };

        // Fetch latest version from registry (blocking call in async context).
        let registry_url_inner = registry_url_owned.clone();
        let skill_id_inner = skill_id.clone();
        let detail_result = tokio::task::spawn_blocking(move || {
            let reg = RegistryClient::new(&registry_url_inner);
            reg.fetch_skill_detail(&skill_id_inner)
        })
        .await
        .context("fetch detail task panicked")?;

        let detail = match detail_result {
            Ok(d) => d,
            Err(e) => {
                summary.push(UpdateRow {
                    skill_id: skill_id.clone(),
                    current: installed_str.clone(),
                    latest: "-".to_string(),
                    status: format!("error: registry unreachable ({e:#})"),
                });
                had_error = true;
                continue;
            }
        };

        let latest_parsed = detail
            .latest_version
            .as_deref()
            .map(Version::parse)
            .transpose()
            .with_context(|| {
                format!(
                    "registry returned invalid semver for '{skill_id}': {:?}",
                    detail.latest_version
                )
            })?;

        let latest_display = detail.latest_version.as_deref().unwrap_or("-").to_string();

        let action = decide_update_action(&installed, latest_parsed);

        match action {
            UpdateAction::NoVersions => {
                summary.push(UpdateRow {
                    skill_id,
                    current: installed_str,
                    latest: latest_display,
                    status: "no published versions".to_string(),
                });
            }
            UpdateAction::AlreadyUpToDate => {
                summary.push(UpdateRow {
                    skill_id,
                    current: installed_str,
                    latest: latest_display,
                    status: "up to date".to_string(),
                });
            }
            UpdateAction::Upgrade {
                latest: ref latest_ver,
            } => {
                let state_db = state.db_path.clone();
                let state_root = state.root_dir.clone();
                let url_clone = registry_url_owned.clone();
                let skill_clone = skill_id.clone();
                let ver_str = latest_ver.to_string();

                let install_result = tokio::task::spawn_blocking(move || {
                    let install_state = AppState {
                        root_dir: state_root,
                        db_path: state_db,
                    };
                    let registry = RegistryClient::new(&url_clone);
                    install_from_registry(&install_state, &registry, &skill_clone, Some(&ver_str))
                })
                .await
                .context("install task panicked")?;

                let status = match install_result {
                    Ok(_) => format!("updated {} → {}", installed_str, latest_ver),
                    Err(e) => {
                        had_error = true;
                        format!("error: update failed ({e:#})")
                    }
                };

                summary.push(UpdateRow {
                    skill_id,
                    current: installed_str,
                    latest: latest_display,
                    status,
                });
            }
        }
    }

    // ── Print summary table ───────────────────────────────────────────────────

    let id_w = summary
        .iter()
        .map(|r| r.skill_id.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let cur_w = summary
        .iter()
        .map(|r| r.current.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let lat_w = summary
        .iter()
        .map(|r| r.latest.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<id_w$}  {:<cur_w$}  {:<lat_w$}  STATUS",
        "SKILL_ID", "CURRENT", "LATEST",
    );
    println!("{}", "-".repeat(id_w + cur_w + lat_w + 14));

    for row in &summary {
        println!(
            "{:<id_w$}  {:<cur_w$}  {:<lat_w$}  {}",
            row.skill_id, row.current, row.latest, row.status,
        );
    }

    if had_error {
        anyhow::bail!("one or more skills could not be updated — see table above");
    }

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

    let is_ssh = std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some();

    if is_ssh {
        println!("SSH session detected.");
        println!();
        println!("Option A — SSH tunnel (browser login):");
        println!();
        println!("  Step 1: In a NEW terminal on your LOCAL machine, run:");
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| "<this-machine>".to_string());
        println!("    ssh -L {port}:localhost:{port} {hostname}");
        println!();
        println!("  Step 2: Open this URL in your browser:");
        println!("    {}", init.auth_url);
        println!();
        println!("  Keep the tunnel open until login completes.");
        println!();
        println!("Option B — Personal Access Token (no tunnel needed):");
        println!("  1. Open https://app.vectorhawk.ai/portal/settings in your browser.");
        println!("  2. Create a token (starts with vh_pat_...).");
        println!("  3. Run:  vectorhawk auth token <vh_pat_...>");
        println!();
    } else {
        let browser_opened = try_open_browser(&init.auth_url);
        if browser_opened {
            println!("Opening browser for VectorHawk login...");
            println!();
            println!("If your browser does not open automatically, open this URL:");
            println!("  {}", init.auth_url);
        } else {
            // Could not open browser — headless environment (no DISPLAY, no
            // TERM_PROGRAM, or xdg-open / open returned an error).
            // Print the URL so the user can copy it to any browser.
            println!("Could not open browser automatically.");
            println!();
            println!("Open this URL in your browser to continue:");
            println!("  {}", init.auth_url);
            println!();
            println!("Waiting for callback on http://127.0.0.1:{port}/oauth/cli/callback ...");
        }
    }
    println!();

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

    finish_auth(&state).await;
    Ok(())
}

/// Attempt to open a URL in the system default browser.
///
/// Returns `true` if the browser launch command succeeded (process spawned and
/// — for macOS/Linux — exited with status 0), `false` otherwise.
///
/// On Linux the function additionally checks for `DISPLAY` and `WAYLAND_DISPLAY`
/// before attempting `xdg-open`; if neither is set the environment is almost
/// certainly headless and `xdg-open` will fail silently.
///
/// On macOS `open` is a reliable synchronous launcher: we wait for it and check
/// the exit code.
fn try_open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        // `open` on macOS exits with 0 on success and 1 when the URL cannot
        // be opened.  Wait for it to finish so we get the real exit code.
        match std::process::Command::new("open").arg(url).status() {
            Ok(s) => return s.success(),
            Err(_) => return false,
        }
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux we need at least one of DISPLAY or WAYLAND_DISPLAY to be set
        // for a graphical browser to work.
        let has_display = std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("TERM_PROGRAM").is_some();
        if !has_display {
            return false;
        }
        match std::process::Command::new("xdg-open").arg(url).status() {
            Ok(s) => return s.success(),
            Err(_) => return false,
        }
    }

    #[cfg(target_os = "windows")]
    {
        match std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .status()
        {
            Ok(s) => return s.success(),
            Err(_) => return false,
        }
    }

    // Unknown platform — report failure so the URL gets printed.
    #[allow(unreachable_code)]
    false
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
                    let slug = derive_publisher_slug(&user.display_name);
                    println!("Logged in as {} ({}).", user.display_name, user.email);
                    println!("Publisher ID: {slug}");
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
    finish_auth(&state).await;
    Ok(())
}

// ── auth pair ────────────────────────────────────────────────────────────────

async fn cmd_auth_pair(code: &str, registry_url: &str) -> Result<()> {
    use vectorhawkd_core::{auth::save_tokens, state::AppState};

    let state = AppState::bootstrap().context("failed to bootstrap application state")?;

    // Retrieve or generate a stable device UUID. SQLite calls go directly on
    // this thread — AppState wraps a non-Send Connection, so no spawn_blocking.
    let device_uuid = state
        .get_sync_state("device_uuid")
        .context("failed to read sync_state")?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| {
        #[cfg(unix)]
        {
            let mut buf = vec![0u8; 256];
            if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
                if let Some(end) = buf.iter().position(|&b| b == 0) {
                    return String::from_utf8_lossy(&buf[..end]).to_string();
                }
            }
        }
        "unknown".to_string()
    });

    #[cfg(target_os = "macos")]
    let platform = "macos";
    #[cfg(target_os = "linux")]
    let platform = "linux";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let platform = "windows";

    #[cfg(target_arch = "aarch64")]
    let arch = "aarch64";
    #[cfg(not(target_arch = "aarch64"))]
    let arch = "x86_64";

    let body = serde_json::json!({
        "code": code,
        "device_uuid": device_uuid,
        "hostname": hostname,
        "platform": platform,
        "arch": arch,
        "agent_version": env!("CARGO_PKG_VERSION"),
    });

    // HTTP call — use the async reqwest client already in scope via the
    // existing registry client infrastructure.
    let resp = reqwest::Client::new()
        .post(format!("{registry_url}/api/devices/confirm-pair"))
        .json(&body)
        .send()
        .await
        .context("failed to reach registry")?;

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Pairing code not found. Make sure you copied it correctly from the portal.");
    }
    if status == reqwest::StatusCode::GONE {
        anyhow::bail!(
            "Pairing code has expired or was already used. \
             Return to the portal to generate a new one."
        );
    }
    if !status.is_success() {
        anyhow::bail!("Pairing failed ({status}): {body_text}");
    }

    #[derive(serde::Deserialize)]
    struct PairResponse {
        device_id: String,
        access_token: String,
        refresh_token: String,
    }
    let pair: PairResponse =
        serde_json::from_str(&body_text).context("unexpected response format from registry")?;

    save_tokens(
        &state,
        registry_url,
        &pair.access_token,
        &pair.refresh_token,
    )
    .context("failed to save auth token")?;

    // Persist device_uuid and device_id so the daemon uses this registration.
    state
        .set_sync_state("device_uuid", &device_uuid)
        .context("failed to save device_uuid")?;
    state
        .set_sync_state("device_id", &pair.device_id)
        .context("failed to save device_id")?;

    println!("Paired successfully.");
    println!("Device ID : {}", pair.device_id);
    println!("Registry  : {registry_url}");
    println!();
    finish_auth(&state).await;

    Ok(())
}

// ── mcp serve ─────────────────────────────────────────────────────────────────

async fn cmd_mcp_serve(server: Option<String>) -> Result<()> {
    // Delegate entirely to the shim library, which owns the per-frame read-loop
    // and the mid-session daemon-kill fallback logic (AC4).
    //
    // When --server <slug> is supplied the shim acts as a single-backend
    // adapter: it strips the `<slug>__` prefix from outbound tool names and
    // re-adds it for `tools/call`. This is used by the per-server entries
    // that F2 writes into ~/.claude.json.
    vectorhawkd_shim::run_shim(server).await
}

// ── plugin install ─────────────────────────────────────────────────────────────

async fn cmd_plugin_install(slug: &str, version: Option<&str>, registry_url: &str) -> Result<()> {
    use vectorhawkd_core::{auth::load_tokens, state::AppState};

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let tokens = load_tokens(&state, registry_url)
        .ok()
        .flatten()
        .ok_or_else(|| {
            anyhow::anyhow!("Not logged in to {registry_url}. Run `vectorhawk auth login` first.")
        })?;

    let mut body = serde_json::json!({ "plugin_slug": slug });
    if let Some(v) = version {
        body["version"] = serde_json::Value::String(v.to_string());
    }

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/plugin-installations",
            registry_url.trim_end_matches('/')
        ))
        .bearer_auth(&tokens.access_token)
        .json(&body)
        .send()
        .await
        .context("failed to reach registry")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("Authentication failed — run `vectorhawk auth login` and try again.");
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Plugin '{slug}' not found in the catalog.");
    }
    if status == reqwest::StatusCode::CONFLICT {
        println!("Plugin '{slug}' is already installed.");
        return Ok(());
    }
    if !status.is_success() {
        anyhow::bail!("Install failed ({status}): {text}");
    }

    println!("Installing plugin '{slug}'…");
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(ver) = v.get("version").and_then(|s| s.as_str()) {
            println!("  version: {ver}");
        }
        for w in v
            .get("warnings")
            .and_then(|w| w.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(s) = w.as_str() {
                println!("  warning: {s}");
            }
        }
    }
    println!(
        "Requested. The daemon will register it as a Claude Code plugin shortly — \
         run `claude plugin list` (restart Claude Code to pick it up)."
    );
    Ok(())
}

// ── mcp setup ────────────────────────────────────────────────────────────────

async fn cmd_mcp_setup(client: Option<&str>, dry_run: bool) -> Result<()> {
    use vectorhawkd_mcp::setup::{
        build_mcp_entry, detect_ai_clients, detect_claude_code, uninstall_claude_skills,
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

        // Remove VectorHawk's own command-skills whenever Claude Code is present.
        // These wrapped MCP tools as slash commands and cluttered the skills
        // list; management now lives in the portal. User-installed skills are
        // untouched.
        let has_claude_code = clients.iter().any(|c| c.name == "Claude Code");
        if has_claude_code || wrote_claude_code {
            match uninstall_claude_skills() {
                Ok(removed) if !removed.is_empty() => {
                    println!(
                        "Removed {} VectorHawk command-skill(s) from ~/.claude/skills/ \
                         (manage skills in the portal or via the vectorhawk MCP tools).",
                        removed.len()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("warning: failed to remove command-skills: {e:#}");
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

            match uninstall_claude_skills() {
                Ok(removed) if !removed.is_empty() => {
                    println!(
                        "Removed {} VectorHawk command-skill(s) from ~/.claude/skills/.",
                        removed.len()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("warning: failed to remove command-skills: {e:#}");
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

// ── mcp remove ───────────────────────────────────────────────────────────────

async fn cmd_mcp_remove() -> Result<()> {
    use vectorhawkd_mcp::setup::{detect_ai_clients, remove_mcp_entry, uninstall_claude_skills};

    let clients = detect_ai_clients();
    if clients.is_empty() {
        println!("No supported AI clients detected — nothing to remove.");
    } else {
        for config in &clients {
            match remove_mcp_entry(config) {
                Ok(true) => println!(
                    "{}: removed vectorhawk MCP entry from {}.",
                    config.name,
                    config.config_path.display()
                ),
                Ok(false) => println!("{}: vectorhawk was not configured — skipped.", config.name),
                Err(e) => eprintln!(
                    "warning: failed to update {} config at {}: {e:#}",
                    config.name,
                    config.config_path.display()
                ),
            }
        }
    }

    match uninstall_claude_skills() {
        Ok(removed) if !removed.is_empty() => {
            println!(
                "Removed {} VectorHawk slash command(s) from ~/.claude/skills/.",
                removed.len()
            );
        }
        Ok(_) => {
            println!("No VectorHawk slash commands found in ~/.claude/skills/.");
        }
        Err(e) => {
            eprintln!("warning: failed to remove slash commands: {e:#}");
        }
    }

    println!("Done. Restart your AI client to apply the change.");
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

async fn cmd_daemon_restart() -> Result<()> {
    tokio::task::spawn_blocking(install::restart)
        .await
        .context("restart task panicked")?
        .context("daemon restart failed")
}

// ── sync status ───────────────────────────────────────────────────────────────

async fn cmd_sync_status() -> Result<()> {
    use rusqlite::Connection;
    use vectorhawkd_core::state::AppState;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;

    // Print sync_state key/value table.
    println!("=== Sync State ===");
    let sync_rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT key, value FROM sync_state ORDER BY key")
            .context("failed to prepare sync_state query")?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("failed to query sync_state")?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect sync_state rows")?
    };

    if sync_rows.is_empty() {
        println!("  (no sync state — daemon may not be registered yet)");
    } else {
        for (k, v) in &sync_rows {
            println!("  {k}: {v}");
        }
    }
    println!();

    // Print installation records with reconciler state.
    println!("=== Installation Records ===");

    struct InstallRow {
        skill_id: String,
        active_version: String,
        source: String,
        installation_id: Option<String>,
        deactivated: bool,
        current_status: String,
        installed_at: String,
    }

    // The new columns (installation_id, source, deactivated) may not exist on
    // older databases; use coalesce/default-value queries to be defensive.
    let rows: Vec<InstallRow> = {
        // Try querying with new columns; fall back gracefully.
        let mut stmt = conn
            .prepare(
                "SELECT skill_id, active_version, \
                 COALESCE(source, 'local'), \
                 installation_id, \
                 COALESCE(deactivated, 0), \
                 current_status, \
                 installed_at \
                 FROM installed_skills ORDER BY skill_id",
            )
            .context("failed to prepare installations query")?;

        let mapped = stmt
            .query_map([], |row| {
                Ok(InstallRow {
                    skill_id: row.get(0)?,
                    active_version: row.get(1)?,
                    source: row.get(2)?,
                    installation_id: row.get(3)?,
                    deactivated: row.get::<_, i64>(4).map(|v| v != 0)?,
                    current_status: row.get(5)?,
                    installed_at: row.get(6)?,
                })
            })
            .context("failed to execute installations query")?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect installation rows")?
    };

    if rows.is_empty() {
        println!("  (no installations)");
        return Ok(());
    }

    println!(
        "{:<30} {:<10} {:<10} {:<12} {:<38}",
        "SKILL ID", "VERSION", "SOURCE", "STATUS", "INSTALLATION ID"
    );
    println!("{}", "-".repeat(105));
    for r in &rows {
        let status = if r.deactivated {
            "deactivated".to_string()
        } else {
            r.current_status.clone()
        };
        let iid = r.installation_id.as_deref().unwrap_or("(local)");
        println!(
            "{:<30} {:<10} {:<10} {:<12} {:<38}",
            r.skill_id, r.active_version, r.source, status, iid
        );
        if !r.installed_at.is_empty() {
            println!("  installed_at: {}", r.installed_at);
        }
    }

    Ok(())
}

// ── skill uninstall ───────────────────────────────────────────────────────────

/// Uninstall a locally installed skill.
///
/// Removes:
/// - `<root>/skills/<skill_id>/` (all version dirs + active/ symlink)
/// - `installed_skills` row
/// - `skill_versions` rows (skill_ratings and skill_execution_counts are kept)
///
/// If a managed `installation_id` is present, also PATCHes the backend to
/// `"deactivated"` so the reconciler does not reinstall on the next sync.
async fn cmd_skill_uninstall(id: &str, registry_url: Option<&str>) -> Result<()> {
    use rusqlite::{Connection, OptionalExtension};
    use vectorhawkd_core::{
        audit::{write_audit_event_direct, AuditEvent},
        installer::uninstall_skill,
        state::AppState,
    };

    let state = AppState::bootstrap().context("failed to bootstrap state")?;
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;

    // Read the row BEFORE we delete it, to capture installation_id + version.
    struct SkillRow {
        active_version: String,
        installation_id: Option<String>,
    }

    let row: Option<SkillRow> = conn
        .query_row(
            "SELECT active_version, installation_id \
             FROM installed_skills WHERE skill_id = ?1",
            [id],
            |row| {
                Ok(SkillRow {
                    active_version: row.get(0)?,
                    installation_id: row.get(1)?,
                })
            },
        )
        .optional()
        .context("failed to query installed_skills")?;

    let info = match row {
        Some(r) => r,
        None => {
            anyhow::bail!("skill '{id}' is not installed");
        }
    };

    // Perform the local removal (files + DB rows).
    uninstall_skill(&state, id).with_context(|| format!("failed to remove skill '{id}'"))?;

    // Emit audit event.
    let ts = chrono::Utc::now().to_rfc3339();
    write_audit_event_direct(
        &state.db_path,
        &AuditEvent {
            event_type: "skill_uninstalled".to_string(),
            payload: serde_json::json!({
                "skill_id":  id,
                "version":   info.active_version,
                "source":    "cli",
                "ts":        ts,
            }),
        },
    );

    // Managed-install coordination: if this device had a backend desired-state
    // installation, PATCH it to "deactivated" so the reconciler doesn't
    // reinstall the skill on the next SSE sync tick.
    match &info.installation_id {
        Some(iid) if !iid.is_empty() => {
            let url = registry_url.unwrap_or("https://app.vectorhawk.ai");
            match deactivate_backend_installation(iid, url, &state).await {
                Ok(()) => {
                    println!(
                        "Uninstalled skill '{id}' (version {}) and deactivated managed installation.",
                        info.active_version
                    );
                }
                Err(e) => {
                    // Non-fatal: the local removal already succeeded.
                    eprintln!(
                        "warning: local removal succeeded, but could not deactivate managed \
                         installation {iid} on the backend: {e:#}"
                    );
                    eprintln!(
                        "  The skill may be reinstalled by the reconciler on the next sync. \
                         To prevent this, remove it from the portal."
                    );
                    println!(
                        "Uninstalled skill '{id}' (version {}) locally.",
                        info.active_version
                    );
                }
            }
        }
        _ => {
            // No managed installation — purely local, no backend call needed.
            println!(
                "Uninstalled skill '{id}' (version {}).",
                info.active_version
            );
        }
    }

    Ok(())
}

/// PATCH `state: "deactivated"` to `PATCH /api/installations/{id}`.
///
/// Used by the CLI uninstall path so the reconciler does not immediately
/// reinstall a just-removed skill on its next SSE sync tick.
async fn deactivate_backend_installation(
    installation_id: &str,
    registry_url: &str,
    state: &vectorhawkd_core::state::AppState,
) -> Result<()> {
    let url = format!(
        "{}/api/installations/{}",
        registry_url.trim_end_matches('/'),
        installation_id
    );

    let db_path = state.db_path.clone();
    let reg_url = registry_url.to_string();
    let token = tokio::task::spawn_blocking(move || load_all_tokens_from_db(&db_path, &reg_url))
        .await
        .context("token-load task panicked")?
        .context("failed to load auth tokens")?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let body = serde_json::json!({ "state": "deactivated" });

    let req = match token {
        Some(t) => client.patch(&url).bearer_auth(t).json(&body),
        None => client.patch(&url).json(&body),
    };

    let resp = req
        .send()
        .await
        .with_context(|| format!("PATCH {url} failed"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // Installation already removed on the backend — that's fine.
        return Ok(());
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PATCH {url} returned {status}: {body_text}");
    }

    Ok(())
}

/// PATCH `state: "deactivated"` to `PATCH /api/plugin-installations/{id}/state`.
async fn deactivate_backend_plugin_installation(
    installation_id: &str,
    registry_url: &str,
    state: &vectorhawkd_core::state::AppState,
) -> Result<()> {
    let url = format!(
        "{}/api/plugin-installations/{}/state",
        registry_url.trim_end_matches('/'),
        installation_id
    );

    let db_path = state.db_path.clone();
    let reg_url = registry_url.to_string();
    let token = tokio::task::spawn_blocking(move || load_all_tokens_from_db(&db_path, &reg_url))
        .await
        .context("token-load task panicked")?
        .context("failed to load auth tokens")?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let body = serde_json::json!({ "state": "deactivated" });

    let req = match token {
        Some(t) => client.patch(&url).bearer_auth(t).json(&body),
        None => client.patch(&url).json(&body),
    };

    let resp = req
        .send()
        .await
        .with_context(|| format!("PATCH {url} failed"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("PATCH {url} returned {status}: {body_text}");
    }

    Ok(())
}

/// Load the Bearer token for `registry_url` from the local auth DB.
///
/// Returns `Ok(None)` when the user is not logged in (no token row), and
/// `Ok(Some(token))` when a token is found. Errors only on DB failures.
fn load_all_tokens_from_db(
    db_path: &camino::Utf8Path,
    registry_url: &str,
) -> Result<Option<String>> {
    use vectorhawkd_core::auth::load_all_tokens;
    use vectorhawkd_core::state::AppState;

    // Reconstruct a minimal AppState from just the db_path (we only need
    // load_all_tokens which uses state.db_path).
    let root_dir = db_path
        .parent()
        .map(|p| p.to_owned())
        .ok_or_else(|| anyhow::anyhow!("db_path has no parent"))?;
    let state = AppState {
        root_dir,
        db_path: db_path.to_owned(),
    };

    let tokens = load_all_tokens(&state).context("failed to load auth tokens")?;
    let token = tokens
        .into_iter()
        .find(|r| r.registry_url == registry_url)
        .map(|r| r.access_token);
    Ok(token)
}

// ── plugin uninstall ──────────────────────────────────────────────────────────

/// Uninstall a locally installed plugin.
///
/// Removes:
/// - `~/.claude/plugins/marketplaces/vectorhawk/plugins/<slug>/`
/// - `~/.claude/plugins/cache/vectorhawk/<slug>/`
/// - Entry in `~/.claude/plugins/marketplaces/vectorhawk/.claude-plugin/marketplace.json`
/// - Entry in `~/.claude/plugins/installed_plugins.json`
/// - `enabledPlugins["<slug>@vectorhawk"]` in `~/.claude/settings.json`
///
/// Also PATCHes the backend plugin-installation to "deactivated" if an
/// installation_id is discoverable from the managed_path_markers table.
async fn cmd_plugin_uninstall(slug: &str, registry_url: Option<&str>) -> Result<()> {
    use rusqlite::{Connection, OptionalExtension};
    use vectorhawkd_core::{
        audit::{write_audit_event_direct, AuditEvent},
        state::AppState,
    };
    use vectorhawkd_daemon::managed_paths::uninstall_plugin_bundle;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    // Check whether this plugin is locally installed by probing the marketplace
    // directory.  This is the canonical signal rather than a SQLite table
    // (plugins don't have an installed_skills row).
    let plugin_installed = is_plugin_installed_locally(slug)?;
    if !plugin_installed {
        anyhow::bail!(
            "plugin '{slug}' is not installed locally. \
             Run `claude plugin list` to see installed plugins."
        );
    }

    // Retrieve managed installation_id from managed_path_markers if present.
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let installation_id: Option<String> = conn
        .query_row(
            "SELECT installation_id FROM managed_path_markers \
             WHERE kind = 'plugin' AND slug = ?1 LIMIT 1",
            [slug],
            |row| row.get(0),
        )
        .optional()
        .context("failed to query managed_path_markers")?
        .flatten();
    drop(conn);

    // Run the local removal (filesystem + Claude Code JSON files).
    tokio::task::spawn_blocking({
        let slug_owned = slug.to_string();
        move || uninstall_plugin_bundle(&slug_owned)
    })
    .await
    .context("plugin uninstall task panicked")?
    .with_context(|| format!("failed to remove plugin '{slug}' from local marketplace"))?;

    // Emit audit event.
    let ts = chrono::Utc::now().to_rfc3339();
    write_audit_event_direct(
        &state.db_path,
        &AuditEvent {
            event_type: "plugin_uninstalled".to_string(),
            payload: serde_json::json!({
                "slug":   slug,
                "source": "cli",
                "ts":     ts,
            }),
        },
    );

    // Managed-install coordination.
    let url = registry_url.unwrap_or("https://app.vectorhawk.ai");
    match &installation_id {
        Some(iid) if !iid.is_empty() => {
            match deactivate_backend_plugin_installation(iid, url, &state).await {
                Ok(()) => {
                    println!("Uninstalled plugin '{slug}' and deactivated managed installation.");
                }
                Err(e) => {
                    eprintln!(
                        "warning: local removal succeeded, but could not deactivate managed \
                         plugin installation {iid} on the backend: {e:#}"
                    );
                    eprintln!(
                        "  The plugin may be reinstalled by the reconciler on the next sync. \
                         To prevent this, remove it from the portal."
                    );
                    println!("Uninstalled plugin '{slug}' locally.");
                }
            }
        }
        _ => {
            // No managed installation record — warn the user if they are logged in.
            let has_token = state.get_sync_state("device_id").ok().flatten().is_some();
            if has_token {
                eprintln!(
                    "warning: no managed installation record found for plugin '{slug}'. \
                     If it was installed via the portal, remove it there too to prevent \
                     the reconciler from reinstalling it."
                );
            }
            println!("Uninstalled plugin '{slug}' locally.");
        }
    }

    Ok(())
}

/// Returns `true` if the plugin's marketplace source directory exists under
/// `~/.claude/plugins/marketplaces/vectorhawk/plugins/<slug>/`.
fn is_plugin_installed_locally(slug: &str) -> Result<bool> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    let plugin_src = home
        .join(".claude")
        .join("plugins")
        .join("marketplaces")
        .join("vectorhawk")
        .join("plugins")
        .join(slug);
    Ok(plugin_src.exists())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "commands_tests.rs"]
mod commands_tests;
