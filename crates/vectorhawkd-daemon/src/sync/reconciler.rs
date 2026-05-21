//! Desired-state reconciler.
//!
//! Consumes [`SyncEvent`]s from the SSE client channel and converges local
//! skill state:
//!
//! - `Install`    → download (or reuse cached), verify SHA-256, install, symlink.
//! - `Deactivate` → remove `active/` symlink; mark row in SQLite.
//! - `Purge`      → delete files; remove SQLite row.
//! - `Snapshot`   → diff vs. local state; enqueue derived events.
//!
//! After any state change [`Notifier`] fires `tools/list_changed` to all
//! connected shims.
//!
//! # Worker pool
//!
//! Install operations run in a pool of up to `MAX_CONCURRENT_INSTALLS`
//! concurrent `spawn_blocking` tasks.  Deactivate and Purge are serialised
//! (low volume).
//!
//! # Error handling
//!
//! On install failure: report `error` to the backend, retry once after 30s,
//! then give up and leave the installation in `error` state for the portal.

use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use uuid::Uuid;

use crate::sync::sse_client::{InstallationRecord, SyncEvent};
use vectorhawkd_core::{
    auth::load_all_tokens,
    registry::RegistryClient,
    state::{AppState, McpInstallRow},
};

/// One async mutex per skill_id (or mcp_server_id), used to serialize
/// operations on the same resource while letting different resources run in
/// parallel. The outer `std::sync::Mutex` is only held during the
/// get-or-insert lookup; the per-resource `tokio::sync::Mutex` guards the
/// actual work.
pub(crate) type SkillLockMap = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

