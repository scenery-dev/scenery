//! Auto-manage: upload first, convert last.
//!
//! The discovery scan classifies every skill on the machine
//! ([`super::foreign`]), reports the batch, and the backend answers with a
//! per-item directive (`manage`, `track`, `hold`, `skip`). This module acts on
//! the directives:
//!
//! 1. **Re-check locally, right now.** The skill is re-classified and
//!    re-hashed. A changed tree (the user is editing it) aborts; a tree
//!    modified in the last 10 minutes is deferred. The runner never trusts a
//!    directive over its own classification: the effective treatment is the
//!    most conservative of the two (hold, then track, then manage).
//! 2. **Pack safely.** No symlink may be in an upload at all (the skill is
//!    held), secret patterns hold it, and `.git`, other dot-directories and
//!    secret-named files never travel ([`scenery_core::pack`]).
//! 3. **Upload** (`POST /runner/skills/auto-manage`, user bearer). Nothing on
//!    disk has changed yet, so offline or a backend error just means the next
//!    tick retries, with backoff.
//! 4. **Convert last.** On `managed` + mode `manage`, the registry copy is
//!    installed with the existing transactional conversion (stage, back up
//!    the original, rename-swap, marker): see [`super::conversion`]. On mode
//!    `track` nothing is installed and nothing is written into the skill;
//!    `tracked_skills` records it.
//!
//! The local folder name and the frontmatter `name` are never changed. A
//! private skill's registry id is `<owner_namespace>~<slug>`; the runner
//! records that id against the local folder (`skill_local_names`) so the sync
//! stream, revocations and artifact URLs reach the right folder.
//!
//! Bounded work per tick: at most [`MAX_CONVERSIONS_PER_TICK`] conversions and
//! [`MAX_TRACKED_UPLOADS_PER_TICK`] tracked uploads, so the first scan after
//! an upgrade does not rewrite a whole machine at once.
//!
//! The access token is loaded ONCE per scan tick by the caller and passed in:
//! reading it per skill would hit the OS keychain once per skill.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use scenery_core::{
    registry::{
        AutoManageOutcomeKind, AutoManageReply, AutoManageRequest, AutoManageResponse,
        RegistryClient,
    },
    state::{AppState, AutoManagePolicy},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::discoveries::DiscoveryItem;
use super::foreign::{classify_skill, ClassifyContext, Treatment};
use super::pusher::ManagedPathsPusher;
use super::tracked;

/// `sync_state` key: the owner namespace the backend assigned this user
/// (`<owner_namespace>~<slug>` private ids). Learned from the first response.
pub const OWNER_NAMESPACE_KEY: &str = "auto_manage_owner_namespace";

pub const MAX_CONVERSIONS_PER_TICK: usize = 5;
pub const MAX_TRACKED_UPLOADS_PER_TICK: usize = 10;

/// A terminal answer for an unchanged skill is not retried for this long; a
/// changed tree is always retried. Keeps an admin policy change or a fix on
/// the backend from being masked forever.
const TERMINAL_RETRY_AFTER_SECS: i64 = 24 * 3600;
/// Tracked foreign-installer skills are re-uploaded at most this often.
const FOREIGN_REUPLOAD_MIN_SECS: i64 = 3600;

// ── Wire types ──────────────────────────────────────────────────────────────

/// One per-item instruction from the backend (skills only, runner >= 2.3.0).
#[derive(Debug, Clone, Deserialize)]
pub struct Directive {
    pub discovery_id: String,
    pub source_path: String,
    /// `manage` | `track` | `hold` | `skip`; anything else is ignored.
    pub action: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub tracked_reason: Option<String>,
}

/// The parts of a discovery upload response this module reads. Everything is
/// optional: an older backend sends neither, and the runner then does nothing.
#[derive(Debug, Default, Deserialize)]
pub struct DiscoveryResponse {
    #[serde(default)]
    pub directives: Option<Vec<Directive>>,
    #[serde(default)]
    pub policy: Option<AutoManagePolicy>,
}

// ── Attempts (backoff and "unchanged since the last answer") ─────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus {
    Managed,
    Tracked,
    Held,
    Blocked,
    Revoked,
    Skipped,
    Failed,
}

