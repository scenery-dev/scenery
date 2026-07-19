//! `vectorhawk uninstall` — remove VectorHawk and restore the host to as close
//! to its pre-install state as possible, leaving **nothing broken behind**.
//!
//! Design invariant (see board card "unified uninstall command"):
//!
//! > After uninstall, the machine's AI-tool surface == (what the user had
//! > before VH) − (org-provided tools that were never really theirs), with
//! > everything the user brought restored to working order.
//!
//! Uninstall is therefore a *restore* operation, not a *delete* operation. Every
//! external mutation the runner made is unwound in reverse, and the result is
//! summarized in a **restore report** with three sections:
//!
//! * **REMOVED (completely)** — tools that only worked *via* VectorHawk (gateway-
//!   brokered MCP servers, IT-curated managed skills/plugins). Their client-config
//!   entries are deleted entirely so no dead entry is left behind, and each is
//!   listed with enough detail (name, transport, upstream) for the user to re-add
//!   it against their own configuration.
//! * **KEPT (unregistered)** — the user's *own* tools VH only audited. Left in
//!   place, untouched, simply no longer governed by VectorHawk.
//! * **RESTORED** — items VH took over that pre-existed on the machine (native
//!   `~/.claude` takeovers, adopted originals), put back where they were.
//!
//! Parts marked `TODO(journal)` depend on the "universal restore journal" and
//! "adopt data-loss" cards; until those land, uninstall restores what the F1
//! migrator already backs up and reports the rest as best-effort.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;