pub(crate) fn skill_lock(locks: &SkillLockMap, skill_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = locks.lock().expect("skill_locks mutex poisoned");
    map.entry(skill_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_CONCURRENT_INSTALLS: usize = 4;

/// How long to wait before retrying a failed install.
const RETRY_DELAY_SECS: u64 = 30;

/// Coalesce interval: if multiple installs complete within this window, send
/// only one `tools/list_changed` notification.
const COALESCE_MS: u64 = 500;

// ── Reconciler state ──────────────────────────────────────────────────────────

/// Shared statistics updated by worker tasks and read by `doctor`.
#[derive(Debug, Default, Clone)]
pub struct ReconcilerStats {
    pub installed: u32,
    pub pending: u32,
    pub errors: u32,
    pub mcp_installs_handled: u32,
    pub mcp_deactivates_handled: u32,
    pub mcp_errors: u32,
}

/// Handle returned by [`spawn`], consumed by `doctor` output.
#[derive(Clone)]
pub struct ReconcilerHandle {
    stats: Arc<Mutex<ReconcilerStats>>,
}

impl ReconcilerHandle {
    /// Return a snapshot of the current reconciler statistics.
    pub fn stats(&self) -> ReconcilerStats {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawn the reconciler task and return a handle for status queries.
pub fn spawn(
    rx: mpsc::Receiver<SyncEvent>,
    state: Arc<AppState>,
    list_changed_tx: broadcast::Sender<()>,
) -> ReconcilerHandle {
    let stats = Arc::new(Mutex::new(ReconcilerStats::default()));
    let handle = ReconcilerHandle {
        stats: Arc::clone(&stats),
    };

    let registry_url = {
        // We read the registry URL from sync_state if available; otherwise the
        // reconciler falls back to the default production URL.
        // (The SSE config already has it — in a future refactor we'd pass it
        //  through SyncConfig.  For now we default to production.)
        std::env::var("VECTORHAWK_REGISTRY_URL")
            .ok()
            .unwrap_or_else(|| "https://app.vectorhawk.ai".to_string())
    };

    // Note: we do NOT cache the access_token here.  The SSE client may refresh
    // the token at any time (on 401).  `report_installation_status` loads the
    // current token from SQLite each call so it always uses the latest value.

    tokio::spawn(run_loop(rx, state, registry_url, list_changed_tx, stats));

    handle
}

// ── Main reconciler loop ──────────────────────────────────────────────────────

async fn run_loop(
    mut rx: mpsc::Receiver<SyncEvent>,
    state: Arc<AppState>,
    registry_url: String,
    list_changed_tx: broadcast::Sender<()>,
    stats: Arc<Mutex<ReconcilerStats>>,
) {
    // Semaphore limits concurrent install workers.
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_INSTALLS));

    // Per-skill serialization. Operations on the *same* skill_id (install,
    // deactivate, purge) acquire the same lock before touching filesystem
    // or SQLite, so a fast POST + DELETE pair can't interleave and leave
    // orphan symlinks (the race the v1.0.35 snapshot cross-check catches
    // reactively). Different skill_ids stay parallel under the semaphore.
    let skill_locks: SkillLockMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Track install worker join handles so we can coalesce notifications.
    let mut install_tasks: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();

    // Notification coalescing: track whether a notification is pending.
    let mut notify_pending = false;
    let mut coalesce_deadline: Option<tokio::time::Instant> = None;

    loop {
        // How long until the coalesce deadline (if any).
        let coalesce_sleep = match coalesce_deadline {
            Some(deadline) => {
                let now = tokio::time::Instant::now();
                if deadline <= now {
                    Duration::ZERO
                } else {
                    deadline - now
                }
            }
            None => Duration::from_secs(3600), // effectively infinite
        };

        tokio::select! {
            // ── Incoming SSE event ────────────────────────────────────────
            event = rx.recv() => {
                let event = match event {
                    Some(e) => e,
                    None => {
                        info!("reconciler: event channel closed — stopping");
                        break;
                    }
                };

                dispatch_event(
                    event,
                    &state,
                    &registry_url,
                    &sem,
                    &skill_locks,
                    &stats,
                    &mut install_tasks,
                ).await;

                // Process any snapshot-derived events. The current event was
                // already dispatched above; if it was a snapshot,
                // dispatch_event spawned all derived events into install_tasks
                // before returning.
            }

            // ── Install worker completion ─────────────────────────────────
            maybe_result = install_tasks.join_next(), if !install_tasks.is_empty() => {
                if let Some(result) = maybe_result {
                    let changed = match result {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(error = %e, "install worker panicked");
                            false
                        }
                    };
                    if changed {
                        fire_notification(&list_changed_tx, &mut notify_pending, &mut coalesce_deadline);
                    }
                }
            }

            // ── Coalesce deadline ─────────────────────────────────────────
            _ = tokio::time::sleep(coalesce_sleep), if notify_pending => {
                // Coalesce window elapsed — fire the notification.
                let _ = list_changed_tx.send(());
                notify_pending = false;
                coalesce_deadline = None;
            }
        }
    }
}

/// Spawn handler tasks for a single SSE event into `install_tasks`.
///
/// Every handler — install, deactivate, purge — acquires the per-skill
/// mutex before doing any work. Different skill_ids stay parallel under
/// the semaphore; rapid POST/DELETE/POST for the same skill_id is
/// serialized so the install/deactivate critical sections can't
/// interleave.
///
/// Snapshot events are not spawned directly; they fan out into derived
/// install/deactivate/purge events which are spawned the same way.
async fn dispatch_event(
    event: SyncEvent,
    state: &Arc<AppState>,
    registry_url: &str,
    sem: &Arc<tokio::sync::Semaphore>,
    skill_locks: &SkillLockMap,
    stats: &Arc<Mutex<ReconcilerStats>>,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    match event {
        SyncEvent::Install {
            installation_id,
            skill_id,
            version,
        } => {
            spawn_install(
                installation_id,
                skill_id,
                version,
                state,
                registry_url,
                sem,
                skill_locks,
                stats,
                install_tasks,
            );
        }
        SyncEvent::Deactivate {
            installation_id,
            skill_id,
        } => {
            spawn_deactivate(
                installation_id,
                skill_id,
                state,
                registry_url,
                skill_locks,
                install_tasks,
            );
        }
        SyncEvent::Purge {
            installation_id,
            skill_id,
        } => {
            spawn_purge(
                installation_id,
                skill_id,
                state,
                registry_url,
                skill_locks,
                install_tasks,
            );
        }
        SyncEvent::InstallMcp {
            installation_id,
            mcp_server_id,
            mcp_server_name,
            package_source,
            version_pin,
            server_config,
            auth_type,
            gateway_server_id,
        } => {
            spawn_install_mcp(
                installation_id,
                mcp_server_id,
                mcp_server_name,
                package_source,
                version_pin,
                server_config,
                auth_type,
                gateway_server_id,
                state,
                registry_url,
                skill_locks,
                stats,
                install_tasks,
            );
        }
        SyncEvent::DeactivateMcp {
            installation_id,
            mcp_server_id,
        } => {
            spawn_deactivate_mcp(
                installation_id,
                mcp_server_id,
                state,
                registry_url,
                skill_locks,
                stats,
                install_tasks,
            );
        }
        SyncEvent::Snapshot { installations } => {
            let derived = build_derived_events(installations, Arc::clone(state)).await;
            for d in derived {
                match d {
                    SyncEvent::Install {
                        installation_id,
                        skill_id,
                        version,
                    } => {
                        spawn_install(
                            installation_id,
                            skill_id,
                            version,
                            state,
                            registry_url,
                            sem,
                            skill_locks,
                            stats,
                            install_tasks,
                        );
                    }
                    SyncEvent::Deactivate {
                        installation_id,
                        skill_id,
                    } => {
                        spawn_deactivate(
                            installation_id,
                            skill_id,
                            state,
                            registry_url,
                            skill_locks,
                            install_tasks,
                        );
                    }
                    SyncEvent::Purge {
                        installation_id,
                        skill_id,
                    } => {
                        spawn_purge(
                            installation_id,
                            skill_id,
                            state,
                            registry_url,
                            skill_locks,
                            install_tasks,
                        );
                    }
                    SyncEvent::Snapshot { .. } => {
                        // Nested snapshots not expected; ignore.
                    }
                    // TODO(G3-snapshot): MCP installations are not yet reconciled
                    // from the snapshot payload. The snapshot shape would need a
                    // top-level `mcp_installations` key from the backend. Add
                    // that reconciliation path when the backend ships it.
                    SyncEvent::InstallMcp { .. } | SyncEvent::DeactivateMcp { .. } => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_install(
    installation_id: Uuid,
    skill_id: String,
    version: String,
    state: &Arc<AppState>,
    registry_url: &str,
    sem: &Arc<tokio::sync::Semaphore>,
    skill_locks: &SkillLockMap,
    stats: &Arc<Mutex<ReconcilerStats>>,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    let st = Arc::clone(state);
    let reg_url = registry_url.to_string();
    let sem_clone = Arc::clone(sem);
    let stats_clone = Arc::clone(stats);
    let lock = skill_lock(skill_locks, &skill_id);

    increment_pending(stats);

    install_tasks.spawn(async move {
        // Serialize against any in-flight deactivate/purge/install for the
        // same skill. Acquired before the semaphore so a stalled per-skill
        // queue doesn't burn a semaphore permit.
        let _skill_guard = lock.lock_owned().await;
        let _permit = sem_clone.acquire().await;
        handle_install(
            installation_id,
            &skill_id,
            &version,
            &st,
            &reg_url,
            &stats_clone,
        )
        .await
    });
}

fn spawn_deactivate(
    installation_id: Uuid,
    skill_id: String,
    state: &Arc<AppState>,
    registry_url: &str,
    skill_locks: &SkillLockMap,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    let st = Arc::clone(state);
    let reg_url = registry_url.to_string();
    let lock = skill_lock(skill_locks, &skill_id);

    install_tasks.spawn(async move {
        let _skill_guard = lock.lock_owned().await;
        handle_deactivate(installation_id, &skill_id, &st, &reg_url).await
    });
}

fn spawn_purge(
    installation_id: Uuid,
    skill_id: String,
    state: &Arc<AppState>,
    registry_url: &str,
    skill_locks: &SkillLockMap,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    let st = Arc::clone(state);
    let reg_url = registry_url.to_string();
    let lock = skill_lock(skill_locks, &skill_id);

    install_tasks.spawn(async move {
        let _skill_guard = lock.lock_owned().await;
        handle_purge(installation_id, &skill_id, &st, &reg_url).await
    });
}

// ── MCP install/deactivate spawners ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn spawn_install_mcp(
    installation_id: Uuid,
    mcp_server_id: Uuid,
    mcp_server_name: String,
    package_source: String,
    version_pin: Option<String>,
    server_config: Option<serde_json::Value>,
    auth_type: String,
    gateway_server_id: Option<String>,
    state: &Arc<AppState>,
    registry_url: &str,
    skill_locks: &SkillLockMap,
    stats: &Arc<Mutex<ReconcilerStats>>,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    let st = Arc::clone(state);
    let reg_url = registry_url.to_string();
    let stats_clone = Arc::clone(stats);
    // Key the per-resource lock by mcp_server_id to prevent races on the same
    // server (e.g. rapid install → deactivate → install).
    let lock = skill_lock(skill_locks, &mcp_server_id.to_string());

    install_tasks.spawn(async move {
        let _guard = lock.lock_owned().await;
        handle_install_mcp(
            installation_id,
            mcp_server_id,
            mcp_server_name,
            package_source,
            version_pin,
            server_config,
            auth_type,
            gateway_server_id,
            &st,
            &reg_url,
            &stats_clone,
        )
        .await
    });
}

fn spawn_deactivate_mcp(
    installation_id: Uuid,
    mcp_server_id: Uuid,
    state: &Arc<AppState>,
    registry_url: &str,
    skill_locks: &SkillLockMap,
    stats: &Arc<Mutex<ReconcilerStats>>,
    install_tasks: &mut tokio::task::JoinSet<bool>,
) {
    let st = Arc::clone(state);
    let reg_url = registry_url.to_string();
    let stats_clone = Arc::clone(stats);
    let lock = skill_lock(skill_locks, &mcp_server_id.to_string());

    install_tasks.spawn(async move {
        let _guard = lock.lock_owned().await;
        handle_deactivate_mcp(installation_id, mcp_server_id, &st, &reg_url, &stats_clone).await
    });
}

// ── MCP install handler ───────────────────────────────────────────────────────

/// Handle one `InstallMcp` event.  Returns `true` if the tool list changed.
#[allow(clippy::too_many_arguments)]
async fn handle_install_mcp(
    installation_id: Uuid,
    mcp_server_id: Uuid,
    mcp_server_name: String,
    package_source: String,
    version_pin: Option<String>,
    server_config: Option<serde_json::Value>,
    auth_type: String,
    gateway_server_id: Option<String>,
    state: &Arc<AppState>,
    registry_url: &str,
    stats: &Arc<Mutex<ReconcilerStats>>,
) -> bool {
    report_mcp_installation_status(installation_id, "installing", None, registry_url, state).await;

    let server_config_str = server_config
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok());

    let row = McpInstallRow {
        mcp_server_id: mcp_server_id.to_string(),
        installation_id: installation_id.to_string(),
        mcp_server_name: mcp_server_name.clone(),
        package_source: package_source.clone(),
        version_pin: version_pin.clone(),
        server_config: server_config_str,
        auth_type: auth_type.clone(),
        gateway_server_id: gateway_server_id.clone(),
    };

    let state_clone = Arc::clone(state);
    let result = tokio::task::spawn_blocking(move || {
        state_clone
            .upsert_mcp_install(&row)
            .context("failed to upsert mcp_installations row")?;
        write_managed_mcp_json(&state_clone)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(
                mcp_server_id = %mcp_server_id,
                mcp_server_name,
                "reconciler: MCP server installed"
            );
            report_mcp_installation_status(installation_id, "installed", None, registry_url, state)
                .await;
            increment_mcp_installs(stats);
            // NOTE: No existing broadcast path for tools/list_changed for MCP
            // servers specifically — the notification channel is keyed on skill
            // changes via the JoinSet return value. Returning `true` here will
            // fire a tools/list_changed notification to all connected shims,
            // which is the correct behavior since managed-mcp.json changed.
            true
        }
        Ok(Err(e)) => {
            warn!(
                mcp_server_id = %mcp_server_id,
                error = %e,
                "reconciler: MCP server install failed"
            );
            report_mcp_installation_status(
                installation_id,
                "error",
                Some(&e.to_string()),
                registry_url,
                state,
            )
            .await;
            increment_mcp_errors(stats);
            false
        }
        Err(e) => {
            warn!(
                mcp_server_id = %mcp_server_id,
                error = %e,
                "reconciler: MCP server install task panicked"
            );
            increment_mcp_errors(stats);
            false
        }
    }
}

// ── MCP deactivate handler ────────────────────────────────────────────────────

/// Handle one `DeactivateMcp` event.  Returns `true` if the tool list changed.
async fn handle_deactivate_mcp(
    installation_id: Uuid,
    mcp_server_id: Uuid,
    state: &Arc<AppState>,
    registry_url: &str,
    stats: &Arc<Mutex<ReconcilerStats>>,
) -> bool {
    let state_clone = Arc::clone(state);
    let server_id_str = mcp_server_id.to_string();

    let result = tokio::task::spawn_blocking(move || {
        state_clone
            .delete_mcp_install(&server_id_str)
            .context("failed to delete mcp_installations row")?;
        write_managed_mcp_json(&state_clone)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(mcp_server_id = %mcp_server_id, "reconciler: MCP server deactivated");
            report_mcp_installation_status(
                installation_id,
                "deactivated",
                None,
                registry_url,
                state,
            )
            .await;
            increment_mcp_deactivates(stats);
            true
        }
        Ok(Err(e)) => {
            warn!(
                mcp_server_id = %mcp_server_id,
                error = %e,
                "reconciler: MCP server deactivate failed"
            );
            increment_mcp_errors(stats);
            false
        }
        Err(e) => {
            warn!(
                mcp_server_id = %mcp_server_id,
                error = %e,
                "reconciler: MCP server deactivate task panicked"
            );
            increment_mcp_errors(stats);
            false
        }
    }
}

// ── managed-mcp.json writer ───────────────────────────────────────────────────

/// Regenerate `managed-mcp.json` from the `mcp_installations` SQLite table.
///
/// Uses the atomic write pattern: write to a `.tmp` file then rename so
/// readers (the MCP server) never see a partial write.
///
/// Path: `{state.root_dir}/managed-mcp.json`
fn write_managed_mcp_json(state: &AppState) -> Result<()> {
    let rows = state
        .list_mcp_installs()
        .context("failed to read mcp_installations for managed-mcp.json")?;

    let mut servers = Vec::with_capacity(rows.len());
    for row in &rows {
        let server_config: Option<serde_json::Value> = row
            .server_config
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("failed to deserialise server_config JSON from mcp_installations")?;

        servers.push(serde_json::json!({
            "name": row.mcp_server_name,
            "package_source": row.package_source,
            "version_pin": row.version_pin,
            "server_config": server_config,
            "auth_type": row.auth_type,
            "gateway_server_id": row.gateway_server_id,
        }));
    }

    let payload = serde_json::json!({ "servers": servers });
    let json_bytes =
        serde_json::to_vec_pretty(&payload).context("failed to serialise managed-mcp.json")?;

    let dest = state.root_dir.as_std_path().join("managed-mcp.json");
    let tmp = state.root_dir.as_std_path().join("managed-mcp.json.tmp");

    std::fs::write(&tmp, &json_bytes)
        .with_context(|| format!("failed to write managed-mcp.json.tmp to {}", tmp.display()))?;

    std::fs::rename(&tmp, &dest).with_context(|| {
        format!(
            "failed to rename managed-mcp.json.tmp → managed-mcp.json at {}",
            dest.display()
        )
    })?;

    tracing::debug!(
        path = %dest.display(),
        server_count = rows.len(),
        "reconciler: managed-mcp.json written"
    );
    Ok(())
}

// ── MCP PATCH callback ────────────────────────────────────────────────────────

/// Send `PATCH /api/mcp-installations/{id}` to report a state transition.
///
/// Mirrors `report_installation_status` for skills. Fire-and-forget: failures
/// are logged at WARN but do not affect local state.
async fn report_mcp_installation_status(
    installation_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    registry_url: &str,
    state: &Arc<AppState>,
) {
    let url = format!(
        "{}/api/mcp-installations/{}",
        registry_url.trim_end_matches('/'),
        installation_id
    );

    let mut body = serde_json::json!({ "state": status });
    if let Some(msg) = error_message {
        body["error_message"] = serde_json::Value::String(msg.to_string());
    }

    let reg_url = registry_url.to_string();
    let state_clone = Arc::clone(state);
    let access_token = tokio::task::spawn_blocking(move || {
        load_all_tokens(&state_clone)
            .ok()
            .and_then(|rows| {
                rows.into_iter()
                    .find(|r| r.registry_url == reg_url)
                    .map(|r| r.access_token)
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "reconciler: failed to build HTTP client for MCP status report");
            return;
        }
    };

    let req = if access_token.is_empty() {
        client.patch(&url).json(&body)
    } else {
        client.patch(&url).bearer_auth(access_token).json(&body)
    };

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(
                installation_id = %installation_id,
                status,
                "reconciler: MCP status reported"
            );
        }
        Ok(resp) => {
            warn!(
                installation_id = %installation_id,
                status,
                http_status = %resp.status(),
                "reconciler: MCP status report returned non-success"
            );
        }
        Err(e) => {
            warn!(
                installation_id = %installation_id,
                status,
                error = %e,
                "reconciler: MCP status report failed"
            );
        }
    }
}

/// Schedule a `tools/list_changed` notification within the coalesce window.
fn fire_notification(
    _tx: &broadcast::Sender<()>,
    pending: &mut bool,
    deadline: &mut Option<tokio::time::Instant>,
) {
    if !*pending {
        *pending = true;
        *deadline = Some(tokio::time::Instant::now() + Duration::from_millis(COALESCE_MS));
    }
    // If already pending, just let the existing deadline stand — coalescing.
}

// ── Stat helpers ──────────────────────────────────────────────────────────────

fn increment_pending(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.pending = g.pending.saturating_add(1);
    }
}

fn decrement_pending_inc_installed(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.pending = g.pending.saturating_sub(1);
        g.installed = g.installed.saturating_add(1);
    }
}

fn decrement_pending_inc_errors(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.pending = g.pending.saturating_sub(1);
        g.errors = g.errors.saturating_add(1);
    }
}