impl AttemptStatus {
    fn as_str(self) -> &'static str {
        match self {
            AttemptStatus::Managed => "managed",
            AttemptStatus::Tracked => "tracked",
            AttemptStatus::Held => "held",
            AttemptStatus::Blocked => "blocked",
            AttemptStatus::Revoked => "revoked",
            AttemptStatus::Skipped => "skipped",
            AttemptStatus::Failed => "failed",
        }
    }
}

/// One `auto_manage_attempts` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub slug: String,
    pub source_path: String,
    pub last_hash: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub fail_count: i64,
    pub last_try: String,
    pub next_try_at: i64,
}

fn open(state: &AppState) -> Result<Connection> {
    Connection::open(&state.db_path).context("auto-manage: failed to open state DB")
}

pub fn get_attempt(state: &AppState, slug: &str, source_path: &str) -> Option<Attempt> {
    let conn = open(state).ok()?;
    conn.query_row(
        "SELECT slug, source_path, last_hash, status, reason, fail_count, last_try, next_try_at \
         FROM auto_manage_attempts WHERE slug = ?1 AND source_path = ?2",
        [slug, source_path],
        |r| {
            Ok(Attempt {
                slug: r.get(0)?,
                source_path: r.get(1)?,
                last_hash: r.get(2)?,
                status: r.get(3)?,
                reason: r.get(4)?,
                fail_count: r.get(5)?,
                last_try: r.get(6)?,
                next_try_at: r.get(7)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Skills the runner has held back or that the backend refused, for the user
/// (`scenery status`). Newest first.
pub fn attention_list(state: &AppState) -> Vec<Attempt> {
    let Ok(conn) = open(state) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT slug, source_path, last_hash, status, reason, fail_count, last_try, next_try_at \
         FROM auto_manage_attempts WHERE status IN ('held','blocked','failed') \
         ORDER BY last_try DESC",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(Attempt {
            slug: r.get(0)?,
            source_path: r.get(1)?,
            last_hash: r.get(2)?,
            status: r.get(3)?,
            reason: r.get(4)?,
            fail_count: r.get(5)?,
            last_try: r.get(6)?,
            next_try_at: r.get(7)?,
        })
    });
    match rows {
        Ok(it) => it.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Record the outcome of acting on a skill. A `Failed` status bumps the
/// failure count and schedules an exponential backoff (5 min doubling, capped
/// at 6 h); every other status resets the count.
pub fn record_attempt(
    state: &AppState,
    slug: &str,
    source_path: &str,
    hash: &str,
    status: AttemptStatus,
    reason: Option<&str>,
) {
    let Ok(conn) = open(state) else { return };
    let prior = get_attempt(state, slug, source_path)
        .map(|a| a.fail_count)
        .unwrap_or(0);
    let (fails, next) = if status == AttemptStatus::Failed {
        let fails = prior + 1;
        let delay = (300i64 << fails.min(7)).min(6 * 3600);
        (fails, now_unix() + delay)
    } else {
        (0, 0)
    };
    if let Err(e) = conn.execute(
        "INSERT INTO auto_manage_attempts \
             (slug, source_path, last_hash, status, reason, fail_count, last_try, next_try_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(slug, source_path) DO UPDATE SET last_hash = excluded.last_hash, \
             status = excluded.status, reason = excluded.reason, \
             fail_count = excluded.fail_count, last_try = excluded.last_try, \
             next_try_at = excluded.next_try_at",
        rusqlite::params![
            slug,
            source_path,
            hash,
            status.as_str(),
            reason,
            fails,
            chrono::Utc::now().to_rfc3339(),
            next
        ],
    ) {
        warn!(slug, error = %e, "auto-manage: could not record attempt");
    }
}

/// Should the runner act on `slug` at `source_path` now? `false` while a
/// failure is backing off, or while the last answer was terminal for this
/// exact tree (and fresh).
pub fn should_attempt(state: &AppState, slug: &str, source_path: &str, tree_hash: &str) -> bool {
    let Some(a) = get_attempt(state, slug, source_path) else {
        return true;
    };
    if a.last_hash.as_deref() != Some(tree_hash) {
        return true;
    }
    if a.status == "failed" {
        return now_unix() >= a.next_try_at;
    }
    // Terminal for this tree: retry only after it has gone stale.
    let age = chrono::DateTime::parse_from_rfc3339(&a.last_try)
        .map(|t| chrono::Utc::now().signed_duration_since(t).num_seconds())
        .unwrap_or(i64::MAX);
    age >= TERMINAL_RETRY_AFTER_SECS
}

// ── Policy ──────────────────────────────────────────────────────────────────

/// The policy in force: the response's when it carried one (and it is cached),
/// else the last cached one, else `None` (never told).
pub fn effective_policy(
    state: &AppState,
    from_response: Option<AutoManagePolicy>,
) -> Option<AutoManagePolicy> {
    if let Some(p) = from_response {
        if let Err(e) = state.cache_auto_manage_policy(&p) {
            warn!(error = %e, "auto-manage: could not cache policy");
        }
        return Some(p);
    }
    state.cached_auto_manage_policy()
}

/// Skills are auto-managed only when the backend has told us it is on. A
/// backend that never sent a policy (older) means "do nothing".
pub fn skills_enabled(policy: Option<AutoManagePolicy>) -> bool {
    policy.is_some_and(|p| p.auto_manage_skills)
}

// ── Processing ──────────────────────────────────────────────────────────────

/// What one tick's directive processing did, for logging and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickSummary {
    pub managed: usize,
    pub tracked: usize,
    pub held: usize,
    pub blocked: usize,
    pub deferred: usize,
    pub skipped: usize,
    pub failed: usize,
    pub unauthorized: bool,
}

/// Everything directive processing needs. `token` is the already-loaded
/// access token (one keychain read per tick, not per skill).
pub struct TickCtx {
    pub state: Arc<AppState>,
    pub registry_url: String,
    pub token: String,
    pub device_id: String,
    pub classify: ClassifyContext,
}

/// Act on a discovery response's directives. A no-op unless the backend's
/// policy (or the cached one) has `auto_manage_skills = true`; absent
/// `directives` (older backend) also does nothing.
pub async fn process_response(
    ctx: &TickCtx,
    resp: DiscoveryResponse,
    items: &[DiscoveryItem],
) -> TickSummary {
    let policy = effective_policy(&ctx.state, resp.policy);
    let mut summary = TickSummary::default();
    let Some(directives) = resp.directives else {
        debug!("auto-manage: backend sent no directives — doing nothing");
        return summary;
    };
    if !skills_enabled(policy) {
        debug!("auto-manage: auto_manage_skills is off (or unknown) — doing nothing");
        return summary;
    }
    process_directives(ctx, directives, items, &mut summary).await;
    if summary != TickSummary::default() {
        info!(?summary, "auto-manage: tick complete");
    }
    summary
}

async fn process_directives(
    ctx: &TickCtx,
    directives: Vec<Directive>,
    items: &[DiscoveryItem],
    summary: &mut TickSummary,
) {
    let by_path: HashMap<&str, &DiscoveryItem> = items
        .iter()
        .filter(|i| i.kind == "skill")
        .map(|i| (i.source_path.as_str(), i))
        .collect();
    let mut conversions = 0usize;
    let mut tracked_uploads = 0usize;

    for d in directives {
        // Only ever act on a path this very scan found. A directive cannot
        // aim the runner at an arbitrary location.
        let Some(item) = by_path.get(d.source_path.as_str()).copied() else {
            debug!(path = %d.source_path, "auto-manage: directive for a path not in this scan — ignored");
            continue;
        };
        let reported_hash = item
            .local
            .as_ref()
            .map(|l| l.tree_hash.clone())
            .unwrap_or_default();
        match d.action.as_str() {
            "skip" => {
                summary.skipped += 1;
                record_skip(&ctx.state, item, &reported_hash, d.reason.as_deref());
            }
            "hold" => {
                summary.held += 1;
                let reason = d.reason.clone().unwrap_or_else(|| "held_by_backend".into());
                note_held(&ctx.state, item, &reported_hash, &reason);
            }
            "manage" | "track" => {
                let wants_track = d.action == "track";
                if !wants_track && conversions >= MAX_CONVERSIONS_PER_TICK
                    || wants_track && tracked_uploads >= MAX_TRACKED_UPLOADS_PER_TICK
                {
                    debug!(slug = %item.slug, "auto-manage: per-tick cap reached — next tick");
                    continue;
                }
                match act_on(ctx, item, &d, wants_track).await {
                    Acted::Managed => {
                        conversions += 1;
                        summary.managed += 1;
                    }
                    Acted::Tracked => {
                        tracked_uploads += 1;
                        summary.tracked += 1;
                    }
                    Acted::Held => summary.held += 1,
                    Acted::Blocked => summary.blocked += 1,
                    Acted::Deferred => summary.deferred += 1,
                    Acted::Skipped => summary.skipped += 1,
                    Acted::Failed => summary.failed += 1,
                    Acted::Unauthorized => {
                        summary.unauthorized = true;
                        warn!(
                            "auto-manage: backend rejected the session (401) — stopping this tick"
                        );
                        return;
                    }
                }
            }
            other => debug!(
                action = other,
                "auto-manage: unknown directive action — ignored"
            ),
        }
    }
}

/// Skip reasons that must not stick: they say "not now", not "not this tree".
fn skip_is_sticky(reason: Option<&str>) -> bool {
    match reason {
        Some("auto_manage_disabled") => false,
        Some(r) if r.starts_with("status_") => false,
        _ => true,
    }
}

fn record_skip(state: &AppState, item: &DiscoveryItem, hash: &str, reason: Option<&str>) {
    if skip_is_sticky(reason) {
        let status = if reason == Some("revoked") {
            AttemptStatus::Revoked
        } else {
            AttemptStatus::Skipped
        };
        record_attempt(state, &item.slug, &item.source_path, hash, status, reason);
    }
}

/// Remember a held skill (once per tree hash) so the user is told: a warning in
/// the log, an audit event for IT, and a row `scenery status` lists.
pub fn note_held(state: &AppState, item: &DiscoveryItem, hash: &str, reason: &str) {
    let already = get_attempt(state, &item.slug, &item.source_path).is_some_and(|a| {
        a.status == "held"
            && a.last_hash.as_deref() == Some(hash)
            && a.reason.as_deref() == Some(reason)
    });
    if already {
        return;
    }
    warn!(
        slug = %item.slug,
        path = %item.source_path,
        reason,
        "auto-manage: skill HELD — not uploaded and not converted; run `scenery status` for details"
    );
    record_attempt(
        state,
        &item.slug,
        &item.source_path,
        hash,
        AttemptStatus::Held,
        Some(reason),
    );
    scenery_core::audit::write_audit_event_direct(
        &state.db_path,
        &scenery_core::audit::AuditEvent {
            event_type: "skill_held".into(),
            payload: serde_json::json!({
                "slug": item.slug,
                "reason": reason,
            }),
        },
    );
}

enum Acted {
    Managed,
    Tracked,
    Held,
    Blocked,
    Deferred,
    Skipped,
    Failed,
    Unauthorized,
}

/// Most conservative of the local classification and the directive.
fn effective_mode(local: &Treatment, wants_track: bool) -> Result<&'static str, Treatment> {
    match local {
        Treatment::Hold(_) => Err(local.clone()),
        Treatment::Track(_) => Ok("track"),
        Treatment::Convert if wants_track => Ok("track"),
        Treatment::Convert => Ok("manage"),
    }
}

async fn act_on(ctx: &TickCtx, item: &DiscoveryItem, d: &Directive, wants_track: bool) -> Acted {
    let state = &ctx.state;
    let slug = item.slug.clone();
    let source_path = item.source_path.clone();
    let reported_hash = item
        .local
        .as_ref()
        .map(|l| l.tree_hash.clone())
        .unwrap_or_default();

    if let Err(e) = scenery_core::safe_path::validate_single_component("skill slug", &slug) {
        warn!(slug, error = %e, "auto-manage: refusing a slug that is not one path component");
        return Acted::Failed;
    }

    // ── 1. Re-check locally, right now (blocking I/O off the executor). ──
    let (cs, sp, cc) = (slug.clone(), source_path.clone(), ctx.classify.clone());
    let cls =
        match tokio::task::spawn_blocking(move || classify_skill(Path::new(&sp), &cs, &cc)).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "auto-manage: classification task panicked");
                return Acted::Failed;
            }
        };
    if !Path::new(&source_path).exists() {
        debug!(slug, "auto-manage: source is gone — nothing to do");
        return Acted::Skipped;
    }
    // The tree changed since it was reported: the user is working on it.
    if !reported_hash.is_empty() && cls.tree_hash != reported_hash {
        debug!(
            slug,
            "auto-manage: tree changed since it was reported — next tick"
        );
        return Acted::Deferred;
    }
    if cls.recently_modified {
        debug!(
            slug,
            "auto-manage: modified in the last 10 minutes — deferred one tick"
        );
        return Acted::Deferred;
    }
    let mode = match effective_mode(&cls.treatment, wants_track) {
        Ok(m) => m,
        Err(Treatment::Hold(reason)) => {
            note_held(state, item, &cls.tree_hash, &reason);
            return Acted::Held;
        }
        Err(_) => unreachable!("effective_mode only errs with a hold"),
    };
    let tracked_reason = match &cls.treatment {
        Treatment::Track(r) => r.clone(),
        _ => d
            .tracked_reason
            .clone()
            .unwrap_or_else(|| "tracked_by_backend".into()),
    };

    if !should_attempt(state, &slug, &source_path, &cls.tree_hash) {
        debug!(
            slug,
            "auto-manage: unchanged since the last answer / backing off — skipped"
        );
        return Acted::Skipped;
    }
    if mode == "track" && !track_upload_due(state, &source_path, &tracked_reason, &cls.tree_hash) {
        record_attempt(
            state,
            &slug,
            &source_path,
            &cls.tree_hash,
            AttemptStatus::Tracked,
            Some(&tracked_reason),
        );
        return Acted::Skipped;
    }

    // Revocation, local cache, fail closed: a skill whose private registry id is
    // revoked is never uploaded again. Matched by `<owner_namespace>~<slug>`,
    // never by bare slug.
    if let Some(ns) = state.get_sync_state(OWNER_NAMESPACE_KEY).ok().flatten() {
        let id = format!("{ns}~{slug}");
        let revoked = scenery_core::revocation::is_revoked(state, "skill", &id, None)
            .unwrap_or_else(|e| {
                warn!(slug, error = %e, "auto-manage: revocation check failed; refusing");
                true
            });
        if revoked {
            warn!(slug, skill_id = %id, "auto-manage: skill is revoked — nothing uploaded");
            record_attempt(
                state,
                &slug,
                &source_path,
                &cls.tree_hash,
                AttemptStatus::Revoked,
                Some("revoked"),
            );
            scenery_core::audit::write_audit_event_direct(
                &state.db_path,
                &scenery_core::audit::AuditEvent {
                    event_type: "auto_manage_blocked".into(),
                    payload: serde_json::json!({"slug": slug, "reason": "revoked"}),
                },
            );
            return Acted::Skipped;
        }
    }

    // ── 2. Pack (a link anywhere, or a secret, holds the skill). ──
    let sp = source_path.clone();
    let packed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        scenery_core::pack::pack_dir_to_tar_gz_no_links(Path::new(&sp))
    })
    .await;
    let gz = match packed {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            if let Some(held) = e.downcast_ref::<scenery_core::pack::PackRefused>() {
                let reason = if held.reason.contains("symbolic link") {
                    "contains_symlink"
                } else {
                    "pack_refused"
                };
                warn!(slug, error = %e, "auto-manage: skill held, not uploaded");
                note_held(state, item, &cls.tree_hash, reason);
                return Acted::Held;
            }
            warn!(slug, error = %e, "auto-manage: could not pack the skill");
            record_attempt(
                state,
                &slug,
                &source_path,
                &cls.tree_hash,
                AttemptStatus::Failed,
                Some("pack_failed"),
            );
            return Acted::Failed;
        }
        Err(e) => {
            warn!(error = %e, "auto-manage: pack task panicked");
            return Acted::Failed;
        }
    };

    // ── 3. Upload. ──
    let (url, token, device_id) = (
        ctx.registry_url.clone(),
        ctx.token.clone(),
        ctx.device_id.clone(),
    );
    let (up_slug, up_hash, up_path, up_mode, up_disc) = (
        slug.clone(),
        cls.tree_hash.clone(),
        source_path.clone(),
        mode.to_string(),
        d.discovery_id.clone(),
    );
    // The blocking client must be built AND dropped on the blocking thread.
    let reply = tokio::task::spawn_blocking(move || {
        RegistryClient::new(url)
            .with_auth(token)
            .auto_manage(AutoManageRequest {
                tar_gz: gz,
                slug: &up_slug,
                tree_hash: &up_hash,
                device_id: &device_id,
                source_path: &up_path,
                mode: &up_mode,
                discovery_id: Some(&up_disc),
            })
    })
    .await;
    let reply = match reply {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!(slug, error = %e, "auto-manage: upload failed — will retry with backoff");
            record_attempt(
                state,
                &slug,
                &source_path,
                &cls.tree_hash,
                AttemptStatus::Failed,
                Some("upload_failed"),
            );
            return Acted::Failed;
        }
        Err(e) => {
            warn!(error = %e, "auto-manage: upload task panicked");
            return Acted::Failed;
        }
    };

    let resp = match reply {
        AutoManageReply::Done(r) => r,
        AutoManageReply::Refused(r) => {
            return refused(state, item, &cls.tree_hash, r.status, &r.code, &r.message);
        }
    };

    finish(ctx, item, &cls.tree_hash, mode, &tracked_reason, resp).await
}

