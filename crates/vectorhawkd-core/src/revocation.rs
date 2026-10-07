//! Broadcast-model artifact revocation: local cache + install-time gate.
//!
//! One org-level revocation record on the backend fans out as a single
//! `revoke` SSE event to every connected daemon, plus a durable
//! `revocations` list carried in the `snapshot` event / `GET
//! /sync/snapshot` poll for daemons that were offline when the live event
//! fired. Both paths land here: the live event via [`upsert_revocation`],
//! the durable list via [`replace_all`].
//!
//! [`is_revoked`] is the one check every install path must make before
//! installing anything (SSE `install`/`install_mcp`/`install_plugin`,
//! snapshot-derived installs, `vectorhawk skill install`, and the
//! auto-updater) — revoke overrides desired state, so a stale `install`
//! event or a CLI command must never resurrect something an admin pulled.
//!
//! `version` is always normalized to `""` for "whole artifact, every
//! version" so the SQLite primary key stays a clean upsert target (see the
//! schema doc comment in `state::SCHEMA_REVOKE_SQL`).

use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// Normalize `None` / `Some("")` to the on-disk "whole artifact" sentinel.
fn norm(version: Option<&str>) -> &str {
    version.unwrap_or("").trim()
}

/// Record (or refresh) one revocation. Idempotent: revoking the same
/// `(artifact_type, artifact_key, version)` twice is a no-op on the second
/// call (just bumps `revoked_at`).
pub fn upsert_revocation(
    state: &AppState,
    artifact_type: &str,
    artifact_key: &str,
    version: Option<&str>,
) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB for revoke")?;
    conn.execute(
        "INSERT INTO artifact_revocations (artifact_type, artifact_key, version, revoked_at) \
         VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP) \
         ON CONFLICT (artifact_type, artifact_key, version) \
         DO UPDATE SET revoked_at = CURRENT_TIMESTAMP",
        params![artifact_type, artifact_key, norm(version)],
    )
    .context("failed to upsert artifact_revocations row")?;
    Ok(())
}

/// Remove one revocation locally (mirrors an admin "unblock" clearing the
/// record server-side). Not currently called from the live-event path —
/// the durable snapshot list is authoritative for "no longer revoked" via
/// [`replace_all`] — but kept as a direct primitive for tests and for a
/// future dedicated `unrevoke` SSE event.
pub fn clear_revocation(
    state: &AppState,
    artifact_type: &str,
    artifact_key: &str,
    version: Option<&str>,
) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB for unrevoke")?;
    conn.execute(
        "DELETE FROM artifact_revocations \
         WHERE artifact_type = ?1 AND artifact_key = ?2 AND version = ?3",
        params![artifact_type, artifact_key, norm(version)],
    )
    .context("failed to delete artifact_revocations row")?;
    Ok(())
}

/// One entry of a durable revocation list, as delivered by the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationEntry {
    pub artifact_type: String,
    pub artifact_key: String,
    /// `None` / `Some("")` both mean "whole artifact, every version".
    pub version: Option<String>,
}

/// Replace the entire local revocation cache with `entries` in one
/// transaction (delete-all then insert-all).
///
/// This is deliberately a full replace, not a merge: the snapshot's
/// `revocations` list is the backend's current authoritative set, so an
/// entry that dropped out (admin "unblock") must disappear locally too —
/// that's how unblock lifts the local install-time gate without a
/// dedicated `unrevoke` event. Called with an empty `entries` slice this
/// clears the cache entirely, which is correct: the backend is telling us
/// nothing is revoked right now.
///
/// Callers MUST NOT call this for a snapshot that omits the `revocations`
/// key altogether (older backend) — that case must leave the local cache
/// untouched, which is why the wire type on the daemon side is
/// `Option<Vec<RevocationRecord>>` and the reconciler only calls this when
/// the option is `Some`.
pub fn replace_all(state: &AppState, entries: &[RevocationEntry]) -> Result<()> {
    let mut conn =
        Connection::open(&state.db_path).context("failed to open state DB for revocation sync")?;
    let tx = conn
        .transaction()
        .context("failed to open transaction for revocation sync")?;
    tx.execute("DELETE FROM artifact_revocations", [])
        .context("failed to clear artifact_revocations")?;
    for e in entries {
        tx.execute(
            "INSERT INTO artifact_revocations (artifact_type, artifact_key, version, revoked_at) \
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
            params![e.artifact_type, e.artifact_key, norm(e.version.as_deref())],
        )
        .context("failed to insert artifact_revocations row during sync")?;
    }
    tx.commit()
        .context("failed to commit artifact_revocations sync")?;
    Ok(())
}