fn increment_mcp_installs(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.mcp_installs_handled = g.mcp_installs_handled.saturating_add(1);
    }
}

fn increment_mcp_deactivates(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.mcp_deactivates_handled = g.mcp_deactivates_handled.saturating_add(1);
    }
}

fn increment_mcp_errors(stats: &Arc<Mutex<ReconcilerStats>>) {
    if let Ok(mut g) = stats.lock() {
        g.mcp_errors = g.mcp_errors.saturating_add(1);
    }
}

// ── Install handler ───────────────────────────────────────────────────────────

/// Handle one `Install` event.  Returns `true` if the tool list changed.
async fn handle_install(
    installation_id: Uuid,
    skill_id: &str,
    version: &str,
    state: &Arc<AppState>,
    registry_url: &str,
    stats: &Arc<Mutex<ReconcilerStats>>,
) -> bool {
    let result = do_install(installation_id, skill_id, version, state, registry_url).await;

    match result {
        Ok(()) => {
            decrement_pending_inc_installed(stats);
            true
        }
        Err(e) => {
            warn!(
                skill_id,
                version,
                error = %e,
                "reconciler: install failed — retrying in {RETRY_DELAY_SECS}s"
            );
            // Report error to backend.
            report_installation_status(
                installation_id,
                "error",
                Some(&e.to_string()),
                registry_url,
                state,
            )
            .await;

            // Wait then retry once.
            tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
            match do_install(installation_id, skill_id, version, state, registry_url).await {
                Ok(()) => {
                    decrement_pending_inc_installed(stats);
                    true
                }
                Err(retry_err) => {
                    warn!(
                        skill_id,
                        version,
                        error = %retry_err,
                        "reconciler: install retry failed — leaving in error state"
                    );
                    report_installation_status(
                        installation_id,
                        "error",
                        Some(&retry_err.to_string()),
                        registry_url,
                        state,
                    )
                    .await;
                    decrement_pending_inc_errors(stats);
                    false
                }
            }
        }
    }
}