/// Is a track-mode upload due? First sighting always; afterwards only a
/// foreign-installer skill whose tree changed, at most hourly (the installer's
/// update IS the new version). Dev skills are never re-uploaded on edits: a
/// new private version is uploaded only when the user chooses Share.
fn track_upload_due(state: &AppState, path: &str, reason: &str, hash: &str) -> bool {
    let Ok(Some(row)) = tracked::get(state, path) else {
        return true;
    };
    if row.last_tree_hash.as_deref() == Some(hash) {
        return false;
    }
    if !reason.starts_with("foreign_installer") {
        return false;
    }
    match row
        .last_upload_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    {
        Some(t) => {
            chrono::Utc::now().signed_duration_since(t).num_seconds() >= FOREIGN_REUPLOAD_MIN_SECS
        }
        None => true,
    }
}

fn refused(
    state: &AppState,
    item: &DiscoveryItem,
    hash: &str,
    status: u16,
    code: &str,
    message: &str,
) -> Acted {
    match (status, code) {
        (401, _) => Acted::Unauthorized,
        (403, "auto_manage_disabled") => {
            info!("auto-manage: the org has switched auto-management off");
            // Remember it, so nothing more is attempted until a response says otherwise.
            let mut p = state
                .cached_auto_manage_policy()
                .unwrap_or(AutoManagePolicy {
                    auto_manage_skills: false,
                    auto_submit_mcp: true,
                });
            p.auto_manage_skills = false;
            let _ = state.cache_auto_manage_policy(&p);
            Acted::Skipped
        }
        (403, c) => {
            // api_key_not_allowed and friends: this credential can never do it.
            warn!(code = c, message, "auto-manage: refused (403)");
            record_attempt(
                state,
                &item.slug,
                &item.source_path,
                hash,
                AttemptStatus::Blocked,
                Some(c),
            );
            Acted::Blocked
        }
        (404, c) => {
            // device_not_found / discovery_not_found: stale server-side rows;
            // the next upload re-creates them.
            debug!(code = c, "auto-manage: refused (404) — retry next tick");
            record_attempt(
                state,
                &item.slug,
                &item.source_path,
                hash,
                AttemptStatus::Failed,
                Some(c),
            );
            Acted::Failed
        }
        (409, c) => {
            // held | mode_mismatch | already_shared
            warn!(code = c, message, "auto-manage: refused (409)");
            let st = if c == "held" {
                AttemptStatus::Held
            } else {
                AttemptStatus::Skipped
            };
            record_attempt(state, &item.slug, &item.source_path, hash, st, Some(c));
            if c == "held" {
                Acted::Held
            } else {
                Acted::Skipped
            }
        }
        (s, c) => {
            warn!(status = s, code = c, "auto-manage: unexpected refusal");
            record_attempt(
                state,
                &item.slug,
                &item.source_path,
                hash,
                AttemptStatus::Failed,
                Some(c),
            );
            Acted::Failed
        }
    }
}

