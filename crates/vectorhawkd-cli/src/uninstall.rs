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
//! The restore journal (`vectorhawkd_core::restore_journal`) is the source of
//! truth for what to undo: every AI-client config edit and every F2 push into
//! `~/.claude/...` is journaled at write time, so uninstall never has to guess
//! which file/key a given mutation touched. When a brokered/managed install
//! predates the journal (or its entry was lost to disk corruption), the
//! pre-journal DB/marker-file heuristics remain as a best-effort fallback.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use vectorhawkd_core::restore_journal::{JournalEntry, JournalSource, RestoreJournal};

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

    // Load the restore journal once, up front, and share it across the steps
    // below — it is the source of truth for both what to restore and exactly
    // which client-config key a brokered install touched. A corrupt or
    // missing journal degrades to an empty entry list (never blocks
    // uninstall) — see `load_journal_entries`.
    let journal_entries = load_journal_entries(&mut report);

    // ── 2. Restore first, so a failure mid-way never loses user data. ──
    // Native `~/.claude` takeovers + adopted originals from the restore journal.
    restore_from_journal(&journal_entries, &mut report);

    // ── 3. Strip every client-config entry VectorHawk wrote. ──
    strip_client_configs(&plan, &journal_entries, &mut report);

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
///
/// The exact (file, JSON key) a brokered install touched is derived from the
/// restore journal (`JournalSource::Brokered` entries record `detail.
/// server_key` + `detail.mcp_key`) — this is precise, unlike guessing from
/// `McpInstallRow::mcp_server_id`, which is the backend's UUID and is *not*
/// the slug the F2 pusher actually wrote into the config (see
/// `crate::mcp_server_slug` in vectorhawkd-daemon). Rows the journal doesn't
/// cover (pre-journal installs, a lost entry) fall back to the old best-effort
/// sweep across every detected client.
fn strip_client_configs(plan: &Plan, journal_entries: &[JournalEntry], report: &mut RestoreReport) {
    use std::collections::HashSet;
    use vectorhawkd_mcp::setup::remove_mcp_entry;

    // VH's own shim entry — always stripped from every detected client.
    for client in &plan.clients {
        if let Err(e) = remove_mcp_entry(client) {
            report.warnings.push(format!(
                "{}: could not remove the vectorhawk entry ({}): {e:#}",
                client.name,
                client.config_path.display()
            ));
        }
    }

    let mut handled: HashSet<String> = HashSet::new();

    // Journal-precise removal.
    for row in &plan.brokered {
        let Some(entry) = latest_brokered_entry(journal_entries, &row.installation_id) else {
            continue;
        };
        let server_key = entry
            .detail
            .get("server_key")
            .and_then(|v| v.as_str())
            .unwrap_or(row.mcp_server_id.as_str());
        let mcp_key = entry
            .detail
            .get("mcp_key")
            .and_then(|v| v.as_str())
            .unwrap_or("mcpServers");
        let target = PathBuf::from(&entry.target_path);

        match remove_named_key_from_file(&target, mcp_key, server_key) {
            Ok(true) => {
                let client_label = plan
                    .clients
                    .iter()
                    .find(|c| c.config_path == target)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| entry.target_path.clone());
                record_removed(report, row, RemovedKind::BrokeredMcp, &client_label);
                handled.insert(row.mcp_server_id.clone());
            }
            Ok(false) => {}
            Err(e) => report.warnings.push(format!(
                "{}: could not remove brokered server '{}' via restore journal: {e:#}",
                entry.target_path, row.mcp_server_name
            )),
        }
    }

    // Best-effort DB-driven fallback for anything the journal didn't cover.
    for client in &plan.clients {
        for row in &plan.brokered {
            if handled.contains(&row.mcp_server_id) {
                continue;
            }
            match remove_named_key_from_file(
                &client.config_path,
                &client.mcp_key,
                &row.mcp_server_id,
            ) {
                Ok(true) => {
                    record_removed(report, row, RemovedKind::BrokeredMcp, &client.name);
                    handled.insert(row.mcp_server_id.clone());
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

/// Most recent journal entry recording a brokered MCP push for the install
/// whose backend row ID is `installation_id`. Matched on `detail.
/// installation_id` (set by `ManagedPathsPusher::push_mcp`) rather than the
/// journal's own `slug` field, which is the config key, not a stable ID.
fn latest_brokered_entry<'a>(
    entries: &'a [JournalEntry],
    installation_id: &str,
) -> Option<&'a JournalEntry> {
    entries.iter().rev().find(|e| {
        e.source == JournalSource::Brokered
            && e.detail.get("installation_id").and_then(|v| v.as_str()) == Some(installation_id)
    })
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

/// Remove `key` from the `map_key` map inside the JSON file at `path`.
/// Mirrors `setup::remove_mcp_entry` but for a caller-supplied file/map/key
/// triple, so brokered-slug removal isn't tied to a single `ClientConfig`.
/// Returns `Ok(false)` (a no-op) when the file, the map, or the key is
/// absent — never an error.
fn remove_named_key_from_file(path: &Path, map_key: &str, key: &str) -> Result<bool> {
    use std::fs;
    if !path.exists() {
        return Ok(false);
    }
    let text = fs::read_to_string(path)?;
    let mut obj: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&text)
        .unwrap_or(serde_json::Value::Object(Default::default()))
    {
        serde_json::Value::Object(m) => m,
        _ => return Ok(false),
    };
    let removed = if let Some(serde_json::Value::Object(map)) = obj.get_mut(map_key) {
        map.remove(key).is_some()
    } else {
        false
    };
    if removed {
        fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::Value::Object(obj))?,
        )?;
    }
    Ok(removed)
}

