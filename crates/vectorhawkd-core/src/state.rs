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
#[derive(Clone)]
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

        // Idempotent column additions — these fail silently if the column
        // already exists (SQLITE_ERROR extended code 1 from the "duplicate column"
        // error). Any other error is a real problem and propagates.
        add_column_if_missing(
            &conn,
            "ALTER TABLE execution_history ADD COLUMN model_source TEXT",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE execution_history ADD COLUMN cost_usd REAL DEFAULT 0.0",
        )?;

        // RUN2: desired-state reconciler columns on installed_skills.
        add_column_if_missing(
            &conn,
            "ALTER TABLE installed_skills ADD COLUMN installation_id TEXT",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE installed_skills ADD COLUMN source TEXT DEFAULT 'local'",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE installed_skills ADD COLUMN deactivated INTEGER DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE installed_skills ADD COLUMN deactivated_at TEXT",
        )?;

        // RUN2: SSE resume state and device registration.
        conn.execute_batch(SCHEMA_RUN2_SQL)
            .context("failed to apply RUN2 schema additions")?;

        // G3: MCP installation desired-state table.
        conn.execute_batch(SCHEMA_G3_SQL)
            .context("failed to apply G3 schema additions")?;

        // F1: managed-paths reconciler marker table.
        conn.execute_batch(SCHEMA_F1_SQL)
            .context("failed to apply F1 schema additions")?;

        Ok(Self { root_dir, db_path })
    }

    /// Return all skill IDs currently tracked in `installed_skills`.
    ///
    /// Used by the daemon sync loop to determine which skills to pass to
    /// `check_skill_status`. Returns an empty vec if no skills are installed.
    /// Write the current Unix timestamp to the `meta` table as `last_sync_at`.
    pub fn record_sync_time(&self) -> Result<()> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_sync_at', ?1)",
            rusqlite::params![now.to_string()],
        )
        .context("failed to record sync time")?;
        Ok(())
    }

    pub fn list_installed_skill_ids(&self) -> Result<Vec<String>> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        let mut stmt = conn
            .prepare("SELECT skill_id FROM installed_skills")
            .context("failed to prepare skill id query")?;
        let ids: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("failed to query installed skills")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to collect skill ids")?;
        Ok(ids)
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

// ── Sync state helpers (RUN2) ─────────────────────────────────────────────────

impl AppState {
    /// Read a key from the `sync_state` key/value table.
    ///
    /// Returns `None` if the key does not exist or the table was not yet created.
    pub fn get_sync_state(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        let result = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to read sync_state")?;
        Ok(result)
    }

    /// Write a key/value pair to the `sync_state` table (upsert).
    pub fn set_sync_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )
        .context("failed to write sync_state")?;
        Ok(())
    }
}

// ── MCP installation state (G3) ──────────────────────────────────────────────

/// One row in the `mcp_installations` table.
#[derive(Debug, Clone)]
pub struct McpInstallRow {
    pub mcp_server_id: String,
    pub installation_id: String,
    pub mcp_server_name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    /// Raw JSON string from the `server_config` column.
    pub server_config: Option<String>,
    pub auth_type: String,
    pub gateway_server_id: Option<String>,
}

impl AppState {
    /// Upsert one MCP installation row.
    ///
    /// On conflict (same `mcp_server_id`) the row is fully replaced so that a
    /// re-install with a new `installation_id` or updated config is idempotent.
    pub fn upsert_mcp_install(&self, row: &McpInstallRow) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for mcp_installations upsert")?;
        conn.execute(
            "INSERT INTO mcp_installations \
             (mcp_server_id, installation_id, mcp_server_name, package_source, \
              version_pin, server_config, auth_type, gateway_server_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(mcp_server_id) DO UPDATE SET \
               installation_id  = excluded.installation_id, \
               mcp_server_name  = excluded.mcp_server_name, \
               package_source   = excluded.package_source, \
               version_pin      = excluded.version_pin, \
               server_config    = excluded.server_config, \
               auth_type        = excluded.auth_type, \
               gateway_server_id = excluded.gateway_server_id",
            rusqlite::params![
                row.mcp_server_id,
                row.installation_id,
                row.mcp_server_name,
                row.package_source,
                row.version_pin,
                row.server_config,
                row.auth_type,
                row.gateway_server_id,
            ],
        )
        .context("failed to upsert mcp_installations row")?;
        Ok(())
    }

    /// Delete one MCP installation row by `mcp_server_id`.
    ///
    /// No-ops if the row does not exist.
    pub fn delete_mcp_install(&self, mcp_server_id: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for mcp_installations delete")?;
        conn.execute(
            "DELETE FROM mcp_installations WHERE mcp_server_id = ?1",
            rusqlite::params![mcp_server_id],
        )
        .context("failed to delete mcp_installations row")?;
        Ok(())
    }

    /// Return all rows in `mcp_installations`.
    pub fn list_mcp_installs(&self) -> Result<Vec<McpInstallRow>> {
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for mcp_installations list")?;
        let mut stmt = conn
            .prepare(
                "SELECT mcp_server_id, installation_id, mcp_server_name, package_source, \
                 version_pin, server_config, auth_type, gateway_server_id \
                 FROM mcp_installations",
            )
            .context("failed to prepare mcp_installations list query")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(McpInstallRow {
                    mcp_server_id: row.get(0)?,
                    installation_id: row.get(1)?,
                    mcp_server_name: row.get(2)?,
                    package_source: row.get(3)?,
                    version_pin: row.get(4)?,
                    server_config: row.get(5)?,
                    auth_type: row.get(6)?,
                    gateway_server_id: row.get(7)?,
                })
            })
            .context("failed to query mcp_installations")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect mcp_installations rows")?;

        Ok(rows)
    }
}

