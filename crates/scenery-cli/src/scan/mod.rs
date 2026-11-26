//! `scenery scan --once`: a read-only, storage-free inventory of this
//! machine's skills, MCP servers and plugins. Never bootstraps `AppState`,
//! never touches the keychain, a state dir, client configs or a LaunchAgent.

pub mod report;
pub mod summary;

use anyhow::{Context, Result};
use clap::Args;
use scenery_core::DEFAULT_REGISTRY_URL;
use std::path::PathBuf;
use time::OffsetDateTime;

#[derive(Debug, Clone, Args)]
pub struct ScanArgs {
    /// Run one scan and exit (the only mode; reserved for clarity).
    #[arg(long, required = true)]
    pub once: bool,
    /// Sign in as yourself in a browser instead of using an org scan token.
    #[arg(long, conflicts_with = "token_file")]
    pub login: bool,
    /// File holding the org scan token (the token is never taken from argv).
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    #[arg(long, env = "SCENERY_REGISTRY_URL", default_value = DEFAULT_REGISTRY_URL,
          value_parser = crate::parse_registry_url)]
    pub registry_url: String,
    /// Collect and print the summary; upload nothing.
    #[arg(long, conflicts_with = "from_file")]
    pub dry_run: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
    /// Print only the counts line.
    #[arg(long)]
    pub quiet: bool,
    /// Also save the report to this path (created private, never overwritten).
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Upload a previously saved report.
    #[arg(long, value_name = "PATH")]
    pub from_file: Option<PathBuf>,
}

/// Injected environment, so tests need no real euid or clock.
pub(crate) struct ScanEnv {
    pub euid: u32,
    pub home: PathBuf,
    pub now: OffsetDateTime,
}

fn real_euid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        1
    }
}

pub fn run(args: ScanArgs) -> Result<i32> {
    let home = dirs::home_dir().context("could not resolve the home directory")?;
    let env = ScanEnv {
        euid: real_euid(),
        home,
        now: OffsetDateTime::now_utc(),
    };
    run_with(args, &env)
}

pub(crate) fn run_with(args: ScanArgs, env: &ScanEnv) -> Result<i32> {
    run_to(args, env, &mut std::io::stdout())
}

pub(crate) fn run_to(args: ScanArgs, env: &ScanEnv, out: &mut dyn std::io::Write) -> Result<i32> {
    if env.euid == 0 {
        eprintln!(
            "scenery scan: refusing to run as root. Run it as the user whose \
             machine you want to scan, not with sudo."
        );
        return Ok(2);
    }
    let root = scenery_core::state::AppState::resolve_root_dir()?;
    if root.join("state.db").exists() {
        writeln!(
            out,
            "The Scenery runner is already installed on this machine, so no \
             scan is needed."
        )?;
        return Ok(0);
    }
    if !args.dry_run {
        eprintln!("scenery scan: upload not available in this build (use --dry-run)");
        return Ok(2);
    }
    // Dry run: never reads the session, so MCP args stay out of the output.
    let items = scenery_daemon::managed_paths::scan_collect::collect_for_scan(
        &env.home,
        &args.registry_url,
        false,
    )?;
    let report = report::build(items, env.now)?;
    if let Some(out) = &args.out {
        let path = report::save(&report, Some(out))?;
        eprintln!("Report saved to {}", path.display());
    }
    if args.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        write!(out, "{}", summary::render(&report.items, args.quiet))?;
    }
    Ok(0)
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod scan_tests;
