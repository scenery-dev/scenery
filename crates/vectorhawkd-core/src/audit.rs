use crate::{
    registry::{AuditEventPayload, RegistryClient},
    state::AppState,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, warn};

/// A single audit event to be recorded and (eventually) uploaded to the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Category of event (e.g. `"tool_called"`, `"unmanaged_server_detected"`).
    pub event_type: String,
    /// Arbitrary JSON payload carrying event-specific fields.
    pub payload: serde_json::Value,
}

/// Abstraction over the audit buffer backend.
///
/// M0: `NoOpAuditBuffer` — events are accepted and silently dropped.
/// M1: `SqliteAuditBuffer` — events are persisted in the `audit_events` table
///     and flushed to the registry on the 300 s background tick (or after 100
///     events, whichever comes first).
pub trait AuditBuffer: Send + Sync {
    /// Record an event. Non-fatal: implementors should log on error, not panic.
    fn record(&self, event: &AuditEvent) -> Result<()>;

    /// Flush buffered events to the registry. Returns the count uploaded.
    ///
    /// Called on the background 300 s tick (or when the event count threshold
    /// is reached). For the no-op impl this is always 0.
    fn flush(&self, state: &AppState) -> Result<usize>;
}

// ── No-op implementation ──────────────────────────────────────────────────────

/// Audit buffer that accepts events but does nothing with them.
/// Used in tests that don't need audit verification.
pub struct NoOpAuditBuffer;

impl AuditBuffer for NoOpAuditBuffer {
    fn record(&self, _event: &AuditEvent) -> Result<()> {
        Ok(())
    }

    fn flush(&self, _state: &AppState) -> Result<usize> {
        Ok(0)
    }
}

// ── SQLite-backed implementation ──────────────────────────────────────────────

/// Audit buffer backed by the `audit_events` SQLite table.
///
/// Every `record()` call writes a row to SQLite via `spawn_blocking` at
/// the call site in the async daemon path. `flush()` reads all un-uploaded
/// rows, posts them to the registry's audit endpoint, and DELETEs the rows
/// on success (leaving them for retry on failure).
///
/// # spawn_blocking requirement (M1.6 audit)
///
/// Both `record()` and `flush()` issue synchronous SQLite I/O. They MUST
/// NOT be called directly from an async context running on a current-thread
/// Tokio executor. All call sites in the daemon's per-connection tasks or
/// background loops must wrap these calls in `tokio::task::spawn_blocking`.
///
/// The daemon's registry sync loop already does this correctly:
/// ```ignore
/// tokio::task::spawn_blocking(move || run_sync_tick(...)).await?;
/// ```
///
/// When M1.4 wires audit into the per-tool-call path (`socket_dispatch.rs`),
/// the same pattern must be applied. See the comment in `socket_dispatch.rs`.
///
/// Callers must invoke `flush()` on a background task — it issues synchronous
/// I/O and must not run on the async executor thread. Use
/// `tokio::task::spawn_blocking`.
///
/// The 100-event in-memory threshold is tracked here; the 30-second timer is
/// owned by the daemon's sync loop which calls `flush()` on each tick.
pub struct SqliteAuditBuffer {
    registry: Arc<RegistryClient>,
    db_path: camino::Utf8PathBuf,
    /// In-memory count of events written since the last flush. When this
    /// reaches `FLUSH_BATCH_SIZE` the next `record()` call triggers an
    /// inline flush before writing the new event.
    ///
    /// Guarded by a Mutex because `AuditBuffer::record` takes `&self`.
    /// Contention is negligible (serialised tool-call writes only).
    pending_count: Mutex<usize>,
}

/// Flush when this many events are pending, regardless of the 30-s timer.
const FLUSH_BATCH_SIZE: usize = 100;

impl SqliteAuditBuffer {
    pub fn new(registry: Arc<RegistryClient>, state: &AppState) -> Self {
        Self {
            registry,
            db_path: state.db_path.clone(),
            pending_count: Mutex::new(0),
        }
    }

