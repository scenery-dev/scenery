//! Skills tracked in place, and the kill switch that reaches them.
//!
//! A tracked skill (a dev symlink, a git work tree, another installer's skill)
//! is governed but never converted: Scenery uploads a private snapshot once,
//! keeps a `tracked_skills` row, and writes nothing into the skill. What makes
//! it governed is [`neutralize`]: when the skill is revoked (by registry
//! `skill_id`, never by bare slug), or the user or device is revoked, Scenery
//! stops AI clients from loading it WITHOUT deleting or editing the user's
//! content, least invasive first:
//!
//! 1. **Symlink**: remove the link only. The target (the user's repo) is never
//!    touched.
//! 2. **Real directory**: rename `SKILL.md` to `SKILL.md.scenery-disabled`
//!    (clients need `SKILL.md`). A byte-identical copy is saved first under
//!    `restore-backups/<ts>/adopted/<slug>/`, and an existing disabled file is
//!    never overwritten (numbered suffix). Nothing else in the tree changes.
//!
//! The sweep covers EVERY client root (`~/.agents/skills`, `~/.pi/agent/skills`,
//! `~/.claude/skills`, the Antigravity directory, extra scan roots), because a
//! revoked skill is usually also linked into `~/.claude/skills`.
//!
//! Every action is recorded in `tracked_neutralizations`, so unrevoke
//! ([`reconcile`]) and `scenery uninstall` ([`restore_all`]) put the links and
//! `SKILL.md` files back exactly. (A dedicated ledger rather than the restore
//! journal: a generic journal replay of a `FileDelete` without a backup deletes
//! its target, which for a recreated link would be wrong.)
//!
//! [`reconcile`] is the single convergence point. It runs on every discovery
//! tick, after revoke/unrevoke events and after a user/device revoke. If the
//! other installer or the user recreates a revoked skill, the next reconcile
//! neutralizes it again and records `revoked_skill_reappeared`.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use scenery_core::{restore_journal::RestoreJournal, state::AppState};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{info, warn};

use super::foreign::fold_slug;

pub const DISABLED_SUFFIX: &str = ".scenery-disabled";

pub const REASON_SKILL_REVOKED: &str = "skill_revoked";
pub const REASON_USER_REVOKED: &str = "user_revoked";
pub const REASON_DEVICE_REVOKED: &str = "device_revoked";

/// One `tracked_skills` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRow {
    pub path: String,
    pub slug: String,
    pub reason: String,
    pub skill_id: Option<String>,
    pub skill_uuid: Option<String>,
    pub version: Option<String>,
    pub last_tree_hash: Option<String>,
    pub last_upload_at: Option<String>,
    pub neutralized: bool,
    pub neutralized_reason: Option<String>,
}