/// Perform the actual install: download artifact, verify SHA-256, install, update SQLite.
async fn do_install(
    installation_id: Uuid,
    skill_id: &str,
    version: &str,
    state: &Arc<AppState>,
    registry_url: &str,
) -> Result<()> {
    let skill_id = skill_id.to_string();
    let version = version.to_string();
    let state_clone = Arc::clone(state);
    let reg_url = registry_url.to_string();

    // Check if this version is already installed locally — if so, just flip symlink.
    let already_local = check_version_local(&state_clone, &skill_id, &version).await?;
    if already_local {
        info!(
            skill_id,
            version, "reconciler: version already local — flipping symlink"
        );
        flip_active_symlink(Arc::clone(&state_clone), skill_id.clone(), version.clone()).await?;
        report_installation_status(installation_id, "installed", None, &reg_url, &state_clone)
            .await;
        return Ok(());
    }

    // Report "installing" to backend.
    report_installation_status(installation_id, "installing", None, &reg_url, &state_clone).await;

    // Clone reg_url before moving into closure.
    let reg_url_for_install = reg_url.clone();
    let skill_id_for_install = skill_id.clone();
    let version_for_install = version.clone();

    // Download + install on blocking thread.
    tokio::task::spawn_blocking(move || {
        install_from_registry_blocking(
            &state_clone,
            &reg_url_for_install,
            &skill_id_for_install,
            &version_for_install,
            installation_id,
        )
    })
    .await
    .context("install_blocking task panicked")??;

    report_installation_status(installation_id, "installed", None, &reg_url, state).await;
    Ok(())
}