    /// Write one event row to SQLite.
    fn write_event(&self, event: &AuditEvent) -> Result<()> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB for audit")?;
        let payload_json = serde_json::to_string(&event.payload)
            .context("failed to serialize audit event payload")?;
        let now = unix_now() as i64;
        conn.execute(
            "INSERT INTO audit_events (event_type, payload, created_at, uploaded) VALUES (?1, ?2, ?3, 0)",
            params![event.event_type, payload_json, now],
        )
        .context("failed to insert audit event")?;
        debug!(event_type = %event.event_type, "audit event recorded");
        Ok(())
    }

    /// Read all un-uploaded rows and attempt to upload them.
    /// Deletes rows only on success; leaves them for retry on failure.
    fn upload_pending(&self) -> Result<usize> {
        let conn =
            Connection::open(&self.db_path).context("failed to open state DB for audit flush")?;

        // Collect pending events.
        let mut stmt = conn
            .prepare(
                "SELECT id, event_type, payload, created_at FROM audit_events WHERE uploaded = 0 ORDER BY id",
            )
            .context("failed to prepare audit query")?;

        struct Row {
            id: i64,
            event_type: String,
            payload_json: String,
            created_at: i64,
        }

        let rows: Vec<Row> = stmt
            .query_map([], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    payload_json: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .context("failed to query audit events")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to collect audit rows")?;

        if rows.is_empty() {
            return Ok(0);
        }

        let payloads: Vec<AuditEventPayload> = rows
            .iter()
            .map(|r| {
                let payload: serde_json::Value =
                    serde_json::from_str(&r.payload_json).unwrap_or(serde_json::Value::Null);
                AuditEventPayload {
                    event_type: r.event_type.clone(),
                    payload,
                    created_at: r.created_at,
                }
            })
            .collect();

        let count = payloads.len();
        self.registry
            .upload_audit_batch(&payloads)
            .context("failed to upload audit batch to registry")?;

        // Upload succeeded: delete the rows we just sent.
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        // Build a parameterized DELETE for all IDs in the batch.
        // SQLite supports up to 999 variables; our 100-event batch is well under.
        let placeholders: String = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let delete_sql = format!("DELETE FROM audit_events WHERE id IN ({placeholders})");
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        conn.execute(&delete_sql, params_ref.as_slice())
            .context("failed to delete uploaded audit events")?;

        debug!(count, "audit batch uploaded and cleared");
        Ok(count)
    }
}

impl AuditBuffer for SqliteAuditBuffer {
    fn record(&self, event: &AuditEvent) -> Result<()> {
        // Check if we've hit the batch threshold.
        let should_flush = {
            let mut count = self
                .pending_count
                .lock()
                .expect("audit pending_count mutex poisoned");
            *count += 1;
            if *count >= FLUSH_BATCH_SIZE {
                *count = 0;
                true
            } else {
                false
            }
        };

        // Write the event first so it's durable before we attempt the flush.
        if let Err(e) = self.write_event(event) {
            warn!(error = %e, "failed to record audit event; continuing");
        }

        // Inline flush on threshold (best-effort — daemon won't crash on error).
        if should_flush {
            if let Err(e) = self.upload_pending() {
                warn!(error = %e, "threshold-triggered audit flush failed; will retry on next tick");
            }
        }

        Ok(())
    }

    fn flush(&self, _state: &AppState) -> Result<usize> {
        // Reset the pending counter so the threshold countdown restarts cleanly.
        {
            let mut count = self
                .pending_count
                .lock()
                .expect("audit pending_count mutex poisoned");
            *count = 0;
        }
        self.upload_pending()
    }
}

// ── Offline-safe direct write ─────────────────────────────────────────────────

/// Write one audit event row directly to the SQLite store at `db_path`.
///
/// This is the escape hatch for callers (CLI commands) that do not hold a
/// `SqliteAuditBuffer` — typically because the daemon is not running and there
/// is no `RegistryClient` in scope.  The row survives in `audit_events` with
/// `uploaded = 0`; the daemon's `SqliteAuditBuffer::flush()` on its next sync
/// tick will upload it and delete the row.
///
/// On failure the error is logged at WARN level and swallowed.  Audit recording
/// must never block or abort a CLI command.
pub fn write_audit_event_direct(db_path: &camino::Utf8Path, event: &AuditEvent) {
    let result = write_audit_event_direct_inner(db_path, event);
    if let Err(e) = result {
        warn!(error = %e, event_type = %event.event_type, "failed to record audit event to local store");
    }
}

