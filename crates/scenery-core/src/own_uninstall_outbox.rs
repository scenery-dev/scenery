//! Durable outbox for user-uninstalled OWN skills (state.db, survives restarts).
//!
//! A user uninstall removes the skill locally at once, but the backend must also
//! archive the private copy and deactivate its installs. If that call cannot be
//! made (offline, dev box stopped) the backend keeps a live install row and a
//! reconnect snapshot would bring the skill back. So the uninstall writes a row
//! here FIRST:
//!
//! - the reconciler never derives an `Install` for a tombstoned skill from a
//!   snapshot ([`is_tombstoned`]);
//! - the daemon retries [`process`] with backoff until the backend confirms,
//!   then the row is cleared;
//! - the row is also cleared by an explicit live `install` event (a portal
//!   Reinstall) and when the user recreates the folder and it is managed again.

use crate::{registry::RegistryClient, state::AppState};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// Backoff: 30 s doubling per failed attempt, capped at one hour.
const BASE_BACKOFF_SECS: i64 = 30;
const MAX_BACKOFF_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Registry id of the private skill (`adam~pdf`, or its UUID).
    pub skill_id: String,
    /// Folder name on this device.
    pub local_slug: String,
    pub installation_id: Option<String>,
    pub local_edits: bool,
    pub registry_url: String,
    pub attempts: i64,
    pub next_try_at: i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open(state: &AppState) -> Result<Connection> {
    Connection::open(&state.db_path).context("own-uninstall outbox: open state DB")
}

/// Record (or refresh) a pending uninstall. Due immediately.
pub fn record(state: &AppState, e: &Entry) -> Result<()> {
    open(state)?.execute(
        "INSERT INTO own_uninstall_outbox \
         (skill_id, local_slug, installation_id, local_edits, registry_url, created_at, attempts, next_try_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0) \
         ON CONFLICT(skill_id) DO UPDATE SET local_slug = ?2, installation_id = ?3, \
           local_edits = ?4, registry_url = ?5, attempts = 0, next_try_at = 0",
        params![
            e.skill_id,
            e.local_slug,
            e.installation_id,
            e.local_edits as i64,
            e.registry_url,
            now_secs()
        ],
    )?;
    Ok(())
}

/// Is `name` (a registry id or a local folder name) waiting for confirmation?
/// Fail-open on a DB error would risk a resurrection, so errors count as `false`
/// only because the caller has nothing better; they are logged by the caller's
/// own DB use.
pub fn is_tombstoned(state: &AppState, name: &str) -> bool {
    open(state)
        .and_then(|c| {
            c.query_row(
                "SELECT 1 FROM own_uninstall_outbox WHERE skill_id = ?1 OR local_slug = ?1 LIMIT 1",
                [name],
                |_| Ok(true),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(anyhow::Error::from(other)),
            })
        })
        .unwrap_or(false)
}

/// Drop the tombstone for `name` (registry id or folder name). True if one existed.
pub fn clear(state: &AppState, name: &str) -> bool {
    open(state)
        .and_then(|c| {
            Ok(c.execute(
                "DELETE FROM own_uninstall_outbox WHERE skill_id = ?1 OR local_slug = ?1",
                [name],
            )? > 0)
        })
        .unwrap_or(false)
}

/// Entries whose retry time has come.
pub fn due(state: &AppState) -> Vec<Entry> {
    let Ok(conn) = open(state) else {
        return vec![];
    };
    let Ok(mut st) = conn.prepare(
        "SELECT skill_id, local_slug, installation_id, local_edits, registry_url, attempts, next_try_at \
         FROM own_uninstall_outbox WHERE next_try_at <= ?1 ORDER BY created_at",
    ) else {
        return vec![];
    };
    st.query_map([now_secs()], |r| {
        Ok(Entry {
            skill_id: r.get(0)?,
            local_slug: r.get(1)?,
            installation_id: r.get(2)?,
            local_edits: r.get::<_, i64>(3)? != 0,
            registry_url: r.get(4)?,
            attempts: r.get(5)?,
            next_try_at: r.get(6)?,
        })
    })
    .map(|it| it.flatten().collect())
    .unwrap_or_default()
}

/// The uninstall entry point for CLI and MCP: durably record the pending backend
/// call, then try it once. `Ok(true)` = the backend confirmed (row cleared);
/// `Err` = it stays queued and the daemon retries with backoff, and the
/// reconciler will not reinstall the skill meanwhile.
pub fn submit(
    state: &AppState,
    registry_url: &str,
    own: &crate::installer::OwnSkillUninstall,
) -> Result<bool> {
    let e = Entry {
        skill_id: own.skill_id.clone(),
        local_slug: own.local_slug.clone(),
        installation_id: own.installation_id.clone(),
        local_edits: own.local_edits,
        registry_url: registry_url.to_string(),
        attempts: 0,
        next_try_at: 0,
    };
    record(state, &e)?;
    process(state, &e)
}