/// Check if a specific version of a skill is already installed in the versioned
/// directory layout (i.e. `skills/{skill_id}/versions/{version}/` exists).
async fn check_version_local(state: &Arc<AppState>, skill_id: &str, version: &str) -> Result<bool> {
    let version_dir = state
        .root_dir
        .join("skills")
        .join(skill_id)
        .join("versions")
        .join(version);
    Ok(version_dir.exists())
}

/// Flip the `active/` symlink to point at an already-installed version directory.
async fn flip_active_symlink(
    state: Arc<AppState>,
    skill_id: String,
    version: String,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let install_root = state.root_dir.join("skills").join(&skill_id);
        let version_dir = install_root.join("versions").join(&version);
        let active_dir = install_root.join("active");

        if active_dir.exists() || active_dir.is_symlink() {
            std::fs::remove_file(&active_dir)
                .or_else(|_| std::fs::remove_dir_all(&active_dir))
                .ok();
        }

        #[cfg(target_family = "unix")]
        std::os::unix::fs::symlink(&version_dir, &active_dir)
            .with_context(|| format!("failed to create active symlink for {skill_id}@{version}"))?;

        #[cfg(not(target_family = "unix"))]
        anyhow::bail!("symlink not supported on this platform");

        // Update SQLite row.
        let conn = rusqlite::Connection::open(&state.db_path)
            .context("failed to open state DB for symlink flip")?;
        conn.execute(
            "UPDATE installed_skills \
             SET active_version = ?1, deactivated = 0, deactivated_at = NULL, \
                 current_status = 'active' \
             WHERE skill_id = ?2",
            rusqlite::params![version, skill_id],
        )
        .context("failed to update installed_skills after symlink flip")?;

        // Mirror the active dir into ~/.claude/skills/<id> so Claude Code's
        // native Skills mechanism picks it up alongside the runner's MCP
        // exposure. install_unpacked_skill already does this; flip path
        // must too so a "re-install of already-local version" stays in sync.
        vectorhawkd_core::installer::register_in_claude_skills(&skill_id, &active_dir);

        Ok(())
    })
    .await
    .context("flip_active_symlink task panicked")?
}

/// Download the artifact from the registry CDN and install it into the versioned layout.
/// Called from a `spawn_blocking` context.
fn install_from_registry_blocking(
    state: &AppState,
    registry_url: &str,
    skill_id: &str,
    version: &str,
    installation_id: Uuid,
) -> Result<()> {
    use vectorhawkd_core::installer::{install_unpacked_skill, InstallMode};
    use vectorhawkd_manifest::SkillPackage;

    let registry = RegistryClient::new(registry_url);

    // Fetch artifact metadata (SHA-256, download URL).
    let meta = registry
        .fetch_artifact_metadata(skill_id, version)
        .with_context(|| format!("failed to fetch artifact metadata for {skill_id}@{version}"))?;

    // Download to a temp path.
    let tmp_path = state
        .root_dir
        .join("tmp")
        .join(format!("{skill_id}-{version}-{installation_id}.cskill.tmp"));

    registry
        .download_artifact(&meta.download_url, &meta.sha256, &tmp_path)
        .with_context(|| format!("artifact download failed for {skill_id}@{version}"))?;

    // Unpack the .cskill archive to a temp directory.
    let unpack_dir = state
        .root_dir
        .join("tmp")
        .join(format!("{skill_id}-{version}-{installation_id}-unpacked"));

    unpack_cskill_archive(&tmp_path, &unpack_dir)
        .with_context(|| format!("failed to unpack .cskill for {skill_id}@{version}"))?;

    // Clean up the downloaded archive.
    let _ = std::fs::remove_file(&tmp_path);

    // Load and validate the unpacked bundle.
    let pkg = SkillPackage::load_from_dir(&unpack_dir).with_context(|| {
        format!("failed to load unpacked skill bundle for {skill_id}@{version}")
    })?;

    // Install into the versioned layout.
    install_unpacked_skill(state, &pkg, InstallMode::Copy)
        .with_context(|| format!("install_unpacked_skill failed for {skill_id}@{version}"))?;

    // Record installation_id and source in the SQLite row.
    let conn = rusqlite::Connection::open(&state.db_path)
        .context("failed to open state DB after install")?;
    conn.execute(
        "UPDATE installed_skills SET installation_id = ?1, source = 'registry', deactivated = 0 \
         WHERE skill_id = ?2",
        rusqlite::params![installation_id.to_string(), skill_id],
    )
    .context("failed to record installation_id after install")?;

    // Clean up unpack directory.
    let _ = std::fs::remove_dir_all(&unpack_dir);

    info!(
        skill_id,
        version, "reconciler: skill installed from registry"
    );
    Ok(())
}

