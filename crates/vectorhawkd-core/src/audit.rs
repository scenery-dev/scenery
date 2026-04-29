use crate::state::AppState;
use anyhow::Result;
use serde::{Deserialize, Serialize};

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
///     and flushed to the registry on the 300 s background tick.
pub trait AuditBuffer: Send + Sync {
    /// Record an event. Non-fatal: implementors should log on error, not panic.
    fn record(&self, event: &AuditEvent) -> Result<()>;

    /// Flush buffered events to the registry. Returns the count uploaded.
    ///
    /// Called on the background 300 s tick. For the no-op impl this is always 0.
    fn flush(&self, state: &AppState) -> Result<usize>;
}

// ── No-op implementation ──────────────────────────────────────────────────────

/// Audit buffer that accepts events but does nothing with them.
/// Used by the M0 daemon and in tests that don't need audit verification.
pub struct NoOpAuditBuffer;

impl AuditBuffer for NoOpAuditBuffer {
    fn record(&self, _event: &AuditEvent) -> Result<()> {
        Ok(())
    }

    fn flush(&self, _state: &AppState) -> Result<usize> {
        Ok(0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state() -> (AppState, camino::Utf8PathBuf) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = camino::Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-audit-test-{nanos}")),
        )
        .expect("temp path UTF-8");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        (state, root)
    }

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
        let (state, root) = temp_state();
        let buf = NoOpAuditBuffer;
        let flushed = buf.flush(&state).unwrap();
        assert_eq!(flushed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
