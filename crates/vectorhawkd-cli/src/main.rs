//! `vectorhawk` — VectorHawk runner user CLI.
//!
//! Subcommand tree:
//!
//! ```text
//! vectorhawk doctor
//! vectorhawk skill list
//! vectorhawk skill install <id> [--path <bundle>]
//! vectorhawk skill info <id>
//! vectorhawk skill run <id> --input <file> [--stub]
//! vectorhawk skill import <path>
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
//! Claude Code / Cursor / etc. configs. On socket connect success it relays
//! over `SocketBackend`; on 2 s timeout it falls back to `EmbeddedBackend`.

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
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List all installed skills.
    List,

    /// Install a skill from a local bundle directory.
    Install {
        /// Path to the skill bundle directory to install.
        #[arg(long, value_name = "BUNDLE_PATH")]
        path: camino::Utf8PathBuf,

        /// Symlink instead of copying the bundle (Unix only, dev mode).
        #[arg(long, default_value_t = false)]
        link: bool,
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
    },

    /// Validate a skill bundle directory.
    Validate {
        /// Path to the skill bundle directory.
        path: camino::Utf8PathBuf,
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
        Command::Doctor { registry_url } => cmd_doctor(registry_url.as_deref()).await,

        Command::Skill(SkillCommand::List) => cmd_skill_list().await,
        Command::Skill(SkillCommand::Install { path, link }) => cmd_skill_install(path, link).await,
        Command::Skill(SkillCommand::Info { id }) => cmd_skill_info(&id).await,
        Command::Skill(SkillCommand::Run { id, input, stub }) => {
            cmd_skill_run(&id, input, stub).await
        }
        Command::Skill(SkillCommand::Import { path }) => cmd_skill_import(path).await,
        Command::Skill(SkillCommand::Validate { path }) => cmd_skill_validate(path).await,

        Command::Auth(AuthCommand::Login { registry_url }) => cmd_auth_login(&registry_url).await,
        Command::Auth(AuthCommand::Logout { registry_url }) => cmd_auth_logout(&registry_url).await,
        Command::Auth(AuthCommand::Status { registry_url }) => cmd_auth_status(&registry_url).await,

        Command::Mcp(McpCommand::Serve) => cmd_mcp_serve().await,
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            cmd_mcp_setup(client.as_deref(), dry_run).await
        }
        Command::Mcp(McpCommand::Sync) => cmd_mcp_sync().await,
        Command::Mcp(McpCommand::Backends) => cmd_mcp_backends().await,

        Command::Daemon(DaemonCommand::Run {
            foreground: _,
            registry_url,
        }) => cmd_daemon_run(registry_url).await,
        Command::Daemon(DaemonCommand::Install) => cmd_daemon_install().await,
        Command::Daemon(DaemonCommand::Uninstall) => cmd_daemon_uninstall().await,
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

async fn cmd_skill_install(path: camino::Utf8PathBuf, link: bool) -> Result<()> {
    use vectorhawkd_core::installer::{install_unpacked_skill, InstallMode};
    use vectorhawkd_core::state::AppState;
    use vectorhawkd_manifest::SkillPackage;

    let state = AppState::bootstrap().context("failed to bootstrap state")?;

    let pkg = SkillPackage::load_from_dir(&path)
        .with_context(|| format!("failed to load skill bundle at {path}"))?;

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
        let ollama_url = std::env::var("OLLAMA_BASE_URL")
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

async fn cmd_skill_import(path: camino::Utf8PathBuf) -> Result<()> {
    use vectorhawkd_core::importer::import_local_skill_md;

    let bundle = import_local_skill_md(&path)
        .with_context(|| format!("failed to import SKILL.md at {path}"))?;

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
                        .unwrap_or_else(|| {
                            "~/.claude.json (Claude Code not detected)".to_string()
                        })
                );
                return Ok(());
            }

            // ── M2: provision the daemon before writing AI client config ──────
            //
            // If the daemon is not installed/running, install it now so the
            // AI client finds it on first use. `ensure_installed` is idempotent.
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

                    // Wait up to 5 s for the socket to appear.
                    wait_for_daemon_socket(5_000).await;
                }
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
                "vectorhawk mcp setup: client '{other}' is not yet supported \
                 (M0 supports only 'claude-code')"
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

// ── mcp sync / backends (deferred — M1.4) ────────────────────────────────────

async fn cmd_mcp_sync() -> Result<()> {
    eprintln!("vectorhawk mcp sync: M1.4 sync trigger not yet implemented");
    std::process::exit(2);
}

async fn cmd_mcp_backends() -> Result<()> {
    eprintln!("vectorhawk mcp backends: M1.4 backend list not yet implemented");
    std::process::exit(2);
}

// ── daemon subcommands ────────────────────────────────────────────────────────

async fn cmd_daemon_run(registry_url: Option<String>) -> Result<()> {
    use vectorhawkd_daemon::{run_daemon, DaemonOpts};

    let opts = DaemonOpts {
        registry_url,
        socket_path_override: None,
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
