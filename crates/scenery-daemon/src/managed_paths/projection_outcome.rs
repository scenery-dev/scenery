//! The daemon's report of what happened when it projected an installed skill
//! onto disk (`~/.agents/skills/<slug>` plus the `~/.claude/skills/<slug>`
//! link), stored in `sync_state` so `scenery skill install` can surface it as
//! soon as it is known instead of guessing from the CLI's own environment.
//!
//! The daemon marks `installed_skills` active BEFORE it pushes, and the push
//! or the link can be refused (a real, unmarked directory at either path), so
//! the CLI needs an explicit outcome rather than a poll on the files.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use scenery_core::state::AppState;
use std::path::{Path, PathBuf};

/// What the daemon did for one install's native projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionOutcome {
    /// Canonical dir, marker and Claude link are in place (or the filesystem
    /// reconciler is disabled, so there is nothing to project).
    Ok,
    /// The canonical copy is live but the Claude link was refused at `path`.
    LinkRefused { path: PathBuf },
    /// The push was refused because user content sits at `path`.
    PushRefused { path: PathBuf },
    /// The push failed for another reason.
    PushFailed { detail: String },
}

fn key(skill_id: &str) -> String {
    format!("native_projection:{skill_id}")
}

fn encode(outcome: &ProjectionOutcome, at: DateTime<Utc>) -> String {
    let (kind, payload) = match outcome {
        ProjectionOutcome::Ok => ("ok", String::new()),
        ProjectionOutcome::LinkRefused { path } => ("link_refused", path.display().to_string()),
        ProjectionOutcome::PushRefused { path } => ("push_refused", path.display().to_string()),
        ProjectionOutcome::PushFailed { detail } => ("push_failed", detail.replace('\t', " ")),
    };
    format!("{kind}\t{}\t{payload}", at.to_rfc3339())
}

fn decode(raw: &str) -> Option<(DateTime<Utc>, ProjectionOutcome)> {
    let mut parts = raw.splitn(3, '\t');
    let kind = parts.next()?;
    let at = DateTime::parse_from_rfc3339(parts.next()?)
        .ok()?
        .with_timezone(&Utc);
    let payload = parts.next().unwrap_or("");
    let outcome = match kind {
        "ok" => ProjectionOutcome::Ok,
        "link_refused" => ProjectionOutcome::LinkRefused {
            path: Path::new(payload).to_path_buf(),
        },
        "push_refused" => ProjectionOutcome::PushRefused {
            path: Path::new(payload).to_path_buf(),
        },
        "push_failed" => ProjectionOutcome::PushFailed {
            detail: payload.to_string(),
        },
        _ => return None,
    };
    Some((at, outcome))
}

/// Record the outcome for `skill_id` (registry id), stamped now.
pub fn record(state: &AppState, skill_id: &str, outcome: &ProjectionOutcome) -> Result<()> {
    state
        .set_sync_state(&key(skill_id), &encode(outcome, Utc::now()))
        .context("failed to record native projection outcome")
}

/// The latest outcome for `skill_id` recorded at or after `since`. An older
/// record belongs to an earlier install and is ignored.
pub fn read_since(
    state: &AppState,
    skill_id: &str,
    since: DateTime<Utc>,
) -> Result<Option<ProjectionOutcome>> {
    let raw = state
        .get_sync_state(&key(skill_id))
        .context("failed to read native projection outcome")?;
    Ok(raw
        .as_deref()
        .and_then(decode)
        .filter(|(at, _)| *at >= since)
        .map(|(_, o)| o))
}

#[cfg(test)]
#[path = "projection_outcome_tests.rs"]
mod tests;
