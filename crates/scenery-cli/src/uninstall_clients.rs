//! Client-config half of `scenery uninstall`.
//!
//! Rule: uninstall restores, never deletes. The user ends up with their own
//! config, including edits made while Scenery was installed.
//!
//! Per client config file:
//!
//! 1. **Shortcut.** If the file is byte-identical to what `mcp setup` last
//!    wrote (SHA-256 in the journal) and the pre-setup backup is clean, the
//!    backup is copied back. A file Scenery created and nobody touched is
//!    deleted.
//! 2. **Surgical.** Otherwise remove only the keys Scenery added (`scenery`
//!    and brokered entries), editing in place without reformatting. A file
//!    Scenery created is deleted only if nothing but an empty container is
//!    left.
//! 3. **Refuse.** A file that does not parse is left alone, the client is
//!    reported as failed, and the command exits non-zero.
//!
//! Report facts come from re-reading each file after the write.

use super::{record_removed, KeptItem, Plan, RemovedKind, RestoreReport, RestoredItem};
use scenery_core::restore_journal::{sha256_hex, JournalEntry, JournalOp, JournalSource};
use scenery_mcp::config_edit;
use scenery_mcp::setup::{ClientConfig, ConfigFormat, MCP_SERVER_NAME};
use std::path::{Path, PathBuf};

/// One config file to clean, with the brokered keys known to live in it.
struct Target {
    client: ClientConfig,
    /// `(entry name, index into plan.brokered)`.
    brokered: Vec<(String, usize)>,
}

/// What happened to one file.
#[derive(Default)]
struct FileOutcome {
    /// Brokered entry names that were really removed.
    removed_brokered: Vec<String>,
    /// Entries still in the file afterwards, excluding `scenery`.
    remaining: Vec<String>,
}

/// Clean every client config of Scenery's entries and fill the REMOVED,
/// KEPT and RESTORED sections plus any failures in `report`.
pub(super) fn strip_client_configs(
    plan: &Plan,
    journal_entries: &[JournalEntry],
    report: &mut RestoreReport,
) {
    let mut targets = build_targets(plan, journal_entries);
    let mut handled_rows = vec![false; plan.brokered.len()];
    let mut kept = Vec::new();

    for target in &mut targets {
        let setup_entry = latest_setup_entry(journal_entries, &target.client.config_path);
        let outcome = clean_target(target, setup_entry, report);
        for name in &outcome.removed_brokered {
            let Some((_, idx)) = target.brokered.iter().find(|(k, _)| k == name) else {
                continue;
            };
            handled_rows[*idx] = true;
            record_removed(
                report,
                &plan.brokered[*idx],
                RemovedKind::BrokeredMcp,
                &target.client.name,
            );
        }
        for name in outcome.remaining {
            kept.push(KeptItem {
                name,
                location: target.client.name.clone(),
                note: "your own server — left in place, no longer audited by Scenery".into(),
            });
        }
    }
    report.kept = kept;

    // A brokered server that lives only in state (never written to a config,
    // or already gone) is still listed so the user knows how to re-add it.
    for (idx, row) in plan.brokered.iter().enumerate() {
        if !handled_rows[idx] && !report.removed.iter().any(|r| r.name == row.mcp_server_name) {
            record_removed(report, row, RemovedKind::BrokeredMcp, "(state only)");
        }
    }
}

/// Entries that `mcp setup` journaled for `path`, newest last.
fn latest_setup_entry<'a>(entries: &'a [JournalEntry], path: &Path) -> Option<&'a JournalEntry> {
    let target = path.to_string_lossy();
    entries.iter().rev().find(|e| {
        e.op == JournalOp::ConfigEdit
            && e.target_path == target
            && e.slug.as_deref() == Some(MCP_SERVER_NAME)
    })
}

