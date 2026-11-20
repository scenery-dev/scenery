//! `scenery uninstall` — remove Scenery and restore the host to as close
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
//! * **REMOVED (completely)** — tools that only worked *via* Scenery (gateway-
//!   brokered MCP servers, IT-curated managed skills/plugins). Their client-config
//!   entries are deleted entirely so no dead entry is left behind, and each is
//!   listed with enough detail (name, transport, upstream) for the user to re-add
//!   it against their own configuration.
//! * **KEPT (unregistered)** — the user's *own* tools VH only audited. Left in
//!   place, untouched, simply no longer governed by Scenery.
//! * **RESTORED** — items VH took over that pre-existed on the machine (native
//!   `~/.claude` takeovers, adopted originals), put back where they were.
//!
//! The restore journal (`scenery_core::restore_journal`) is the source of
//! truth for what to undo: every AI-client config edit and every F2 push into
//! `~/.claude/...` is journaled at write time, so uninstall never has to guess
//! which file/key a given mutation touched. When a brokered/managed install
//! predates the journal (or its entry was lost to disk corruption), the
//! pre-journal DB/marker-file heuristics remain as a best-effort fallback.

use anyhow::{Context, Result};
use scenery_core::restore_journal::{JournalEntry, JournalOp, RestoreJournal};
use serde::Serialize;

#[path = "uninstall_clients.rs"]
mod clients;
use clients::strip_client_configs;
use std::path::{Path, PathBuf};

/// Flags for `scenery uninstall`.
#[derive(Debug, Default)]
pub struct UninstallOpts {
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Also delete the Scenery data directory (state.db, installed skills,
    /// cache) and the OS-keychain auth token. Off by default so a re-install can
    /// pick up where it left off.
    pub purge: bool,
    /// Print the plan and report but make no changes.
    pub dry_run: bool,
    /// Where to write the report file. Defaults to `~/Scenery-uninstall-<ts>`.
    pub report_dir: Option<PathBuf>,
    /// Registry base URL, for the backend device-deprovision call.
    pub registry_url: String,
    /// Restore client configs and remove Scenery's skills from clients on this
    /// machine only. No server calls: the device stays paired and nothing is
    /// archived. Used by the macOS pkg uninstaller.
    pub local_only: bool,
}

/// One tool that only worked via Scenery and was removed outright.
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
    /// AI client (or "Scenery" for non-client items) this belongs to.
    pub client: String,
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
    /// Restores that did not happen. Any entry makes the command exit non-zero.
    pub failed: Vec<String>,
    pub purged_data_dir: bool,
    /// Copies of the user's work that were set aside rather than overwritten
    /// or deleted: the user edited a converted directory after Scenery took
    /// it over, so both the restored original and the edited version exist.
    pub preserved: Vec<PreservedItem>,
    /// Set when `--purge` kept `restore-backups/` (and the journal) because a
    /// restore failed: where those backups live.
    pub backups_kept_at: Option<String>,
    /// The user's OWN skills (Scenery converted them from their own copy) that
    /// they kept editing: their current version stays in place, unmanaged, and
    /// the original is kept in the uninstall report folder. Restoring the old
    /// original over newer work would be data loss.
    pub kept_own: Vec<KeptOwnItem>,
}

/// A skill of the user's own whose later edits were kept (plan 4.5.5).
#[derive(Debug, Serialize)]
pub struct KeptOwnItem {
    pub name: String,
    /// Where the user's current (edited) version stays, now unmanaged.
    pub current_version_at: String,
    /// The original, as it was before Scenery converted it.
    pub original_copy_at: String,
}

/// Both versions of something the user changed after Scenery converted it.
#[derive(Debug, Serialize)]
pub struct PreservedItem {
    pub name: String,
    /// The original, restored at its original path.
    pub restored_original_at: String,
    /// What was at that path before the restore (the converted/edited tree),
    /// moved here untouched.
    pub current_copy_at: String,
    pub reason: String,
}

/// Printed when a user runs `scenery uninstall` on a Mac where the pkg manages
/// the daemon. The system agent keeps running, so a user-level uninstall would
/// half-remove Scenery; IT removes it with the pkg uninstaller instead.
pub const PKG_MANAGED_UNINSTALL_MSG: &str =
    "Scenery is managed by your IT department on this Mac. Ask IT to remove it.";

/// Pure decision: refuse a user-run uninstall when the pkg manages this Mac,
/// unless it is the pkg uninstaller's own `--local-only` call.
pub(crate) fn refuse_uninstall(pkg_managed: bool, local_only: bool) -> bool {
    pkg_managed && !local_only
}

/// Entry point wired from `main.rs`.
pub async fn run(opts: UninstallOpts) -> Result<()> {
    if refuse_uninstall(scenery_core::pkg_mode::is_pkg_managed(), opts.local_only) {
        anyhow::bail!(PKG_MANAGED_UNINSTALL_MSG);
    }
    let mut report = RestoreReport::default();

    // The restore journal is the source of truth for what to restore and for
    // which client-config key a brokered install touched. A corrupt or missing
    // one degrades to an empty list (never blocks uninstall).
    let journal_entries = load_journal_entries(&mut report);

    // ── 1. Plan (read-only): classify everything before mutating anything. ──
    let plan =
        build_plan(&mut report, &journal_entries).context("failed to inspect current install")?;

    // Show the plan and, unless --yes, confirm. A user demoing Scenery needs
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

    // ── 2. Stop every daemon first. ──
    // A running daemon re-pushes client configs and re-saves refreshed
    // tokens, so nothing below may run while one is up — including the
    // `brew services` one, which `install::uninstall` does not own.
    stop_daemons(&mut report).await?;

    // ── 3. Restore first, so a failure mid-way never loses user data. ──
    // Native `~/.claude` takeovers + adopted originals from the restore journal.
    let originals_dir = report_folder(&opts);
    keep_unsynced_branch_edits(&originals_dir, &mut report);
    restore_from_journal_in(&journal_entries, &mut report, &originals_dir);
    forget_kept_own_markers(&report);
    // Skills tracked in place were never changed; undo only what the kill
    // switch did to them (links, SKILL.md renames).
    restore_tracked_skills(&mut report);

    // ── 4. Clean every client config: restore, or strip only Scenery's keys. ──
    strip_client_configs(&plan, &journal_entries, &mut report);

    // ── 5. Remove managed skills/plugins VH pushed into ~/.claude. ──
    remove_managed_artifacts(&plan, &mut report);

    remove_lock_files(&mut report);

    // ── 6. Deprovision this device, revoke every session server side, and
    //       drop the local secrets. ──
    if opts.local_only {
        report.kept.push(KeptItem {
            name: "sign-out".into(),
            location: opts.registry_url.clone(),
            note: "local-only: skipped sign-out and device deprovision".into(),
        });
    } else {
        sign_out_everywhere(&opts.registry_url, &mut report).await;
    }

    // A fully successful uninstall starts the next install from a clean
    // ledger; otherwise keep it so a re-run can finish the job.
    if report.failed.is_empty() {
        clear_journal(&mut report);
    }

    // ── 7. Optional hard purge of local state. ──
    if opts.purge {
        run_purge_step(&mut report);
    }

    // ── 8. Write + print the restore report. ──
    let paths = write_report(&report, &opts)?;
    print_summary(&report, &paths);
    println!("\nRestart your AI client(s) to apply the change.");
    failure_result(&report)
}

/// Catalog skills the user edited (a branch) and never uploaded: copy the edits
/// to `<report folder>/branches/<slug>/` before the sweep removes the skill. A
/// synced branch needs no copy. A failed copy fails the item loudly.
fn keep_unsynced_branch_edits(folder: &Path, report: &mut RestoreReport) {
    let Ok(state) = scenery_core::state::AppState::bootstrap() else {
        return;
    };
    for saved in
        scenery_daemon::managed_paths::skill_merge::save_unsynced_branch_edits(&state, folder, None)
    {
        match (saved.copy_at, saved.error) {
            (Some(at), _) => report.kept.push(KeptItem {
                name: saved.slug,
                location: at.display().to_string(),
                note: "your edits of this catalog skill were never uploaded; a copy is kept here"
                    .into(),
            }),
            (None, err) => {
                let msg = format!(
                    "could not copy your unsynced edits of '{}' before removal: {}",
                    saved.slug,
                    err.unwrap_or_default()
                );
                report.warnings.push(msg.clone());
                report.failed.push(msg);
            }
        }
    }
}

/// Non-zero exit when anything could not be restored.
fn failure_result(report: &RestoreReport) -> Result<()> {
    if report.failed.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{} item(s) could not be restored — see the report and warnings above",
        report.failed.len()
    )
}

/// Empty the restore journal (best effort, reported as a warning).
fn clear_journal(report: &mut RestoreReport) {
    use scenery_core::state::AppState;
    let result = AppState::bootstrap().and_then(|state| RestoreJournal::for_state(&state).clear());
    if let Err(e) = result {
        report
            .warnings
            .push(format!("could not clear the restore journal: {e:#}"));
    }
}

/// Delete lock files Scenery created beside Claude's files. The daemons are
/// stopped by now, so nothing holds them.
fn remove_lock_files(report: &mut RestoreReport) {
    let Some(home) = dirs_home() else {
        return;
    };
    let claude = home.join(".claude");
    let plugins = claude.join("plugins");
    let locks = [
        home.join(".claude.json.lock"),
        claude.join("settings.lock"),
        plugins.join("installed_plugins.lock"),
        plugins.join("known_marketplaces.lock"),
    ];
    for lock in locks {
        match std::fs::remove_file(&lock) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report
                .warnings
                .push(format!("could not remove {}: {e}", lock.display())),
        }
    }
}

/// Stop and deregister our own service unit and the `brew services` one.
async fn stop_daemons(report: &mut RestoreReport) -> Result<()> {
    if let Err(e) = tokio::task::spawn_blocking(crate::install::uninstall)
        .await
        .context("uninstall task panicked")?
    {
        report
            .warnings
            .push(format!("failed to remove the background service: {e:#}"));
    }
    if let Err(e) = tokio::task::spawn_blocking(crate::install::stop_brew_service)
        .await
        .context("uninstall task panicked")?
    {
        report
            .warnings
            .push(format!("failed to stop the brew services daemon: {e:#}"));
    }
    Ok(())
}

