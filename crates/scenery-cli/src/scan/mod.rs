//! `scenery scan --once`: a read-only, storage-free inventory of this
//! machine's skills, MCP servers and plugins. Never bootstraps `AppState`,
//! never touches the keychain, a state dir, client configs or a LaunchAgent.

mod flow;
mod from_file;
pub mod login;
pub mod report;
pub mod summary;
pub mod upload;

use anyhow::{Context, Result};
use clap::Args;
use scenery_core::DEFAULT_REGISTRY_URL;
use std::path::PathBuf;
use std::time::Duration;
use time::OffsetDateTime;

#[derive(Debug, Clone, Args)]
pub struct ScanArgs {
    /// Run one scan and exit (the only mode; reserved for clarity).
    #[arg(long, required = true)]
    pub once: bool,
    /// Sign in as yourself in a browser instead of using an org scan token.
    /// The session is never saved and is revoked when the scan exits. Set
    /// SCENERY_NO_BROWSER=1 to print the sign-in URL instead of opening it.
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
    /// `SCENERY_SCAN_TOKEN`, read once by `run` so tests need no process env.
    pub token: Option<String>,
    /// Where a failed upload's report is saved (the system temp dir).
    pub save_dir: PathBuf,
    /// Waits between upload retries.
    pub backoff: Vec<Duration>,
    /// Divides the wait after a 429 (1 in production; tests shrink it).
    pub rate_wait_divisor: u32,
    /// Opens the sign-in URL; returns whether a browser opened (`--login`).
    pub open_browser: fn(&str) -> bool,
    /// How long `--login` waits for the browser sign-in.
    pub login_timeout: Duration,
    /// Revoke a `--login` session on SIGINT/SIGTERM/SIGHUP (off in unit
    /// tests: the handlers would stay installed in the test process).
    pub watch_signals: bool,
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

pub async fn run(args: ScanArgs) -> Result<i32> {
    let home = dirs::home_dir().context("could not resolve the home directory")?;
    let env = ScanEnv {
        euid: real_euid(),
        home,
        now: OffsetDateTime::now_utc(),
        save_dir: std::env::temp_dir(),
        token: std::env::var("SCENERY_SCAN_TOKEN").ok(),
        backoff: upload::DEFAULT_BACKOFF.to_vec(),
        rate_wait_divisor: 1,
        open_browser: if std::env::var_os("SCENERY_NO_BROWSER").is_some() {
            |_| false
        } else {
            crate::try_open_browser
        },
        watch_signals: true,
        login_timeout: login::SIGN_IN_TIMEOUT,
    };
    run_to(args, &env, &mut std::io::stdout()).await
}

pub(crate) async fn run_to(
    args: ScanArgs,
    env: &ScanEnv,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
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
    if let Some(p) = &args.out {
        // Checked before any sign-in or upload, so nothing is wasted on it.
        if p.symlink_metadata().is_ok() {
            writeln!(
                out,
                "scenery scan: {} already exists; pass a new --out path.",
                p.display()
            )?;
            return Ok(2);
        }
    }
    if !args.dry_run {
        return flow::upload_flow(&args, env, out).await;
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