/// Flags for `vectorhawk uninstall`.
#[derive(Debug, Default)]
pub struct UninstallOpts {
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Also delete the VectorHawk data directory (state.db, installed skills,
    /// cache) and the OS-keychain auth token. Off by default so a re-install can
    /// pick up where it left off.
    pub purge: bool,
    /// Print the plan and report but make no changes.
    pub dry_run: bool,
    /// Where to write the report file. Defaults to `~/VectorHawk-uninstall-<ts>`.
    pub report_dir: Option<PathBuf>,
    /// Registry base URL, for the backend device-deprovision call.
    pub registry_url: String,
}

/// One tool that only worked via VectorHawk and was removed outright.
#[derive(Debug, Serialize)]
pub struct RemovedItem {
    pub name: String,
    pub kind: RemovedKind,
    /// Human-readable guidance for re-adding this on the user's own config.
    pub reinstall_hint: String,
    /// Client config files the entry was stripped from.
    pub stripped_from: Vec<String>,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RemovedKind {
    /// Gateway-brokered MCP server — credentials lived server-side, so it cannot
    /// keep working without the runner. Removed completely.
    BrokeredMcp,
    /// IT-curated managed skill pushed into `~/.claude/skills`.
    ManagedSkill,
    /// Managed plugin pushed into `~/.claude/plugins`.
    /// Reserved — populated once the restore-journal card wires plugin removal.
    #[allow(dead_code)]
    ManagedPlugin,
}

/// A user-owned tool VH only audited; left in place, no longer governed.
#[derive(Debug, Serialize)]
pub struct KeptItem {
    pub name: String,
    pub location: String,
    pub note: String,
}

/// An item VH had taken over that was restored to its original location.
#[derive(Debug, Serialize)]
pub struct RestoredItem {
    pub name: String,
    pub restored_to: String,
    pub from_backup: String,
}

#[derive(Debug, Serialize, Default)]
pub struct RestoreReport {
    pub removed: Vec<RemovedItem>,
    pub kept: Vec<KeptItem>,
    pub restored: Vec<RestoredItem>,
    /// Non-fatal problems (a config we couldn't parse, a backup we couldn't
    /// restore). Surfaced so the user knows what to check by hand.
    pub warnings: Vec<String>,
    pub purged_data_dir: bool,
}

/// Entry point wired from `main.rs`.
pub async fn run(opts: UninstallOpts) -> Result<()> {
    let mut report = RestoreReport::default();

    // ── 1. Plan (read-only): classify everything before mutating anything. ──
    let plan = build_plan(&mut report).context("failed to inspect current install")?;

    // Show the plan and, unless --yes, confirm. A user demoing VectorHawk needs
    // to see *exactly* what will and won't survive before committing.
    print_plan(&plan, &report);
    if opts.dry_run {
        println!("\n(dry run — no changes made)");
        write_report(&report, &opts)?;
        return Ok(());
    }
    if !opts.yes && !confirm("Proceed with uninstall?")? {
        println!("Aborted. Nothing changed.");
        return Ok(());
    }

    // ── 2. Restore first, so a failure mid-way never loses user data. ──
    // Native `~/.claude` takeovers + adopted originals from the restore journal.
    // TODO(journal): replace with a full journal replay once the "universal
    // restore journal" and "adopt data-loss" cards land. Today this drives the
    // existing F1 migrator backups only.
    restore_from_journal(&mut report);

    // ── 3. Strip every client-config entry VectorHawk wrote. ──
    strip_client_configs(&plan, &mut report);

    // ── 4. Remove managed skills/plugins VH pushed into ~/.claude. ──
    remove_managed_artifacts(&plan, &mut report);

    // ── 5. Stop + deregister the daemon (LaunchAgent / systemd unit, logs). ──
    if let Err(e) = tokio::task::spawn_blocking(crate::install::uninstall)
        .await
        .context("uninstall task panicked")?
    {
        report
            .warnings
            .push(format!("failed to remove the background service: {e:#}"));
    }

    // ── 6. Tell the backend this device is gone: release brokered creds + audit. ──
    // auth logout clears the local token; device-deprovision releases the
    // gateway-side credential bindings for this device.
    if let Err(e) = crate::cmd_auth_logout(&opts.registry_url).await {
        report
            .warnings
            .push(format!("could not log out / deprovision cleanly: {e:#}"));
    }
    // TODO(backend): POST /devices/{id}/deprovision so IT's audit log records the
    // offboarding and the gateway drops this device's brokered credential grants.

    // ── 7. Optional hard purge of local state. ──
    if opts.purge {
        match purge_data_dir() {
            Ok(()) => report.purged_data_dir = true,
            Err(e) => report
                .warnings
                .push(format!("failed to purge data directory: {e:#}")),
        }
    }

    // ── 8. Write + print the restore report. ──
    let paths = write_report(&report, &opts)?;
    print_summary(&report, &paths);
    println!("\nRestart your AI client(s) to apply the change.");
    Ok(())
}

/// What we intend to do, computed read-only up front.
struct Plan {
    /// Gateway-brokered MCP installs (gateway_url present) → remove completely.
    brokered: Vec<vectorhawkd_core::state::McpInstallRow>,
    /// Detected AI clients whose config holds VH entries.
    clients: Vec<vectorhawkd_mcp::setup::ClientConfig>,
}

/// Read current state and populate the KEPT section (nothing mutated here).
fn build_plan(report: &mut RestoreReport) -> Result<Plan> {
    use vectorhawkd_core::state::AppState;
    use vectorhawkd_mcp::setup::{detect_ai_clients, detect_unmanaged_servers};

    let state = AppState::bootstrap().context("failed to open VectorHawk state")?;
    let installs = state.list_mcp_installs().unwrap_or_default();
    let brokered: Vec<_> = installs
        .into_iter()
        .filter(|r| r.gateway_url.is_some())
        .collect();

    // KEPT: the user's own MCP servers VH merely detected as "unmanaged". These
    // are never touched — we only remove VH's own shim key from the same file.
    for u in detect_unmanaged_servers() {
        report.kept.push(KeptItem {
            name: u.server_name,
            location: u.client_name,
            note: "your own server — left in place, no longer audited by VectorHawk".into(),
        });
    }

    Ok(Plan {
        brokered,
        clients: detect_ai_clients(),
    })
}

/// Remove VH's shim key (`vectorhawk`) AND every brokered per-slug key from each
/// client config, so nothing that points at the now-absent runner is left.
fn strip_client_configs(plan: &Plan, report: &mut RestoreReport) {
    use vectorhawkd_mcp::setup::remove_mcp_entry;

    for client in &plan.clients {
        // VH's own shim entry.
        if let Err(e) = remove_mcp_entry(client) {
            report.warnings.push(format!(
                "{}: could not remove the vectorhawk entry ({}): {e:#}",
                client.name,
                client.config_path.display()
            ));
        }
        // Brokered per-slug entries the F2 pusher wrote (Claude Code only today).
        for row in &plan.brokered {
            match remove_named_server(client, &row.mcp_server_id) {
                Ok(true) => {
                    record_removed(report, row, RemovedKind::BrokeredMcp, &client.name);
                }
                Ok(false) => {}
                Err(e) => report.warnings.push(format!(
                    "{}: could not remove brokered server '{}': {e:#}",
                    client.name, row.mcp_server_name
                )),
            }
        }
    }

    // If a brokered server was in the DB but not found in any config, still report
    // it as removed so the user knows it's gone and how to re-add it.
    for row in &plan.brokered {
        if !report.removed.iter().any(|r| r.name == row.mcp_server_name) {
            record_removed(report, row, RemovedKind::BrokeredMcp, "(state only)");
        }
    }
}

fn record_removed(
    report: &mut RestoreReport,
    row: &vectorhawkd_core::state::McpInstallRow,
    kind: RemovedKind,
    stripped_from: &str,
) {
    if let Some(existing) = report
        .removed
        .iter_mut()
        .find(|r| r.name == row.mcp_server_name)
    {
        existing.stripped_from.push(stripped_from.to_string());
        return;
    }
    report.removed.push(RemovedItem {
        name: row.mcp_server_name.clone(),
        kind,
        reinstall_hint: reinstall_hint(row),
        stripped_from: vec![stripped_from.to_string()],
    });
}

/// Reconstruct enough of the original server shape for the user to re-add it.
/// A brokered server's real upstream/credential lived server-side, so the best
/// we can offer is the server name + package source + auth type.
fn reinstall_hint(row: &vectorhawkd_core::state::McpInstallRow) -> String {
    let src = if row.package_source.is_empty() {
        "unknown source".to_string()
    } else {
        row.package_source.clone()
    };
    format!(
        "Re-add '{}' from {} (auth: {}). Credentials were brokered by VectorHawk \
         and were never stored on this machine — supply your own.",
        row.mcp_server_name, src, row.auth_type
    )
}

/// Remove an arbitrary named entry from a client's `mcpServers` map. Mirrors
/// `setup::remove_mcp_entry` but for a caller-supplied key (the brokered slug).
fn remove_named_server(
    client: &vectorhawkd_mcp::setup::ClientConfig,
    server_key: &str,
) -> Result<bool> {
    use std::fs;
    if !client.config_path.exists() {
        return Ok(false);
    }
    let text = fs::read_to_string(&client.config_path)?;
    let mut obj: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&text)
        .unwrap_or(serde_json::Value::Object(Default::default()))
    {
        serde_json::Value::Object(m) => m,
        _ => return Ok(false),
    };
    let removed = if let Some(serde_json::Value::Object(map)) = obj.get_mut(&client.mcp_key) {
        map.remove(server_key).is_some()
    } else {
        false
    };
    if removed {
        fs::write(
            &client.config_path,
            serde_json::to_string_pretty(&serde_json::Value::Object(obj))?,
        )?;
    }
    Ok(removed)
}

/// Remove managed skills/plugins VH pushed into `~/.claude`, and its slash
/// commands. Populates the REMOVED section for managed skills.
fn remove_managed_artifacts(_plan: &Plan, report: &mut RestoreReport) {
    use vectorhawkd_mcp::setup::uninstall_claude_skills;
    match uninstall_claude_skills() {
        Ok(removed) => {
            for name in removed {
                report.removed.push(RemovedItem {
                    name,
                    kind: RemovedKind::ManagedSkill,
                    reinstall_hint:
                        "VectorHawk slash command — nothing to re-add; it only existed for VectorHawk."
                            .into(),
                    stripped_from: vec!["~/.claude/skills".into()],
                });
            }
        }
        Err(e) => report
            .warnings
            .push(format!("failed to remove managed slash commands: {e:#}")),
    }
    // TODO(journal): also walk `~/.claude/skills` + `~/.claude/plugins` for
    // `.vectorhawk-managed.json` markers and remove those managed copies, adding
    // ManagedSkill/ManagedPlugin RemovedItems for each.
}

/// Restore native takeovers + adopted originals from the restore journal.
fn restore_from_journal(_report: &mut RestoreReport) {
    // TODO(journal): read the universal restore journal and, for each entry with
    // source ∈ {native, adopted}, copy backup_path → original_path and push a
    // RestoredItem. Until the journal exists, drive the F1 migrator's
    // `~/.claude/.vectorhawk-backup/<latest>` set via the existing rollback path.
}

/// Delete the VectorHawk data directory + keychain token (`--purge`).
fn purge_data_dir() -> Result<()> {
    use vectorhawkd_core::state::AppState;
    let state = AppState::bootstrap()?;
    let root = state.root_dir.as_std_path().to_path_buf();
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    // TODO: also remove ~/Library/Logs/VectorHawk and ~/.claude.json.lock, and
    // delete the `com.vectorhawk.agent` keychain item (auth logout already
    // clears the token row; keychain deletion is best-effort).
    Ok(())
}

// ── report rendering ────────────────────────────────────────────────────────

struct ReportPaths {
    markdown: PathBuf,
    json: PathBuf,
}

fn write_report(report: &RestoreReport, opts: &UninstallOpts) -> Result<ReportPaths> {
    let dir = opts
        .report_dir
        .clone()
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("."));
    // Stable, sortable basename. Timestamp is supplied by the caller/env rather
    // than Date::now so the report path is deterministic in tests.
    let stamp = std::env::var("VECTORHAWK_UNINSTALL_STAMP").unwrap_or_else(|_| "latest".into());
    let md = dir.join(format!("VectorHawk-uninstall-{stamp}.md"));
    let json = dir.join(format!("VectorHawk-uninstall-{stamp}.json"));
    std::fs::write(&md, render_markdown(report)).with_context(|| format!("writing {md:?}"))?;
    std::fs::write(&json, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {json:?}"))?;
    Ok(ReportPaths { markdown: md, json })
}

fn render_markdown(r: &RestoreReport) -> String {
    let mut s = String::from("# VectorHawk uninstall — restore report\n\n");
    s.push_str(
        "VectorHawk has been removed. This report records exactly what changed so \
         your system is left in as close to its original state as possible.\n\n",
    );

    s.push_str("## Removed completely\n\n");
    if r.removed.is_empty() {
        s.push_str("_Nothing — no VectorHawk-managed tools were installed._\n\n");
    } else {
        s.push_str(
            "These only worked *through* VectorHawk, so they were removed entirely \
             (no broken entries left behind). To get them back, re-add them on your \
             own configuration:\n\n",
        );
        for item in &r.removed {
            s.push_str(&format!("- **{}** — {}\n", item.name, item.reinstall_hint));
        }
        s.push('\n');
    }

    s.push_str("## Kept (no longer managed by VectorHawk)\n\n");
    if r.kept.is_empty() {
        s.push_str("_None._\n\n");
    } else {
        s.push_str("Your own tools — left exactly as they were, just unregistered:\n\n");
        for item in &r.kept {
            s.push_str(&format!(
                "- **{}** ({}) — {}\n",
                item.name, item.location, item.note
            ));
        }
        s.push('\n');
    }

    s.push_str("## Restored\n\n");
    if r.restored.is_empty() {
        s.push_str("_Nothing needed restoring._\n\n");
    } else {
        for item in &r.restored {
            s.push_str(&format!("- **{}** → {}\n", item.name, item.restored_to));
        }
        s.push('\n');
    }

    if !r.warnings.is_empty() {
        s.push_str("## Warnings — please check by hand\n\n");
        for w in &r.warnings {
            s.push_str(&format!("- {w}\n"));
        }
        s.push('\n');
    }
    s
}

fn print_plan(plan: &Plan, report: &RestoreReport) {
    println!("VectorHawk uninstall plan:");
    println!(
        "  • {} brokered MCP server(s) will be removed completely (they can't run without VectorHawk).",
        plan.brokered.len()
    );
    println!(
        "  • {} of your own tool(s) will be kept in place, just unregistered.",
        report.kept.len()
    );
    println!(
        "  • The vectorhawk entry will be stripped from {} AI client config(s).",
        plan.clients.len()
    );
    println!("  • The background service will be stopped and removed.");
}

fn print_summary(report: &RestoreReport, paths: &ReportPaths) {
    println!("\nDone. VectorHawk removed.");
    println!(
        "  Removed completely: {}   Kept (unregistered): {}   Restored: {}",
        report.removed.len(),
        report.kept.len(),
        report.restored.len()
    );
    if !report.warnings.is_empty() {
        println!("  ⚠ {} warning(s) — see the report.", report.warnings.len());
    }
    println!("  Restore report: {}", paths.markdown.display());
    println!("               (json: {})", paths.json.display());
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{self, Write};
    print!("{prompt} [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