/// Remove managed skills/plugins VH pushed into `~/.claude`, and its slash
/// commands. Populates the REMOVED section for managed skills/plugins.
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

    let Some(home) = dirs_home() else {
        report.warnings.push(
            "could not determine home directory — skipped the managed skill/plugin sweep".into(),
        );
        return;
    };

    remove_marked_dirs(
        &home.join(".claude").join("skills"),
        RemovedKind::ManagedSkill,
        "~/.claude/skills",
        report,
    );
    remove_marked_dirs(
        &home.join(".claude").join("plugins"),
        RemovedKind::ManagedPlugin,
        "~/.claude/plugins",
        report,
    );
}

/// Remove every top-level directory under `base` that carries a
/// `.vectorhawk-managed.json` marker (F2 pushed it there). Directories
/// without the marker are the user's own and are left completely alone —
/// they're reported as KEPT via `detect_unmanaged_servers` for MCP, but for
/// skills/plugins there's no separate detector today, so they're simply
/// untouched and unlisted.
fn remove_marked_dirs(
    base: &Path,
    kind: RemovedKind,
    location: &'static str,
    report: &mut RestoreReport,
) {
    use vectorhawkd_daemon::managed_paths::marker::read_file_marker;

    let Ok(entries) = std::fs::read_dir(base) else {
        return; // directory doesn't exist — nothing to sweep.
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        match read_file_marker(&path) {
            Ok(Some(_marker)) => {
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    report.warnings.push(format!(
                        "failed to remove managed {location} entry '{name}': {e:#}"
                    ));
                    continue;
                }
                report.removed.push(RemovedItem {
                    name,
                    kind,
                    reinstall_hint:
                        "Pushed by your organization's VectorHawk policy — nothing to re-add; \
                         ask IT if you still need it."
                            .into(),
                    stripped_from: vec![location.into()],
                });
            }
            Ok(None) => {
                // Not managed — the user's own directory. Leave it alone.
            }
            Err(e) => report.warnings.push(format!(
                "could not read the VectorHawk marker for '{}': {e:#}",
                path.display()
            )),
        }
    }
}

/// Read the restore journal, tolerating any failure (missing/corrupt file,
/// unresolvable data dir) by degrading to an empty entry list. A broken
/// journal must never block the rest of uninstall — see the module docs on
/// `RestoreJournal::read_all`.
fn load_journal_entries(report: &mut RestoreReport) -> Vec<JournalEntry> {
    use vectorhawkd_core::state::AppState;

    let state = match AppState::bootstrap() {
        Ok(s) => s,
        Err(e) => {
            report.warnings.push(format!(
                "could not open VectorHawk state to read the restore journal — some \
                 items may not be restored/removed precisely: {e:#}"
            ));
            return Vec::new();
        }
    };

    let journal = RestoreJournal::for_state(&state);
    match journal.read_all() {
        Ok(entries) => entries,
        Err(e) => {
            report.warnings.push(format!(
                "could not read the restore journal ({}) — some items may not be \
                 restored/removed precisely: {e:#}",
                journal.journal_path()
            ));
            Vec::new()
        }
    }
}

