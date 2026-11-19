//! Local edits as versions (plan 4.5, AUTO-MANAGE Phase 9a).
//!
//! The drift scan hashes a managed skill's whole tree. When the tree no longer
//! matches its baseline, [`upload_edit`] sends it to the registry as a new
//! private version (an own skill) or a branch (a catalog skill), then makes the
//! edit the new baseline so it is neither re-uploaded nor reported again.
//!
//! Who owns the edit decides what it is: the backend's snapshot says `own` or
//! `catalog` per install ([`note_snapshot`]), recorded in `skill_local_state`;
//! without a snapshot row an auto-managed marker means `own`, anything else
//! `catalog`. The server has the last word: its answer's `authorship` is what is
//! recorded.
//!
//! Never blocks the user: a scan verdict does not matter here (the backend alerts
//! the user and IT). A tree holding a secret pattern is not uploaded; its hash is
//! remembered (`held_tree`) so the same tree is not retried or re-reported. An
//! upload that fails (offline, 5xx) leaves everything as it was and is retried on
//! the next scan; so does an edit still being typed (newest mtime within the
//! settle window, default 30 s, `SCENERY_LOCAL_EDIT_SETTLE_SECS`), which is the
//! debounce: offline edits collapse to the newest tree with the base version
//! kept.
//!
//! Own skill: the install's pinned version moves to the uploaded one (the backend
//! does it; here the local `installed_skills.active_version` follows) so the next
//! snapshot is already satisfied and the registry copy never overwrites the edit.

use super::marker::{self, ManagedPathMarker, Provenance};
use rusqlite::{Connection, OptionalExtension};
use scenery_core::{
    pack,
    registry::{
        AuthoringReply, BranchReport, LocalVersionRequest, LocalVersionResponse, RegistryClient,
    },
    skill_tree,
    state::AppState,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tracing::{debug, info, warn};

pub const AUTHORSHIP_OWN: &str = "own";
pub const AUTHORSHIP_CATALOG: &str = "catalog";
pub const BRANCH_NONE: &str = "none";
pub const BRANCH_OWN: &str = "own";
pub const BRANCH_BRANCH: &str = "branch";

const ENV_SETTLE: &str = "SCENERY_LOCAL_EDIT_SETTLE_SECS";
const DEFAULT_SETTLE_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorship {
    Own,
    Catalog,
}

impl Authorship {
    pub fn as_str(self) -> &'static str {
        match self {
            Authorship::Own => AUTHORSHIP_OWN,
            Authorship::Catalog => AUTHORSHIP_CATALOG,
        }
    }
}