fn note_failure(state: &AppState, e: &Entry) {
    let attempts = e.attempts + 1;
    let backoff = (BASE_BACKOFF_SECS << attempts.clamp(0, 7)).min(MAX_BACKOFF_SECS);
    if let Ok(c) = open(state) {
        let _ = c.execute(
            "UPDATE own_uninstall_outbox SET attempts = ?2, next_try_at = ?3 WHERE skill_id = ?1",
            params![e.skill_id, attempts, now_secs() + backoff],
        );
    }
}

/// Tell the backend (archive + deactivate). Blocking: call from a blocking
/// context. On success the tombstone is cleared and `Ok(true)` returned; on
/// failure it stays, with backoff, and the error is returned. A backend that
/// predates the archive call (404/405/426) still needs the installation
/// deactivated, which is what stops a snapshot reinstall there.
pub fn process(state: &AppState, e: &Entry) -> Result<bool> {
    let client = RegistryClient::new(&e.registry_url).with_stored_auth(state);
    if let Ok(Some(d)) = state.get_sync_state("device_id") {
        client.set_device_id(d);
    }
    let result = (|| -> Result<()> {
        client.notify_own_skill_uninstalled(&e.skill_id, e.local_edits)?;
        if let Some(iid) = e.installation_id.as_deref().filter(|i| !i.is_empty()) {
            client.deactivate_installation(iid)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            // Only drop OUR row: a re-record (new uninstall) keeps its own.
            clear(state, &e.skill_id);
            Ok(true)
        }
        Err(err) => {
            note_failure(state, e);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    fn state(dir: &tempfile::TempDir) -> AppState {
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        AppState::bootstrap_in(root).unwrap()
    }

    fn entry(url: &str) -> Entry {
        Entry {
            skill_id: "adam~pdf".into(),
            local_slug: "pdf".into(),
            installation_id: Some("11111111-1111-4111-8111-111111111111".into()),
            local_edits: false,
            registry_url: url.into(),
            attempts: 0,
            next_try_at: 0,
        }
    }

    #[test]
    fn record_survives_a_restart_and_matches_id_or_folder() {
        let dir = tempfile::tempdir().unwrap();
        record(&state(&dir), &entry("http://x")).unwrap();
        let reopened = state(&dir); // a daemon restart
        assert!(is_tombstoned(&reopened, "adam~pdf"));
        assert!(is_tombstoned(&reopened, "pdf"));
        assert!(!is_tombstoned(&reopened, "other"));
        assert_eq!(due(&reopened).len(), 1);
        assert!(clear(&reopened, "pdf"));
        assert!(!is_tombstoned(&reopened, "adam~pdf"));
    }

    #[test]
    fn mcp_style_uninstall_while_the_backend_is_unreachable_is_retried_later_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(&dir);
        // Nothing listens on port 9: the first attempt fails but is queued.
        let dead = "http://127.0.0.1:9";
        crate::auth::save_tokens(&st, dead, "acc", "ref").unwrap();
        let own = crate::installer::OwnSkillUninstall {
            skill_id: "adam~pdf".into(),
            local_edits: true,
            local_slug: "pdf".into(),
            installation_id: Some("11111111-1111-4111-8111-111111111111".into()),
        };
        assert!(submit(&st, dead, &own).is_err());
        assert!(is_tombstoned(&st, "pdf"), "still queued");
        assert!(due(&st).is_empty(), "backs off after a failure");

        // The backend comes back: the daemon retry (here: process) confirms.
        let mut server = Server::new();
        crate::auth::save_tokens(&st, &server.url(), "acc", "ref").unwrap();
        let archive = server
            .mock("POST", "/runner/skills/user-uninstall")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"skill_id": "adam~pdf", "local_edits": true}),
            ))
            .with_status(200)
            .with_body(r#"{"archived":true,"skill_id":"adam~pdf","reason":null}"#)
            .create();
        let deactivate = server
            .mock(
                "PATCH",
                "/api/installations/11111111-1111-4111-8111-111111111111",
            )
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"state": "deactivated"}),
            ))
            .with_status(200)
            .with_body("{}")
            .create();
        let mut e = entry(&server.url());
        e.local_edits = true;
        assert!(process(&st, &e).unwrap());
        archive.assert();
        deactivate.assert();
        assert!(!is_tombstoned(&st, "pdf"), "confirmed: tombstone cleared");
    }

    #[test]
    fn a_rejected_deactivate_keeps_the_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(&dir);
        let mut server = Server::new();
        crate::auth::save_tokens(&st, &server.url(), "acc", "ref").unwrap();
        let _a = server
            .mock("POST", "/runner/skills/user-uninstall")
            .with_status(200)
            .with_body(r#"{"archived":true,"skill_id":"adam~pdf","reason":null}"#)
            .create();
        let _d = server
            .mock("PATCH", mockito::Matcher::Any)
            .with_status(500)
            .create();
        record(&st, &entry(&server.url())).unwrap();
        assert!(process(&st, &entry(&server.url())).is_err());
        assert!(is_tombstoned(&st, "pdf"));
    }
}