fn open(state: &AppState) -> Result<Connection> {
    Connection::open(&state.db_path).context("tracked: failed to open state DB")
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedRow> {
    Ok(TrackedRow {
        path: row.get(0)?,
        slug: row.get(1)?,
        reason: row.get(2)?,
        skill_id: row.get(3)?,
        skill_uuid: row.get(4)?,
        version: row.get(5)?,
        last_tree_hash: row.get(6)?,
        last_upload_at: row.get(7)?,
        neutralized: row.get::<_, i64>(8)? != 0,
        neutralized_reason: row.get(9)?,
    })
}

const COLS: &str = "path, slug, reason, skill_id, skill_uuid, version, last_tree_hash, \
                    last_upload_at, neutralized, neutralized_reason";

/// Insert or refresh a tracked skill after a successful `mode=track` upload.
#[allow(clippy::too_many_arguments)]
pub fn upsert(
    state: &AppState,
    path: &str,
    slug: &str,
    reason: &str,
    skill_id: Option<&str>,
    skill_uuid: Option<&str>,
    version: Option<&str>,
    tree_hash: &str,
) -> Result<()> {
    let conn = open(state)?;
    conn.execute(
        "INSERT INTO tracked_skills \
             (path, slug, reason, skill_id, skill_uuid, version, last_tree_hash, last_upload_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(path) DO UPDATE SET slug = excluded.slug, reason = excluded.reason, \
             skill_id = COALESCE(excluded.skill_id, skill_id), \
             skill_uuid = COALESCE(excluded.skill_uuid, skill_uuid), \
             version = COALESCE(excluded.version, version), \
             last_tree_hash = excluded.last_tree_hash, \
             last_upload_at = excluded.last_upload_at",
        rusqlite::params![
            path,
            slug,
            reason,
            skill_id,
            skill_uuid,
            version,
            tree_hash,
            chrono::Utc::now().to_rfc3339()
        ],
    )
    .context("tracked: failed to upsert tracked_skills")?;
    Ok(())
}

pub fn list(state: &AppState) -> Result<Vec<TrackedRow>> {
    let conn = open(state)?;
    let mut stmt = conn
        .prepare(&format!("SELECT {COLS} FROM tracked_skills ORDER BY path"))
        .context("tracked: prepare list")?;
    let rows = stmt
        .query_map([], map_row)
        .context("tracked: list")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("tracked: read rows")?;
    Ok(rows)
}

pub fn get(state: &AppState, path: &str) -> Result<Option<TrackedRow>> {
    let conn = open(state)?;
    conn.query_row(
        &format!("SELECT {COLS} FROM tracked_skills WHERE path = ?1"),
        [path],
        map_row,
    )
    .optional()
    .context("tracked: get")
}

/// Is any tracked skill (neutralized or not) using folder `slug`? The
/// reconciler uses this to skip a registry install of the same slug while the
/// owner tracks it (a registry copy would be pushed over the user's directory).
pub fn is_slug_tracked(state: &AppState, slug: &str) -> bool {
    let folded = fold_slug(slug);
    list(state)
        .map(|rows| rows.iter().any(|r| fold_slug(&r.slug) == folded))
        .unwrap_or(false)
}

// ── Client roots ────────────────────────────────────────────────────────────

/// Every directory a client loads skills from, plus the scan roots.
pub fn client_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add = |p: Option<PathBuf>| {
        if let Some(p) = p {
            if !roots.contains(&p) {
                roots.push(p);
            }
        }
    };
    add(super::paths::agents_skills_dir());
    add(super::paths::claude_skills_link_dir());
    add(dirs::home_dir().map(|h| h.join(".pi").join("agent").join("skills")));
    add(super::pusher::antigravity_skills_link_dir());
    for r in super::discoveries::extra_roots() {
        add(Some(r));
    }
    roots
}

fn resolves_to(entry: &Path, canonical_target: Option<&Path>) -> bool {
    match (fs::canonicalize(entry).ok(), canonical_target) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

// ── Neutralize ──────────────────────────────────────────────────────────────

/// What one [`neutralize`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NeutralizeReport {
    pub links_removed: usize,
    pub skill_md_disabled: usize,
}

impl NeutralizeReport {
    pub fn changed(&self) -> usize {
        self.links_removed + self.skill_md_disabled
    }
}