async fn finish(
    ctx: &TickCtx,
    item: &DiscoveryItem,
    hash: &str,
    mode: &str,
    tracked_reason: &str,
    resp: AutoManageResponse,
) -> Acted {
    let state = &ctx.state;
    let slug = &item.slug;
    let source_path = &item.source_path;

    match resp.outcome {
        AutoManageOutcomeKind::Blocked => {
            let reason = resp.reason.clone().unwrap_or_else(|| "blocked".into());
            warn!(slug = %slug, reason, "auto-manage: backend blocked the skill — original untouched");
            record_attempt(
                state,
                slug,
                source_path,
                hash,
                AttemptStatus::Blocked,
                Some(&reason),
            );
            scenery_core::audit::write_audit_event_direct(
                &state.db_path,
                &scenery_core::audit::AuditEvent {
                    event_type: "auto_manage_blocked".into(),
                    payload: serde_json::json!({"slug": slug, "reason": reason}),
                },
            );
            Acted::Blocked
        }
        AutoManageOutcomeKind::Revoked => {
            warn!(slug = %slug, "auto-manage: skill is revoked — nothing converted");
            record_attempt(
                state,
                slug,
                source_path,
                hash,
                AttemptStatus::Revoked,
                Some("revoked"),
            );
            Acted::Skipped
        }
        AutoManageOutcomeKind::Managed => {
            let (Some(skill_id), Some(version)) = (resp.skill_id.clone(), resp.version.clone())
            else {
                warn!(slug = %slug, "auto-manage: backend said managed without a skill_id/version");
                record_attempt(
                    state,
                    slug,
                    source_path,
                    hash,
                    AttemptStatus::Failed,
                    Some("bad_response"),
                );
                return Acted::Failed;
            };
            if let Some(ns) = resp.owner_namespace.as_deref() {
                let _ = state.set_sync_state(OWNER_NAMESPACE_KEY, ns);
            }
            let skill_uuid = resp.skill_uuid.clone().unwrap_or_default();

            if mode == "track" {
                if let Err(e) = tracked::upsert(
                    state,
                    source_path,
                    slug,
                    tracked_reason,
                    Some(&skill_id),
                    Some(&skill_uuid),
                    Some(&version),
                    hash,
                ) {
                    warn!(slug = %slug, error = %e, "auto-manage: could not record tracked skill");
                    return Acted::Failed;
                }
                record_attempt(
                    state,
                    slug,
                    source_path,
                    hash,
                    AttemptStatus::Tracked,
                    Some(tracked_reason),
                );
                scenery_core::audit::write_audit_event_direct(
                    &state.db_path,
                    &scenery_core::audit::AuditEvent {
                        event_type: "skill_tracked".into(),
                        payload: serde_json::json!({
                            "slug": slug, "skill_id": skill_id,
                            "reason": tracked_reason, "tree_hash": hash,
                        }),
                    },
                );
                // A revocation that arrived while the upload was in flight.
                tracked::reconcile(state);
                return Acted::Tracked;
            }

            convert(ctx, item, hash, &resp, &skill_id, &skill_uuid, &version).await
        }
    }
}

