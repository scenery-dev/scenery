//! Tells a snapshot redelivery of a revocation apart from a new one.
//!
//! The snapshot re-delivers the full revocation list on every SSE reconnect
//! (e.g. after a token refresh) and on the periodic tick. Local teardown is
//! idempotent and ALWAYS runs for every entry — an earlier attempt may have
//! been partial (client-projected content such as `~/.agents/skills/<slug>`,
//! the `~/.claude/skills` link or the `~/.claude.json` entry is removed
//! non-fatally), and no check of local state can prove that every client
//! projection is gone. What this gate decides is only whether a repeat pass
//! that removes nothing may stay silent (no `artifact revoked` log line, no
//! duplicate `*_revoked` audit event): that is true for an entry that was
//! already in the local cache before this snapshot replaced it.
//!
//! An entry that is new to the cache (e.g. revoked while the device was
//! offline) is never "already applied".

use scenery_core::revocation::{self, RevocationEntry};
use scenery_core::state::AppState;

fn norm(v: Option<&str>) -> &str {
    v.unwrap_or("").trim()
}

/// For each of `entries`, in order: was it already in the local revocation
/// cache? Fails open (`false` = treat as new, so log + audit) if the cache
/// cannot be read.
pub(super) fn entries_already_cached(state: &AppState, entries: &[RevocationEntry]) -> Vec<bool> {
    let prior = match revocation::list_all(state) {
        Ok(p) => p,
        Err(_) => return vec![false; entries.len()],
    };
    entries
        .iter()
        .map(|e| {
            prior.iter().any(|p| {
                p.artifact_type == e.artifact_type
                    && p.artifact_key == e.artifact_key
                    && norm(p.version.as_deref()) == norm(e.version.as_deref())
            })
        })
        .collect()
}