/// Stop clients from loading tracked skill `row`. Idempotent: a second pass
/// over an already neutralized skill changes nothing. See the module docs.
pub fn neutralize(state: &AppState, row: &TrackedRow, reason: &str) -> Result<NeutralizeReport> {
    let tracked_path = Path::new(&row.path);
    let tracked_canon = fs::canonicalize(tracked_path).ok();
    let folded = fold_slug(&row.slug);
    let journal = RestoreJournal::for_state(state);
    let conn = open(state)?;
    let mut report = NeutralizeReport::default();
    let mut seen: Vec<PathBuf> = Vec::new();

    // Decide everything BEFORE changing anything: removing one link makes a
    // link that pointed through it dangle, and a dangling link no longer
    // resolves to the tracked path.
    enum Act {
        Unlink(PathBuf),
        Disable(PathBuf),
    }
    let mut plan: Vec<Act> = Vec::new();
    for root in client_roots() {
        let Ok(rd) = fs::read_dir(&root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let Ok(meta) = fs::symlink_metadata(&entry) else {
                continue;
            };
            let is_link = meta.file_type().is_symlink();
            let same_name = fold_slug(&name) == folded;
            let is_tracked_entry = entry == tracked_path;
            let link_to_tracked = is_link && resolves_to(&entry, tracked_canon.as_deref());
            if !(same_name || is_tracked_entry || link_to_tracked) || seen.contains(&entry) {
                continue;
            }
            seen.push(entry.clone());

            if is_link {
                // A link is removed only when it IS the tracked entry or
                // resolves to it. A same-name link to somewhere else is a
                // different skill's business.
                if is_tracked_entry || link_to_tracked {
                    plan.push(Act::Unlink(entry));
                }
            } else if meta.is_dir() {
                // A Scenery-managed directory is a different, governed skill
                // that happens to share the name.
                if scenery_mcp::ownership::is_scenery_managed(&entry) {
                    continue;
                }
                if matches!(fs::symlink_metadata(entry.join("SKILL.md")), Ok(m) if m.is_file()) {
                    plan.push(Act::Disable(entry));
                }
            }
        }
    }

    for act in plan {
        match act {
            Act::Unlink(entry) => {
                let target = fs::read_link(&entry)
                    .map(|t| t.to_string_lossy().to_string())
                    .unwrap_or_default();
                fs::remove_file(&entry)
                    .with_context(|| format!("tracked: removing link {}", entry.display()))?;
                record(
                    &conn,
                    &row.path,
                    &entry,
                    "unlink",
                    Some(&target),
                    None,
                    None,
                )?;
                report.links_removed += 1;
                info!(slug = %row.slug, link = %entry.display(), "tracked: removed link to revoked skill (target untouched)");
            }
            Act::Disable(entry) => {
                let skill_md = entry.join("SKILL.md");
                let backup = journal.new_backup_dest("adopted", &row.slug);
                fs::create_dir_all(backup.as_std_path())
                    .with_context(|| format!("tracked: creating backup dir {backup}"))?;
                let backup_file = backup.join("SKILL.md");
                fs::copy(&skill_md, backup_file.as_std_path())
                    .with_context(|| format!("tracked: backing up {}", skill_md.display()))?;
                let disabled = free_disabled_name(&entry);
                // Ledger FIRST (see above).
                let id = record(
                    &conn,
                    &row.path,
                    &entry,
                    "rename",
                    None,
                    Some(backup.as_str()),
                    Some(&disabled.to_string_lossy()),
                )?;
                if let Err(e) = fs::rename(&skill_md, &disabled) {
                    forget(&conn, id);
                    return Err(e)
                        .with_context(|| format!("tracked: disabling {}", skill_md.display()));
                }
                report.skill_md_disabled += 1;
                info!(slug = %row.slug, dir = %entry.display(), "tracked: disabled SKILL.md of revoked skill (original backed up)");
            }
        }
    }

    conn.execute(
        "UPDATE tracked_skills SET neutralized = 1, neutralized_reason = ?1, \
             neutralized_at = COALESCE(neutralized_at, ?2) WHERE path = ?3",
        rusqlite::params![reason, chrono::Utc::now().to_rfc3339(), row.path],
    )
    .context("tracked: marking neutralized")?;

    if report.changed() > 0 {
        scenery_core::audit::write_audit_event_direct(
            &state.db_path,
            &scenery_core::audit::AuditEvent {
                event_type: "tracked_skill_neutralized".into(),
                payload: serde_json::json!({
                    "slug": row.slug,
                    "skill_id": row.skill_id,
                    "reason": reason,
                    "links_removed": report.links_removed,
                    "skill_md_disabled": report.skill_md_disabled,
                }),
            },
        );
    }
    Ok(report)
}