/// Manage mode, after a successful upload: install the registry version, which
/// converts the user's directory transactionally.
async fn convert(
    ctx: &TickCtx,
    item: &DiscoveryItem,
    hash: &str,
    resp: &AutoManageResponse,
    skill_id: &str,
    skill_uuid: &str,
    version: &str,
) -> Acted {
    let state = &ctx.state;
    let slug = &item.slug;
    let source_path = &item.source_path;

    // The registry id differs from the folder name for a private skill. Record
    // it so events, revocations and artifact URLs reach this folder, and so the
    // install fetches `<owner>~<slug>`, not the catalog's `<slug>`.
    if skill_id != slug {
        if let Err(e) = state.record_skill_local_name(skill_id, slug) {
            warn!(slug = %slug, error = %e, "auto-manage: could not record the local folder name");
            record_attempt(
                state,
                slug,
                source_path,
                hash,
                AttemptStatus::Failed,
                Some("local_name"),
            );
            return Acted::Failed;
        }
    }
    // Recorded right after the upload answered, before the install: a crash
    // from here on is picked up by the next Install event for this skill.
    if let Err(e) =
        state.record_pending_auto_managed_takeover(slug, source_path, skill_id, skill_uuid)
    {
        warn!(slug = %slug, error = %e, "auto-manage: could not record the pending takeover");
        record_attempt(
            state,
            slug,
            source_path,
            hash,
            AttemptStatus::Failed,
            Some("takeover_record"),
        );
        return Acted::Failed;
    }

    let installation_id = resp
        .installation_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(|| {
            warn!(slug = %slug, "auto-manage: no installation_id in the response — using a local id");
            Uuid::new_v4()
        });

    let pusher = ManagedPathsPusher::new(state);
    let installed = crate::sync::reconciler::install_verified_version(
        installation_id,
        slug,
        version,
        state,
        &ctx.registry_url,
        Some(&pusher),
    )
    .await;
    if let Err(e) = installed {
        warn!(slug = %slug, error = %e, "auto-manage: install failed — the original is untouched; retrying");
        record_attempt(
            state,
            slug,
            source_path,
            hash,
            AttemptStatus::Failed,
            Some("install_failed"),
        );
        return Acted::Failed;
    }

    // The push is best-effort inside the reconciler: confirm the conversion
    // really happened before calling it done.
    let canonical = super::paths::agents_skills_dir().map(|d| d.join(slug));
    let marked = canonical
        .as_deref()
        .is_some_and(scenery_mcp::ownership::is_scenery_managed);
    let source_gone_or_canonical = !Path::new(source_path).exists()
        || super::takeover::source_is_canonical_dest(slug, Path::new(source_path));
    if !marked || !source_gone_or_canonical {
        warn!(slug = %slug, "auto-manage: install finished but the conversion did not complete; retrying");
        record_attempt(
            state,
            slug,
            source_path,
            hash,
            AttemptStatus::Failed,
            Some("conversion_incomplete"),
        );
        return Acted::Failed;
    }

    record_attempt(state, slug, source_path, hash, AttemptStatus::Managed, None);
    audit_converted(state, slug, source_path, skill_id, version, hash, canonical);
    info!(slug = %slug, skill_id, version, "auto-manage: skill converted (original backed up)");
    Acted::Managed
}

fn audit_converted(
    state: &AppState,
    slug: &str,
    source_path: &str,
    skill_id: &str,
    version: &str,
    tree_hash_before: &str,
    canonical: Option<PathBuf>,
) {
    let backup = scenery_core::restore_journal::RestoreJournal::for_state(state)
        .read_all()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .find(|e| {
            e.source == scenery_core::restore_journal::JournalSource::Adopted
                && e.slug.as_deref() == Some(slug)
        })
        .and_then(|e| e.backup_path);
    let tree_hash_after = canonical.and_then(|c| {
        open(state).ok().and_then(|conn| {
            super::marker::marker_provenance(&conn, &c.to_string_lossy()).tree_hash
        })
    });
    scenery_core::audit::write_audit_event_direct(
        &state.db_path,
        &scenery_core::audit::AuditEvent {
            event_type: "skill_auto_managed".into(),
            payload: serde_json::json!({
                "slug": slug,
                "skill_id": skill_id,
                "version": version,
                "source_path": source_path,
                "backup_path": backup,
                "tree_hash_before": tree_hash_before,
                "tree_hash_after": tree_hash_after,
            }),
        },
    );
}

#[cfg(test)]
#[path = "auto_manage_tests.rs"]
mod tests;