/// Every client file plus the brokered keys to strip from each.
///
/// Journal-recorded brokered keys apply to their own file. For installs the
/// journal does not cover, the backend id is tried as a key in every file.
fn build_targets(plan: &Plan, journal_entries: &[JournalEntry]) -> Vec<Target> {
    let mut targets: Vec<Target> = plan
        .clients
        .iter()
        .map(|c| Target {
            client: ClientConfig {
                name: c.name.clone(),
                config_path: c.config_path.clone(),
                mcp_key: c.mcp_key.clone(),
                already_configured: c.already_configured,
                format: c.format,
            },
            brokered: Vec::new(),
        })
        .collect();

    let mut journaled = vec![false; plan.brokered.len()];
    for (idx, row) in plan.brokered.iter().enumerate() {
        let Some(entry) = latest_brokered_entry(journal_entries, &row.installation_id) else {
            continue;
        };
        let key = entry
            .detail
            .get("server_key")
            .and_then(|v| v.as_str())
            .unwrap_or(row.mcp_server_id.as_str())
            .to_string();
        let mcp_key = entry
            .detail
            .get("mcp_key")
            .and_then(|v| v.as_str())
            .unwrap_or("mcpServers");
        let path = PathBuf::from(&entry.target_path);
        let pos = targets
            .iter()
            .position(|t| t.client.config_path == path && t.client.mcp_key == mcp_key);
        let pos = match pos {
            Some(p) => p,
            None => {
                targets.push(Target {
                    client: ClientConfig {
                        name: entry
                            .client
                            .clone()
                            .unwrap_or_else(|| entry.target_path.clone()),
                        config_path: path,
                        mcp_key: mcp_key.to_string(),
                        already_configured: false,
                        format: ConfigFormat::Json,
                    },
                    brokered: Vec::new(),
                });
                targets.len() - 1
            }
        };
        targets[pos].brokered.push((key, idx));
        journaled[idx] = true;
    }

    // Fallback only for rows whose journal key is not actually in its file.
    for (idx, row) in plan.brokered.iter().enumerate() {
        if journaled[idx] && journal_key_present(&targets, idx) {
            continue;
        }
        for t in &mut targets {
            t.brokered.push((row.mcp_server_id.clone(), idx));
        }
    }
    targets
}

/// `true` if row `idx`'s journal-recorded key is in its target file now.
fn journal_key_present(targets: &[Target], idx: usize) -> bool {
    targets.iter().any(|t| {
        t.brokered.iter().any(|(key, i)| {
            *i == idx
                && std::fs::read_to_string(&t.client.config_path)
                    .ok()
                    .and_then(|text| {
                        config_edit::entry_names(t.client.format, &text, &t.client.mcp_key).ok()
                    })
                    .is_some_and(|names| names.contains(key))
        })
    })
}

/// Most recent journal entry recording a brokered MCP push for the install
/// whose backend row ID is `installation_id`.
pub(super) fn latest_brokered_entry<'a>(
    entries: &'a [JournalEntry],
    installation_id: &str,
) -> Option<&'a JournalEntry> {
    entries.iter().rev().find(|e| {
        e.source == JournalSource::Brokered
            && e.detail.get("installation_id").and_then(|v| v.as_str()) == Some(installation_id)
    })
}

/// Clean one file. Failures go to `report` and leave the file untouched.
fn clean_target(
    target: &Target,
    setup_entry: Option<&JournalEntry>,
    report: &mut RestoreReport,
) -> FileOutcome {
    let client = &target.client;
    let path = &client.config_path;
    if !path.exists() {
        return FileOutcome::default();
    }
    let mut outcome = FileOutcome::default();
    match try_clean(target, setup_entry, report, &mut outcome) {
        Ok(()) => {
            outcome.remaining = remaining_servers(target, report);
        }
        Err(e) => fail(report, client, &format!("{e:#}")),
    }
    outcome
}

fn fail(report: &mut RestoreReport, client: &ClientConfig, why: &str) {
    let msg = format!(
        "{}: could not restore {} — left untouched: {why}",
        client.name,
        client.config_path.display()
    );
    report.warnings.push(msg.clone());
    report.failed.push(msg);
}

/// Names still in the file after cleaning, minus `scenery`. A leftover
/// Scenery or brokered entry is a failure, not a quiet success.
fn remaining_servers(target: &Target, report: &mut RestoreReport) -> Vec<String> {
    let client = &target.client;
    let Ok(text) = std::fs::read_to_string(&client.config_path) else {
        return Vec::new(); // deleted: nothing remains
    };
    let names = config_edit::entry_names(client.format, &text, &client.mcp_key).unwrap_or_default();
    let mut left = Vec::new();
    for name in names {
        let ours = name == MCP_SERVER_NAME || target.brokered.iter().any(|(k, _)| *k == name);
        if ours {
            fail(
                report,
                client,
                &format!("entry '{name}' is still present after the edit"),
            );
        } else {
            left.push(name);
        }
    }
    left
}