/// Unpack a `.cskill` archive into `dest`.
///
/// Supports two on-disk formats:
/// - **tar.gz** (`\x1f\x8b` magic): produced by the backend compile pipeline.
/// - **ZIP** (`PK\x03\x04` magic): legacy format; kept for forward-compat.
///
/// The format is auto-detected by reading the first two magic bytes.
fn unpack_cskill_archive(archive_path: &camino::Utf8Path, dest: &camino::Utf8Path) -> Result<()> {
    use std::io::Read;

    // Peek at the first two bytes to detect the format.
    let mut magic = [0u8; 2];
    {
        let mut f = std::fs::File::open(archive_path).with_context(|| {
            format!("failed to open archive for magic detection: {archive_path}")
        })?;
        f.read_exact(&mut magic)
            .with_context(|| format!("archive too small to detect format: {archive_path}"))?;
    }

    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create unpack dir: {dest}"))?;

    if magic == [0x1f, 0x8b] {
        // tar.gz format — used by backend compile pipeline.
        unpack_tar_gz(archive_path, dest)
    } else if magic == [0x50, 0x4b] {
        // ZIP format (PK magic).
        unpack_zip(archive_path, dest)
    } else {
        anyhow::bail!(
            "unrecognised archive format (magic bytes {:02x}{:02x}): {archive_path}",
            magic[0],
            magic[1]
        )
    }
}

/// Unpack a tar.gz archive into `dest`, stripping a single top-level directory
/// if all entries share one (i.e. the archive has a wrapper dir).
fn unpack_tar_gz(archive_path: &camino::Utf8Path, dest: &camino::Utf8Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open tar.gz: {archive_path}"))?;

    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(false);
    archive.set_overwrite(true);

    // Collect entries to determine if there is a single top-level wrapper directory.
    // We re-open rather than buffer, since tar::Archive doesn't implement Seek.
    let file2 = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open tar.gz (2nd pass): {archive_path}"))?;
    let gz2 = flate2::read::GzDecoder::new(file2);
    let mut archive2 = tar::Archive::new(gz2);

    // Determine strip prefix: if every path starts with the same first component
    // and there are no root-level files, strip it.
    let mut top_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut has_root_file = false;
    for entry in archive2
        .entries()
        .context("failed to iterate tar entries (1st pass)")?
    {
        let entry = entry.context("failed to read tar entry (1st pass)")?;
        let path = entry.path().context("invalid tar entry path")?;
        let components: Vec<_> = path.components().collect();
        if components.is_empty() {
            continue;
        }
        if let std::path::Component::Normal(name) = &components[0] {
            let name_str = name.to_string_lossy().to_string();
            if components.len() == 1 {
                has_root_file = true;
            }
            top_dirs.insert(name_str);
        }
    }

    // Strip the top-level wrapper dir if: exactly one top-level name, no root files.
    let strip_prefix: Option<String> = if !has_root_file && top_dirs.len() == 1 {
        top_dirs.into_iter().next()
    } else {
        None
    };

    // Third pass: actually extract.
    let file3 = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open tar.gz (3rd pass): {archive_path}"))?;
    let gz3 = flate2::read::GzDecoder::new(file3);
    let mut archive3 = tar::Archive::new(gz3);

    for entry in archive3
        .entries()
        .context("failed to iterate tar entries (extract)")?
    {
        let mut entry = entry.context("failed to read tar entry (extract)")?;
        let path = entry.path().context("invalid tar entry path (extract)")?;

        // Compute destination path, stripping wrapper prefix if applicable.
        let rel_path: std::path::PathBuf = if let Some(ref _prefix) = strip_prefix {
            let components: Vec<_> = path.components().collect();
            if components.len() <= 1 {
                // This is the wrapper dir itself — skip it.
                continue;
            }
            components[1..].iter().collect()
        } else {
            path.to_path_buf()
        };

        // Safety: reject absolute paths and path traversal.
        if rel_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            anyhow::bail!("unsafe path in tar archive: {}", rel_path.display());
        }

        let target = std::path::PathBuf::from(dest.as_str()).join(&rel_path);

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create dir: {}", target.display()))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry
                .unpack(&target)
                .with_context(|| format!("failed to unpack tar entry: {}", target.display()))?;
        }
    }

    Ok(())
}

/// Unpack a ZIP archive into `dest`.
fn unpack_zip(archive_path: &camino::Utf8Path, dest: &camino::Utf8Path) -> Result<()> {
    use std::io::Read;

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open ZIP archive: {archive_path}"))?;

    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive: {archive_path}"))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .with_context(|| format!("failed to access ZIP entry {i}"))?;

        let name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("ZIP entry has unsafe path"))?
            .to_owned();

        let target = std::path::PathBuf::from(dest.as_str()).join(&name);

        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create dir: {}", target.display()))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)
                .with_context(|| format!("failed to create: {}", target.display()))?;
            let mut buf = [0u8; 65536];
            loop {
                let n = entry.read(&mut buf).context("error reading ZIP entry")?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(&mut out, &buf[..n])?;
            }
        }
    }

    Ok(())
}

