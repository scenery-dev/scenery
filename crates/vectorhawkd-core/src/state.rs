use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use std::fs;

/// Bootstrapped application state: root directory and SQLite path.
///
/// The root directory is `~/Library/Application Support/VectorHawk/` on macOS
/// and `$XDG_DATA_HOME/vectorhawk/` (defaulting to `~/.local/share/vectorhawk/`)
/// on Linux. The daemon owns this state; the CLI reads it directly for now (M1
/// will decide whether CLI goes through the socket or reads SQLite directly).
pub struct AppState {
    pub root_dir: Utf8PathBuf,
    pub db_path: Utf8PathBuf,
}

impl AppState {
    /// Resolve the platform-appropriate data directory and bootstrap.
    ///
    /// Uses `dirs::data_dir()` which returns:
    /// - macOS: `~/Library/Application Support`
    /// - Linux: `$XDG_DATA_HOME` or `~/.local/share`
    /// - Windows: `%APPDATA%` (deferred — not supported in M0)
    pub fn bootstrap() -> Result<Self> {
        let base = dirs::data_dir()
            .context("failed to resolve platform data directory (HOME not set?)")?;
        let root_dir = Utf8PathBuf::from_path_buf(base.join("VectorHawk"))
            .map_err(|p| anyhow::anyhow!("non-UTF-8 data dir path: {}", p.display()))?;
        Self::bootstrap_in(root_dir)
    }

    /// Bootstrap state in a specific directory. Useful for tests and
    /// the daemon's `--state-dir` override flag (planned for M2).
    pub fn bootstrap_in(root_dir: Utf8PathBuf) -> Result<Self> {
        fs::create_dir_all(root_dir.join("skills"))
            .with_context(|| format!("failed to create skills dir under {root_dir}"))?;
        fs::create_dir_all(root_dir.join("cache"))
            .with_context(|| format!("failed to create cache dir under {root_dir}"))?;
        fs::create_dir_all(root_dir.join("logs"))
            .with_context(|| format!("failed to create logs dir under {root_dir}"))?;
        fs::create_dir_all(root_dir.join("policy"))
            .with_context(|| format!("failed to create policy dir under {root_dir}"))?;
        fs::create_dir_all(root_dir.join("tmp"))
            .with_context(|| format!("failed to create tmp dir under {root_dir}"))?;

        let db_path = root_dir.join("state.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open SQLite at {db_path}"))?;
        conn.execute_batch(SCHEMA_SQL)
            .context("failed to apply database schema")?;

        Ok(Self { root_dir, db_path })
    }

    /// Convenience: return the path where the daemon Unix socket is expected.
    ///
    /// macOS: `~/Library/Application Support/VectorHawk/agent.sock`
    /// Linux: `$XDG_RUNTIME_DIR/vectorhawk/agent.sock` (falls back to root_dir)
    pub fn socket_path(&self) -> Utf8PathBuf {
        #[cfg(target_os = "linux")]
        {
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
                let base = Utf8PathBuf::from_path_buf(
                    std::path::PathBuf::from(runtime).join("vectorhawk"),
                );
                if let Ok(base) = base {
                    return base.join("agent.sock");
                }
            }
        }
        // macOS and fallback: socket lives in the data dir alongside state.db
        self.root_dir.join("agent.sock")
    }
}

// ── SQLite schema ─────────────────────────────────────────────────────────────

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS installed_skills (
    skill_id TEXT PRIMARY KEY,
    active_version TEXT NOT NULL,
    install_root TEXT NOT NULL,
    channel TEXT,
    current_status TEXT NOT NULL DEFAULT 'active',
    installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS skill_versions (
    skill_id TEXT NOT NULL,
    version TEXT NOT NULL,
    install_path TEXT NOT NULL,
    source_type TEXT NOT NULL,
    installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(skill_id, version)
);

CREATE TABLE IF NOT EXISTS policy_cache (
    skill_id TEXT PRIMARY KEY,
    policy_json TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_tokens (
    registry_url TEXT PRIMARY KEY,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    saved_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS execution_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id TEXT NOT NULL,
    version TEXT NOT NULL,
    status TEXT NOT NULL,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    latency_ms INTEGER,
    executed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    uploaded INTEGER NOT NULL DEFAULT 0
);
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vh-state-tests-{label}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temp path should be UTF-8")
    }

    fn cleanup(path: &Utf8Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn bootstrap_in_creates_expected_directories_and_tables() {
        let root = temp_root("bootstrap");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        assert_eq!(state.root_dir, root);
        assert!(state.root_dir.join("skills").exists());
        assert!(state.root_dir.join("cache").exists());
        assert!(state.root_dir.join("logs").exists());
        assert!(state.root_dir.join("policy").exists());
        assert!(state.root_dir.join("tmp").exists());
        assert!(state.db_path.exists());

        let conn = Connection::open(&state.db_path).expect("db should open");
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN \
                 ('installed_skills','skill_versions','policy_cache','auth_tokens',\
                  'execution_history','audit_events')",
                [],
                |row| row.get(0),
            )
            .expect("should query sqlite_master");
        assert_eq!(table_count, 6, "all six tables should exist");

        cleanup(&state.root_dir);
    }

    #[test]
    fn bootstrap_in_is_idempotent() {
        let root = temp_root("idempotent");
        AppState::bootstrap_in(root.clone()).expect("first bootstrap should succeed");
        AppState::bootstrap_in(root.clone()).expect("second bootstrap should also succeed");
        cleanup(&root);
    }

    #[test]
    fn socket_path_is_inside_root_on_macos() {
        let root = temp_root("socket");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let sock = state.socket_path();
        // On macOS (and Linux without XDG_RUNTIME_DIR set in test env) should be in root_dir
        assert!(
            sock.as_str().contains("VectorHawk") || sock.as_str().contains("vh-state-tests"),
            "socket path should be under the data dir: {sock}"
        );
        cleanup(&root);
    }
}