// ── Schema helpers ────────────────────────────────────────────────────────────

/// Execute an `ALTER TABLE … ADD COLUMN …` statement, ignoring the error if
/// the column already exists.
///
/// SQLite surfaces duplicate-column errors as `SQLITE_ERROR` (extended code 1)
/// with message text containing "duplicate column name". We check the extended
/// code and swallow exactly that case; all other errors propagate.
fn add_column_if_missing(conn: &Connection, sql: &str) -> Result<()> {
    match conn.execute(sql, []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _)) if err.extended_code == 1 => {
            // Extended code 1 = SQLITE_ERROR, which SQLite emits for
            // "duplicate column name: <col>". Safe to ignore.
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("failed to execute: {sql}")),
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

-- GAP-05: per-skill rating storage (synced flag controls registry upload).
CREATE TABLE IF NOT EXISTS skill_ratings (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id TEXT NOT NULL,
    version  TEXT NOT NULL,
    rating   TEXT NOT NULL CHECK (rating IN ('up', 'down')),
    rated_at INTEGER NOT NULL,
    synced   INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_ratings_one_per_version
    ON skill_ratings (skill_id, version);

-- GAP-05: per-skill execution counts used for rating-prompt schedule and stats upload.
CREATE TABLE IF NOT EXISTS skill_execution_counts (
    skill_id        TEXT NOT NULL,
    version         TEXT NOT NULL,
    count           INTEGER NOT NULL DEFAULT 0,
    total_runs      INTEGER NOT NULL DEFAULT 0,
    successful_runs INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (skill_id, version)
);

-- Generic key/value store for daemon metadata (e.g. last_sync_at).
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Additive schema applied on top of SCHEMA_SQL.  All statements are
/// idempotent (`IF NOT EXISTS`) so they are safe to run on every startup.
const SCHEMA_RUN2_SQL: &str = r#"
-- RUN2: SSE resume / device-registration state.
CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// G3: MCP server desired-state table.
///
/// Rows are upserted on `install_mcp` events and deleted on `deactivate_mcp`.
/// The daemon regenerates `managed-mcp.json` from this table on every change.
const SCHEMA_G3_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS mcp_installations (
    mcp_server_id    TEXT PRIMARY KEY,
    installation_id  TEXT NOT NULL,
    mcp_server_name  TEXT NOT NULL,
    package_source   TEXT NOT NULL,
    version_pin      TEXT,
    server_config    TEXT,
    auth_type        TEXT NOT NULL,
    gateway_server_id TEXT,
    installed_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
"#;

/// F1: managed-paths reconciler marker table.
///
/// Tracks every skill, plugin, and MCP entry that has been migrated into
/// VectorHawk ownership.  The `path` column is the absolute filesystem path
/// (or `<path>:<key>` virtual key for MCP entries) and serves as the
/// idempotency key so re-runs are no-ops.
const SCHEMA_F1_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS managed_path_markers (
    path            TEXT NOT NULL,
    kind            TEXT NOT NULL,
    slug            TEXT NOT NULL,
    installation_id TEXT,
    source_sha256   TEXT NOT NULL,
    migrated_at     TEXT NOT NULL,
    PRIMARY KEY (path)
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
                  'execution_history','audit_events','skill_ratings','skill_execution_counts',\
                  'sync_state','mcp_installations','managed_path_markers')",
                [],
                |row| row.get(0),
            )
            .expect("should query sqlite_master");
        assert_eq!(table_count, 11, "all eleven tables should exist");

        cleanup(&state.root_dir);
    }

    #[test]
    fn bootstrap_in_is_idempotent() {
        let root = temp_root("idempotent");
        AppState::bootstrap_in(root.clone()).expect("first bootstrap should succeed");
        AppState::bootstrap_in(root.clone()).expect("second bootstrap should also succeed");
        cleanup(&root);
    }

    // macOS uses `~/Library/Application Support/VectorHawk/agent.sock`; Linux
    // uses `$XDG_RUNTIME_DIR/vectorhawk/agent.sock` which is NOT under the data
    // root the test creates. Gate this assertion to macOS only — the equivalent
    // Linux assertion would have to introspect XDG_RUNTIME_DIR.
    #[cfg(target_os = "macos")]
    #[test]
    fn socket_path_is_inside_root_on_macos() {
        let root = temp_root("socket");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");
        let sock = state.socket_path();
        assert!(
            sock.as_str().contains("VectorHawk") || sock.as_str().contains("vh-state-tests"),
            "socket path should be under the data dir: {sock}"
        );
        cleanup(&root);
    }
}