/// Restore native takeovers + adopted originals from the restore journal.
///
/// Replays `entries` in reverse (most-recent mutation first) and restores
/// only entries whose `source` is `Native`/`Adopted` — `Brokered`/`Managed`
/// entries are handled by `strip_client_configs`/`remove_managed_artifacts`
/// instead, since there's nothing of the user's to put back. A single bad
/// entry (missing backup, permission error) is recorded as a warning and
/// never aborts the rest of the replay.
fn restore_from_journal(entries: &[JournalEntry], report: &mut RestoreReport) {
    use std::collections::HashSet;

    let mut handled: HashSet<&str> = HashSet::new();

    for entry in entries.iter().rev() {
        if !entry.source.should_restore() {
            continue;
        }
        // A file can be journaled repeatedly (e.g. every `mcp setup` re-run
        // re-edits the same client config); every such entry reuses the same
        // original `backup_path`, so only the most-recent one — first seen
        // walking in reverse — needs replaying.
        if !handled.insert(entry.target_path.as_str()) {
            continue;
        }

        if let Err(e) = restore_one_entry(entry) {
            report.warnings.push(format!(
                "could not restore '{}' from the restore journal: {e:#}",
                entry.target_path
            ));
            continue;
        }

        report.restored.push(RestoredItem {
            name: entry
                .slug
                .clone()
                .unwrap_or_else(|| entry.target_path.clone()),
            restored_to: entry.target_path.clone(),
            from_backup: entry
                .backup_path
                .clone()
                .unwrap_or_else(|| "(nothing — it did not exist before VectorHawk)".into()),
        });
    }
}

/// Perform the filesystem side effect for one restorable journal entry: copy
/// `backup_path` → `target_path`, or — when `backup_path` is `None`, meaning
/// `target_path` did not exist before VectorHawk touched it — delete
/// `target_path` outright rather than fabricate a restore.
fn restore_one_entry(entry: &JournalEntry) -> Result<()> {
    let target = Path::new(&entry.target_path);
    match &entry.backup_path {
        Some(bp) => {
            let backup = Path::new(bp);
            anyhow::ensure!(backup.exists(), "backup path {bp} no longer exists");
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            if backup.is_dir() {
                if target.exists() {
                    std::fs::remove_dir_all(target)
                        .with_context(|| format!("clearing {} before restore", target.display()))?;
                }
                copy_dir_recursive(backup, target)?;
            } else {
                std::fs::copy(backup, target)
                    .with_context(|| format!("restoring {}", target.display()))?;
            }
        }
        None => {
            if target.is_dir() {
                std::fs::remove_dir_all(target)
                    .with_context(|| format!("removing {}", target.display()))?;
            } else if target.exists() {
                std::fs::remove_file(target)
                    .with_context(|| format!("removing {}", target.display()))?;
            }
        }
    }
    Ok(())
}

/// Recursively copy `src` into `dest`, creating `dest` (and parents) as needed.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry.context("reading restore-journal backup dir entry")?;
        let dest_path = dest.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", entry.path().display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)
                .with_context(|| format!("copying to {}", dest_path.display()))?;
        }
    }
    Ok(())
}

/// Delete the VectorHawk data directory + logs + lock file + keychain token
/// (`--purge`). Best-effort beyond the data directory itself: a missing log
/// dir, an absent lock file, or an unavailable keychain must not fail the
/// purge — `auth logout` (step 6 of `run`) already released the token row
/// for `opts.registry_url`; this is defense-in-depth for the raw secret.
fn purge_data_dir() -> Result<()> {
    use vectorhawkd_core::state::AppState;
    let state = AppState::bootstrap()?;
    let root = state.root_dir.as_std_path().to_path_buf();
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }

    if let Some(home) = dirs_home() {
        // macOS: `vectorhawk daemon install` points the LaunchAgent's
        // StandardOut/ErrorPath at `~/Library/Logs/VectorHawk/` (see
        // `install/macos.rs::log_dir`). Linux has no equivalent file — the
        // systemd user unit's stdout/stderr go to the journal
        // (`journalctl --user -u vectorhawk-agent`), not a log directory.
        #[cfg(target_os = "macos")]
        {
            let logs_dir = home.join("Library").join("Logs").join("VectorHawk");
            if logs_dir.exists() {
                let _ = std::fs::remove_dir_all(&logs_dir);
            }
        }

        let lock_file = home.join(".claude.json.lock");
        if lock_file.exists() {
            let _ = std::fs::remove_file(&lock_file);
        }
    }

    delete_keychain_item();

    Ok(())
}

