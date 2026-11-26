//! `scenery scan --once`: a read-only, storage-free inventory of this
//! machine's skills, MCP servers and plugins. Never bootstraps `AppState`,
//! never touches the keychain, a state dir, client configs or a LaunchAgent.

pub mod report;
pub mod summary;
pub mod upload;

use anyhow::{Context, Result};
use clap::Args;
use scenery_core::DEFAULT_REGISTRY_URL;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;
use upload::{ScanAuth, UploadError};

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
    /// `SCENERY_SCAN_TOKEN`, read once by `run` so tests need no process env.
    pub token: Option<String>,
    /// Where a failed upload's report is saved (the system temp dir).
    pub save_dir: PathBuf,
    /// Waits between upload retries.
    pub backoff: Vec<Duration>,
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
    if !args.dry_run {
        return upload_flow(&args, env, out).await;
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

/// Resolve the org scan token: the environment, else `--token-file`.
fn resolve_token(args: &ScanArgs, env: &ScanEnv) -> Result<Option<String>, String> {
    let raw = match (&env.token, &args.token_file) {
        (Some(t), _) => t.trim().to_string(),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("could not read the token file: {}", e.kind()))?;
            text.lines().next().unwrap_or("").trim().to_string()
        }
        (None, None) => return Ok(None),
    };
    if raw.starts_with("scn_scan_") {
        Ok(Some(raw))
    } else {
        Err("the scan token must start with scn_scan_".to_string())
    }
}

const NO_AUTH: &str = "Set SCENERY_SCAN_TOKEN or run `scenery scan --once --login`.";

/// Save the report and print the failure line; exit 3.
fn save_and_fail(
    env: &ScanEnv,
    report: &report::ScanReport,
    why: &str,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    let ts = env.now.unix_timestamp();
    let target = env.save_dir.join(format!("scenery-scan-{ts}.json"));
    match report::save(report, Some(&target)) {
        Ok(path) => writeln!(
            out,
            "Upload failed: {why} Report saved to {p} — send it to IT, or retry with: \
             scenery scan --once --from-file {p}",
            p = path.display()
        )?,
        Err(e) => writeln!(
            out,
            "Upload failed: {why} The report could not be saved: {e:#}"
        )?,
    }
    Ok(3)
}

async fn upload_flow(args: &ScanArgs, env: &ScanEnv, out: &mut dyn std::io::Write) -> Result<i32> {
    if args.login {
        writeln!(out, "scenery scan: --login is not available in this build")?;
        return Ok(2);
    }
    let auth = match resolve_token(args, env) {
        Ok(Some(t)) => ScanAuth::Token(t),
        Ok(None) => {
            writeln!(out, "{NO_AUTH}")?;
            return Ok(2);
        }
        Err(msg) => {
            writeln!(out, "scenery scan: {msg}")?;
            return Ok(2);
        }
    };
    let client = match upload::client() {
        Ok(c) => c,
        Err(e) => {
            writeln!(out, "scenery scan: {e}")?;
            return Ok(2);
        }
    };
    let base = args.registry_url.as_str();
    if let Some(path) = &args.from_file {
        return from_file(path, &client, base, &auth, env, out).await;
    }
    let session = match upload::session(&client, base, &auth).await {
        Ok(s) => s,
        Err(UploadError::RunnerPresent) => return runner_present(out),
        Err(e) => {
            let items = collect(env, base, false)?;
            let report = report::build(items, env.now)?;
            return save_and_fail(env, &report, &format!("{e}."), out);
        }
    };
    let mut items = collect(env, base, session.auto_submit_mcp)?;
    let found = items.len();
    if found > session.max_items {
        let report = report::build(items, env.now)?;
        let why = format!(
            "{found} items is over this org's limit of {}.",
            session.max_items
        );
        return save_and_fail(env, &report, &why, out);
    }
    // Re-apply the collector's caps (args, byte budget) with the session's limit.
    scenery_daemon::managed_paths::scan_collect::enforce_limits(
        &mut items,
        session.max_items,
        scenery_daemon::managed_paths::scan_collect::MAX_ITEMS_BYTES,
    );
    let report = report::build(items, env.now)?;
    if let Some(path) = &args.out {
        let saved = report::save(&report, Some(path))?;
        writeln!(out, "Report saved to {}", saved.display())?;
    }
    match upload::post_report(&client, base, &auth, &report, &env.backoff).await {
        Ok(receipt) => {
            write!(out, "{}", summary::render(&report.items, args.quiet))?;
            print_receipt(&session.org_name, &receipt, out)?;
            Ok(0)
        }
        Err(UploadError::RunnerPresent) => runner_present(out),
        Err(e) => save_and_fail(env, &report, &format!("{e}."), out),
    }
}

fn collect(
    env: &ScanEnv,
    base: &str,
    include_args: bool,
) -> Result<Vec<scenery_daemon::managed_paths::discoveries::DiscoveryItem>> {
    scenery_daemon::managed_paths::scan_collect::collect_for_scan(&env.home, base, include_args)
}

fn runner_present(out: &mut dyn std::io::Write) -> Result<i32> {
    writeln!(out, "{}", UploadError::RunnerPresent)?;
    Ok(0)
}

fn print_receipt(org: &str, r: &upload::ScanReceipt, out: &mut dyn std::io::Write) -> Result<()> {
    writeln!(
        out,
        "Uploaded to {org}: {} new, {} already known. IT reviews them at {}.",
        r.accepted, r.deduped, r.inventory_url
    )?;
    if !r.rejected.is_empty() {
        writeln!(
            out,
            "{} items were rejected by the server:",
            r.rejected.len()
        )?;
        for item in &r.rejected {
            writeln!(out, "  {} {}: {}", item.kind, item.slug, item.message)?;
        }
    }
    Ok(())
}

/// `--from-file`: post a saved report as is. The file stays in place.
async fn from_file(
    path: &Path,
    client: &reqwest::Client,
    base: &str,
    auth: &ScanAuth,
    env: &ScanEnv,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            writeln!(
                out,
                "scenery scan: could not read {}: {}",
                path.display(),
                e.kind()
            )?;
            return Ok(2);
        }
    };
    let format_ok = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("format").and_then(|f| f.as_str()).map(str::to_string))
        .is_some_and(|f| f == report::REPORT_FORMAT);
    if !format_ok {
        writeln!(
            out,
            "scenery scan: {} is not a scan report.",
            path.display()
        )?;
        return Ok(2);
    }
    match upload::post_body(client, base, auth, body, &env.backoff).await {
        Ok(receipt) => {
            print_receipt("your org", &receipt, out)?;
            Ok(0)
        }
        Err(UploadError::RunnerPresent) => runner_present(out),
        Err(e) => {
            writeln!(
                out,
                "Upload failed: {e}. Report kept at {p} — send it to IT, or retry with: \
                 scenery scan --once --from-file {p}",
                p = path.display()
            )?;
            Ok(3)
        }
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod scan_tests;