/// `SKILL.md.scenery-disabled`, or the first free `…disabled.N`.
fn free_disabled_name(dir: &Path) -> PathBuf {
    let first = dir.join(format!("SKILL.md{DISABLED_SUFFIX}"));
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    for n in 1.. {
        let p = dir.join(format!("SKILL.md{DISABLED_SUFFIX}.{n}"));
        if fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    unreachable!("an unused disabled-file name always exists")
}

fn forget(conn: &Connection, id: i64) {
    let _ = conn.execute("DELETE FROM tracked_neutralizations WHERE id = ?1", [id]);
}

fn record(
    conn: &Connection,
    tracked_path: &str,
    entry: &Path,
    action: &str,
    link_target: Option<&str>,
    backup: Option<&str>,
    renamed_to: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO tracked_neutralizations \
             (tracked_path, entry_path, action, link_target, backup_path, renamed_to) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            tracked_path,
            entry.to_string_lossy(),
            action,
            link_target,
            backup,
            renamed_to
        ],
    )
    .context("tracked: recording neutralization")?;
    Ok(conn.last_insert_rowid())
}

// ── Restore ─────────────────────────────────────────────────────────────────

/// Undo every recorded neutralization of `row`: recreate links that are still
/// absent, rename `…scenery-disabled` back to `SKILL.md` where no `SKILL.md`
/// has appeared since. Returns the restored entry paths. Never overwrites
/// anything: a recreated `SKILL.md` keeps winning and the disabled copy stays.
pub fn restore(state: &AppState, row: &TrackedRow) -> Result<Vec<PathBuf>> {
    let conn = open(state)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, entry_path, action, link_target, renamed_to \
             FROM tracked_neutralizations WHERE tracked_path = ?1 ORDER BY id DESC",
        )
        .context("tracked: prepare restore")?;
    type Rec = (i64, String, String, Option<String>, Option<String>);
    let recs: Vec<Rec> = stmt
        .query_map([&row.path], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .context("tracked: query restore")?
        .collect::<rusqlite::Result<_>>()
        .context("tracked: read restore rows")?;
    drop(stmt);

    let mut restored = Vec::new();
    for (id, entry, action, link_target, renamed_to) in recs {
        let entry = PathBuf::from(entry);
        let done = match action.as_str() {
            "unlink" => restore_link(&entry, link_target.as_deref()),
            "rename" => restore_skill_md(&entry, renamed_to.as_deref()),
            _ => true,
        };
        if done {
            conn.execute("DELETE FROM tracked_neutralizations WHERE id = ?1", [id])
                .context("tracked: clearing restored record")?;
            restored.push(entry);
        }
    }
    conn.execute(
        "UPDATE tracked_skills SET neutralized = 0, neutralized_reason = NULL, \
             neutralized_at = NULL WHERE path = ?1",
        [&row.path],
    )
    .context("tracked: clearing neutralized flag")?;
    if !restored.is_empty() {
        info!(slug = %row.slug, entries = restored.len(), "tracked: restored neutralized skill");
    }
    Ok(restored)
}

fn restore_link(entry: &Path, target: Option<&str>) -> bool {
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        return true;
    };
    if fs::symlink_metadata(entry).is_ok() {
        warn!(entry = %entry.display(), "tracked: not recreating link, something is there now");
        return true; // nothing to do; do not retry forever
    }
    #[cfg(unix)]
    {
        if let Some(parent) = entry.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match std::os::unix::fs::symlink(target, entry) {
            Ok(()) => true,
            Err(e) => {
                warn!(entry = %entry.display(), error = %e, "tracked: could not recreate link");
                false
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        true
    }
}

fn restore_skill_md(entry: &Path, renamed_to: Option<&str>) -> bool {
    let Some(disabled) = renamed_to.map(PathBuf::from) else {
        return true;
    };
    if fs::symlink_metadata(&disabled).is_err() {
        return true; // already restored or removed by the user
    }
    let skill_md = entry.join("SKILL.md");
    if fs::symlink_metadata(&skill_md).is_ok() {
        warn!(
            dir = %entry.display(),
            "tracked: SKILL.md exists again; leaving the disabled copy beside it"
        );
        return true;
    }
    match fs::rename(&disabled, &skill_md) {
        Ok(()) => true,
        Err(e) => {
            warn!(dir = %entry.display(), error = %e, "tracked: could not restore SKILL.md");
            false
        }
    }
}

/// Restore every neutralized tracked skill (`scenery uninstall`). Returns
/// `(slug, restored entry)` pairs for the report.
pub fn restore_all(state: &AppState) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for row in list(state)? {
        for p in restore(state, &row)? {
            out.push((row.slug.clone(), p));
        }
    }
    Ok(out)
}

