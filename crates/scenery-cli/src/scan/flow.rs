//! The upload path of `scenery scan --once`: pick the credential (org scan
//! token, or a `--login` session revoked before returning), read the scan
//! session, collect, post, and save the report when the upload fails.

use super::upload::{self, ScanAuth, Uplink, UploadError};
use super::{from_file, login, report, summary, ScanArgs, ScanEnv};
use anyhow::Result;
use std::path::PathBuf;

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

/// Where a failed upload's report lives.
enum Dest {
    /// Already written (the `--out` copy); just point at it.
    Written(PathBuf),
    /// Write it here now.
    New(PathBuf),
}

/// The state of the `--out` copy.
enum OutCopy {
    /// Not written yet (or no `--out`).
    Pending,
    Written(PathBuf),
    /// Writing `--out` failed; never try that path again.
    Failed,
}

/// The existing `--out` copy, else `--out` if not yet tried, else a fresh
/// temp-dir name, so the report is never lost.
fn dest(args: &ScanArgs, env: &ScanEnv, copy: OutCopy) -> Dest {
    match (copy, &args.out) {
        (OutCopy::Written(p), _) => Dest::Written(p),
        (OutCopy::Pending, Some(p)) => Dest::New(p.clone()),
        (OutCopy::Pending, None) | (OutCopy::Failed, _) => {
            Dest::New(report::fresh_path(&env.save_dir, env.now))
        }
    }
}

/// Save the report (unless already saved) and print the failure line; exit 3.
fn save_and_fail(
    dest: Dest,
    report: &report::ScanReport,
    why: &str,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    let saved = match dest {
        Dest::Written(p) => Ok(p),
        Dest::New(p) => report::save(report, Some(&p)),
    };
    match saved {
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

pub(super) async fn upload_flow(
    args: &ScanArgs,
    env: &ScanEnv,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    let auth = if args.login {
        None
    } else {
        match resolve_token(args, env) {
            Ok(Some(t)) => Some(ScanAuth::Token(t)),
            Ok(None) => {
                writeln!(out, "{NO_AUTH}")?;
                return Ok(2);
            }
            Err(msg) => {
                writeln!(out, "scenery scan: {msg}")?;
                return Ok(2);
            }
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
    if let Some(auth) = auth {
        let link = Uplink {
            client: &client,
            base,
            auth: &auth,
        };
        return upload_with(args, env, &link, out).await;
    }
    let session = match login::sign_in(base, env.open_browser, env.login_timeout).await {
        Ok(s) => s,
        Err(e) => {
            writeln!(out, "scenery scan: {e:#}")?;
            return Ok(2);
        }
    };
    let auth = ScanAuth::Bearer(session.access_token.clone());
    let link = Uplink {
        client: &client,
        base,
        auth: &auth,
    };
    // Every exit after a successful sign-in revokes the session: errors and
    // upload failures here, and an interrupt via the signal watchers.
    let guard = if env.watch_signals {
        match login::revoke_on_signal(base, &session) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("warning: an interrupt will not sign out the scan session: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let result = upload_with(args, env, &link, out).await;
    login::sign_out(base, &session).await;
    if let Some(g) = guard {
        g.disarm();
    }
    result
}

async fn upload_with(
    args: &ScanArgs,
    env: &ScanEnv,
    link: &Uplink<'_>,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    if let Some(path) = &args.from_file {
        return from_file::run(path, link, &env.backoff, out).await;
    }
    let session = match upload::session(link).await {
        Ok(s) => s,
        Err(UploadError::RunnerPresent) => return runner_present(out),
        Err(e) => {
            let items = collect(env, link.base, false)?;
            let report = report::build(items, env.now)?;
            return save_and_fail(
                dest(args, env, OutCopy::Pending),
                &report,
                &format!("{e}."),
                out,
            );
        }
    };
    if let Some(email) = &session.user_email {
        writeln!(out, "Signed in as {email}.")?;
    }
    let mut items = collect(env, link.base, session.auto_submit_mcp)?;
    let found = items.len();
    if found > session.max_items {
        let report = report::build(items, env.now)?;
        let why = format!(
            "{found} items is over this org's limit of {}.",
            session.max_items
        );
        return save_and_fail(dest(args, env, OutCopy::Pending), &report, &why, out);
    }
    // Re-apply the collector's caps (args, byte budget) with the session's limit.
    scenery_daemon::managed_paths::scan_collect::enforce_limits(
        &mut items,
        session.max_items,
        scenery_daemon::managed_paths::scan_collect::MAX_ITEMS_BYTES,
    );
    let report = report::build(items, env.now)?;
    let written = save_out(args, &report, out)?;
    match upload::post_report(link, &report, &env.backoff).await {
        Ok(receipt) => {
            write!(out, "{}", summary::render(&report.items, args.quiet))?;
            print_receipt(&session.org_name, &receipt, out)?;
            Ok(0)
        }
        Err(UploadError::RunnerPresent) => runner_present(out),
        Err(e) => save_and_fail(dest(args, env, written), &report, &format!("{e}."), out),
    }
}

/// Write the `--out` copy. A failure here does not stop the upload.
fn save_out(
    args: &ScanArgs,
    report: &report::ScanReport,
    out: &mut dyn std::io::Write,
) -> Result<OutCopy> {
    let Some(path) = &args.out else {
        return Ok(OutCopy::Pending);
    };
    match report::save(report, Some(path)) {
        Ok(saved) => {
            writeln!(out, "Report saved to {}", saved.display())?;
            Ok(OutCopy::Written(saved))
        }
        Err(e) => {
            writeln!(
                out,
                "scenery scan: could not save --out ({e:#}); uploading anyway."
            )?;
            Ok(OutCopy::Failed)
        }
    }
}

fn collect(
    env: &ScanEnv,
    base: &str,
    include_args: bool,
) -> Result<Vec<scenery_daemon::managed_paths::discoveries::DiscoveryItem>> {
    scenery_daemon::managed_paths::scan_collect::collect_for_scan(&env.home, base, include_args)
}

pub(super) fn runner_present(out: &mut dyn std::io::Write) -> Result<i32> {
    writeln!(out, "{}", UploadError::RunnerPresent)?;
    Ok(0)
}

pub(super) fn print_receipt(
    org: &str,
    r: &upload::ScanReceipt,
    out: &mut dyn std::io::Write,
) -> Result<()> {
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