// ── Deactivate handler ────────────────────────────────────────────────────────

/// Handle one `Deactivate` event.  Returns `true` if the tool list changed.
async fn handle_deactivate(
    installation_id: Uuid,
    skill_id: &str,
    state: &Arc<AppState>,
    registry_url: &str,
) -> bool {
    let skill_id = skill_id.to_string();
    let state_clone = Arc::clone(state);
    let reg_url = registry_url.to_string();

    let result =
        tokio::task::spawn_blocking(move || deactivate_skill_blocking(&state_clone, &skill_id))
            .await;

    match result {
        Ok(Ok(())) => {
            report_installation_status(installation_id, "deactivated", None, &reg_url, state).await;
            true
        }
        Ok(Err(e)) => {
            warn!(error = %e, "reconciler: deactivate failed");
            false
        }
        Err(e) => {
            warn!(error = %e, "reconciler: deactivate task panicked");
            false
        }
    }
}

fn deactivate_skill_blocking(state: &AppState, skill_id: &str) -> Result<()> {
    let install_root = state.root_dir.join("skills").join(skill_id);
    let active_dir = install_root.join("active");

    // Remove the active symlink; keep files on disk.
    if active_dir.exists() || active_dir.is_symlink() {
        std::fs::remove_file(&active_dir)
            .or_else(|_| std::fs::remove_dir_all(&active_dir))
            .with_context(|| format!("failed to remove active symlink for {skill_id}"))?;
    }

    let now = chrono::Utc::now().to_rfc3339();
    let conn = rusqlite::Connection::open(&state.db_path)
        .context("failed to open state DB for deactivate")?;
    conn.execute(
        "UPDATE installed_skills \
         SET deactivated = 1, deactivated_at = ?1, current_status = 'deactivated' \
         WHERE skill_id = ?2",
        rusqlite::params![now, skill_id],
    )
    .context("failed to mark skill as deactivated in SQLite")?;

    // Remove the Claude Code skills symlink so the skill stops appearing
    // there. Mirror to `installer::deactivate_skill`'s behaviour.
    vectorhawkd_core::installer::unregister_from_claude_skills(skill_id);

    info!(skill_id, "reconciler: skill deactivated");
    Ok(())
}

// ── Purge handler ─────────────────────────────────────────────────────────────

/// Handle one `Purge` event.  Returns `true` if the tool list changed.
async fn handle_purge(
    installation_id: Uuid,
    skill_id: &str,
    state: &Arc<AppState>,
    registry_url: &str,
) -> bool {
    let skill_id = skill_id.to_string();
    let state_clone = Arc::clone(state);
    let reg_url = registry_url.to_string();

    let result =
        tokio::task::spawn_blocking(move || purge_skill_blocking(&state_clone, &skill_id)).await;

    match result {
        Ok(Ok(())) => {
            report_installation_status(installation_id, "removed", None, &reg_url, state).await;
            true
        }
        Ok(Err(e)) => {
            warn!(error = %e, "reconciler: purge failed");
            false
        }
        Err(e) => {
            warn!(error = %e, "reconciler: purge task panicked");
            false
        }
    }
}

fn purge_skill_blocking(state: &AppState, skill_id: &str) -> Result<()> {
    let install_root = state.root_dir.join("skills").join(skill_id);

    // Delete all files for this skill.
    if install_root.exists() {
        std::fs::remove_dir_all(&install_root)
            .with_context(|| format!("failed to delete skill dir: {install_root}"))?;
    }

    let conn =
        rusqlite::Connection::open(&state.db_path).context("failed to open state DB for purge")?;
    conn.execute(
        "DELETE FROM installed_skills WHERE skill_id = ?1",
        rusqlite::params![skill_id],
    )
    .context("failed to remove skill from SQLite")?;
    conn.execute(
        "DELETE FROM skill_versions WHERE skill_id = ?1",
        rusqlite::params![skill_id],
    )
    .context("failed to remove skill_versions from SQLite")?;

    vectorhawkd_core::installer::unregister_from_claude_skills(skill_id);

    info!(skill_id, "reconciler: skill purged");
    Ok(())
}

// ── Snapshot diff ─────────────────────────────────────────────────────────────

/// Diff a snapshot against local SQLite state and return derived events.
async fn build_derived_events(
    installations: Vec<InstallationRecord>,
    state: Arc<AppState>,
) -> Vec<SyncEvent> {
    tokio::task::spawn_blocking(move || build_derived_events_blocking(installations, &state))
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "reconciler: snapshot diff task panicked");
            vec![]
        })
}

