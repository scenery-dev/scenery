use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// `sync_state` key the local MCP HTTP resource server's bound port
/// (Zero-CLI Phase 2, §8) is persisted under. Shared between
/// `vectorhawkd-daemon::mcp_http` (which writes it once, on first bind) and
/// `vectorhawkd-cli`'s `mcp setup` (which reads it back to build Claude
/// Code's HTTP+OAuth entry URL) so both sides read the exact same key.
pub const MCP_HTTP_PORT_SYNC_STATE_KEY: &str = "mcp_http_port";

/// `sync_state` key the local MCP HTTP resource server caches its resolved
/// bound-user-id under (the `sub` of the MCP access token this daemon is
/// currently bound to). Shared between `vectorhawkd-daemon::mcp_http` (which
/// reads/writes it) and [`AppState::clear_device_session`] (M6 — which must
/// clear it alongside `device_id`/`device_uuid` on logout/revocation so a
/// different user logging in afterward never gets rejected as a stale
/// `sub_mismatch`).
pub const MCP_USER_ID_SYNC_STATE_KEY: &str = "mcp_user_id";

/// `sync_state` key the per-user local key for the MCP HTTP resource
/// server's `X-VectorHawk-Local-Key` header is persisted under (H3 — defeats
/// another OS user on this machine from reaching `127.0.0.1:<mcp_http_port>`,
/// which has no same-user ACL the way the Unix socket does). User-private
/// because `sync_state` lives in `state.db` under the per-user data
/// directory. Shared between `vectorhawkd-daemon::mcp_http` (which checks
/// it on every request) and `vectorhawkd-cli`'s `mcp setup` (which writes it
/// into Claude Code's HTTP entry headers) — both read/write the same
/// `state.db`, so neither side ever needs to ask the other for the value.
pub const LOCAL_MCP_KEY_SYNC_STATE_KEY: &str = "mcp_local_key";

/// Env var that lets the local MCP HTTP resource server (and `mcp setup`'s
/// decision to write a Claude Code HTTP+OAuth entry) accept a non-`https://`
/// issuer/registry URL (M9). Loud, dev-only: every call site that honors
/// this logs a warning when it does. Tests exercise the HTTP RS against a
/// `mockito` `http://` server with this set; production registries are
/// always `https://`.
pub const MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV: &str = "VECTORHAWK_MCP_HTTP_ALLOW_INSECURE_ISSUER";

/// M9: whether the local MCP HTTP resource server may be started — or its
/// Claude Code config entry written — against `registry_url` as the OAuth
/// issuer. `true` for `https://`, or for anything when
/// [`MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV`] is exactly `"1"`. Pure/no I/O
/// besides the env read, so both `vectorhawkd-daemon::mcp_http::start_listener`
/// and `vectorhawkd-cli`'s `resolve_mcp_http_port` can call it and always
/// agree. Callers are responsible for logging loudly when the override is
/// what let an insecure issuer through — this function itself stays silent.
pub fn mcp_http_issuer_allowed(registry_url: &str) -> bool {
    registry_url.starts_with("https://")
        || std::env::var(MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV).as_deref() == Ok("1")
}

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
    /// Live-flippable kill switch for third-party (non-VectorHawk-brokered)
    /// inference calls. Seeded from `sync_state` on startup via
    /// `seed_block_third_party_from_sync_state`; later tasks flip it in
    /// response to remote policy pushes without a daemon restart.
    pub block_third_party_inference: Arc<AtomicBool>,
}

impl AppState {
    /// Resolve the platform-appropriate data directory (`<data_dir>/VectorHawk`)
    /// without bootstrapping SQLite or creating any subdirectories.
    ///
    /// Uses `dirs::data_dir()` which returns:
    /// - macOS: `~/Library/Application Support`
    /// - Linux: `$XDG_DATA_HOME` or `~/.local/share`
    /// - Windows: `%APPDATA%` (deferred — not supported in M0)
    ///
    /// Exposed standalone (not just via `bootstrap()`) for call sites that
    /// need `root_dir` — e.g. the restore journal — without paying for a
    /// full SQLite bootstrap.
    pub fn resolve_root_dir() -> Result<Utf8PathBuf> {
        let base = dirs::data_dir()
            .context("failed to resolve platform data directory (HOME not set?)")?;
        Utf8PathBuf::from_path_buf(base.join("VectorHawk"))
            .map_err(|p| anyhow::anyhow!("non-UTF-8 data dir path: {}", p.display()))
    }