/// Is `artifact_key` (optionally scoped to `version`) revoked right now?
///
/// Checks both the whole-artifact row (`version = ''`) and, when `version`
/// is given, the version-specific row — either one blocks. This is the
/// gate every install path calls before installing anything.
pub fn is_revoked(
    state: &AppState,
    artifact_type: &str,
    artifact_key: &str,
    version: Option<&str>,
) -> Result<bool> {
    let conn =
        Connection::open(&state.db_path).context("failed to open state DB for revoke check")?;
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM artifact_revocations \
             WHERE artifact_type = ?1 AND artifact_key = ?2 \
               AND (version = '' OR version = ?3) LIMIT 1",
            params![artifact_type, artifact_key, norm(version)],
            |row| row.get(0),
        )
        .optional_result()?;
    Ok(hit.is_some())
}

/// `rusqlite::OptionalExtension` is the usual way to do this, but importing
/// it just for one call site reads worse than a tiny local helper.
trait OptionalResult<T> {
    fn optional_result(self) -> rusqlite::Result<Option<T>>;
}
impl<T> OptionalResult<T> for rusqlite::Result<T> {
    fn optional_result(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state(label: &str) -> AppState {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("vh-revocation-tests-{label}-{nanos}"));
        let root = Utf8PathBuf::from_path_buf(root).expect("utf8 path");
        AppState::bootstrap_in(root).expect("bootstrap")
    }

    fn cleanup(state: &AppState) {
        let _ = std::fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn whole_artifact_revoke_blocks_every_version() {
        let state = temp_state("whole");
        upsert_revocation(&state, "skill", "code-review", None).unwrap();

        assert!(is_revoked(&state, "skill", "code-review", None).unwrap());
        assert!(is_revoked(&state, "skill", "code-review", Some("1.2.3")).unwrap());
        assert!(!is_revoked(&state, "skill", "other-skill", Some("1.2.3")).unwrap());

        cleanup(&state);
    }

    #[test]
    fn per_version_revoke_only_blocks_that_version() {
        let state = temp_state("per-version");
        upsert_revocation(&state, "skill", "code-review", Some("1.0.0")).unwrap();

        assert!(is_revoked(&state, "skill", "code-review", Some("1.0.0")).unwrap());
        assert!(!is_revoked(&state, "skill", "code-review", Some("2.0.0")).unwrap());
        // No version given on the check side still matches the specific-row
        // key only when it IS the whole-artifact sentinel; a version-scoped
        // revocation must not block an unscoped check for a different use.
        // (In practice every install path always has a concrete version.)

        cleanup(&state);
    }

    #[test]
    fn upsert_is_idempotent() {
        let state = temp_state("idempotent");
        upsert_revocation(&state, "mcp_server", "srv-1", None).unwrap();
        upsert_revocation(&state, "mcp_server", "srv-1", None).unwrap();

        let conn = Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artifact_revocations WHERE artifact_key = 'srv-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "revoking twice must not duplicate the row");

        cleanup(&state);
    }

    #[test]
    fn replace_all_clears_entries_dropped_from_the_list() {
        let state = temp_state("replace-all");
        upsert_revocation(&state, "skill", "a", None).unwrap();
        upsert_revocation(&state, "skill", "b", None).unwrap();
        assert!(is_revoked(&state, "skill", "a", None).unwrap());
        assert!(is_revoked(&state, "skill", "b", None).unwrap());

        // Backend's current list only has "a" — "b" was unblocked server-side.
        replace_all(
            &state,
            &[RevocationEntry {
                artifact_type: "skill".to_string(),
                artifact_key: "a".to_string(),
                version: None,
            }],
        )
        .unwrap();

        assert!(is_revoked(&state, "skill", "a", None).unwrap());
        assert!(
            !is_revoked(&state, "skill", "b", None).unwrap(),
            "unblock must clear the local gate so a new install is allowed again"
        );

        cleanup(&state);
    }

    #[test]
    fn replace_all_with_empty_list_clears_everything() {
        let state = temp_state("replace-all-empty");
        upsert_revocation(&state, "plugin", "p1", None).unwrap();
        replace_all(&state, &[]).unwrap();
        assert!(!is_revoked(&state, "plugin", "p1", None).unwrap());
        cleanup(&state);
    }

    #[test]
    fn clear_revocation_removes_only_that_entry() {
        let state = temp_state("clear-one");
        upsert_revocation(&state, "skill", "a", None).unwrap();
        upsert_revocation(&state, "skill", "a", Some("1.0.0")).unwrap();
        clear_revocation(&state, "skill", "a", Some("1.0.0")).unwrap();
        // whole-artifact row still blocks everything regardless.
        assert!(is_revoked(&state, "skill", "a", Some("1.0.0")).unwrap());
        clear_revocation(&state, "skill", "a", None).unwrap();
        assert!(!is_revoked(&state, "skill", "a", Some("1.0.0")).unwrap());
        cleanup(&state);
    }
}