fn build_derived_events_blocking(
    installations: Vec<InstallationRecord>,
    state: &AppState,
) -> Vec<SyncEvent> {
    let conn = match rusqlite::Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "reconciler: cannot open DB for snapshot diff");
            return vec![];
        }
    };

    // Build a map of locally installed skills: skill_id → (version, deactivated).
    let local_state = load_local_skill_state(&conn);

    let mut events = Vec::new();

    for record in &installations {
        // Skip records with a non-semver version sentinel — the artifact
        // endpoint requires a concrete version and would 404. The backend
        // should have resolved "latest" before sending; if it didn't, the
        // safest action is to leave the row alone rather than spin in a
        // retry loop.
        if record.version.is_empty() || record.version == "latest" {
            warn!(
                skill_id = %record.skill_id,
                state = %record.state,
                version = %record.version,
                "reconciler: snapshot row has unresolved version — skipping"
            );
            continue;
        }

        match record.state.as_str() {
            // "desired", "installing", and "installed" all mean the same thing
            // from the runner's perspective: this skill should be present and
            // active locally at this version. If it isn't, install. The state
            // is the desired-state machine on the backend, not the runner's
            // job to enforce — the runner reports its own actual state.
            "desired" | "installing" | "installed" => {
                // The runner considers a skill "locally installed" only when
                // the DB flag agrees *and* the on-disk `active/` symlink is
                // present at the right version. Without the filesystem check
                // a prior race (deactivate beats install task) can leave the
                // DB in `active` while the symlink is gone — we'd then skip
                // the install event and the skill would silently stay broken.
                let db_match = local_state
                    .get(&record.skill_id)
                    .map(|(ver, deactivated)| ver == &record.version && !deactivated)
                    .unwrap_or(false);
                let fs_match = state
                    .root_dir
                    .join("skills")
                    .join(&record.skill_id)
                    .join("active")
                    .exists();

                if !(db_match && fs_match) {
                    events.push(SyncEvent::Install {
                        installation_id: record.installation_id,
                        skill_id: record.skill_id.clone(),
                        version: record.version.clone(),
                    });
                }
            }
            "deactivated" => {
                // Should be deactivated. Generate a deactivate event when
                // either the SQLite flag *or* the filesystem disagree with
                // the desired state — install and deactivate tasks can
                // race in the reconciler, leaving a `deactivated=1` flag
                // alongside an orphaned `active/` symlink (and matching
                // ~/.claude/skills entry). Without a filesystem cross-check
                // we'd silently leak the skill into Claude Code despite
                // the portal saying it's removed.
                let db_active = local_state
                    .get(&record.skill_id)
                    .map(|(_, deactivated)| !deactivated)
                    .unwrap_or(false);
                let fs_active = state
                    .root_dir
                    .join("skills")
                    .join(&record.skill_id)
                    .join("active")
                    .exists();

                if db_active || fs_active {
                    events.push(SyncEvent::Deactivate {
                        installation_id: record.installation_id,
                        skill_id: record.skill_id.clone(),
                    });
                }
            }
            "removed" => {
                // Should be purged; if locally present, enqueue purge.
                if local_state.contains_key(&record.skill_id) {
                    events.push(SyncEvent::Purge {
                        installation_id: record.installation_id,
                        skill_id: record.skill_id.clone(),
                    });
                }
            }
            "error" => {
                // Backend recorded the last install attempt failed. Don't
                // auto-retry from the snapshot — the row will be retried when
                // the user re-issues an install (which flips state to
                // "desired") or the reconciler's in-process retry path runs.
            }
            other => {
                warn!(
                    skill_id = %record.skill_id,
                    state = other,
                    "reconciler: unknown installation state in snapshot — skipping"
                );
            }
        }
    }

    events
}

/// Load all locally installed skills as a map: skill_id → (version, deactivated).
fn load_local_skill_state(conn: &rusqlite::Connection) -> HashMap<String, (String, bool)> {
    let mut stmt =
        match conn.prepare("SELECT skill_id, active_version, deactivated FROM installed_skills") {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "reconciler: failed to prepare local state query");
                return HashMap::new();
            }
        };

    let rows: Vec<(String, String, bool)> = match stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|v| v != 0).unwrap_or(false),
            ))
        })
        .and_then(|iter| iter.collect::<rusqlite::Result<Vec<_>>>())
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "reconciler: failed to read local skill state");
            return HashMap::new();
        }
    };

    rows.into_iter()
        .map(|(id, ver, deactivated)| (id, (ver, deactivated)))
        .collect()
}

// ── Backend status reporting ──────────────────────────────────────────────────

/// Send `PATCH /api/installations/{id}` to report a state transition.
///
/// Loads the current access token from SQLite on each call so it always uses
/// the latest value — the SSE client may refresh the token at any time.
/// Fire-and-forget: failures are logged at WARN but do not affect local state.
async fn report_installation_status(
    installation_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    registry_url: &str,
    state: &Arc<AppState>,
) {
    let url = format!(
        "{}/api/installations/{}",
        registry_url.trim_end_matches('/'),
        installation_id
    );

    let mut body = serde_json::json!({ "state": status });
    if let Some(msg) = error_message {
        body["error_message"] = serde_json::Value::String(msg.to_string());
    }

    // Load the current access token fresh from SQLite.
    let reg_url = registry_url.to_string();
    let state_clone = Arc::clone(state);
    let access_token = tokio::task::spawn_blocking(move || {
        load_all_tokens(&state_clone)
            .ok()
            .and_then(|rows| {
                rows.into_iter()
                    .find(|r| r.registry_url == reg_url)
                    .map(|r| r.access_token)
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    // Use an async client here (we are already in an async context).
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "reconciler: failed to build HTTP client for status report");
            return;
        }
    };

    let req = if access_token.is_empty() {
        client.patch(&url).json(&body)
    } else {
        client.patch(&url).bearer_auth(access_token).json(&body)
    };

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(installation_id = %installation_id, status, "reconciler: status reported");
        }
        Ok(resp) => {
            warn!(
                installation_id = %installation_id,
                status,
                http_status = %resp.status(),
                "reconciler: status report returned non-success"
            );
        }
        Err(e) => {
            warn!(
                installation_id = %installation_id,
                status,
                error = %e,
                "reconciler: status report failed"
            );
        }
    }
}

// ── Test-only re-exports ──────────────────────────────────────────────────────
//
// These thin wrappers expose private functions to the sibling test modules
// without making them part of the public library API.

#[cfg(test)]
pub(crate) fn write_managed_mcp_json_for_test(state: &AppState) -> anyhow::Result<()> {
    write_managed_mcp_json(state)
}

#[cfg(test)]
pub(crate) async fn report_mcp_installation_status_for_test(
    installation_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    registry_url: &str,
    state: &Arc<AppState>,
) {
    report_mcp_installation_status(installation_id, status, error_message, registry_url, state)
        .await;
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "reconciler_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_reconciler_tests.rs"]
mod mcp_tests;