fn write_audit_event_direct_inner(
    db_path: &camino::Utf8Path,
    event: &AuditEvent,
) -> Result<()> {
    let conn = Connection::open(db_path).context("failed to open state DB for audit")?;
    let payload_json = serde_json::to_string(&event.payload)
        .context("failed to serialize audit event payload")?;
    let now = unix_now() as i64;
    conn.execute(
        "INSERT INTO audit_events (event_type, payload, created_at, uploaded) \
         VALUES (?1, ?2, ?3, 0)",
        params![event.event_type, payload_json, now],
    )
    .context("failed to insert audit event")?;
    debug!(event_type = %event.event_type, "audit event written directly to local store");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state(label: &str) -> (AppState, Utf8PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-audit-test-{label}-{nanos}")),
        )
        .expect("temp path UTF-8");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        (state, root)
    }

    // ── NoOpAuditBuffer ───────────────────────────────────────────────────────

    #[test]
    fn no_op_buffer_accepts_any_event() {
        let buf = NoOpAuditBuffer;
        let event = AuditEvent {
            event_type: "tool_called".to_string(),
            payload: serde_json::json!({"tool": "github__create_issue"}),
        };
        assert!(buf.record(&event).is_ok());
    }

    #[test]
    fn no_op_buffer_flush_returns_zero() {
        let (state, root) = temp_state("noop");
        let buf = NoOpAuditBuffer;
        let flushed = buf.flush(&state).unwrap();
        assert_eq!(flushed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── SqliteAuditBuffer ─────────────────────────────────────────────────────

    #[test]
    fn sqlite_buffer_records_events_in_db() {
        let (state, root) = temp_state("sqlite-record");
        let registry = Arc::new(RegistryClient::new("http://127.0.0.1:1"));
        let buf = SqliteAuditBuffer::new(Arc::clone(&registry), &state);

        let event = AuditEvent {
            event_type: "tool_called".to_string(),
            payload: serde_json::json!({"tool": "stub__echo"}),
        };
        buf.record(&event).unwrap();

        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE event_type = 'tool_called'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "event should be in audit_events table");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sqlite_buffer_flush_deletes_rows_on_success() {
        use mockito::Server;

        let (state, root) = temp_state("sqlite-flush");
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/runner/audit")
            .with_status(200)
            .create();

        let registry = Arc::new(RegistryClient::new(server.url()));
        let buf = SqliteAuditBuffer::new(Arc::clone(&registry), &state);

        for i in 0..5u32 {
            buf.record(&AuditEvent {
                event_type: "tool_called".to_string(),
                payload: serde_json::json!({"seq": i}),
            })
            .unwrap();
        }

        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 5);
        drop(conn);

        let flushed = buf.flush(&state).unwrap();
        assert_eq!(flushed, 5);

        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, 0,
            "all rows should be deleted after successful upload"
        );

        mock.assert();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sqlite_buffer_flush_leaves_rows_when_registry_unreachable() {
        let (state, root) = temp_state("sqlite-retry");
        // Point at a non-listening port.
        let registry = Arc::new(RegistryClient::new("http://127.0.0.1:1"));
        let buf = SqliteAuditBuffer::new(Arc::clone(&registry), &state);

        buf.record(&AuditEvent {
            event_type: "tool_called".to_string(),
            payload: serde_json::json!({}),
        })
        .unwrap();

        let result = buf.flush(&state);
        assert!(result.is_err(), "should fail when registry is unreachable");

        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "row should remain for retry after flush failure");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sqlite_buffer_flush_returns_zero_when_empty() {
        use mockito::Server;

        let (state, root) = temp_state("sqlite-empty");
        let mut server = Server::new();
        // Should not be called at all.
        server.mock("POST", "/api/runner/audit").expect(0).create();

        let registry = Arc::new(RegistryClient::new(server.url()));
        let buf = SqliteAuditBuffer::new(Arc::clone(&registry), &state);

        let flushed = buf.flush(&state).unwrap();
        assert_eq!(flushed, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sqlite_buffer_survives_daemon_restart() {
        use mockito::Server;

        let (state, root) = temp_state("sqlite-persist");
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/runner/audit")
            .with_status(200)
            .create();

        {
            // Simulate first daemon run: write events, then "crash" without flushing.
            let registry = Arc::new(RegistryClient::new(server.url()));
            let buf = SqliteAuditBuffer::new(Arc::clone(&registry), &state);
            for i in 0..3u32 {
                buf.record(&AuditEvent {
                    event_type: "tool_called".to_string(),
                    payload: serde_json::json!({"seq": i}),
                })
                .unwrap();
            }
            // Drop buf without flushing — events survive in SQLite.
        }

        // Simulate second daemon run: events should still be there and flushable.
        let registry2 = Arc::new(RegistryClient::new(server.url()));
        let buf2 = SqliteAuditBuffer::new(Arc::clone(&registry2), &state);
        let flushed = buf2.flush(&state).unwrap();
        assert_eq!(flushed, 3, "events from previous run should be uploaded");

        mock.assert();
        let _ = std::fs::remove_dir_all(&root);
    }
}