/// Clear the tokens for `registry_url` and every other registry this machine
/// signed in to, the device session, and the H3 local key. Uninstall is not
/// `auth logout`: no Scenery secret may stay behind on the machine.
async fn sign_out_everywhere(registry_url: &str, report: &mut RestoreReport) {
    use scenery_core::auth::{clear_tokens, load_all_tokens};
    use scenery_core::{secret_file, state::AppState};

    deprovision_this_device(registry_url, report).await;
    if let Err(e) = crate::cmd_auth_logout(registry_url).await {
        report
            .warnings
            .push(format!("could not log out cleanly: {e:#}"));
    }
    // Every other registry this machine signed in to: revoke that session
    // server side too before its tokens are dropped below.
    if let Ok(state) = AppState::bootstrap() {
        for tokens in load_all_tokens(&state).unwrap_or_default() {
            crate::revoke_session(&state, &tokens.registry_url, "family").await;
        }
    }
    let result = AppState::bootstrap().and_then(|state| {
        for tokens in load_all_tokens(&state)? {
            clear_tokens(&state, &tokens.registry_url)?;
        }
        state.clear_local_mcp_key()?;
        // Catches token files whose auth_tokens row was already gone.
        match std::fs::remove_dir_all(secret_file::secrets_dir(state.root_dir.as_std_path())) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    });
    if let Err(e) = result {
        report
            .warnings
            .push(format!("could not clear stored credentials: {e:#}"));
    }
    delete_keychain_items();
}

/// Offboard this device on `registry_url`: `POST /api/devices/{id}/deprovision`,
/// which revokes its sessions and brokered gateway grants and records the
/// offboarding in IT's audit log. Refreshes an expired access token first.
/// Skipped when the machine never paired or holds no token. A failure is a
/// report warning: uninstall still clears everything locally.
async fn deprovision_this_device(registry_url: &str, report: &mut RestoreReport) {
    use scenery_core::auth::{load_tokens, save_tokens, AuthClient};
    use scenery_core::state::AppState;

    let Ok(state) = AppState::bootstrap() else {
        return;
    };
    let Some(device_id) = state
        .get_sync_state("device_id")
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(tokens) = load_tokens(&state, registry_url).ok().flatten() else {
        return;
    };
    let url = registry_url.to_string();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let client = AuthClient::new(url.as_str());
        let mut access_token = tokens.access_token;
        if AuthClient::needs_refresh(&access_token) && !tokens.refresh_token.is_empty() {
            let fresh = client.refresh(&tokens.refresh_token)?;
            save_tokens(&state, &url, &fresh.access_token, &fresh.refresh_token)?;
            access_token = fresh.access_token;
        }
        client.deprovision_device(&access_token, &device_id)
    })
    .await;
    let failure = match result {
        Ok(Ok(())) => return,
        Ok(Err(e)) => format!("{e:#}"),
        Err(e) => e.to_string(),
    };
    report.warnings.push(format!(
        "could not deprovision this device on {registry_url}: {failure}"
    ));
}

/// What we intend to do, computed read-only up front.
struct Plan {
    /// Gateway-brokered MCP installs (gateway_url present) → remove completely.
    brokered: Vec<scenery_core::state::McpInstallRow>,
    /// Detected AI clients whose config holds VH entries.
    clients: Vec<scenery_mcp::setup::ClientConfig>,
}

/// Read current state and populate the KEPT section (nothing mutated here).
fn build_plan(report: &mut RestoreReport, journal_entries: &[JournalEntry]) -> Result<Plan> {
    use scenery_core::state::AppState;
    use scenery_mcp::setup::{detect_ai_clients, unmanaged_servers_in};

    let state = AppState::bootstrap().context("failed to open Scenery state")?;
    let installs = state.list_mcp_installs().unwrap_or_default();
    let brokered: Vec<_> = installs
        .into_iter()
        .filter(|r| r.gateway_url.is_some())
        .collect();

    let clients = detect_ai_clients();
    let plan = Plan { brokered, clients };

    // KEPT (preview for the plan and --dry-run): the user's own servers,
    // minus brokered ones, which are removed. The real list is rebuilt from
    // the files after the edit.
    let brokered_keys = clients::brokered_keys(&plan, journal_entries);
    for u in unmanaged_servers_in(&plan.clients, &brokered_keys) {
        report.kept.push(KeptItem {
            name: u.server_name,
            location: u.client_name,
            note: "your own server — left in place, no longer audited by Scenery".into(),
        });
    }

    Ok(plan)
}