/// One `skill_local_state` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalState {
    pub authorship: Option<String>,
    pub branch_state: String,
    pub base_version: Option<String>,
    pub local_version: Option<String>,
    pub local_version_id: Option<String>,
    pub uploaded_tree: Option<String>,
    pub held_tree: Option<String>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn load(conn: &Connection, skill_id: &str) -> Option<LocalState> {
    conn.query_row(
        "SELECT authorship, branch_state, base_version, local_version, local_version_id, \
                uploaded_tree, held_tree \
         FROM skill_local_state WHERE skill_id = ?1",
        [skill_id],
        |r| {
            Ok(LocalState {
                authorship: r.get(0)?,
                branch_state: r.get(1)?,
                base_version: r.get(2)?,
                local_version: r.get(3)?,
                local_version_id: r.get(4)?,
                uploaded_tree: r.get(5)?,
                held_tree: r.get(6)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Record the snapshot's `authorship` (and, when the backend still holds a
/// branch for this device, its `branch_state` / `base_version`) for a skill.
/// Does not touch the upload bookkeeping.
pub fn note_snapshot(
    conn: &Connection,
    skill_id: &str,
    authorship: Option<&str>,
    branch_state: Option<&str>,
    base_version: Option<&str>,
) {
    let Some(a) = authorship.filter(|a| matches!(*a, AUTHORSHIP_OWN | AUTHORSHIP_CATALOG)) else {
        return;
    };
    let res = conn.execute(
        "INSERT INTO skill_local_state (skill_id, authorship, branch_state, base_version, updated_at) \
         VALUES (?1, ?2, COALESCE(?3, 'none'), ?4, ?5) \
         ON CONFLICT(skill_id) DO UPDATE SET authorship = excluded.authorship, \
           base_version = CASE WHEN skill_local_state.branch_state = 'branch' \
             THEN skill_local_state.base_version \
             ELSE COALESCE(excluded.base_version, skill_local_state.base_version) END, \
           updated_at = excluded.updated_at",
        rusqlite::params![skill_id, a, branch_state, base_version, now_secs()],
    );
    if let Err(e) = res {
        debug!(skill_id, error = %e, "local-edits: could not record authorship");
    }
}

/// Own or catalog, for the skill with this registry id and marker provenance.
pub fn authorship(conn: &Connection, skill_id: &str, prov: &Provenance) -> Authorship {
    match load(conn, skill_id).and_then(|s| s.authorship).as_deref() {
        Some(AUTHORSHIP_OWN) => Authorship::Own,
        Some(AUTHORSHIP_CATALOG) => Authorship::Catalog,
        _ if prov.is_auto_managed() => Authorship::Own,
        _ => Authorship::Catalog,
    }
}

/// The registry id for a marker row: its provenance id, else the registry name
/// recorded for the local folder, else the folder name.
pub fn registry_id_for(state: &AppState, marker: &ManagedPathMarker, prov: &Provenance) -> String {
    prov.skill_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| state.registry_name_for_local(&marker.slug))
        .unwrap_or_else(|| marker.slug.clone())
}

/// The skill is a plain registry version again (restored or reinstalled): forget
/// any branch and report `none`. `new_base` is the registry version now on the
/// device: the next edit starts from it, not from the old branch's base (`None`
/// falls back to the installed version at upload time).
pub fn reset_branch(conn: &Connection, skill_id: &str, new_base: Option<&str>) {
    let _ = conn.execute(
        "UPDATE skill_local_state SET branch_state = 'none', local_version = NULL, \
         local_version_id = NULL, uploaded_tree = NULL, held_tree = NULL, \
         base_version = ?3, updated_at = ?2 \
         WHERE skill_id = ?1",
        rusqlite::params![skill_id, now_secs(), new_base],
    );
}

/// Is the live tree at `dir` different from the marker baseline at `path`?
/// Used before any install or push over an existing managed skill dir, so an
/// edit that is still settling, offline or held is never overwritten (plan
/// 4.5.1). A tree that cannot be hashed counts as dirty: refusing to overwrite
/// what cannot be verified is the safe side. Markers without a v2 tree baseline
/// are not judged here (the drift scan re-baselines them).
pub fn tree_is_dirty(conn: &Connection, path: &str, dir: &Path) -> bool {
    let prov = marker::marker_provenance(conn, path);
    let (Some(v), Some(baseline)) = (prov.hash_version, prov.tree_hash.as_deref()) else {
        return false;
    };
    if v < marker::HASH_VERSION_TREE || !dir.is_dir() {
        return false;
    }
    match skill_tree::survey(dir) {
        Ok(s) => s.tree_hash != baseline,
        Err(e) => {
            warn!(path, error = %e, "local-edits: cannot hash the tree; treating it as edited");
            true
        }
    }
}

/// The marker path of the managed skill with this local slug, if any.
pub fn marker_path_for_slug(conn: &Connection, slug: &str) -> Option<String> {
    conn.query_row(
        "SELECT path FROM managed_path_markers WHERE kind = 'skill' AND slug = ?1 LIMIT 1",
        [slug],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

const ENV_STRICT_GRACE: &str = "SCENERY_STRICT_UPLOAD_GRACE_SECS";
const DEFAULT_STRICT_GRACE_SECS: i64 = 24 * 3600;

/// How long a strict-mode edit may stay unquarantined while its upload keeps
/// failing for a transient reason (default 24 h).
pub fn strict_upload_grace_secs() -> i64 {
    std::env::var(ENV_STRICT_GRACE)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_STRICT_GRACE_SECS)
}

fn first_fail_key(path: &str) -> String {
    format!("edit_upload_first_fail:{path}")
}

/// Record the first failed upload attempt for the skill dir at `path` (kept if
/// already set) and return its unix time.
pub fn note_first_upload_failure(state: &AppState, path: &str) -> i64 {
    let key = first_fail_key(path);
    if let Some(t) = state
        .get_sync_state(&key)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
    {
        return t;
    }
    let now = now_secs();
    let _ = state.set_sync_state(&key, &now.to_string());
    now
}

pub fn clear_first_upload_failure(state: &AppState, path: &str) {
    let _ = state.delete_sync_state(&first_fail_key(path));
}

/// Has this skill's upload been failing for longer than the grace window?
pub fn upload_grace_expired(state: &AppState, path: &str) -> bool {
    now_secs() - note_first_upload_failure(state, path) >= strict_upload_grace_secs()
}

const BLOCKED_PREFIX: &str = "update_blocked:";

/// A catalog update was skipped for `slug` because its tree has edits that are
/// not uploaded yet. Shown by the drift report and `scenery doctor`.
pub fn mark_update_blocked(state: &AppState, slug: &str) {
    let _ = state.set_sync_state(&format!("{BLOCKED_PREFIX}{slug}"), &now_secs().to_string());
}

pub fn clear_update_blocked(state: &AppState, slug: &str) {
    let _ = state.delete_sync_state(&format!("{BLOCKED_PREFIX}{slug}"));
}

/// Slugs whose catalog update is currently blocked by unuploaded local edits.
pub fn update_blocked_slugs(state: &AppState) -> Vec<String> {
    let Ok(conn) = Connection::open(&state.db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT key FROM sync_state WHERE key LIKE 'update_blocked:%'")
    else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| r.get::<_, String>(0));
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|k| k.strip_prefix(BLOCKED_PREFIX).map(str::to_string))
        .collect()
}

fn settle_window() -> Duration {
    Duration::from_secs(
        std::env::var(ENV_SETTLE)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SETTLE_SECS),
    )
}

/// What [`upload_edit`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// Stored as a version / branch; the tree is the new baseline.
    Stored {
        authorship: Authorship,
        branch_state: String,
        version: Option<String>,
        conflict: bool,
        update_available: bool,
        scan_verdict: Option<String>,
    },
    /// Still being edited (newest mtime inside the settle window): next scan.
    Settling,
    /// Not uploaded and not retried until the tree changes (a secret pattern
    /// was found, or the registry refused it for good). Names only.
    Held { reason: String },
    /// Could not reach the registry or not signed in (offline, 5xx, 401, 426):
    /// transient, retried next scan.
    Retry { reason: String },
    /// This tree can never be uploaded as it is (cannot be hashed, packed or
    /// secret-scanned). Not retried by waiting.
    Failed { reason: String },
    /// This exact tree was already uploaded (strict mode keeps the old
    /// baseline, so the same edit is seen again): nothing to send.
    AlreadyUploaded,
}

/// Upload the current tree of `marker`'s skill as a local version.
///
/// `force` skips the settle window (strict-mode catalog edits, which are about
/// to be quarantined and must be uploaded first). On [`EditOutcome::Stored`] the
/// tree is already the marker's new baseline.
pub async fn upload_edit(
    state: &Arc<AppState>,
    registry_url: &str,
    marker: &ManagedPathMarker,
    force: bool,
) -> EditOutcome {
    upload_edit_with(state, registry_url, marker, force, true).await
}

/// Strict-mode variant (plan 4.5.1): saves the edit privately but does NOT move
/// the marker baseline, so the tree stays drifted until IT resolves it or the
/// edit is restored. `uploaded_tree` records what was sent, so the same tree is
/// not uploaded again ([`EditOutcome::AlreadyUploaded`]).
pub async fn upload_edit_keep_baseline(
    state: &Arc<AppState>,
    registry_url: &str,
    marker: &ManagedPathMarker,
) -> EditOutcome {
    upload_edit_with(state, registry_url, marker, true, false).await
}

async fn upload_edit_with(
    state: &Arc<AppState>,
    registry_url: &str,
    marker: &ManagedPathMarker,
    force: bool,
    rebaseline: bool,
) -> EditOutcome {
    let (st, mk) = (Arc::clone(state), marker.clone());
    let prep = tokio::task::spawn_blocking(move || prepare(&st, &mk, force, rebaseline)).await;
    let prep = match prep {
        Ok(p) => p,
        Err(e) => {
            return EditOutcome::Retry {
                reason: format!("hash task failed: {e}"),
            }
        }
    };
    let prep = match prep {
        Ok(p) => p,
        Err(done) => return done,
    };

    let token = scenery_core::auth::load_tokens(state, registry_url)
        .ok()
        .flatten()
        .map(|t| t.access_token)
        .filter(|t| !t.is_empty());
    let device_id = state.get_sync_state("device_id").ok().flatten();
    let (Some(token), Some(device_id)) = (token, device_id) else {
        return EditOutcome::Retry {
            reason: "not signed in or not paired".into(),
        };
    };

    let (url, rid, th, base, dev) = (
        registry_url.to_string(),
        prep.skill_id.clone(),
        prep.tree_hash.clone(),
        prep.base_version.clone(),
        device_id,
    );
    let packed = prep.tar_gz;
    let reply = tokio::task::spawn_blocking(move || {
        RegistryClient::new(url)
            .with_auth(token)
            .upload_local_version(LocalVersionRequest {
                tar_gz: packed,
                skill_id: &rid,
                tree_hash: &th,
                device_id: &dev,
                base_version: base.as_deref(),
            })
    })
    .await;
    let reply = match reply {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return EditOutcome::Retry {
                reason: format!("{e:#}"),
            }
        }
        Err(e) => {
            return EditOutcome::Retry {
                reason: format!("upload task failed: {e}"),
            }
        }
    };
    let resp = match reply {
        AuthoringReply::Done(r) => r,
        AuthoringReply::Refused(r) => {
            // 401 / 426 / 5xx-like conditions are transient for this purpose;
            // a 404 (skill unknown to the registry) will not heal by waiting.
            return match r.status {
                404 | 422 => hold(state, &prep.skill_id, &prep.tree_hash, &r.code),
                _ => EditOutcome::Retry {
                    reason: format!("{} ({})", r.code, r.status),
                },
            };
        }
    };
    match resp.outcome.as_str() {
        "stored" => {
            finish_stored(
                state,
                registry_url,
                marker,
                &prep.skill_id,
                &prep.tree_hash,
                resp,
                rebaseline,
            )
            .await
        }
        other => hold(
            state,
            &prep.skill_id,
            &prep.tree_hash,
            resp.reason.as_deref().unwrap_or(other),
        ),
    }
}

struct Prepared {
    skill_id: String,
    tree_hash: String,
    base_version: Option<String>,
    tar_gz: Vec<u8>,
}

fn hold(state: &AppState, skill_id: &str, tree_hash: &str, reason: &str) -> EditOutcome {
    if let Ok(conn) = Connection::open(&state.db_path) {
        let _ = conn.execute(
            "INSERT INTO skill_local_state (skill_id, held_tree, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(skill_id) DO UPDATE SET held_tree = excluded.held_tree, updated_at = excluded.updated_at",
            rusqlite::params![skill_id, tree_hash, now_secs()],
        );
    }
    warn!(skill_id, reason, "local-edits: edit not uploaded");
    EditOutcome::Held {
        reason: reason.to_string(),
    }
}

/// Blocking half: survey, settle/held/already-uploaded checks, secret pre-check,
/// pack. `Err` is an early answer.
fn prepare(
    state: &AppState,
    marker: &ManagedPathMarker,
    force: bool,
    rebaseline: bool,
) -> Result<Prepared, EditOutcome> {
    let dir = Path::new(&marker.path);
    let conn = Connection::open(&state.db_path).map_err(|e| EditOutcome::Retry {
        reason: format!("state DB: {e}"),
    })?;
    let prov = marker::marker_provenance(&conn, &marker.path);
    let skill_id = registry_id_for(state, marker, &prov);
    let survey = skill_tree::survey(dir).map_err(|e| EditOutcome::Failed {
        reason: format!("could not hash the tree: {e:#}"),
    })?;
    let tree_hash = survey.tree_hash.clone();
    let local = load(&conn, &skill_id).unwrap_or_default();

    if local.held_tree.as_deref() == Some(tree_hash.as_str()) {
        return Err(EditOutcome::Held {
            reason: "unchanged since it was held".into(),
        });
    }
    if !rebaseline && local.uploaded_tree.as_deref() == Some(tree_hash.as_str()) {
        return Err(EditOutcome::AlreadyUploaded);
    }
    if !force && survey.modified_within(SystemTime::now(), settle_window()) {
        return Err(EditOutcome::Settling);
    }
    // 4.6.2: nothing with a secret in it is ever uploaded.
    match skill_tree::scan_secrets(dir) {
        Ok(hits) if !hits.is_empty() => {
            return Err(hold(state, &skill_id, &tree_hash, "secrets_detected"));
        }
        Ok(_) => {}
        Err(e) => {
            return Err(EditOutcome::Failed {
                reason: format!("secret scan failed: {e:#}"),
            })
        }
    }
    let tar_gz = pack::pack_dir_to_tar_gz_no_links(dir).map_err(|e| EditOutcome::Failed {
        reason: format!("could not pack the tree: {e:#}"),
    })?;
    // The version the edit started from: what the last upload (or install) left.
    let base_version = local.base_version.clone().or_else(|| {
        conn.query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
            [&skill_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    });
    Ok(Prepared {
        skill_id,
        tree_hash,
        base_version,
        tar_gz,
    })
}

async fn finish_stored(
    state: &Arc<AppState>,
    registry_url: &str,
    marker: &ManagedPathMarker,
    skill_id: &str,
    tree_hash: &str,
    resp: LocalVersionResponse,
    rebaseline: bool,
) -> EditOutcome {
    let auth = if resp.authorship == AUTHORSHIP_OWN {
        Authorship::Own
    } else {
        Authorship::Catalog
    };
    let branch_state = if resp.branch_state.is_empty() {
        match auth {
            Authorship::Own => BRANCH_OWN.to_string(),
            Authorship::Catalog => BRANCH_BRANCH.to_string(),
        }
    } else {
        resp.branch_state.clone()
    };
    let version = resp.version.clone();

    let (st, mk, d, sid, th, bs, rv) = (
        Arc::clone(state),
        marker.clone(),
        PathBuf::from(&marker.path),
        skill_id.to_string(),
        tree_hash.to_string(),
        branch_state.clone(),
        resp.clone(),
    );
    let installation_id = tokio::task::spawn_blocking(move || -> Option<String> {
        let conn = Connection::open(&st.db_path).ok()?;
        // The marker baseline moves to what was uploaded (tree hash + SKILL.md
        // hash): the edit is now accounted for.
        let md = std::fs::read(d.join("SKILL.md"))
            .map(|b| hex::encode(Sha256::digest(&b)))
            .unwrap_or_else(|_| mk.source_sha256.clone());
        // The tree may have changed again while uploading: baseline what was
        // actually uploaded, so the newer edit is seen next scan.
        if rebaseline {
            if let Err(e) = marker::set_tree_baseline(&conn, &mk.path, &th, &md) {
                warn!(skill_id = %sid, error = %e, "local-edits: could not re-baseline the marker");
            }
        }
        let base = match rv.authorship.as_str() {
            AUTHORSHIP_OWN => rv.version.clone(),
            _ => load(&conn, &sid)
                .and_then(|s| s.base_version)
                .or_else(|| rv.head_version.clone().filter(|_| false))
                .or_else(|| {
                    conn.query_row(
                        "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
                        [&sid],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                }),
        };
        let _ = conn.execute(
            "INSERT INTO skill_local_state \
               (skill_id, authorship, branch_state, base_version, local_version, local_version_id, uploaded_tree, held_tree, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8) \
             ON CONFLICT(skill_id) DO UPDATE SET authorship = excluded.authorship, \
               branch_state = excluded.branch_state, base_version = excluded.base_version, \
               local_version = excluded.local_version, local_version_id = excluded.local_version_id, \
               uploaded_tree = excluded.uploaded_tree, held_tree = NULL, updated_at = excluded.updated_at",
            rusqlite::params![
                sid,
                rv.authorship,
                bs,
                base,
                rv.version,
                rv.version_id,
                th,
                now_secs()
            ],
        );
        // An own skill's pin moved server-side to the uploaded version; follow it
        // locally so the snapshot is satisfied and nothing is re-downloaded.
        if rebaseline && rv.authorship == AUTHORSHIP_OWN {
            if let Some(v) = rv.version.as_deref() {
                let _ = conn.execute(
                    "UPDATE installed_skills SET active_version = ?1 WHERE skill_id = ?2",
                    rusqlite::params![v, sid],
                );
            }
        }
        conn.query_row(
            "SELECT installation_id FROM managed_path_markers WHERE path = ?1",
            [&mk.path],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .or_else(|| {
            conn.query_row(
                "SELECT installation_id FROM installed_skills WHERE skill_id = ?1",
                [&sid],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
    })
    .await
    .ok()
    .flatten();

    // Tell the registry what this device now runs (best effort; the upload
    // already recorded it server-side).
    report_branch(state, registry_url, installation_id, skill_id).await;

    info!(
        skill_id,
        ?version,
        branch_state,
        conflict = resp.conflict,
        update_available = resp.update_available,
        verdict = ?resp.scan.verdict,
        "local-edits: edit uploaded as a private version"
    );
    EditOutcome::Stored {
        authorship: auth,
        branch_state,
        version,
        conflict: resp.conflict,
        update_available: resp.update_available,
        scan_verdict: resp.scan.verdict,
    }
}

/// `PATCH /api/installations/{id}` with the skill's recorded branch fields.
pub async fn report_branch(
    state: &Arc<AppState>,
    registry_url: &str,
    installation_id: Option<String>,
    skill_id: &str,
) {
    let Some(iid) = installation_id.filter(|i| !i.is_empty()) else {
        return;
    };
    let token = scenery_core::auth::load_tokens(state, registry_url)
        .ok()
        .flatten()
        .map(|t| t.access_token)
        .filter(|t| !t.is_empty());
    let Some(token) = token else { return };
    let ls = Connection::open(&state.db_path)
        .ok()
        .and_then(|c| load(&c, skill_id));
    let Some(ls) = ls else { return };
    let url = registry_url.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let client = RegistryClient::new(url).with_auth(token);
        let res = client.report_branch_state(
            &iid,
            &BranchReport {
                branch_state: if ls.branch_state.is_empty() {
                    BRANCH_NONE
                } else {
                    &ls.branch_state
                },
                base_version: ls.base_version.as_deref(),
                local_version_id: ls.local_version_id.as_deref(),
            },
        );
        if let Err(e) = res {
            debug!(error = %e, "local-edits: branch report failed (the upload already recorded it)");
        }
    })
    .await;
}

#[cfg(test)]
#[path = "local_edits_tests.rs"]
mod tests;