fn try_clean(
    target: &Target,
    setup_entry: Option<&JournalEntry>,
    report: &mut RestoreReport,
    outcome: &mut FileOutcome,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let client = &target.client;
    let path = &client.config_path;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(bytes.clone()).context("file is not valid UTF-8")?;

    if let Some(entry) = setup_entry {
        if try_shortcut(target, entry, &bytes, report)? {
            return Ok(());
        }
    }

    // Surgical: strip our keys in place.
    let mut edited = text.clone();
    let mut removed_any = false;
    let keys = std::iter::once(MCP_SERVER_NAME.to_string())
        .chain(target.brokered.iter().map(|(k, _)| k.clone()));
    for key in keys {
        if let Some(next) =
            config_edit::remove_entry(client.format, &edited, &client.mcp_key, &key)?
        {
            edited = next;
            removed_any = true;
            if key != MCP_SERVER_NAME {
                outcome.removed_brokered.push(key);
            }
        }
    }

    let created_container = setup_entry
        .and_then(|e| e.detail.get("mcp_key_created"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if created_container {
        edited = config_edit::prune_empty_container(client.format, &edited, &client.mcp_key)?;
    }

    let scenery_created = setup_entry.is_some_and(created_by_scenery);
    if scenery_created && config_edit::is_empty_config(client.format, &edited, &client.mcp_key)? {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        report.restored.push(RestoredItem {
            client: client.name.clone(),
            name: "config file".into(),
            restored_to: path.display().to_string(),
            from_backup: "removed: Scenery created it and nothing else was in it".into(),
        });
        return Ok(());
    }

    if edited != text {
        scenery_mcp::setup::write_client_config(path, &edited)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if removed_any {
        report.restored.push(RestoredItem {
            client: client.name.clone(),
            name: "scenery entry".into(),
            restored_to: path.display().to_string(),
            from_backup: "removed in place; the rest of the file was not touched".into(),
        });
    }
    Ok(())
}

/// Did `mcp setup` create this file (it did not exist before)? Entries from
/// before the journal recorded this fall back to "no backup means created".
fn created_by_scenery(entry: &JournalEntry) -> bool {
    match entry.detail.get("file_existed").and_then(|v| v.as_bool()) {
        Some(existed) => !existed,
        None => entry.backup_path.is_none(),
    }
}

/// Full-backup restore, only when the live file is byte-identical to what
/// setup wrote and the backup holds none of our keys. `Ok(true)` if done.
fn try_shortcut(
    target: &Target,
    entry: &JournalEntry,
    live: &[u8],
    report: &mut RestoreReport,
) -> anyhow::Result<bool> {
    use anyhow::Context;
    let client = &target.client;
    let path = &client.config_path;
    let Some(expected) = entry
        .detail
        .get("post_setup_sha256")
        .and_then(|v| v.as_str())
    else {
        return Ok(false);
    };
    if sha256_hex(live) != expected {
        return Ok(false);
    }

    let Some(backup) = entry.backup_path.as_deref() else {
        if !created_by_scenery(entry) {
            return Ok(false);
        }
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        report.restored.push(RestoredItem {
            client: client.name.clone(),
            name: "config file".into(),
            restored_to: path.display().to_string(),
            from_backup: "removed: Scenery created it and it was unchanged".into(),
        });
        return Ok(true);
    };

    let Ok(backup_text) = std::fs::read_to_string(backup) else {
        return Ok(false); // no usable backup: go surgical
    };
    let names = config_edit::entry_names(client.format, &backup_text, &client.mcp_key)
        .unwrap_or_else(|_| vec![MCP_SERVER_NAME.to_string()]);
    let dirty = names
        .iter()
        .any(|n| n == MCP_SERVER_NAME || target.brokered.iter().any(|(k, _)| k == n));
    if dirty {
        return Ok(false);
    }
    scenery_mcp::setup::write_client_config(path, &backup_text)
        .with_context(|| format!("restoring {}", path.display()))?;
    report.restored.push(RestoredItem {
        client: client.name.clone(),
        name: "config file".into(),
        restored_to: path.display().to_string(),
        from_backup: format!("{backup} (file unchanged since setup, restored byte for byte)"),
    });
    Ok(true)
}

/// Names Scenery will strip, for the dry-run plan: brokered keys the journal
/// or the backend ids point at.
pub(super) fn brokered_keys(plan: &Plan, journal_entries: &[JournalEntry]) -> Vec<String> {
    let mut keys = Vec::new();
    for row in &plan.brokered {
        keys.push(row.mcp_server_id.clone());
        if let Some(entry) = latest_brokered_entry(journal_entries, &row.installation_id) {
            if let Some(k) = entry.detail.get("server_key").and_then(|v| v.as_str()) {
                keys.push(k.to_string());
            }
        }
    }
    keys
}
