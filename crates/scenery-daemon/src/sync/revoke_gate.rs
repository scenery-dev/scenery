//! Decides which entries of a snapshot's durable revocation list still need
//! local teardown.
//!
//! The snapshot re-delivers the full revocation list on every SSE reconnect
//! (e.g. after a token refresh) and on the periodic tick. Re-running teardown,
//! the `skill purged` / `artifact revoked` logs and the `*_revoked` audit event
//! for an entry that was already applied is noise, so an entry is skipped when
//! it was already in the local cache AND nothing it covers is still present.
//! An entry that is new to the cache (e.g. revoked while the device was
//! offline) always applies, and so does one whose artifact is still on disk
//! (e.g. a crash after the block was persisted but before teardown finished).

use anyhow::{Context, Result};
use scenery_core::revocation::{self, RevocationEntry};
use scenery_core::state::AppState;

fn norm(v: Option<&str>) -> &str {
    v.unwrap_or("").trim()
}

/// For each of `entries`, in order: should teardown run? Fails open (apply)
/// on any error reading the cache or local state.
pub(super) fn entries_needing_apply(state: &AppState, entries: &[RevocationEntry]) -> Vec<bool> {
    let prior = match revocation::list_all(state) {
        Ok(p) => p,
        Err(_) => return vec![true; entries.len()],
    };
    entries
        .iter()
        .map(|e| {
            let seen = prior.iter().any(|p| {
                p.artifact_type == e.artifact_type
                    && p.artifact_key == e.artifact_key
                    && norm(p.version.as_deref()) == norm(e.version.as_deref())
            });
            !seen || local_state_remains(state, e).unwrap_or(true)
        })
        .collect()
}

/// Is anything the revocation covers still present locally?
fn local_state_remains(state: &AppState, e: &RevocationEntry) -> Result<bool> {
    let version = Some(norm(e.version.as_deref())).filter(|v| !v.is_empty());
    match e.artifact_type.as_str() {
        "skill" => skill_remains(state, &e.artifact_key, version),
        "mcp_server" => {
            let installed = state
                .list_mcp_installs()
                .context("failed to read mcp_installations")?
                .iter()
                .any(|r| r.mcp_server_id == e.artifact_key);
            Ok(
                installed
                    && super::reconciler::mcp_revoke_applies(state, &e.artifact_key, version)?,
            )
        }
        "plugin" => {
            let installed = crate::managed_paths::list_governed_plugins()?;
            Ok(installed
                .get(&e.artifact_key)
                .is_some_and(|iv| version.map_or(true, |v| iv == v)))
        }
        // Unknown type: no teardown exists, so there is nothing to redo.
        _ => Ok(false),
    }
}

fn skill_remains(state: &AppState, skill_id: &str, version: Option<&str>) -> Result<bool> {
    let conn = rusqlite::Connection::open(&state.db_path)
        .context("failed to open state DB for revoke check")?;
    let in_db: bool = match version {
        None => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM installed_skills WHERE skill_id = ?1)",
            rusqlite::params![skill_id],
            |r| r.get(0),
        ),
        Some(v) => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM installed_skills WHERE skill_id = ?1 AND active_version = ?2) \
                 OR EXISTS(SELECT 1 FROM skill_versions WHERE skill_id = ?1 AND version = ?2)",
            rusqlite::params![skill_id, v],
            |r| r.get(0),
        ),
    }
    .context("failed to query installed skill")?;
    if in_db {
        return Ok(true);
    }
    let dir = state.root_dir.join("skills").join(skill_id);
    let on_disk = match version {
        None => dir.exists(),
        Some(v) => !v.contains(['/', '\\']) && dir.join("versions").join(v).exists(),
    };
    if on_disk {
        return Ok(true);
    }
    Ok(!crate::managed_paths::plugins_bundling_skill(skill_id, version)?.is_empty())
}