fn record_removed(
    report: &mut RestoreReport,
    row: &scenery_core::state::McpInstallRow,
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
fn reinstall_hint(row: &scenery_core::state::McpInstallRow) -> String {
    let src = if row.package_source.is_empty() {
        "unknown source".to_string()
    } else {
        row.package_source.clone()
    };
    format!(
        "Re-add '{}' from {} (auth: {}). Credentials were brokered by Scenery \
         and were never stored on this machine — supply your own.",
        row.mcp_server_name, src, row.auth_type
    )
}

/// Remove managed skills/plugins VH pushed into `~/.claude`, and its slash
/// commands. Populates the REMOVED section for managed skills/plugins.
fn remove_managed_artifacts(_plan: &Plan, report: &mut RestoreReport) {
    use scenery_mcp::setup::uninstall_claude_skills;
    match uninstall_claude_skills() {
        Ok(removed) => {
            for name in removed {
                report.removed.push(RemovedItem {
                    name,
                    kind: RemovedKind::ManagedSkill,
                    reinstall_hint:
                        "Scenery slash command — nothing to re-add; it only existed for Scenery."
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

    // Order matters. `~/.claude/skills/<slug>` is now a *symlink* into the
    // canonical `~/.agents/skills/<slug>`, and the marker is only readable
    // through that link while its target still exists. Sweeping the link root
    // first drops the aliases; sweeping the canonical root second removes the
    // real content and produces the report entry. Reversed, every link would
    // dangle, read as unmarked, and be left behind.
    remove_marked_dirs(
        &home.join(".claude").join("skills"),
        RemovedKind::ManagedSkill,
        "~/.claude/skills",
        &["~/.claude/skills"],
        report,
    );
    // Antigravity alias root (`~/.gemini/config/skills`); same rule as above.
    remove_marked_dirs(
        &home.join(".gemini").join("config").join("skills"),
        RemovedKind::ManagedSkill,
        "~/.gemini/config/skills",
        &["~/.gemini/config/skills"],
        report,
    );
    remove_marked_dirs(
        &home.join(".agents").join("skills"),
        RemovedKind::ManagedSkill,
        "~/.agents/skills",
        // The alias at the Claude path was removed by the sweep above, so a
        // skill that lived at both is reported once, naming both locations.
        &[
            "~/.agents/skills",
            "~/.claude/skills",
            "~/.gemini/config/skills",
        ],
        report,
    );
    remove_marked_dirs(
        &home.join(".claude").join("plugins"),
        RemovedKind::ManagedPlugin,
        "~/.claude/plugins",
        &["~/.claude/plugins"],
        report,
    );

    // Our `scenery` plugin marketplace (dir, cache, known_marketplaces.json,
    // settings.json, `<slug>@scenery` registry entries). Other marketplaces
    // are never touched.
    match scenery_daemon::managed_paths::uninstall_marketplace() {
        Ok(slugs) => {
            for name in slugs {
                // A plugin already reported by the marked-dir sweep above is
                // not listed twice.
                if report.removed.iter().any(|r| r.name == name) {
                    continue;
                }
                report.removed.push(RemovedItem {
                    name,
                    kind: RemovedKind::ManagedPlugin,
                    reinstall_hint:
                        "Pushed by your organization's Scenery policy — nothing to re-add; \
                         ask IT if you still need it."
                            .into(),
                    stripped_from: vec!["~/.claude/plugins".into()],
                });
            }
        }
        Err(e) => report.warnings.push(format!(
            "failed to remove the Scenery plugin marketplace: {e:#}"
        )),
    }
}

/// Remove every top-level entry under `base` that carries a
/// `.scenery-managed.json` marker (F2 pushed it there). Directories
/// without the marker are the user's own and are left completely alone —
/// they're reported as KEPT via `detect_unmanaged_servers` for MCP, but for
/// skills/plugins there's no separate detector today, so they're simply
/// untouched and unlisted.
///
/// Symlinked entries are unlinked but **not** reported: since the pivot to
/// `~/.agents/skills`, a marked symlink is only ever an alias Scenery
/// created for a client that cannot read the canonical root, and the real
/// content it points at is reported by that root's own sweep. Reporting both
/// would double-count one skill; `stripped_from` on the canonical entry names
/// every location instead. Note the marker is read *through* the link, so an
/// alias is identified by the ownership of its target, never by its own path.
fn remove_marked_dirs(
    base: &Path,
    kind: RemovedKind,
    location: &'static str,
    stripped_from: &[&str],
    report: &mut RestoreReport,
) {
    use scenery_daemon::managed_paths::marker::read_file_marker;

    let Ok(entries) = std::fs::read_dir(base) else {
        return; // directory doesn't exist — nothing to sweep.
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // `is_symlink` uses `symlink_metadata`, so it is true for a link even
        // when the link target is a directory — unlike `is_dir()` below,
        // which follows. That distinction is the whole point here:
        // `remove_dir_all` on a symlink would not remove the real content.
        let is_link = path.is_symlink();
        if !is_link && !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        match read_file_marker(&path) {
            Ok(Some(_marker)) if is_link => {
                // Drop the alias only. The canonical sweep removes the content
                // and files the report entry.
                if let Err(e) = std::fs::remove_file(&path) {
                    report.warnings.push(format!(
                        "failed to remove managed {location} link '{name}': {e:#}"
                    ));
                }
            }
            Ok(Some(marker)) => {
                let builtin = marker.origin.as_deref()
                    == Some(scenery_daemon::managed_paths::marker::ORIGIN_BUILTIN);
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    report.warnings.push(format!(
                        "failed to remove managed {location} entry '{name}': {e:#}"
                    ));
                    continue;
                }
                report.removed.push(RemovedItem {
                    name,
                    kind,
                    reinstall_hint: if builtin {
                        "Built into Scenery (it only teaches your AI client to use Scenery's \
                         authoring tools) — nothing to re-add."
                            .into()
                    } else {
                        "Pushed by your organization's Scenery policy — nothing to re-add; \
                         ask IT if you still need it."
                            .into()
                    },
                    stripped_from: stripped_from.iter().map(|s| (*s).to_string()).collect(),
                });
            }
            Ok(None) => {
                // Not managed — the user's own directory. Leave it alone.
            }
            Err(e) => report.warnings.push(format!(
                "could not read the Scenery marker for '{}': {e:#}",
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
    use scenery_core::state::AppState;

    let state = match AppState::bootstrap() {
        Ok(s) => s,
        Err(e) => {
            report.warnings.push(format!(
                "could not open Scenery state to read the restore journal — some \
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
/// Restores only entries whose `source` is `Native`/`Adopted` — `Brokered`/
/// `Managed` entries are handled by `strip_client_configs`/
/// `remove_managed_artifacts` instead, since there's nothing of the user's to
/// put back. A single bad entry (missing backup, permission error) is
/// recorded as a warning and never aborts the rest of the replay.
///
/// Which backup wins for a path:
/// - If the path has any `Adopted` entry with a backup, the **oldest** one that
///   still exists. The oldest is the pre-Scenery state; a later one can be a
///   backup of a half-converted tree taken by a retried conversion.
/// - Otherwise (native takeovers, config files) the most recent entry, which
///   reuses the same original backup anyway.
///
/// A directory restore never destroys what is there now: see
/// [`restore_one_entry`].
#[cfg(test)]
fn restore_from_journal(entries: &[JournalEntry], report: &mut RestoreReport) {
    restore_from_journal_in(entries, report, &report_folder(&UninstallOpts::default()));
}

/// [`restore_from_journal`] with the folder that receives the originals of
/// the user's own skills they kept editing (`originals_dir`, created only when
/// needed).
fn restore_from_journal_in(
    entries: &[JournalEntry],
    report: &mut RestoreReport,
    originals_dir: &Path,
) {
    use std::collections::HashSet;

    let mut handled: HashSet<&str> = HashSet::new();

    for newest in entries.iter().rev() {
        if !newest.source.should_restore() || newest.op == JournalOp::ConfigEdit {
            continue;
        }
        // Client-config edits are handled key by key in `strip_client_configs`:
        // copying a backup over a live config would destroy later edits.
        //
        // A file can be journaled repeatedly (e.g. every `mcp setup` re-run
        // re-edits the same client config), so each path is handled once.
        if !handled.insert(newest.target_path.as_str()) {
            continue;
        }

        let chosen = match choose_entry(entries, newest, report) {
            Ok(e) => e,
            Err(msg) => {
                report.warnings.push(msg.clone());
                report.failed.push(msg);
                continue;
            }
        };

        let pristine = installed_tree_hash(entries, &chosen.target_path);
        // The user's OWN skill (auto-managed): later edits win over the old
        // original. Decided by the origin the pusher journaled, never by a
        // marker file anyone could ship.
        let own_dest = installed_origin_is_own(entries, &chosen.target_path).then(|| {
            originals_dir
                .join("originals")
                .join(chosen.slug.clone().unwrap_or_else(|| "skill".to_string()))
        });
        let outcome = match restore_one_entry(chosen, pristine.as_deref(), own_dest.as_deref()) {
            Ok(o) => o,
            Err(e) => {
                let msg = format!(
                    "could not restore '{}' from the restore journal: {e:#}",
                    chosen.target_path
                );
                report.warnings.push(msg.clone());
                report.failed.push(msg);
                continue;
            }
        };

        let name = chosen
            .slug
            .clone()
            .unwrap_or_else(|| chosen.target_path.clone());
        // An own skill converted from ANOTHER root (`~/.pi/...` -> canonical):
        // the original goes back to its own path, but the managed copy at the
        // canonical path is swept next. If the user edited it, copy it out first.
        if let Some(kept) =
            save_edited_canonical_of_own_skill(entries, chosen, originals_dir, report)
        {
            report.preserved.push(kept);
        }
        if let Some(original_copy) = outcome.kept_current_with_original_at {
            report.kept_own.push(KeptOwnItem {
                name,
                current_version_at: chosen.target_path.clone(),
                original_copy_at: original_copy.display().to_string(),
            });
            continue;
        }
        if let Some(kept) = outcome.preserved_current_at {
            report.preserved.push(PreservedItem {
                name: name.clone(),
                restored_original_at: chosen.target_path.clone(),
                current_copy_at: kept.display().to_string(),
                reason: outcome.preserved_reason,
            });
        }
        report.restored.push(RestoredItem {
            client: chosen.client.clone().unwrap_or_else(|| "Scenery".into()),
            name,
            restored_to: chosen.target_path.clone(),
            from_backup: chosen
                .backup_path
                .clone()
                .unwrap_or_else(|| "(nothing — it did not exist before Scenery)".into()),
        });
    }
}

/// Pick the journal entry to replay for `newest`'s path (see
/// [`restore_from_journal`]). `Err` carries the message for `report.failed`.
fn choose_entry<'a>(
    entries: &'a [JournalEntry],
    newest: &'a JournalEntry,
    report: &mut RestoreReport,
) -> std::result::Result<&'a JournalEntry, String> {
    use scenery_core::restore_journal::JournalSource;

    let adopted: Vec<&JournalEntry> = entries
        .iter() // oldest first
        .filter(|e| {
            e.target_path == newest.target_path
                && e.op != JournalOp::ConfigEdit
                && e.source == JournalSource::Adopted
                && e.backup_path.is_some()
        })
        .collect();
    if adopted.is_empty() {
        return Ok(newest);
    }
    let mut missing: Vec<&str> = Vec::new();
    for e in &adopted {
        let bp = e.backup_path.as_deref().unwrap_or_default();
        if std::fs::symlink_metadata(bp).is_ok() {
            for m in missing {
                report.warnings.push(format!(
                    "backup {m} for '{}' no longer exists; restored from the next-oldest backup",
                    e.target_path
                ));
            }
            return Ok(e);
        }
        missing.push(bp);
    }
    Err(format!(
        "could not restore '{}' from the restore journal: none of its {} adopted \
         backup(s) exist on disk",
        newest.target_path,
        adopted.len()
    ))
}

/// The fingerprint of the tree Scenery last installed at `target_path`
/// (newest `managed` push entry that recorded one). `None` for installs made
/// by a runner that predates the fingerprint; those are treated as possibly
/// edited, so nothing is deleted on a guess.
fn installed_tree_hash(entries: &[JournalEntry], target_path: &str) -> Option<String> {
    use scenery_core::restore_journal::JournalSource;
    entries
        .iter()
        .rev()
        .filter(|e| {
            e.target_path == target_path
                && e.source == JournalSource::Managed
                && e.op == JournalOp::ArtifactPush
        })
        .find_map(|e| {
            e.detail
                .get("tree_sha256")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

/// For an adopted entry whose original lived at a path other than the managed
/// canonical one: if the canonical managed copy is an auto-managed skill the
/// user edited since conversion, copy it to
/// `<originals_dir>/edited/<slug>` before the managed-artifact sweep deletes it.
fn save_edited_canonical_of_own_skill(
    entries: &[JournalEntry],
    adopted: &JournalEntry,
    originals_dir: &Path,
    report: &mut RestoreReport,
) -> Option<PreservedItem> {
    use scenery_core::restore_journal::JournalSource;
    use scenery_core::safe_copy::{copy_tree, tree_fingerprint, unique_token};
    use scenery_mcp::ownership::MANAGED_MARKER_FILENAME;

    let slug = adopted.slug.as_deref()?;
    let push = entries.iter().rev().find(|e| {
        e.source == JournalSource::Managed
            && e.op == JournalOp::ArtifactPush
            && e.slug.as_deref() == Some(slug)
            && e.target_path != adopted.target_path
            && e.detail.get("origin").and_then(|v| v.as_str()) == Some("auto_managed")
    })?;
    let canonical = Path::new(&push.target_path);
    if !std::fs::symlink_metadata(canonical).ok()?.is_dir() {
        return None;
    }
    let written = push.detail.get("tree_sha256").and_then(|v| v.as_str());
    let pristine = written.is_some_and(|h| {
        tree_fingerprint(canonical, &[MANAGED_MARKER_FILENAME])
            .map(|cur| cur == h)
            .unwrap_or(false)
    });
    if pristine {
        return None;
    }
    let mut dest = originals_dir.join("edited").join(slug);
    if std::fs::symlink_metadata(&dest).is_ok() {
        dest = originals_dir
            .join("edited")
            .join(format!("{slug}-{}", unique_token()));
    }
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            report
                .warnings
                .push(format!("could not keep your edited '{slug}': {e}"));
            return None;
        }
    }
    match copy_tree(canonical, &dest) {
        Ok(_) => {
            let _ = std::fs::remove_file(dest.join(MANAGED_MARKER_FILENAME));
            Some(PreservedItem {
                name: slug.to_string(),
                restored_original_at: adopted.target_path.clone(),
                current_copy_at: dest.display().to_string(),
                reason: "you edited the managed copy after Scenery converted it".into(),
            })
        }
        Err(e) => {
            // The sweep would delete the only copy of the edits: say so loudly
            // and fail the item rather than lose them silently.
            let msg = format!(
                "could not copy your edited '{slug}' out of {} before removing it: {e:#}",
                canonical.display()
            );
            report.warnings.push(msg.clone());
            report.failed.push(msg);
            // Leave the edits where they are, unmanaged, so the sweep keeps its hands off.
            let _ = std::fs::remove_file(canonical.join(MANAGED_MARKER_FILENAME));
            None
        }
    }
}

/// Did Scenery convert this path from the user's own skill (`origin =
/// auto_managed` on the newest push entry)?
fn installed_origin_is_own(entries: &[JournalEntry], target_path: &str) -> bool {
    use scenery_core::restore_journal::JournalSource;
    entries
        .iter()
        .rev()
        .find(|e| {
            e.target_path == target_path
                && e.source == JournalSource::Managed
                && e.op == JournalOp::ArtifactPush
        })
        .and_then(|e| e.detail.get("origin").and_then(|v| v.as_str()))
        == Some("auto_managed")
}

/// What a restore did besides putting the original back.
#[derive(Debug, Default)]
struct RestoreOutcome {
    /// An OWN skill edited since conversion: its current tree was left in
    /// place (unmanaged) and the original was copied here instead.
    kept_current_with_original_at: Option<PathBuf>,
    /// Where the pre-existing tree was moved instead of being deleted.
    preserved_current_at: Option<PathBuf>,
    preserved_reason: String,
}

/// Perform the filesystem side effect for one restorable journal entry: put
/// `backup_path` back at `target_path`, or — when `backup_path` is `None`,
/// meaning `target_path` did not exist before Scenery touched it — delete
/// `target_path` outright rather than fabricate a restore.
///
/// A directory restore is staged then renamed into place, copies symlinks as
/// symlinks, and never destroys what is at `target_path` now unless that is
/// provably the tree Scenery installed (`installed_hash`, the fingerprint
/// recorded when it was pushed, matches). Anything else (the user edited it
/// after the conversion, it is a different tree, or there is no fingerprint
/// to compare against) is renamed to `<name>.scenery-edited-<token>` beside
/// the target, with its Scenery marker removed so the later managed-artifact
/// sweep does not delete it, and reported in [`RestoreOutcome`].
fn restore_one_entry(
    entry: &JournalEntry,
    installed_hash: Option<&str>,
    own_originals_dest: Option<&Path>,
) -> Result<RestoreOutcome> {
    let target = Path::new(&entry.target_path);
    let mut outcome = RestoreOutcome::default();
    match &entry.backup_path {
        Some(bp) => {
            let backup = Path::new(bp);
            let backup_meta = std::fs::symlink_metadata(backup)
                .map_err(|_| anyhow::anyhow!("backup path {bp} no longer exists"))?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            if backup_meta.is_dir() {
                restore_dir(
                    backup,
                    target,
                    installed_hash,
                    own_originals_dest,
                    &mut outcome,
                )?;
            } else {
                scenery_core::safe_copy::copy_tree(backup, target)
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
    Ok(outcome)
}

/// Directory half of [`restore_one_entry`]. Order matters: copy the backup to
/// a hidden staging sibling first (a failure here changes nothing), then move
/// what is at `target` out of the way, then rename the staging dir in.
fn restore_dir(
    backup: &Path,
    target: &Path,
    installed_hash: Option<&str>,
    own_originals_dest: Option<&Path>,
    outcome: &mut RestoreOutcome,
) -> Result<()> {
    use scenery_core::safe_copy::{copy_tree, tree_fingerprint, unique_token};
    use scenery_mcp::ownership::MANAGED_MARKER_FILENAME;

    // The user's OWN skill, edited since Scenery converted it: keep the
    // CURRENT tree in place and put the original beside the report. Putting
    // the old original over newer work would be data loss.
    if let Some(dest) = own_originals_dest {
        let pristine = std::fs::symlink_metadata(target)
            .map(|m| m.is_dir())
            .unwrap_or(false)
            && installed_hash.is_some_and(|h| {
                tree_fingerprint(target, &[MANAGED_MARKER_FILENAME])
                    .map(|cur| cur == h)
                    .unwrap_or(false)
            });
        let live = std::fs::symlink_metadata(target)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if live && !pristine {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            // Never merge into an earlier copy.
            let dest = if std::fs::symlink_metadata(dest).is_ok() {
                dest.with_file_name(format!(
                    "{}-{}",
                    dest.file_name().and_then(|n| n.to_str()).unwrap_or("skill"),
                    unique_token()
                ))
            } else {
                dest.to_path_buf()
            };
            copy_tree(backup, &dest)
                .with_context(|| format!("copying the original to {}", dest.display()))?;
            // No longer Scenery's: the managed-artifact sweep must not delete it.
            let _ = std::fs::remove_file(target.join(MANAGED_MARKER_FILENAME));
            remove_client_links_to(target);
            outcome.kept_current_with_original_at = Some(dest);
            return Ok(());
        }
    }

    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", target.display()))?;
    let token = unique_token();
    let staging = parent.join(format!(".{name}.scenery-restore-{token}"));

    if let Err(e) = copy_tree(backup, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e).with_context(|| format!("staging the restore of {}", target.display()));
    }

    let mut aside: Option<PathBuf> = None;
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e).with_context(|| format!("stat {}", target.display()));
        }
        Ok(meta) => {
            let pristine = meta.is_dir()
                && installed_hash.is_some_and(|h| {
                    tree_fingerprint(target, &[MANAGED_MARKER_FILENAME])
                        .map(|cur| cur == h)
                        .unwrap_or(false)
                });
            if pristine {
                // Exactly what Scenery installed (and published): safe to drop.
                if let Err(e) = std::fs::remove_dir_all(target) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(e).with_context(|| format!("clearing {}", target.display()));
                }
            } else {
                let dest = parent.join(format!("{name}.scenery-edited-{token}"));
                if let Err(e) = std::fs::rename(target, &dest) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(e).with_context(|| {
                        format!("moving {} aside to preserve it", target.display())
                    });
                }
                // No longer Scenery's: it must not be swept as a managed dir.
                let _ = std::fs::remove_file(dest.join(MANAGED_MARKER_FILENAME));
                outcome.preserved_reason = if installed_hash.is_some() {
                    "it changed after Scenery installed it".into()
                } else {
                    "no record of what Scenery installed there, so it was not assumed unchanged"
                        .into()
                };
                aside = Some(dest);
            }
        }
    }

    if let Err(e) = std::fs::rename(&staging, target) {
        if let Some(a) = &aside {
            let _ = std::fs::rename(a, target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e)
            .with_context(|| format!("moving the restored copy into {}", target.display()));
    }
    outcome.preserved_current_at = aside;
    Ok(())
}

/// Step 7 of `run` (`--purge`). The data dir holds `restore-backups/` and the
/// journal needed to re-run the restore. If ANY restore failed those are the
/// only copy of the user's originals, so they are never purged (everything
/// else still is) and the report says where they live.
fn run_purge_step(report: &mut RestoreReport) {
    let keep_restore_data = !report.failed.is_empty();
    match purge_data_dir(keep_restore_data) {
        Ok(root) if keep_restore_data => {
            let kept = root.join("restore-backups");
            report.warnings.push(format!(
                "--purge kept your restore backups because {} item(s) could not be \
                 restored: {} (the restore journal beside it lets `scenery uninstall` \
                 be re-run). Everything else in {} was purged.",
                report.failed.len(),
                kept.display(),
                root.display()
            ));
            report.backups_kept_at = Some(kept.display().to_string());
        }
        Ok(_) => report.purged_data_dir = true,
        Err(e) => report
            .warnings
            .push(format!("failed to purge data directory: {e:#}")),
    }
}

/// Delete the Scenery data directory + logs + lock file + keychain token
/// (`--purge`) and return its path. Best-effort beyond the data directory
/// itself: a missing log dir, an absent lock file, or an unavailable keychain
/// must not fail the purge — `auth logout` (step 6 of `run`) already released
/// the token row for `opts.registry_url`; this is defense-in-depth for the raw
/// secret.
///
/// With `keep_restore_data` the restore backups and the journal beside them
/// survive (see [`purge_data_dir_at`]).
fn purge_data_dir(keep_restore_data: bool) -> Result<PathBuf> {
    use scenery_core::state::AppState;
    let state = AppState::bootstrap()?;
    let root = state.root_dir.as_std_path().to_path_buf();
    purge_data_dir_at(&root, keep_restore_data)?;

    // macOS: `scenery daemon install` points the LaunchAgent's
    // StandardOut/ErrorPath at `~/Library/Logs/Scenery/` (see
    // `install/macos.rs::log_dir`). Linux has no equivalent file: the
    // systemd user unit's stdout/stderr go to the journal
    // (`journalctl --user -u scenery`), not a log directory.
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs_home() {
        let logs_dir = home.join("Library").join("Logs").join("Scenery");
        if logs_dir.exists() {
            let _ = std::fs::remove_dir_all(&logs_dir);
        }
    }

    Ok(root)
}

/// Names under the data dir that hold the user's originals and the ledger
/// needed to restore them again.
const RESTORE_DATA: [&str; 3] = [
    "restore-backups",
    "restore-journal.json",
    "restore-journal.json.lock",
];

/// Remove `root`; or, with `keep_restore_data`, everything in it except
/// [`RESTORE_DATA`].
fn purge_data_dir_at(root: &Path, keep_restore_data: bool) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if !keep_restore_data {
        return std::fs::remove_dir_all(root)
            .with_context(|| format!("removing {}", root.display()));
    }
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        if RESTORE_DATA.iter().any(|k| entry.file_name() == *k) {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let removed = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Best-effort deletion of every `com.scenery.agent` keychain item.
/// `clear_tokens` deletes the item for each registry `state.db` still knows
/// about; an item for a registry whose row is gone would survive that, so
/// this sweeps the service via the platform's own CLI rather than pulling
/// the `keyring` crate into this binary. `security delete-generic-password`
/// removes one matching item per call, hence the loop. Never fails, and
/// leaves the keychain alone when `SCENERY_DISABLE_KEYCHAIN` is set.
#[cfg(target_os = "macos")]
fn delete_keychain_items() {
    if scenery_core::auth::keychain_disabled() {
        return;
    }
    for _ in 0..64 {
        let deleted = std::process::Command::new("security")
            .args(["delete-generic-password", "-s", "com.scenery.agent"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !deleted {
            break;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn delete_keychain_items() {
    // Linux never links the `keyring` crate (see scenery-core/src/auth.rs)
    // — tokens live only in SQLite there, cleared by `sign_out_everywhere`.
}

// ── report rendering ────────────────────────────────────────────────────────

struct ReportPaths {
    markdown: PathBuf,
    json: PathBuf,
}

fn report_stamp() -> String {
    // Stable, sortable basename. Timestamp is supplied by the caller/env rather
    // than Date::now so the report path is deterministic in tests.
    std::env::var("SCENERY_UNINSTALL_STAMP").unwrap_or_else(|_| "latest".into())
}

/// `<report dir>/Scenery-uninstall-<stamp>/`: the folder beside the report
/// files that receives anything uninstall keeps for the user (the originals of
/// their own skills they edited). Not created until something is put in it.
fn report_folder(opts: &UninstallOpts) -> PathBuf {
    opts.report_dir
        .clone()
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("Scenery-uninstall-{}", report_stamp()))
}

/// Remove the client links (`~/.claude/skills/<name>`, the Antigravity alias)
/// that point at `dir`, never anything else: a kept own skill must not leave a
/// link that makes a client keep loading it as if Scenery still managed it.
fn remove_client_links_to(dir: &Path) {
    let Some(home) = dirs_home() else { return };
    let Some(name) = dir.file_name() else { return };
    let Ok(canon) = std::fs::canonicalize(dir) else {
        return;
    };
    for base in [
        home.join(".claude").join("skills"),
        home.join(".gemini").join("config").join("skills"),
    ] {
        let link = base.join(name);
        if link.is_symlink()
            && std::fs::canonicalize(&link)
                .map(|p| p == canon)
                .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&link);
        }
    }
}

/// Drop the `managed_path_markers` rows of own skills whose current version
/// was kept: they are unmanaged now, and a later reinstall must not mistake
/// them for managed or suspicious. Best effort.
fn forget_kept_own_markers(report: &RestoreReport) {
    if report.kept_own.is_empty() {
        return;
    }
    let Ok(state) = scenery_core::state::AppState::bootstrap() else {
        return;
    };
    let Ok(conn) = rusqlite::Connection::open(&state.db_path) else {
        return;
    };
    for k in &report.kept_own {
        let _ = conn.execute(
            "DELETE FROM managed_path_markers WHERE path = ?1",
            [&k.current_version_at],
        );
    }
}

/// Undo what the kill switch did to tracked skills (links removed,
/// `SKILL.md` renamed). A tracked skill was never changed otherwise, so after
/// this its files are exactly as found.
fn restore_tracked_skills(report: &mut RestoreReport) {
    let Ok(state) = scenery_core::state::AppState::bootstrap() else {
        return;
    };
    match scenery_daemon::managed_paths::tracked::restore_all(&state) {
        Ok(restored) => {
            for (slug, entry) in restored {
                report.restored.push(RestoredItem {
                    client: "Scenery".into(),
                    name: slug,
                    restored_to: entry.display().to_string(),
                    from_backup: "(the kill switch's change was reversed; the skill itself \
                                  was never modified)"
                        .into(),
                });
            }
        }
        Err(e) => report.warnings.push(format!(
            "could not undo the kill switch on tracked skills: {e:#}"
        )),
    }
}

fn write_report(report: &RestoreReport, opts: &UninstallOpts) -> Result<ReportPaths> {
    let dir = opts
        .report_dir
        .clone()
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("."));
    let stamp = report_stamp();
    let md = dir.join(format!("Scenery-uninstall-{stamp}.md"));
    let json = dir.join(format!("Scenery-uninstall-{stamp}.json"));
    std::fs::write(&md, render_markdown(report)).with_context(|| format!("writing {md:?}"))?;
    std::fs::write(&json, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {json:?}"))?;
    Ok(ReportPaths { markdown: md, json })
}

fn render_markdown(r: &RestoreReport) -> String {
    let mut s = String::from("# Scenery uninstall — restore report\n\n");
    s.push_str(
        "Scenery has been removed. This report records exactly what changed so \
         your system is left in as close to its original state as possible.\n\n",
    );

    s.push_str("## Removed completely\n\n");
    if r.removed.is_empty() {
        s.push_str("_Nothing — no Scenery-managed tools were installed._\n\n");
    } else {
        s.push_str(
            "These only worked *through* Scenery, so they were removed entirely \
             (no broken entries left behind). To get them back, re-add them on your \
             own configuration:\n\n",
        );
        for item in &r.removed {
            s.push_str(&format!("- **{}** — {}\n", item.name, item.reinstall_hint));
        }
        s.push('\n');
    }

    s.push_str("## Kept (no longer managed by Scenery)\n\n");
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
            s.push_str(&format!(
                "- **{}** — {} → {} ({})\n",
                item.client, item.name, item.restored_to, item.from_backup
            ));
        }
        s.push('\n');
    }

    if !r.preserved.is_empty() {
        s.push_str("## Preserved — both versions kept\n\n");
        s.push_str(
            "You changed these after Scenery took them over, so nothing was overwritten \
             or deleted. The original is back at its path and your current version was \
             moved beside it:\n\n",
        );
        for p in &r.preserved {
            s.push_str(&format!(
                "- **{}** — original restored at `{}`; the other version is at `{}` ({})\n",
                p.name, p.restored_original_at, p.current_copy_at, p.reason
            ));
        }
        s.push('\n');
    }

    if !r.kept_own.is_empty() {
        s.push_str("## Your own skills — your current version was kept\n\n");
        s.push_str(
            "You kept editing these after Scenery took them over, so your CURRENT version \
             stays where it is, no longer managed by Scenery. The original (before Scenery) \
             was copied next to this report:\n\n",
        );
        for k in &r.kept_own {
            s.push_str(&format!(
                "- **{}** — your version stays at `{}`; the original is at `{}`\n",
                k.name, k.current_version_at, k.original_copy_at
            ));
        }
        s.push('\n');
    }

    if let Some(kept) = &r.backups_kept_at {
        s.push_str("## Restore backups kept\n\n");
        s.push_str(&format!(
            "`--purge` did not delete your restore backups because some items could not \
             be restored. They are at `{kept}`.\n\n"
        ));
    }

    if !r.failed.is_empty() {
        s.push_str("## Not restored\n\n");
        for f in &r.failed {
            s.push_str(&format!("- {f}\n"));
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
    println!("Scenery uninstall plan:");
    println!(
        "  • {} brokered MCP server(s) will be removed completely (they can't run without Scenery).",
        plan.brokered.len()
    );
    println!(
        "  • {} of your own tool(s) will be kept in place, just unregistered.",
        report.kept.len()
    );
    println!(
        "  • The scenery entry will be stripped from {} AI client config(s).",
        plan.clients.len()
    );
    println!("  • The background service will be stopped and removed.");
}

fn print_summary(report: &RestoreReport, paths: &ReportPaths) {
    println!("\nDone. Scenery removed.");
    println!(
        "  Removed completely: {}   Kept (unregistered): {}   Restored: {}",
        report.removed.len(),
        report.kept.len(),
        report.restored.len()
    );
    for p in &report.preserved {
        println!(
            "  Kept both versions of {}: original at {}, your edited copy at {}",
            p.name, p.restored_original_at, p.current_copy_at
        );
    }
    for k in &report.kept_own {
        println!(
            "  Kept YOUR current version of {} at {} (original copy: {})",
            k.name, k.current_version_at, k.original_copy_at
        );
    }
    if let Some(kept) = &report.backups_kept_at {
        println!("  Restore backups kept (not purged because a restore failed): {kept}");
    }
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
// `with_fake_home` + mutex convention as `scenery-mcp/src/setup.rs` and
// `scenery-daemon/src/managed_paths/pusher_tests.rs`. This keeps the
// developer's real `$HOME` and `~/Library/Application Support/Scenery`
// completely untouched — no test here ever mutates them.
#[cfg(test)]
mod tests {
    #[test]
    fn refuses_user_uninstall_only_when_pkg_managed() {
        assert!(super::refuse_uninstall(true, false));
        assert!(!super::refuse_uninstall(true, true));
        assert!(!super::refuse_uninstall(false, false));
        assert!(!super::refuse_uninstall(false, true));
    }

    use super::*;
    use scenery_core::restore_journal::{JournalOp, JournalSource, RestoreJournal};
    use scenery_core::state::{AppState, McpInstallRow};
    use scenery_mcp::setup::{ClientConfig, ConfigFormat};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Serializes every test in this module that mutates the process-global
    /// `HOME` env var.
    pub(super) static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn temp_dir(label: &str) -> PathBuf {
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
    pub(super) fn with_fake_home<T>(fake_home: &Path, f: impl FnOnce() -> T) -> T {
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

    // ── sign out everywhere ─────────────────────────────────────────────────

    /// Uninstall must leave no Scenery secret behind: the tokens for
    /// every registry this machine signed in to (not just the configured
    /// one) and the H3 local key.
    /// Uninstall deprovisions the paired device (refreshing an expired access
    /// token first) and revokes the session server side, then clears the
    /// local device identity so a reinstall pairs as a fresh device.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn sign_out_everywhere_deprovisions_the_device_and_revokes_the_session() {
        use scenery_core::auth::{load_all_tokens, save_tokens};

        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_dir("sign-out-deprovision");
        let original_home = std::env::var_os("HOME");
        let original_keychain = std::env::var_os("SCENERY_DISABLE_KEYCHAIN");
        std::env::set_var("HOME", &tmp);
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");

        let mut server = mockito::Server::new_async().await;
        let url = server.url();
        let refresh = server
            .mock("POST", "/portal/auth/refresh")
            .with_status(200)
            .with_body(
                r#"{"access_token":"fresh-acc","refresh_token":"fresh-ref","token_type":"bearer"}"#,
            )
            .create_async()
            .await;
        let deprovision = server
            .mock("POST", "/api/devices/dev-9/deprovision")
            .match_header("authorization", "Bearer fresh-acc")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let revoke = server
            .mock("POST", "/portal/auth/revoke")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "refresh_token": "fresh-ref",
                "scope": "family"
            })))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let mut report = RestoreReport::default();
        let outcome: anyhow::Result<(usize, Option<String>)> = async {
            let state = AppState::bootstrap()?;
            save_tokens(&state, &url, "expired-acc", "old-ref")?;
            state.set_sync_state("device_id", "dev-9")?;

            sign_out_everywhere(&url, &mut report).await;

            Ok((
                load_all_tokens(&state)?.len(),
                state.get_sync_state("device_id")?,
            ))
        }
        .await;

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match original_keychain {
            Some(v) => std::env::set_var("SCENERY_DISABLE_KEYCHAIN", v),
            None => std::env::remove_var("SCENERY_DISABLE_KEYCHAIN"),
        }
        let _ = fs::remove_dir_all(&tmp);

        let (remaining, device_id) = outcome.expect("test body should succeed");
        refresh.assert_async().await;
        deprovision.assert_async().await;
        revoke.assert_async().await;
        assert_eq!(remaining, 0);
        assert_eq!(device_id, None, "a reinstall must pair as a fresh device");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn sign_out_everywhere_clears_every_registry_and_the_local_key() {
        use scenery_core::auth::{load_all_tokens, save_tokens};
        use scenery_core::secret_file;

        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_dir("sign-out-everywhere");
        let original_home = std::env::var_os("HOME");
        let original_keychain = std::env::var_os("SCENERY_DISABLE_KEYCHAIN");
        std::env::set_var("HOME", &tmp);
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");

        let mut report = RestoreReport::default();
        let outcome: anyhow::Result<(usize, bool)> = async {
            let state = AppState::bootstrap()?;
            save_tokens(&state, "https://dev.example.invalid", "a1", "r1")?;
            save_tokens(&state, "https://prod.example.invalid", "a2", "r2")?;
            state.local_mcp_key()?;

            sign_out_everywhere("https://prod.example.invalid", &mut report).await;

            Ok((
                load_all_tokens(&state)?.len(),
                secret_file::secrets_dir(state.root_dir.as_std_path()).exists(),
            ))
        }
        .await;

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match original_keychain {
            Some(v) => std::env::set_var("SCENERY_DISABLE_KEYCHAIN", v),
            None => std::env::remove_var("SCENERY_DISABLE_KEYCHAIN"),
        }
        let _ = fs::remove_dir_all(&tmp);

        let (remaining, secrets_left) = outcome.expect("test body should succeed");
        assert_eq!(remaining, 0, "tokens for every registry must be cleared");
        assert!(!secrets_left, "the secrets directory must be gone");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    // ── M6: auth logout clears device/session state ─────────────────────────

    /// Regression for M6: `auth logout` must clear `device_id`/
    /// `device_uuid`/`mcp_user_id`, or a different user logging in
    /// afterward would have their valid MCP access token rejected as a
    /// stale `sub_mismatch` against this user's binding (the daemon-level
    /// half of this is covered by
    /// `scenery_daemon::mcp_http::tests::logout_then_login_as_different_user_succeeds_without_sub_mismatch`;
    /// this test covers the CLI command itself).
    ///
    /// Holds `HOME_MUTEX` across the `.await` on `cmd_auth_logout`, which
    /// clippy flags on principle (`await_holding_lock`) — correctness here
    /// actually *requires* it: `cmd_auth_logout` itself reads `HOME` (via
    /// `AppState::bootstrap`), so the fake value must stay in place for the
    /// whole call, not just up to the `.await`. Safe to hold: every step
    /// inside `cmd_auth_logout` is synchronous (no real I/O yield point),
    /// so the awaited future never actually suspends and nothing else on
    /// this single-threaded test runtime can be blocked by the guard.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn auth_logout_clears_device_session_state() {
        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_dir("auth-logout-clears-session");
        let original = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        // Collected as plain values (no `.expect`/`unwrap` panics) inside
        // this block, so HOME is restored below unconditionally before any
        // assertion can panic — mirrors `with_fake_home`'s `catch_unwind`
        // guarantee for the sync case.
        let outcome: anyhow::Result<(Option<String>, Option<String>, Option<String>)> = async {
            let state = AppState::bootstrap()?;
            state.set_sync_state("device_id", "id-1")?;
            state.set_sync_state("device_uuid", "uuid-1")?;
            state.set_sync_state("mcp_user_id", "user-1")?;

            crate::cmd_auth_logout("https://example.invalid").await?;

            Ok((
                state.get_sync_state("device_id")?,
                state.get_sync_state("device_uuid")?,
                state.get_sync_state("mcp_user_id")?,
            ))
        }
        .await;

        match original {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);

        let (device_id, device_uuid, mcp_user_id) = outcome.expect("test body should succeed");
        assert_eq!(device_id, None);
        assert_eq!(device_uuid, None);
        assert_eq!(mcp_user_id, None);
    }

    pub(super) fn brokered_row(
        mcp_server_id: &str,
        installation_id: &str,
        name: &str,
    ) -> McpInstallRow {
        McpInstallRow {
            mcp_server_id: mcp_server_id.to_string(),
            installation_id: installation_id.to_string(),
            mcp_server_name: name.to_string(),
            package_source: "npm:@acme/slack-mcp".to_string(),
            version_pin: None,
            server_config: None,
            auth_type: "oauth".to_string(),
            gateway_server_id: None,
            gateway_url: Some("https://gateway.scenery.dev/mcp/slack".to_string()),
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
                    "scenery": {"command": "scenery", "args": ["mcp", "serve"]},
                    "slack-mcp": {"command": "scenery", "args": ["mcp", "serve", "--server", "slack-mcp"]},
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
            format: ConfigFormat::Json,
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
            !servers.contains_key("scenery"),
            "scenery shim entry must be gone"
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
        let target_path = dir.join("scenery-created.txt");
        fs::write(&target_path, b"Scenery put this here").unwrap();

        // Native source, no backup_path -> target did not exist pre-Scenery.
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
        fs::write(&backup_path, b"pre-scenery content").unwrap();
        let target_path = dir.join("target.txt");

        let good_entry = JournalEntry::new(
            JournalOp::FileReplace,
            JournalSource::Native,
            target_path.to_string_lossy().to_string(),
        )
        .with_backup_path(backup_path.to_string_lossy().to_string());

        let bad_entry = JournalEntry::new(
            JournalOp::FileReplace,
            JournalSource::Adopted,
            dir.join("other-target.txt").to_string_lossy().to_string(),
        )
        .with_backup_path(dir.join("missing-backup.txt").to_string_lossy().to_string());

        let mut report = RestoreReport::default();
        restore_from_journal(&[bad_entry, good_entry], &mut report);

        assert_eq!(
            fs::read(&target_path).unwrap(),
            b"pre-scenery content",
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

    // ── restore: oldest backup, never destroy edits ───────────────────────────

    use scenery_core::safe_copy::tree_fingerprint;
    use scenery_mcp::ownership::MANAGED_MARKER_FILENAME;

    /// Lay out `<dir>/skills/demo` as a converted (marked) tree, plus the
    /// given backups, and return the target path.
    fn converted_target(dir: &Path, body: &str) -> PathBuf {
        let target = dir.join("skills").join("demo");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), body).unwrap();
        fs::write(target.join(MANAGED_MARKER_FILENAME), "{}").unwrap();
        target
    }

    fn backup_with(dir: &Path, name: &str, body: &str) -> PathBuf {
        let b = dir
            .join("restore-backups")
            .join(name)
            .join("adopted")
            .join("demo");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("SKILL.md"), body).unwrap();
        b
    }

    fn adopted_entry(target: &Path, backup: &Path) -> JournalEntry {
        JournalEntry::new(
            JournalOp::FileDelete,
            JournalSource::Adopted,
            target.to_string_lossy().to_string(),
        )
        .with_slug("demo")
        .with_backup_path(backup.to_string_lossy().to_string())
    }

    fn push_entry(target: &Path, tree_sha256: Option<String>) -> JournalEntry {
        JournalEntry::new(
            JournalOp::ArtifactPush,
            JournalSource::Managed,
            target.to_string_lossy().to_string(),
        )
        .with_slug("demo")
        .with_detail(serde_json::json!({ "tree_sha256": tree_sha256 }))
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    /// A retried conversion journals a SECOND adopted backup, of the
    /// half-written tree. Newest-first replay used to restore that one.
    #[test]
    fn restore_uses_the_oldest_adopted_backup_per_path() {
        let dir = temp_dir("restore-oldest");
        let target = converted_target(&dir, "published");
        let first = backup_with(&dir, "t1", "the user's original");
        let second = backup_with(&dir, "t2", "half-written shadow");
        let entries = [
            adopted_entry(&target, &first),
            adopted_entry(&target, &second),
        ];

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        assert_eq!(report.restored.len(), 1, "one restore per path");
        assert_eq!(report.restored[0].from_backup, first.to_string_lossy());
        assert!(report.failed.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_falls_back_to_the_next_oldest_backup_when_the_oldest_is_gone() {
        let dir = temp_dir("restore-fallback");
        let target = converted_target(&dir, "published");
        let gone = dir.join("restore-backups/t1/adopted/demo");
        let second = backup_with(&dir, "t2", "second backup");
        let entries = [
            adopted_entry(&target, &gone),
            adopted_entry(&target, &second),
        ];

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), b"second backup");
        assert!(report.failed.is_empty());
        assert_eq!(report.warnings.len(), 1, "the missing backup is reported");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_fails_loudly_when_no_adopted_backup_exists() {
        let dir = temp_dir("restore-none");
        let target = converted_target(&dir, "published");
        let entries = [adopted_entry(&target, &dir.join("nope"))];
        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"published",
            "untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The live tree is exactly what Scenery installed: nothing of the user's
    /// to lose, so no stray copy is left behind.
    #[test]
    fn restore_replaces_the_pristine_installed_tree_without_leaving_copies() {
        let dir = temp_dir("restore-pristine");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "the user's original");
        let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&target, &backup),
            push_entry(&target, Some(hash)),
        ];

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        assert!(!target.join(MANAGED_MARKER_FILENAME).exists());
        assert_eq!(
            names_in(&dir.join("skills")),
            vec!["demo"],
            "no stray copies"
        );
        assert!(report.preserved.is_empty());
        assert!(report.failed.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The user kept working in the converted directory. Restoring the
    /// original must not destroy that: both versions survive, and the report
    /// says where each went.
    #[test]
    fn restore_preserves_edits_made_after_the_conversion() {
        let dir = temp_dir("restore-edited");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "the user's original");
        let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&target, &backup),
            push_entry(&target, Some(hash)),
        ];
        // The user's later edit, plus a brand-new file.
        fs::write(target.join("SKILL.md"), "published + my new work").unwrap();
        fs::write(target.join("extra.md"), "added later").unwrap();

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        // Original is back at the path...
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        assert!(!target.join("extra.md").exists());
        // ...and the edited version sits beside it, intact.
        assert_eq!(report.preserved.len(), 1);
        let p = &report.preserved[0];
        assert_eq!(p.restored_original_at, target.to_string_lossy());
        let kept = PathBuf::from(&p.current_copy_at);
        assert!(kept.parent() == target.parent() && kept != target);
        assert_eq!(
            fs::read(kept.join("SKILL.md")).unwrap(),
            b"published + my new work"
        );
        assert_eq!(fs::read(kept.join("extra.md")).unwrap(), b"added later");
        assert!(
            !kept.join(MANAGED_MARKER_FILENAME).exists(),
            "the preserved copy must not look Scenery-managed"
        );
        assert!(report.failed.is_empty());
        assert_eq!(report.restored.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Journals written by 2.2.4 and earlier carry no fingerprint, so
    /// "unchanged" cannot be proven: the live tree is kept, never guessed at.
    #[test]
    fn restore_without_a_fingerprint_preserves_the_live_tree() {
        let dir = temp_dir("restore-legacy");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "the user's original");
        let entries = [adopted_entry(&target, &backup), push_entry(&target, None)];

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        assert_eq!(report.preserved.len(), 1);
        assert_eq!(
            fs::read(PathBuf::from(&report.preserved[0].current_copy_at).join("SKILL.md")).unwrap(),
            b"published"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A preserved copy must survive the managed-artifact sweep that runs
    /// right after the restore (it deletes every marked dir under
    /// `~/.agents/skills`).
    #[test]
    fn preserved_copy_survives_the_managed_artifact_sweep() {
        let fake_home = temp_dir("restore-sweep");
        with_fake_home(&fake_home, || {
            let skills = fake_home.join(".agents").join("skills");
            fs::create_dir_all(&skills).unwrap();
            let target = skills.join("demo");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("SKILL.md"), "published, edited").unwrap();
            fs::write(target.join(MANAGED_MARKER_FILENAME), "{\"marker_version\":1,\"installation_id\":null,\"source_sha256\":\"x\",\"migrated_at\":\"t\"}").unwrap();
            let backup = backup_with(&fake_home, "t1", "original");
            let entries = [adopted_entry(&target, &backup), push_entry(&target, None)];

            let mut report = RestoreReport::default();
            restore_from_journal(&entries, &mut report);
            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            remove_managed_artifacts(&plan, &mut report);

            assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), b"original");
            let kept = PathBuf::from(&report.preserved[0].current_copy_at);
            assert_eq!(
                fs::read(kept.join("SKILL.md")).unwrap(),
                b"published, edited"
            );
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    #[cfg(unix)]
    #[test]
    fn restore_keeps_symlinks_as_symlinks_and_does_not_loop() {
        let dir = temp_dir("restore-symlinks");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "original");
        std::os::unix::fs::symlink("..", backup.join("up")).unwrap();
        std::os::unix::fs::symlink("SKILL.md", backup.join("alias.md")).unwrap();
        let entries = [adopted_entry(&target, &backup)];

        let mut report = RestoreReport::default();
        restore_from_journal(&entries, &mut report);

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(
            fs::read_link(target.join("up")).unwrap(),
            PathBuf::from("..")
        );
        assert_eq!(
            fs::read_link(target.join("alias.md")).unwrap(),
            PathBuf::from("SKILL.md")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ── own (auto-managed) skills: later edits are never lost (plan 4.5.5) ────

    fn own_push_entry(target: &Path, tree_sha256: Option<String>) -> JournalEntry {
        JournalEntry::new(
            JournalOp::ArtifactPush,
            JournalSource::Managed,
            target.to_string_lossy().to_string(),
        )
        .with_slug("demo")
        .with_detail(serde_json::json!({
            "tree_sha256": tree_sha256,
            "origin": "auto_managed",
            "skill_id": "adam~demo",
        }))
    }

    /// The user kept editing their own converted skill: their CURRENT tree
    /// stays, unmanaged; the original goes to the report folder.
    #[test]
    fn an_edited_own_skill_keeps_the_current_tree_and_puts_the_original_in_the_report_folder() {
        let dir = temp_dir("own-edited");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "the user's original");
        let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&target, &backup),
            own_push_entry(&target, Some(hash)),
        ];
        fs::write(target.join("SKILL.md"), "published + months of my work").unwrap();
        fs::write(target.join("new-ref.md"), "added later").unwrap();
        let originals = dir.join("report-folder");

        let mut report = RestoreReport::default();
        restore_from_journal_in(&entries, &mut report, &originals);

        // Current tree stays, no longer Scenery's.
        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"published + months of my work"
        );
        assert_eq!(fs::read(target.join("new-ref.md")).unwrap(), b"added later");
        assert!(!target.join(MANAGED_MARKER_FILENAME).exists());
        // The original is kept beside the report.
        assert_eq!(report.kept_own.len(), 1);
        let k = &report.kept_own[0];
        assert_eq!(k.current_version_at, target.to_string_lossy());
        assert!(k
            .original_copy_at
            .starts_with(&originals.to_string_lossy().to_string()));
        assert_eq!(
            fs::read(PathBuf::from(&k.original_copy_at).join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        // Not reported as a restore, not as a both-versions preserve; nothing failed.
        assert!(report.restored.is_empty() && report.preserved.is_empty());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let md = render_markdown(&report);
        assert!(md.contains("Your own skills") && md.contains("your version stays at"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unedited_own_skill_is_restored_byte_identical_and_listed_under_restored() {
        let dir = temp_dir("own-pristine");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "the user's original");
        let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&target, &backup),
            own_push_entry(&target, Some(hash)),
        ];

        let mut report = RestoreReport::default();
        restore_from_journal_in(&entries, &mut report, &dir.join("report-folder"));

        assert_eq!(
            fs::read(target.join("SKILL.md")).unwrap(),
            b"the user's original"
        );
        assert!(report.kept_own.is_empty() && report.preserved.is_empty());
        assert_eq!(report.restored.len(), 1);
        assert_eq!(report.restored[0].restored_to, target.to_string_lossy());
        assert!(
            !dir.join("report-folder").exists(),
            "nothing to keep, no folder"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An own skill from a non-canonical root (`~/.pi/agent/skills/x`) was
    /// removed after backup; uninstall puts it back at its ORIGINAL path.
    #[test]
    fn a_taken_over_noncanonical_own_skill_is_restored_to_its_original_path() {
        let dir = temp_dir("own-noncanonical");
        let original_path = dir.join("pi/agent/skills/demo");
        let backup = backup_with(&dir, "t1", "pi original");
        // The managed copy lives at the canonical path, a different one.
        let canonical = converted_target(&dir, "published");
        let hash = tree_fingerprint(&canonical, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&original_path, &backup),
            own_push_entry(&canonical, Some(hash)),
        ];

        let mut report = RestoreReport::default();
        restore_from_journal_in(&entries, &mut report, &dir.join("report-folder"));

        assert_eq!(
            fs::read(original_path.join("SKILL.md")).unwrap(),
            b"pi original"
        );
        assert_eq!(report.restored.len(), 1, "listed under RESTORED");
        assert!(report.failed.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Converted from `~/.pi/...` to the canonical root, then edited: the
    /// original goes back to the pi path and the EDITED canonical copy is
    /// copied out before the sweep deletes it.
    #[test]
    fn an_edited_noncanonical_origin_own_skill_keeps_its_edits_through_the_sweep() {
        let fake_home = temp_dir("own-noncanonical-edited");
        with_fake_home(&fake_home, || {
            let canonical = fake_home.join(".agents/skills/demo");
            fs::create_dir_all(&canonical).unwrap();
            fs::write(canonical.join("SKILL.md"), "published").unwrap();
            fs::write(canonical.join(MANAGED_MARKER_FILENAME), "{\"marker_version\":1,\"installation_id\":null,\"source_sha256\":\"x\",\"migrated_at\":\"t\"}").unwrap();
            let hash = tree_fingerprint(&canonical, &[MANAGED_MARKER_FILENAME]).unwrap();
            let original_path = fake_home.join(".pi/agent/skills/demo");
            let backup = backup_with(&fake_home, "t1", "pi original");
            let entries = [
                adopted_entry(&original_path, &backup),
                own_push_entry(&canonical, Some(hash)),
            ];
            fs::write(canonical.join("SKILL.md"), "published + my edits").unwrap();
            fs::write(canonical.join("extra.md"), "mine").unwrap();

            let mut report = RestoreReport::default();
            restore_from_journal_in(&entries, &mut report, &fake_home.join("rep"));
            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            remove_managed_artifacts(&plan, &mut report);

            assert_eq!(
                fs::read(original_path.join("SKILL.md")).unwrap(),
                b"pi original"
            );
            assert!(!canonical.exists(), "the managed copy is swept as usual");
            assert_eq!(report.preserved.len(), 1, "{:?}", report.preserved);
            let kept = PathBuf::from(&report.preserved[0].current_copy_at);
            assert_eq!(
                fs::read(kept.join("SKILL.md")).unwrap(),
                b"published + my edits"
            );
            assert_eq!(fs::read(kept.join("extra.md")).unwrap(), b"mine");
            assert!(!kept.join(MANAGED_MARKER_FILENAME).exists());
            assert!(report.failed.is_empty());
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    /// The kept own skill and its originals copy survive the managed sweep
    /// that runs right after the restore, and the Claude link is dropped.
    #[cfg(unix)]
    #[test]
    fn a_kept_own_skill_survives_the_sweep_and_loses_its_client_link() {
        let fake_home = temp_dir("own-sweep");
        with_fake_home(&fake_home, || {
            let skills = fake_home.join(".agents").join("skills");
            let claude = fake_home.join(".claude").join("skills");
            fs::create_dir_all(&skills).unwrap();
            fs::create_dir_all(&claude).unwrap();
            let target = skills.join("demo");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("SKILL.md"), "published").unwrap();
            fs::write(target.join(MANAGED_MARKER_FILENAME), "{}").unwrap();
            std::os::unix::fs::symlink(&target, claude.join("demo")).unwrap();
            let backup = backup_with(&fake_home, "t1", "the user's original");
            let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
            let entries = [
                adopted_entry(&target, &backup),
                own_push_entry(&target, Some(hash)),
            ];
            fs::write(target.join("SKILL.md"), "my later edit").unwrap();

            let mut report = RestoreReport::default();
            restore_from_journal_in(&entries, &mut report, &fake_home.join("rep"));
            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            remove_managed_artifacts(&plan, &mut report);

            assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), b"my later edit");
            assert!(
                claude.join("demo").symlink_metadata().is_err(),
                "client link removed"
            );
            assert!(
                report.removed.iter().all(|r| r.name != "demo"),
                "not swept as managed"
            );
            assert_eq!(report.kept_own.len(), 1);
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    /// A catalog skill (no auto_managed origin) keeps today's behaviour.
    #[test]
    fn a_catalog_skill_edit_still_restores_the_original_and_preserves_the_edit_beside_it() {
        let dir = temp_dir("catalog-edited");
        let target = converted_target(&dir, "published");
        let backup = backup_with(&dir, "t1", "original");
        let hash = tree_fingerprint(&target, &[MANAGED_MARKER_FILENAME]).unwrap();
        let entries = [
            adopted_entry(&target, &backup),
            push_entry(&target, Some(hash)),
        ];
        fs::write(target.join("SKILL.md"), "edited").unwrap();
        let mut report = RestoreReport::default();
        restore_from_journal_in(&entries, &mut report, &dir.join("rep"));
        assert!(report.kept_own.is_empty());
        assert_eq!(report.preserved.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── --purge keeps the backups when a restore failed ───────────────────────

    fn seed_data_dir(root: &Path) {
        fs::create_dir_all(root.join("restore-backups/t1/adopted/demo")).unwrap();
        fs::write(
            root.join("restore-backups/t1/adopted/demo/SKILL.md"),
            "precious",
        )
        .unwrap();
        fs::write(root.join("restore-journal.json"), "[]").unwrap();
        fs::write(root.join("restore-journal.json.lock"), "").unwrap();
        fs::write(root.join("scratch.txt"), "db").unwrap();
        fs::create_dir_all(root.join("skills/x")).unwrap();
        fs::write(root.join("skills/x/f"), "f").unwrap();
    }

    #[test]
    fn purge_data_dir_at_keeps_restore_data_but_removes_everything_else() {
        let root = temp_dir("purge-keep");
        seed_data_dir(&root);
        purge_data_dir_at(&root, true).unwrap();
        assert_eq!(
            fs::read(root.join("restore-backups/t1/adopted/demo/SKILL.md")).unwrap(),
            b"precious"
        );
        assert!(root.join("restore-journal.json").exists());
        assert!(!root.join("scratch.txt").exists());
        assert!(!root.join("skills").exists());

        purge_data_dir_at(&root, false).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn purge_step_never_deletes_backups_when_a_restore_failed() {
        let fake_home = temp_dir("purge-step-failed");
        with_fake_home(&fake_home, || {
            let state = AppState::bootstrap().unwrap();
            let root = state.root_dir.as_std_path().to_path_buf();
            seed_data_dir(&root);

            let mut report = RestoreReport::default();
            report.failed.push("could not restore '/x'".into());
            run_purge_step(&mut report);

            let kept = root.join("restore-backups");
            assert!(
                kept.join("t1/adopted/demo/SKILL.md").exists(),
                "backups survive"
            );
            assert!(!report.purged_data_dir);
            assert_eq!(
                report.backups_kept_at.as_deref(),
                Some(kept.to_str().unwrap())
            );
            assert!(report
                .warnings
                .iter()
                .any(|w| w.contains(kept.to_str().unwrap())));
            assert!(
                !root.join("scratch.txt").exists(),
                "the rest is still purged"
            );

            let md = render_markdown(&report);
            assert!(md.contains("Restore backups kept") && md.contains(kept.to_str().unwrap()));
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn purge_step_removes_the_whole_data_dir_when_every_restore_succeeded() {
        let fake_home = temp_dir("purge-step-ok");
        with_fake_home(&fake_home, || {
            let state = AppState::bootstrap().unwrap();
            let root = state.root_dir.as_std_path().to_path_buf();
            seed_data_dir(&root);

            let mut report = RestoreReport::default();
            run_purge_step(&mut report);

            assert!(!root.exists());
            assert!(report.purged_data_dir);
            assert!(report.backups_kept_at.is_none());
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn report_lists_preserved_copies() {
        let mut report = RestoreReport::default();
        report.preserved.push(PreservedItem {
            name: "demo".into(),
            restored_original_at: "/h/.agents/skills/demo".into(),
            current_copy_at: "/h/.agents/skills/demo.scenery-edited-x".into(),
            reason: "it changed".into(),
        });
        let md = render_markdown(&report);
        assert!(md.contains("/h/.agents/skills/demo.scenery-edited-x"));
        assert!(md.contains("/h/.agents/skills/demo`"));
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
            scenery_daemon::managed_paths::marker::write_file_marker(
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

    #[test]
    fn whole_product_uninstall_removes_the_builtin_authoring_skill() {
        use scenery_daemon::managed_paths::marker::{
            write_file_marker_with, Provenance, ORIGIN_BUILTIN,
        };
        let fake_home = temp_dir("builtin-sweep");
        with_fake_home(&fake_home, || {
            let canonical = fake_home.join(".agents/skills/authoring-with-scenery");
            fs::create_dir_all(&canonical).unwrap();
            fs::write(canonical.join("SKILL.md"), b"builtin body").unwrap();
            write_file_marker_with(
                &canonical,
                None,
                "abc",
                "2026-11-18T00:00:00Z",
                &Provenance {
                    origin: Some(ORIGIN_BUILTIN.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let links = fake_home.join(".claude/skills");
            fs::create_dir_all(&links).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&canonical, links.join("authoring-with-scenery")).unwrap();

            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            let mut report = RestoreReport::default();
            remove_managed_artifacts(&plan, &mut report);

            assert!(!canonical.exists(), "the built-in skill is removed");
            assert!(links
                .join("authoring-with-scenery")
                .symlink_metadata()
                .is_err());
            let item = report
                .removed
                .iter()
                .find(|r| r.name == "authoring-with-scenery")
                .expect("reported as removed");
            assert!(item.reinstall_hint.contains("Built into Scenery"));
            // Nothing of the user's own to keep or restore.
            assert!(report.kept_own.is_empty());
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn remove_managed_artifacts_removes_scenery_marketplace_but_keeps_others() {
        let fake_home = temp_dir("marketplace-sweep");
        with_fake_home(&fake_home, || {
            let claude = fake_home.join(".claude");
            let plugins = claude.join("plugins");
            let w = |p: PathBuf, v: serde_json::Value| {
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
            };
            let ours = plugins.join("marketplaces/scenery/plugins/acme/.claude-plugin");
            fs::create_dir_all(&ours).unwrap();
            fs::create_dir_all(plugins.join("cache/scenery/acme/1.0.0")).unwrap();
            let official = plugins.join("marketplaces/claude-plugins-official/keep");
            fs::create_dir_all(&official).unwrap();
            fs::create_dir_all(plugins.join("cache/claude-plugins-official/x")).unwrap();
            w(
                plugins.join("known_marketplaces.json"),
                serde_json::json!({
                "scenery": {"source": "dir"}, "claude-plugins-official": {"source": "github"}}),
            );
            w(
                plugins.join("installed_plugins.json"),
                serde_json::json!({
                "version": 2, "plugins": {"acme@scenery": [{"version": "1.0.0"}],
                "other@claude-plugins-official": [{"version": "2"}]}}),
            );
            w(
                claude.join("settings.json"),
                serde_json::json!({
                "theme": "dark",
                "enabledPlugins": {"acme@scenery": true, "other@claude-plugins-official": true},
                "extraKnownMarketplaces": {"scenery": {}, "claude-plugins-official": {}}}),
            );

            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            let mut report = RestoreReport::default();
            remove_managed_artifacts(&plan, &mut report);

            assert!(!plugins.join("marketplaces/scenery").exists());
            assert!(!plugins.join("cache/scenery").exists());
            assert!(official.exists());
            assert!(plugins.join("cache/claude-plugins-official/x").exists());
            let read = |p: PathBuf| -> serde_json::Value {
                serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
            };
            let known = read(plugins.join("known_marketplaces.json"));
            assert!(known.get("scenery").is_none());
            assert!(known.get("claude-plugins-official").is_some());
            let inst = read(plugins.join("installed_plugins.json"));
            assert!(inst["plugins"].get("acme@scenery").is_none());
            assert!(inst["plugins"]
                .get("other@claude-plugins-official")
                .is_some());
            let settings = read(claude.join("settings.json"));
            assert_eq!(settings["theme"], "dark");
            assert!(settings["extraKnownMarketplaces"].get("scenery").is_none());
            assert!(settings["extraKnownMarketplaces"]
                .get("claude-plugins-official")
                .is_some());
            assert!(settings["enabledPlugins"].get("acme@scenery").is_none());
            assert!(settings["enabledPlugins"]
                .get("other@claude-plugins-official")
                .is_some());
            assert!(report
                .removed
                .iter()
                .any(|r| r.name == "acme" && matches!(r.kind, RemovedKind::ManagedPlugin)));
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    /// Post-pivot layout: the real content lives at `~/.agents/skills/<slug>`
    /// and `~/.claude/skills/<slug>` is only a symlink into it. Both must go,
    /// and the skill must be reported exactly once — sweeping only the Claude
    /// path would remove the link, leave the real skill on disk, and still
    /// tell the user it had been removed.
    #[test]
    fn remove_managed_artifacts_removes_canonical_skill_and_its_claude_link() {
        let fake_home = temp_dir("managed-sweep-canonical");
        with_fake_home(&fake_home, || {
            let canonical_root = fake_home.join(".agents").join("skills");
            let link_root = fake_home.join(".claude").join("skills");
            fs::create_dir_all(&canonical_root).unwrap();
            fs::create_dir_all(&link_root).unwrap();

            let canonical = canonical_root.join("pushed-skill");
            fs::create_dir_all(&canonical).unwrap();
            fs::write(canonical.join("SKILL.md"), b"managed body").unwrap();
            scenery_daemon::managed_paths::marker::write_file_marker(
                &canonical,
                Some("inst-1"),
                "deadbeef",
                "2026-07-19T00:00:00Z",
            )
            .unwrap();

            let link = link_root.join("pushed-skill");
            std::os::unix::fs::symlink(&canonical, &link).unwrap();

            // A user's own skill in the canonical root must survive.
            let user_own = canonical_root.join("user-skill");
            fs::create_dir_all(&user_own).unwrap();
            fs::write(user_own.join("SKILL.md"), b"the user's own skill").unwrap();

            let plan = Plan {
                brokered: vec![],
                clients: vec![],
            };
            let mut report = RestoreReport::default();
            remove_managed_artifacts(&plan, &mut report);

            assert!(
                !canonical.exists(),
                "the real skill content must be removed from ~/.agents/skills"
            );
            assert!(
                !link.exists() && !link.is_symlink(),
                "the Claude link must be removed too"
            );
            assert!(user_own.exists(), "the user's own skill must survive");

            let hits: Vec<_> = report
                .removed
                .iter()
                .filter(|r| r.name == "pushed-skill")
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "one skill must be reported exactly once, not once per location"
            );
            assert!(
                hits[0]
                    .stripped_from
                    .iter()
                    .any(|s| s == "~/.agents/skills"),
                "the report must name the canonical location it was removed from"
            );
        });
        let _ = fs::remove_dir_all(&fake_home);
    }

    // ── --local-only: no server calls, tokens kept ────────────────────────────

    /// Run `uninstall --local-only --yes` against a mockito server whose
    /// deprovision/refresh/logout/revoke endpoints all expect zero hits.
    /// Returns whether tokens survived and whether the client config still
    /// holds the Scenery entry.
    #[allow(clippy::await_holding_lock)]
    async fn run_local_only(label: &str) -> (bool, bool, bool, bool) {
        use scenery_core::auth::{load_tokens, save_tokens};

        let _guard = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_dir(label);
        let report_dir = temp_dir(&format!("{label}-report"));
        let original_home = std::env::var_os("HOME");
        let original_keychain = std::env::var_os("SCENERY_DISABLE_KEYCHAIN");
        std::env::set_var("HOME", &tmp);
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");

        let mut server = mockito::Server::new_async().await;
        let url = server.url();
        let mut mocks = Vec::new();
        for path in [
            "/portal/auth/refresh",
            "/api/devices/dev-9/deprovision",
            "/portal/auth/logout",
            "/portal/auth/revoke",
        ] {
            mocks.push(
                server
                    .mock("POST", path)
                    .with_status(200)
                    .with_body("{}")
                    .expect(0)
                    .create_async()
                    .await,
            );
        }
        let any = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(200)
            .expect(0)
            .create_async()
            .await;

        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "scenery": {"command": "scenery", "args": ["mcp", "serve"]},
                    "my-own-server": {"command": "my-tool", "args": []}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let state = AppState::bootstrap().unwrap();
        save_tokens(&state, &url, "expired-acc", "old-ref").unwrap();
        state.set_sync_state("device_id", "dev-9").unwrap();

        let result = run(UninstallOpts {
            yes: true,
            report_dir: Some(report_dir.clone()),
            registry_url: url.clone(),
            local_only: true,
            ..Default::default()
        })
        .await;

        let tokens_kept = load_tokens(&state, &url).unwrap().is_some();
        let device_kept = state.get_sync_state("device_id").unwrap().is_some();
        let cfg = fs::read_to_string(&config_path).unwrap();
        let has_scenery = cfg.contains("\"scenery\"");
        let keeps_own = cfg.contains("my-own-server");

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match original_keychain {
            Some(v) => std::env::set_var("SCENERY_DISABLE_KEYCHAIN", v),
            None => std::env::remove_var("SCENERY_DISABLE_KEYCHAIN"),
        }
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_dir_all(&report_dir);

        result.expect("local-only uninstall should succeed");
        for m in &mocks {
            m.assert_async().await;
        }
        any.assert_async().await;
        (tokens_kept, device_kept, has_scenery, keeps_own)
    }

    #[tokio::test]
    async fn local_only_makes_no_server_calls() {
        // Mock `.expect(0)` assertions run inside the helper.
        run_local_only("local-only-nocalls").await;
    }

    #[tokio::test]
    async fn local_only_keeps_tokens() {
        let (tokens_kept, device_kept, ..) = run_local_only("local-only-tokens").await;
        assert!(tokens_kept, "tokens must survive --local-only");
        assert!(device_kept, "device id must survive --local-only");
    }

    #[tokio::test]
    async fn local_only_still_restores_client_config() {
        let (_, _, has_scenery, keeps_own) = run_local_only("local-only-config").await;
        assert!(!has_scenery, "the Scenery entry must be removed");
        assert!(keeps_own, "the user's own entry must survive");
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
                "scenery": {"command": "scenery", "args": ["mcp", "serve"]},
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
        scenery_daemon::managed_paths::marker::write_file_marker(
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
            registry_url: "https://app.scenery.invalid".to_string(),
            local_only: false,
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

        let stamp = std::env::var("SCENERY_UNINSTALL_STAMP").unwrap_or_else(|_| "latest".into());
        assert!(report_dir
            .join(format!("Scenery-uninstall-{stamp}.md"))
            .exists());

        let _ = fs::remove_dir_all(&fake_home);
        let _ = fs::remove_dir_all(&report_dir);
    }
}

#[cfg(test)]
#[path = "uninstall_clients_tests.rs"]
mod clients_tests;