// ── Convergence ─────────────────────────────────────────────────────────────

/// Why `row` should be neutralized right now, if at all: the user or device is
/// revoked, or the skill's registry id is revoked in the local cache (whole
/// artifact, or this tracked version). Matching is by `skill_id` only.
pub fn desired_neutralization(state: &AppState, row: &TrackedRow) -> Option<&'static str> {
    if state
        .get_sync_state(scenery_core::auth::USER_REVOKED_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return Some(REASON_USER_REVOKED);
    }
    if state
        .get_sync_state("device_revoked_at")
        .ok()
        .flatten()
        .is_some()
    {
        return Some(REASON_DEVICE_REVOKED);
    }
    let id = row.skill_id.as_deref()?;
    // Fail closed: an unreadable cache is treated as revoked.
    let revoked = scenery_core::revocation::is_revoked(state, "skill", id, row.version.as_deref())
        .unwrap_or(true);
    revoked.then_some(REASON_SKILL_REVOKED)
}

/// Bring every tracked skill in line with the revocation state: neutralize
/// what must be, restore what no longer must be. Idempotent and cheap; called
/// on every discovery tick and after revoke/unrevoke/user-revoke/device-revoke.
pub fn reconcile(state: &AppState) {
    let rows = match list(state) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "tracked: cannot list tracked skills");
            return;
        }
    };
    for row in rows {
        match desired_neutralization(state, &row) {
            Some(reason) => match neutralize(state, &row, reason) {
                Ok(report) => {
                    if row.neutralized && report.changed() > 0 {
                        warn!(slug = %row.slug, "tracked: revoked skill reappeared — neutralized again");
                        scenery_core::audit::write_audit_event_direct(
                            &state.db_path,
                            &scenery_core::audit::AuditEvent {
                                event_type: "revoked_skill_reappeared".into(),
                                payload: serde_json::json!({
                                    "slug": row.slug,
                                    "skill_id": row.skill_id,
                                    "path": row.path,
                                    "reason": reason,
                                }),
                            },
                        );
                    }
                }
                Err(e) => {
                    warn!(slug = %row.slug, error = %e, "tracked: neutralize failed (will retry)")
                }
            },
            None if row.neutralized => {
                if let Err(e) = restore(state, &row) {
                    warn!(slug = %row.slug, error = %e, "tracked: restore failed (will retry)");
                }
            }
            None => {}
        }
    }
}

/// Revocation handler hook: a skill with registry id `skill_id` (at `version`,
/// when the revocation is version-scoped) was revoked. Neutralizes the tracked
/// skills that carry exactly that id. Returns whether anything changed on disk.
pub fn on_skill_revoked(state: &AppState, skill_id: &str, version: Option<&str>) -> bool {
    let rows = match list(state) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "tracked: cannot list tracked skills");
            return false;
        }
    };
    let mut changed = false;
    for row in rows {
        if row.skill_id.as_deref() != Some(skill_id) {
            continue;
        }
        if let (Some(v), Some(have)) = (
            version.map(str::trim).filter(|v| !v.is_empty()),
            &row.version,
        ) {
            if v != have {
                continue;
            }
        }
        match neutralize(state, &row, REASON_SKILL_REVOKED) {
            Ok(r) => changed |= r.changed() > 0,
            Err(e) => warn!(slug = %row.slug, error = %e, "tracked: neutralize on revoke failed"),
        }
    }
    changed
}

#[cfg(test)]
#[path = "tracked_tests.rs"]
mod tests;