/// Best-effort deletion of the `com.vectorhawk.agent` OS keychain service.
/// `auth logout` (step 6 of `run`) already deletes the entry for the
/// configured registry via `keyring`, keyed by account `registry::<url>`; a
/// user who previously pointed at a different registry could still have a
/// stale entry under this service, so purge makes a final best-effort sweep
/// via the platform's own CLI rather than pulling the `keyring` crate into
/// this binary. Never fails the purge — a missing/absent item is not an error.
#[cfg(target_os = "macos")]
fn delete_keychain_item() {
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", "com.vectorhawk.agent"])
        .output();
}

#[cfg(not(target_os = "macos"))]
fn delete_keychain_item() {
    // Linux never links the `keyring` crate (see vectorhawkd-core/src/auth.rs)
    // — tokens live only in SQLite there, already removed with the data dir.
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

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Every test that touches the filesystem via `$HOME`-resolving code
// (`AppState::bootstrap`, `detect_ai_clients`, `uninstall_claude_skills`, ...)
// redirects `HOME` to a throwaway temp dir first, following the same
// `with_fake_home` + mutex convention as `vectorhawkd-mcp/src/setup.rs` and
// `vectorhawkd-daemon/src/managed_paths/pusher_tests.rs`. This keeps the
// developer's real `$HOME` and `~/Library/Application Support/VectorHawk`
// completely untouched — no test here ever mutates them.
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use vectorhawkd_core::restore_journal::{JournalOp, JournalSource, RestoreJournal};
    use vectorhawkd_core::state::{AppState, McpInstallRow};
    use vectorhawkd_mcp::setup::ClientConfig;

    /// Serializes every test in this module that mutates the process-global
    /// `HOME` env var.
    static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vh-uninstall-test-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Run `f` with `HOME` pointed at `fake_home`, restoring the original
    /// value afterward even if `f` panics.
    fn with_fake_home<T>(fake_home: &Path, f: impl FnOnce() -> T) -> T {
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

    fn brokered_row(mcp_server_id: &str, installation_id: &str, name: &str) -> McpInstallRow {
        McpInstallRow {
            mcp_server_id: mcp_server_id.to_string(),
            installation_id: installation_id.to_string(),
            mcp_server_name: name.to_string(),
            package_source: "npm:@acme/slack-mcp".to_string(),
            version_pin: None,
            server_config: None,
            auth_type: "oauth".to_string(),
            gateway_server_id: None,
            gateway_url: Some("https://gateway.vectorhawk.ai/mcp/slack".to_string()),
        }
    }

    // ── strip_client_configs: journal-precise brokered removal ───────────────

    #[test]
    fn strip_client_configs_removes_brokered_entry_but_keeps_user_owned_entry() {
        let dir = temp_dir("strip");
        let config_path = dir.join("claude.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]},
                    "slack-mcp": {"command": "vectorhawk", "args": ["mcp", "serve", "--server", "slack-mcp"]},
                    "my-own-server": {"command": "my-tool", "args": []}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let client = ClientConfig {
            name: "Claude Code".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: true,
        };
        let plan = Plan {
            brokered: vec![brokered_row("uuid-slack", "inst-slack-1", "Slack")],
            clients: vec![client],
        };

        let journal_entries = vec![JournalEntry::new(
            JournalOp::ArtifactPush,
            JournalSource::Brokered,
            config_path.to_string_lossy().to_string(),
        )
        .with_slug("slack-mcp")
        .with_client("Claude Code")
        .with_detail(serde_json::json!({
            "server_key": "slack-mcp",
            "mcp_key": "mcpServers",
            "installation_id": "inst-slack-1",
        }))];

        let mut report = RestoreReport::default();
        strip_client_configs(&plan, &journal_entries, &mut report);

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let servers = after["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("vectorhawk"),
            "vectorhawk shim entry must be gone"
        );
        assert!(
            !servers.contains_key("slack-mcp"),
            "brokered entry must be gone completely, not left dead"
        );
        assert_eq!(
            servers["my-own-server"],
            serde_json::json!({"command": "my-tool", "args": []}),
            "the user's own server in the SAME file must survive untouched"
        );

        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].name, "Slack");
        assert!(matches!(report.removed[0].kind, RemovedKind::BrokeredMcp));
        assert_eq!(
            report.removed[0].stripped_from,
            vec!["Claude Code".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── restore_from_journal ───────────────────────────────────────────────

    #[test]
    fn restore_from_journal_restores_adopted_entry_byte_identical() {
        let dir = temp_dir("restore-adopted");
        let backup_path = dir.join("backup").join("SKILL.md");
        fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
        fs::write(
            &backup_path,
            b"---\nname: original\n---\noriginal adopted content\n",
        )
        .unwrap();

        // Target does not exist yet — restore must create it and its parents.
        let target_path = dir.join("live").join("SKILL.md");

        let entry = JournalEntry::new(
            JournalOp::FileReplace,
            JournalSource::Adopted,
            target_path.to_string_lossy().to_string(),
        )
        .with_backup_path(backup_path.to_string_lossy().to_string());

        let mut report = RestoreReport::default();
        restore_from_journal(&[entry], &mut report);

        assert_eq!(
            fs::read(&target_path).unwrap(),
            fs::read(&backup_path).unwrap(),
            "restored file must be byte-identical to the backup"
        );
        assert_eq!(report.restored.len(), 1);
        assert!(report.warnings.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_from_journal_with_no_backup_deletes_target_instead_of_restoring() {
        let dir = temp_dir("restore-delete");
        let target_path = dir.join("vectorhawk-created.txt");
        fs::write(&target_path, b"VectorHawk put this here").unwrap();

        // Native source, no backup_path -> target did not exist pre-VectorHawk.
        let entry = JournalEntry::new(
            JournalOp::FileDelete,
            JournalSource::Native,
            target_path.to_string_lossy().to_string(),
        );

        let mut report = RestoreReport::default();
        restore_from_journal(&[entry], &mut report);

        assert!(
            !target_path.exists(),
            "target must be deleted outright, not restored from a nonexistent backup"
        );
        assert!(report.warnings.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_from_journal_ignores_brokered_and_managed_entries() {
        let dir = temp_dir("restore-skip-managed");
        let target_path = dir.join("managed-skill");
        fs::create_dir_all(&target_path).unwrap();
        fs::write(target_path.join("SKILL.md"), b"managed content").unwrap();

        let entry = JournalEntry::new(
            JournalOp::ArtifactPush,
            JournalSource::Managed,
            target_path.to_string_lossy().to_string(),
        )
        .with_slug("some-skill");

        let mut report = RestoreReport::default();
        restore_from_journal(&[entry], &mut report);

        assert!(
            target_path.exists(),
            "managed entries are removed by remove_managed_artifacts, not restore_from_journal"
        );
        assert!(report.restored.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_from_journal_one_bad_entry_does_not_block_the_rest() {
        // A journal entry whose backup no longer exists on disk must be
        // recorded as a warning, never panic, and never prevent a *different*
        // good entry from being restored.
        let dir = temp_dir("restore-partial-failure");
        let backup_path = dir.join("backup.txt");
        fs::write(&backup_path, b"pre-vectorhawk content").unwrap();
        let target_path = dir.join("target.txt");

        let good_entry = JournalEntry::new(
            JournalOp::ConfigEdit,
            JournalSource::Native,
            target_path.to_string_lossy().to_string(),
        )
        .with_backup_path(backup_path.to_string_lossy().to_string());

        let bad_entry = JournalEntry::new(
            JournalOp::ConfigEdit,
            JournalSource::Adopted,
            dir.join("other-target.txt").to_string_lossy().to_string(),
        )
        .with_backup_path(dir.join("missing-backup.txt").to_string_lossy().to_string());

        let mut report = RestoreReport::default();
        restore_from_journal(&[bad_entry, good_entry], &mut report);

        assert_eq!(
            fs::read(&target_path).unwrap(),
            b"pre-vectorhawk content",
            "the good entry must still be restored despite the bad one"
        );
        assert_eq!(
            report.warnings.len(),
            1,
            "the missing backup must produce exactly one warning, not a panic"
        );
        assert_eq!(report.restored.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_journal_entries_survives_a_corrupt_journal_file_on_disk() {
        let fake_home = temp_dir("load-journal-corrupt");
        with_fake_home(&fake_home, || {
            let state = AppState::bootstrap().expect("bootstrap under fake HOME");
            let journal = RestoreJournal::for_state(&state);
            let journal_path = journal.journal_path();
            fs::write(journal_path.as_std_path(), b"\x00not json at all###").unwrap();

            let mut report = RestoreReport::default();
            let entries = load_journal_entries(&mut report);
            assert!(
                entries.is_empty(),
                "total garbage salvages to nothing, not an error"
            );
            assert!(
                report.warnings.is_empty(),
                "a corrupt journal is tolerated silently by RestoreJournal::read_all"
            );

            // The real assertion: this must return normally rather than panic,
            // proving the rest of uninstall keeps going on a corrupt journal.
            restore_from_journal(&entries, &mut report);
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    // ── remove_managed_artifacts / remove_marked_dirs ─────────────────────────

    #[test]
    fn remove_managed_artifacts_removes_marked_skill_but_keeps_unmarked_sibling() {
        let fake_home = temp_dir("managed-sweep");
        with_fake_home(&fake_home, || {
            let skills_dir = fake_home.join(".claude").join("skills");
            fs::create_dir_all(&skills_dir).unwrap();

            let marked = skills_dir.join("marked-skill");
            fs::create_dir_all(&marked).unwrap();
            fs::write(marked.join("SKILL.md"), b"managed body").unwrap();
            vectorhawkd_daemon::managed_paths::marker::write_file_marker(
                &marked,
                Some("inst-1"),
                "deadbeef",
                "2026-07-19T00:00:00Z",
            )
            .unwrap();

            let unmarked = skills_dir.join("user-skill");
            fs::create_dir_all(&unmarked).unwrap();
            fs::write(unmarked.join("SKILL.md"), b"the user's own skill").unwrap();

            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            let mut report = RestoreReport::default();
            remove_managed_artifacts(&plan, &mut report);

            assert!(!marked.exists(), "marked skill dir must be removed");
            assert!(unmarked.exists(), "unmarked sibling must survive untouched");
            assert_eq!(
                fs::read(unmarked.join("SKILL.md")).unwrap(),
                b"the user's own skill"
            );

            assert!(report
                .removed
                .iter()
                .any(|r| r.name == "marked-skill" && matches!(r.kind, RemovedKind::ManagedSkill)));
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    // ── --dry-run mutates nothing ─────────────────────────────────────────────

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn dry_run_mutates_nothing() {
        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let fake_home = temp_dir("dry-run");
        let report_dir = temp_dir("dry-run-report");
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &fake_home);

        let config_path = fake_home.join(".claude.json");
        let original_content = serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "vectorhawk": {"command": "vectorhawk", "args": ["mcp", "serve"]},
                "my-own-server": {"command": "my-tool", "args": []}
            }
        }))
        .unwrap();
        fs::write(&config_path, &original_content).unwrap();

        let skills_dir = fake_home.join(".claude").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let marked = skills_dir.join("marked-skill");
        fs::create_dir_all(&marked).unwrap();
        fs::write(marked.join("SKILL.md"), b"managed body").unwrap();
        vectorhawkd_daemon::managed_paths::marker::write_file_marker(
            &marked,
            Some("inst-1"),
            "deadbeef",
            "2026-07-19T00:00:00Z",
        )
        .unwrap();

        let result = run(UninstallOpts {
            yes: true,
            purge: false,
            dry_run: true,
            report_dir: Some(report_dir.clone()),
            registry_url: "https://app.vectorhawk.invalid".to_string(),
        })
        .await;

        if let Some(v) = original_home {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }

        assert!(result.is_ok(), "dry-run must succeed: {result:?}");
        assert_eq!(
            fs::read_to_string(&config_path).unwrap(),
            original_content,
            "dry-run must not touch the client config"
        );
        assert!(marked.exists(), "dry-run must not remove managed skills");
        assert_eq!(
            fs::read(marked.join("SKILL.md")).unwrap(),
            b"managed body",
            "dry-run must not touch managed skill content"
        );

        let stamp = std::env::var("VECTORHAWK_UNINSTALL_STAMP").unwrap_or_else(|_| "latest".into());
        assert!(report_dir
            .join(format!("VectorHawk-uninstall-{stamp}.md"))
            .exists());

        let _ = fs::remove_dir_all(&fake_home);
        let _ = fs::remove_dir_all(&report_dir);
    }
}