    /// Resolve the platform-appropriate data directory and bootstrap.
    pub fn bootstrap() -> Result<Self> {
        Self::bootstrap_in(Self::resolve_root_dir()?)
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
        //
        // Legacy tables retired with the local-DB shrink:
        //   - execution_history: per-run telemetry, never uploaded
        //   - policy_cache:      moved to <root>/policy-cache.json
        // Opportunistically dropped here so upgraded daemons free the disk
        // space; brand-new installs never see them since they're not in
        // SCHEMA_SQL anymore.
        let _ = conn.execute_batch("DROP TABLE IF EXISTS execution_history");
        let _ = conn.execute_batch("DROP TABLE IF EXISTS policy_cache");

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

        // AUTH-BACKOFF: track refresh-token failure backoff state per registry.
        // When the daemon repeatedly hits 401 on /portal/auth/refresh (e.g.
        // user revoked their session), we escalate the retry interval instead
        // of hammering the endpoint every 60s. Columns are nullable so any
        // legacy row is treated as "fresh, no backoff".
        add_column_if_missing(
            &conn,
            "ALTER TABLE auth_tokens ADD COLUMN refresh_failures INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE auth_tokens ADD COLUMN next_refresh_attempt_at INTEGER",
        )?;
        add_column_if_missing(
            &conn,
            "ALTER TABLE auth_tokens ADD COLUMN last_refresh_status TEXT",
        )?;

        // G3: MCP installation desired-state table.
        conn.execute_batch(SCHEMA_G3_SQL)
            .context("failed to apply G3 schema additions")?;
        // Gateway-brokered remote servers: proxy URL the daemon connects to
        // with its own portal JWT. Nullable → legacy rows route via server_config.
        add_column_if_missing(
            &conn,
            "ALTER TABLE mcp_installations ADD COLUMN gateway_url TEXT",
        )?;

        // F1: managed-paths reconciler marker table.
        conn.execute_batch(SCHEMA_F1_SQL)
            .context("failed to apply F1 schema additions")?;

        // Adopt-publish auto-upload takeover tracking.
        conn.execute_batch(SCHEMA_ADOPT_SQL)
            .context("failed to apply adopt-takeover schema additions")?;

        // Broadcast-model artifact revocation: durable local cache of the
        // backend's revocation list, reconciled on every snapshot/connect.
        conn.execute_batch(SCHEMA_REVOKE_SQL)
            .context("failed to apply artifact-revocation schema additions")?;

        Ok(Self {
            root_dir,
            db_path,
            block_third_party_inference: Arc::new(AtomicBool::new(false)),
        })
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

    /// Return `(skill_id, install_root, active_version)` for every active
    /// install. Used by the F2 self-heal pass on startup
    /// (`managed_paths::pusher::push_missing_active_skills`) to re-push any
    /// active skill whose canonical `~/.agents/skills/<slug>/` directory has
    /// gone missing, which also re-points its `~/.claude/skills/<slug>` link.
    pub fn list_active_installed_skills(&self) -> Result<Vec<(String, String, String)>> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        let mut stmt = conn
            .prepare(
                "SELECT skill_id, install_root, active_version \
                 FROM installed_skills \
                 WHERE current_status = 'active'",
            )
            .context("failed to prepare active skills query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("failed to query active installed skills")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect active installed skills")?;
        Ok(rows)
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

    /// Remove a key from the `sync_state` table. No-op (not an error) if
    /// the key doesn't exist.
    pub fn delete_sync_state(&self, key: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        conn.execute("DELETE FROM sync_state WHERE key = ?1", [key])
            .context("failed to delete sync_state key")?;
        Ok(())
    }

    /// Whether the local MCP HTTP resource server (Zero-CLI Phase 2 — see
    /// `context/2026-10-13-zero-cli-http-mcp-oauth-design.md`) is enabled
    /// for this daemon.
    ///
    /// `VECTORHAWK_MCP_HTTP=0`/`false` (env, checked first so it always wins
    /// for an operator debugging a specific run) or
    /// `sync_state["mcp_http_enabled"]` set to `0`/`false` disables it.
    /// Default: **on**. Shared by `vectorhawkd-daemon::mcp_http` (which
    /// actually starts the listener) and `vectorhawkd-cli`'s `mcp setup`
    /// (which reads it back, via [`Self::get_sync_state`] on
    /// [`MCP_HTTP_PORT_SYNC_STATE_KEY`], to decide whether Claude Code gets
    /// the HTTP+OAuth entry) so both sides of that decision can never
    /// disagree about whether the feature is on.
    pub fn mcp_http_enabled(&self) -> bool {
        if let Ok(v) = std::env::var("VECTORHAWK_MCP_HTTP") {
            return !(v == "0" || v.eq_ignore_ascii_case("false"));
        }
        match self.get_sync_state("mcp_http_enabled") {
            Ok(Some(v)) => !(v == "0" || v.eq_ignore_ascii_case("false")),
            _ => true,
        }
    }

    /// The per-user local key for the MCP HTTP resource server's
    /// `X-VectorHawk-Local-Key` header (H3), generating and persisting one on
    /// first use. Stable across calls/processes — both the daemon and
    /// `mcp setup` read the same `state.db` row, so they always agree without
    /// an extra IPC round trip, and `mcp setup` re-running keeps the existing
    /// key rather than silently invalidating Claude Code's config.
    ///
    /// 256 bits from the OS CSPRNG (`getrandom`, same source
    /// [`generate_code_verifier`](crate::auth) already uses for PKCE
    /// verifiers), hex-encoded so it's a plain header-safe ASCII string with
    /// no base64 padding/URL-safety edge cases to get wrong in a header
    /// value.
    pub fn local_mcp_key(&self) -> Result<String> {
        if let Some(existing) = self.get_sync_state(LOCAL_MCP_KEY_SYNC_STATE_KEY)? {
            if !existing.is_empty() {
                return Ok(existing);
            }
        }
        self.rotate_local_mcp_key()
    }

    /// Generate a fresh local key and persist it, overwriting any existing
    /// value — `vectorhawk mcp setup --rotate-local-key`. Any client config
    /// written with the previous key stops working against this daemon
    /// until it's rewritten with the new one.
    pub fn rotate_local_mcp_key(&self) -> Result<String> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| {
            anyhow::anyhow!("failed to generate local MCP key (OS RNG unavailable): {e}")
        })?;
        let key = hex::encode(bytes);
        self.set_sync_state(LOCAL_MCP_KEY_SYNC_STATE_KEY, &key)?;
        Ok(key)
    }

    /// Clear this daemon's device/user session identity: `device_id`,
    /// `device_uuid`, and [`MCP_USER_ID_SYNC_STATE_KEY`] (the local MCP HTTP
    /// resource server's cached bound-user-id).
    ///
    /// M6: called by both `vectorhawk auth logout` and
    /// [`AppState::mark_device_revoked`]. Without clearing `mcp_user_id` too,
    /// a user who logs out and a *different* user who logs in afterward
    /// would have their valid MCP access token rejected as `sub_mismatch`
    /// against the first user's stale cached id — the daemon never asks to
    /// re-resolve it once cached. Idempotent: clearing an already-clear key
    /// is a no-op, never an error.
    pub fn clear_device_session(&self) -> Result<()> {
        self.delete_sync_state("device_id")?;
        self.delete_sync_state("device_uuid")?;
        self.delete_sync_state(MCP_USER_ID_SYNC_STATE_KEY)?;
        Ok(())
    }

    /// Record that the backend revoked THIS device (device-revocation-v2):
    /// clears the local `device_uuid`/`device_id` identity and stamps
    /// `device_revoked_at` for `vectorhawk auth status`/`doctor` to
    /// surface to the user.
    ///
    /// Deliberately does NOT itself trigger a new registration. Clearing
    /// `device_uuid` means the daemon's own unattended register-on-startup
    /// path (`register_device` in vectorhawkd-daemon) has nothing cached
    /// to resend — callers of THAT function are responsible for refusing
    /// to auto-generate and register a fresh identity on their own once
    /// `device_revoked_at` is set (see its doc comment), so a revoked
    /// device can only come back through a deliberate `vectorhawk auth
    /// pair`, which is the only code path that both generates a new
    /// `device_uuid` AND calls [`AppState::clear_device_revoked`].
    ///
    /// Idempotent — calling this repeatedly (e.g. a 403 on every sync tick
    /// before the task notices it should stop) is harmless.
    ///
    /// M6: also clears `mcp_user_id` via [`AppState::clear_device_session`] —
    /// a revoked device/user must not leave a stale bound-user-id behind
    /// that a subsequent re-pair as a *different* user could collide with.
    pub fn mark_device_revoked(&self) -> Result<()> {
        self.clear_device_session()?;
        self.set_sync_state("device_revoked_at", &chrono::Utc::now().to_rfc3339())?;
        Ok(())
    }

    /// Clear the `device_revoked_at` marker set by [`AppState::mark_device_revoked`].
    /// Called once a fresh `vectorhawk auth pair` has established a new
    /// device identity, so `auth status`/`doctor` stop reporting the old
    /// revocation against the (now different) device.
    pub fn clear_device_revoked(&self) -> Result<()> {
        self.delete_sync_state("device_revoked_at")
    }

    /// Return a clone of the shared handle for the "block third-party
    /// inference" kill switch. Cheap (`Arc` clone) — safe to call per
    /// request/session; all clones observe flips made through any handle.
    pub fn block_third_party_handle(&self) -> Arc<AtomicBool> {
        self.block_third_party_inference.clone()
    }

    /// Seed the in-memory kill switch from the persisted `sync_state` value,
    /// e.g. on daemon startup after a registry policy sync. Any string other
    /// than exactly `"true"` (including missing/unset) is treated as off.
    pub fn seed_block_third_party_from_sync_state(&self) {
        let on = self
            .get_sync_state("block_third_party_inference")
            .ok()
            .flatten()
            .as_deref()
            == Some("true");
        self.block_third_party_inference
            .store(on, Ordering::Relaxed);
    }

    /// Apply an org-policy snapshot fragment (as carried by the `org_policy`
    /// key of `GET /api/sync/snapshot` / the SSE `snapshot` event) to
    /// persisted `sync_state` and any live atomic it gates.
    ///
    /// Each parameter is `None` when the snapshot omits that field — an
    /// older backend, or a policy nobody has touched yet — and "no change"
    /// is the correct behavior: leave the existing persisted/live value
    /// alone, don't reset it to a default. A `Some` value is applied
    /// unconditionally, exactly like the corresponding live SSE delta
    /// (`managed_paths_policy_update` / `inference_policy_update`) would.
    ///
    /// This is what lets a daemon that was offline (or a one-shot
    /// `vectorhawk mcp sync`) during an admin toggle converge immediately on
    /// reconnect/next tick, instead of staying stale until the next live
    /// toggle or a restart (Card 3 — org-policy snapshot reconcile).
    pub fn apply_org_policy_snapshot(
        &self,
        managed_paths_mode: Option<&str>,
        block_third_party_inference: Option<bool>,
    ) {
        if let Some(mode) = managed_paths_mode {
            if let Err(e) = self.set_sync_state("managed_paths_mode", mode) {
                tracing::warn!(
                    error = %e,
                    mode,
                    "org policy snapshot: failed to persist managed_paths_mode"
                );
            }
        }

        if let Some(enabled) = block_third_party_inference {
            let v = if enabled { "true" } else { "false" };
            if let Err(e) = self.set_sync_state("block_third_party_inference", v) {
                tracing::warn!(
                    error = %e,
                    "org policy snapshot: failed to persist block_third_party_inference"
                );
            }
            // Flip the live atomic directly — the snapshot must take effect
            // immediately, not just on the next seed-from-sync_state (which
            // today only happens at daemon startup).
            self.block_third_party_inference
                .store(enabled, Ordering::Relaxed);
        }
    }

    /// Return the slugs of every MCP server that has been F2-pushed to
    /// `~/.claude.json` (i.e. it has a `managed_path_markers` row with
    /// `kind='mcp'`).
    ///
    /// Used by the aggregator to filter out tools whose backend is already
    /// surfaced as a per-server entry in Claude Code — otherwise the AI
    /// client sees the same tool both natively (under its own server name)
    /// and nested under the `vectorhawk` aggregator.
    pub fn list_managed_mcp_slugs(&self) -> Result<Vec<String>> {
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;
        let mut stmt = conn
            .prepare("SELECT slug FROM managed_path_markers WHERE kind = 'mcp'")
            .context("failed to prepare managed mcp slug query")?;
        let slugs = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("failed to query managed_path_markers")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect managed mcp slugs")?;
        Ok(slugs)
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
    /// Full gateway proxy URL for credential-brokered servers. When present the
    /// daemon connects here with its portal JWT instead of using `server_config`.
    pub gateway_url: Option<String>,
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
              version_pin, server_config, auth_type, gateway_server_id, gateway_url) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(mcp_server_id) DO UPDATE SET \
               installation_id  = excluded.installation_id, \
               mcp_server_name  = excluded.mcp_server_name, \
               package_source   = excluded.package_source, \
               version_pin      = excluded.version_pin, \
               server_config    = excluded.server_config, \
               auth_type        = excluded.auth_type, \
               gateway_server_id = excluded.gateway_server_id, \
               gateway_url      = excluded.gateway_url",
            rusqlite::params![
                row.mcp_server_id,
                row.installation_id,
                row.mcp_server_name,
                row.package_source,
                row.version_pin,
                row.server_config,
                row.auth_type,
                row.gateway_server_id,
                row.gateway_url,
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
                 version_pin, server_config, auth_type, gateway_server_id, gateway_url \
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
                    gateway_url: row.get(8)?,
                })
            })
            .context("failed to query mcp_installations")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect mcp_installations rows")?;

        Ok(rows)
    }
}

// ── Adopt-publish takeover tracking ──────────────────────────────────────────

impl AppState {
    /// Record that `slug` has a discovered `source_path` awaiting removal
    /// ("takeover") once a real VectorHawk-managed install for that slug is
    /// confirmed present on disk.
    ///
    /// Upsert — safe to call repeatedly for the same slug (e.g. on SSE
    /// redelivery of `discovery_adopted`, or a retried adopt). The most
    /// recently reported `source_path` wins.
    pub fn record_pending_adopt_takeover(&self, slug: &str, source_path: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for pending-takeover upsert")?;
        conn.execute(
            "INSERT INTO adopt_pending_takeovers (slug, source_path) VALUES (?1, ?2) \
             ON CONFLICT(slug) DO UPDATE SET source_path = excluded.source_path",
            rusqlite::params![slug, source_path],
        )
        .context("failed to upsert adopt_pending_takeovers row")?;
        Ok(())
    }

    /// Return the discovered `source_path` pending takeover for `slug`, if any.
    ///
    /// `None` means there is nothing to take over — either `slug` was never
    /// adopted from local discovery, or its takeover already completed.
    pub fn pending_adopt_takeover_source(&self, slug: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for pending-takeover lookup")?;
        let result = conn
            .query_row(
                "SELECT source_path FROM adopt_pending_takeovers WHERE slug = ?1",
                [slug],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to read adopt_pending_takeovers")?;
        Ok(result)
    }

    /// Clear the pending-takeover record for `slug`, e.g. after its
    /// `source_path` has been removed. No-ops if no row exists.
    pub fn clear_pending_adopt_takeover(&self, slug: &str) -> Result<()> {
        let conn = Connection::open(&self.db_path)
            .context("failed to open state DB for pending-takeover clear")?;
        conn.execute(
            "DELETE FROM adopt_pending_takeovers WHERE slug = ?1",
            rusqlite::params![slug],
        )
        .context("failed to delete adopt_pending_takeovers row")?;
        Ok(())
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

CREATE TABLE IF NOT EXISTS auth_tokens (
    registry_url TEXT PRIMARY KEY,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    saved_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
    gateway_url      TEXT,
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

/// Adopt-publish auto-upload takeover tracking.
///
/// Records skills adopted from local discovery whose original `source_path`
/// (e.g. `~/.agents/skills/<slug>`) has not yet been removed because a real
/// VectorHawk-managed install was not confirmed present on disk at adopt
/// time (outcome was `pending_review`, or the immediate publish+install had
/// not yet completed). The row is deleted once takeover (source removal)
/// completes — see `managed_paths::takeover` in the daemon crate.
const SCHEMA_ADOPT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS adopt_pending_takeovers (
    slug        TEXT PRIMARY KEY,
    source_path TEXT NOT NULL,
    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
"#;

/// Broadcast-model artifact revocation cache (see `crate::revocation`).
///
/// One row per revoked artifact (or artifact+version). `version` is the
/// empty string `''` as the sentinel for "whole artifact, every version" —
/// never NULL, so the `(artifact_type, artifact_key, version)` primary key
/// stays a clean idempotency target for upserts (NULL would never collide
/// with NULL in a unique index the way we need for "revoke again is a
/// no-op").
///
/// Populated two ways, both converging on the same table:
/// - the live `revoke` SSE event (`revocation::upsert_revocation`) — fast
///   path for an online daemon;
/// - a full-replace from the `revocations` key of a `snapshot` event or the
///   `GET /sync/snapshot` poll (`revocation::replace_all`) — the durable
///   path for a daemon that was offline when the live event fired, and the
///   same mechanism that lets an admin "unblock" clear the row (an entry
///   dropped from the snapshot's list is dropped here too).
const SCHEMA_REVOKE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS artifact_revocations (
    artifact_type TEXT NOT NULL,
    artifact_key  TEXT NOT NULL,
    version       TEXT NOT NULL DEFAULT '',
    revoked_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (artifact_type, artifact_key, version)
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
                 ('installed_skills','skill_versions','auth_tokens',\
                  'audit_events','skill_ratings','skill_execution_counts',\
                  'sync_state','mcp_installations','managed_path_markers',\
                  'adopt_pending_takeovers','artifact_revocations')",
                [],
                |row| row.get(0),
            )
            .expect("should query sqlite_master");
        assert_eq!(
            table_count, 11,
            "all eleven tables should exist (execution_history + policy_cache were retired)"
        );

        cleanup(&state.root_dir);
    }

    #[test]
    fn bootstrap_in_is_idempotent() {
        let root = temp_root("idempotent");
        AppState::bootstrap_in(root.clone()).expect("first bootstrap should succeed");
        AppState::bootstrap_in(root.clone()).expect("second bootstrap should also succeed");
        cleanup(&root);
    }

    // ── block_third_party_inference kill switch ──────────────────────────────

    #[test]
    fn seed_block_third_party_reads_sync_state() {
        let root = temp_root("block-third-party-seed");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        assert!(
            !state.block_third_party_handle().load(Ordering::Relaxed),
            "should default to off before seeding"
        );

        state
            .set_sync_state("block_third_party_inference", "true")
            .expect("set_sync_state should succeed");
        state.seed_block_third_party_from_sync_state();

        assert!(
            state.block_third_party_handle().load(Ordering::Relaxed),
            "handle should observe true after seeding from sync_state"
        );

        cleanup(&root);
    }

    #[test]
    fn seed_block_third_party_defaults_off_when_unset() {
        let root = temp_root("block-third-party-unset");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state.seed_block_third_party_from_sync_state();

        assert!(
            !state.block_third_party_handle().load(Ordering::Relaxed),
            "missing sync_state key should seed to off"
        );

        cleanup(&root);
    }

    // ── org-policy snapshot reconcile (Card 3) ───────────────────────────────

    #[test]
    fn apply_org_policy_snapshot_persists_and_flips_the_live_atomic() {
        let root = temp_root("org-policy-snapshot-both-set");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        assert!(!state.block_third_party_handle().load(Ordering::Relaxed));

        state.apply_org_policy_snapshot(Some("enforced"), Some(true));

        assert_eq!(
            state
                .get_sync_state("managed_paths_mode")
                .unwrap()
                .as_deref(),
            Some("enforced")
        );
        assert_eq!(
            state
                .get_sync_state("block_third_party_inference")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        assert!(
            state.block_third_party_handle().load(Ordering::Relaxed),
            "the live atomic must flip immediately, not just the persisted value"
        );

        cleanup(&root);
    }

    #[test]
    fn apply_org_policy_snapshot_absent_key_means_no_change() {
        let root = temp_root("org-policy-snapshot-absent");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        // Simulate a prior live toggle that already set the policy on.
        state
            .set_sync_state("block_third_party_inference", "true")
            .unwrap();
        state.seed_block_third_party_from_sync_state();
        assert!(state.block_third_party_handle().load(Ordering::Relaxed));

        // A snapshot that omits org_policy entirely (older backend, or simply
        // nothing changed) must leave both the persisted value and the live
        // atomic untouched — `None` means "no change", not "reset to false".
        state.apply_org_policy_snapshot(None, None);

        assert_eq!(
            state
                .get_sync_state("block_third_party_inference")
                .unwrap()
                .as_deref(),
            Some("true"),
            "absent field must not overwrite the persisted value"
        );
        assert!(
            state.block_third_party_handle().load(Ordering::Relaxed),
            "absent field must not reset the live atomic"
        );

        cleanup(&root);
    }

    #[test]
    fn apply_org_policy_snapshot_can_flip_the_flag_back_off() {
        let root = temp_root("org-policy-snapshot-off");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state.apply_org_policy_snapshot(None, Some(true));
        assert!(state.block_third_party_handle().load(Ordering::Relaxed));

        state.apply_org_policy_snapshot(None, Some(false));
        assert!(
            !state.block_third_party_handle().load(Ordering::Relaxed),
            "an explicit false must flip the switch back off (reversibility)"
        );
        assert_eq!(
            state
                .get_sync_state("block_third_party_inference")
                .unwrap()
                .as_deref(),
            Some("false")
        );

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

    // ── Adopt-publish takeover tracking ──────────────────────────────────────

    #[test]
    fn pending_adopt_takeover_roundtrips() {
        let root = temp_root("adopt-takeover-roundtrip");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        assert_eq!(
            state
                .pending_adopt_takeover_source("hello-world")
                .expect("lookup should not error"),
            None,
            "no record until one is written"
        );

        state
            .record_pending_adopt_takeover("hello-world", "/home/user/.agents/skills/hello-world")
            .expect("record should succeed");

        assert_eq!(
            state
                .pending_adopt_takeover_source("hello-world")
                .expect("lookup should succeed"),
            Some("/home/user/.agents/skills/hello-world".to_string())
        );

        state
            .clear_pending_adopt_takeover("hello-world")
            .expect("clear should succeed");

        assert_eq!(
            state
                .pending_adopt_takeover_source("hello-world")
                .expect("lookup after clear should succeed"),
            None,
            "record should be gone after clear"
        );

        cleanup(&root);
    }

    #[test]
    fn record_pending_adopt_takeover_upserts_latest_source_path() {
        let root = temp_root("adopt-takeover-upsert");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state
            .record_pending_adopt_takeover("hello-world", "/first/path")
            .expect("first record should succeed");
        state
            .record_pending_adopt_takeover("hello-world", "/second/path")
            .expect("re-recording the same slug should upsert, not error");

        assert_eq!(
            state
                .pending_adopt_takeover_source("hello-world")
                .expect("lookup should succeed"),
            Some("/second/path".to_string()),
            "the most recently recorded source_path should win"
        );

        cleanup(&root);
    }

    #[test]
    fn clear_pending_adopt_takeover_is_idempotent_when_absent() {
        let root = temp_root("adopt-takeover-clear-absent");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        // No row was ever recorded for this slug — clearing must not error.
        state
            .clear_pending_adopt_takeover("never-recorded")
            .expect("clearing an absent record should be a no-op, not an error");

        cleanup(&root);
    }

    #[test]
    fn delete_sync_state_removes_the_key_and_is_idempotent() {
        let root = temp_root("delete-sync-state");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state.set_sync_state("foo", "bar").expect("set");
        assert_eq!(
            state.get_sync_state("foo").unwrap(),
            Some("bar".to_string())
        );

        state.delete_sync_state("foo").expect("first delete");
        assert_eq!(state.get_sync_state("foo").unwrap(), None);

        // Deleting an already-absent key must not error.
        state
            .delete_sync_state("foo")
            .expect("second delete (no-op)");

        cleanup(&root);
    }

    #[test]
    fn mark_device_revoked_clears_identity_and_stamps_marker() {
        let root = temp_root("mark-device-revoked");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state
            .set_sync_state("device_uuid", "uuid-1")
            .expect("set uuid");
        state.set_sync_state("device_id", "id-1").expect("set id");
        state
            .set_sync_state(MCP_USER_ID_SYNC_STATE_KEY, "user-1")
            .expect("set mcp_user_id");

        state.mark_device_revoked().expect("mark revoked");

        assert_eq!(state.get_sync_state("device_uuid").unwrap(), None);
        assert_eq!(state.get_sync_state("device_id").unwrap(), None);
        assert_eq!(
            state.get_sync_state(MCP_USER_ID_SYNC_STATE_KEY).unwrap(),
            None,
            "M6: mark_device_revoked must also clear the cached mcp_user_id, \
             or a different user re-pairing afterward could hit a stale sub_mismatch"
        );
        assert!(
            state.get_sync_state("device_revoked_at").unwrap().is_some(),
            "device_revoked_at must be stamped"
        );

        cleanup(&root);
    }

    #[test]
    fn clear_device_session_clears_all_three_keys_and_is_idempotent() {
        let root = temp_root("clear-device-session");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state.set_sync_state("device_id", "id-1").expect("set id");
        state
            .set_sync_state("device_uuid", "uuid-1")
            .expect("set uuid");
        state
            .set_sync_state(MCP_USER_ID_SYNC_STATE_KEY, "user-1")
            .expect("set mcp_user_id");

        state.clear_device_session().expect("clear session");

        assert_eq!(state.get_sync_state("device_id").unwrap(), None);
        assert_eq!(state.get_sync_state("device_uuid").unwrap(), None);
        assert_eq!(
            state.get_sync_state(MCP_USER_ID_SYNC_STATE_KEY).unwrap(),
            None
        );

        // Idempotent — clearing an already-clear session must not error.
        state.clear_device_session().expect("clear again (no-op)");

        cleanup(&root);
    }

    #[test]
    fn local_mcp_key_is_generated_once_and_stable_across_calls() {
        let root = temp_root("local-mcp-key-stable");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        let first = state.local_mcp_key().expect("generate key");
        assert_eq!(first.len(), 64, "256 bits hex-encoded is 64 chars");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        let second = state.local_mcp_key().expect("read back same key");
        assert_eq!(first, second, "local_mcp_key must be stable once generated");

        // A second AppState handle onto the same state.db (mirrors the
        // daemon and `mcp setup` being separate processes against the same
        // state.db) must observe the exact same key, with no IPC required.
        let other_handle = AppState::bootstrap_in(root.clone()).expect("bootstrap again");
        assert_eq!(
            other_handle.local_mcp_key().expect("read via other handle"),
            first
        );

        cleanup(&root);
    }

    #[test]
    fn rotate_local_mcp_key_changes_the_persisted_value() {
        let root = temp_root("local-mcp-key-rotate");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        let original = state.local_mcp_key().expect("generate key");
        let rotated = state.rotate_local_mcp_key().expect("rotate key");

        assert_ne!(original, rotated, "rotation must produce a new key");
        assert_eq!(
            state.local_mcp_key().expect("read back"),
            rotated,
            "local_mcp_key must read back the rotated value, not the original"
        );

        cleanup(&root);
    }

    /// Serializes tests that mutate the process-global
    /// `VECTORHAWK_MCP_HTTP_ALLOW_INSECURE_ISSUER` env var — only one test
    /// in this binary touches it, but the guard documents the hazard for
    /// whoever adds the next one.
    static MCP_HTTP_ISSUER_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn mcp_http_issuer_allowed_requires_https_unless_override_env_set() {
        let _guard = MCP_HTTP_ISSUER_ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV);

        assert!(mcp_http_issuer_allowed("https://dev.vectorhawk.ai"));
        assert!(!mcp_http_issuer_allowed("http://dev.vectorhawk.ai"));
        assert!(!mcp_http_issuer_allowed("http://127.0.0.1:1234"));

        std::env::set_var(MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV, "1");
        assert!(
            mcp_http_issuer_allowed("http://127.0.0.1:1234"),
            "the explicit dev override must allow a non-https issuer"
        );

        std::env::remove_var(MCP_HTTP_ALLOW_INSECURE_ISSUER_ENV);
        assert!(!mcp_http_issuer_allowed("http://127.0.0.1:1234"));
    }

    #[test]
    fn clear_device_revoked_removes_the_marker() {
        let root = temp_root("clear-device-revoked");
        let state = AppState::bootstrap_in(root.clone()).expect("bootstrap");

        state.mark_device_revoked().expect("mark revoked");
        assert!(state.get_sync_state("device_revoked_at").unwrap().is_some());

        state.clear_device_revoked().expect("clear");
        assert_eq!(state.get_sync_state("device_revoked_at").unwrap(), None);

        // Clearing when nothing is set must not error.
        state.clear_device_revoked().expect("clear again (no-op)");

        cleanup(&root);
    }
}
