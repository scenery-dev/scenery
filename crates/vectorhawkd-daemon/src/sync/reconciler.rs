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
use vectorhawkd_core::{registry::RegistryClient, state::AppState};

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

                match event {
                    SyncEvent::Install { installation_id, skill_id, version } => {
                        let st = Arc::clone(&state);
                        let reg_url = registry_url.clone();
                        let sem_clone = Arc::clone(&sem);
                        let stats_clone = Arc::clone(&stats);

                        // Increment pending counter.
                        increment_pending(&stats);

                        install_tasks.spawn(async move {
                            let _permit = sem_clone.acquire().await;
                            handle_install(
                                installation_id,
                                &skill_id,
                                &version,
                                &st,
                                &reg_url,
                                &stats_clone,
                            ).await
                        });
                    }

                    SyncEvent::Deactivate { installation_id, skill_id } => {
                        let st = Arc::clone(&state);
                        let reg_url = registry_url.clone();
                        if handle_deactivate(installation_id, &skill_id, &st, &reg_url).await {
                            fire_notification(&list_changed_tx, &mut notify_pending, &mut coalesce_deadline);
                        }
                    }

                    SyncEvent::Purge { installation_id, skill_id } => {
                        let st = Arc::clone(&state);
                        let reg_url = registry_url.clone();
                        if handle_purge(installation_id, &skill_id, &st, &reg_url).await {
                            fire_notification(&list_changed_tx, &mut notify_pending, &mut coalesce_deadline);
                        }
                    }

                    SyncEvent::Snapshot { installations } => {
                        let derived = build_derived_events(installations, Arc::clone(&state)).await;
                        for derived_event in derived {
                            // Feed derived events back into the channel (bounded).
                            // If the channel is full the event is dropped with a warning;
                            // the next snapshot will catch it.
                            if rx.is_closed() {
                                break;
                            }
                            // We can't send on `rx` (the receiver end); instead
                            // we process derived events inline to avoid a second channel.
                            match derived_event {
                                SyncEvent::Install { installation_id, skill_id, version } => {
                                    let st = Arc::clone(&state);
                                    let reg_url = registry_url.clone();
                                    let sem_clone = Arc::clone(&sem);
                                    let stats_clone = Arc::clone(&stats);

                                    increment_pending(&stats);

                                    install_tasks.spawn(async move {
                                        let _permit = sem_clone.acquire().await;
                                        handle_install(
                                            installation_id,
                                            &skill_id,
                                            &version,
                                            &st,
                                            &reg_url,
                                            &stats_clone,
                                        ).await
                                    });
                                }
                                SyncEvent::Deactivate { installation_id, skill_id } => {
                                    let st = Arc::clone(&state);
                                    let reg_url = registry_url.clone();
                                    if handle_deactivate(installation_id, &skill_id, &st, &reg_url).await {
                                        fire_notification(&list_changed_tx, &mut notify_pending, &mut coalesce_deadline);
                                    }
                                }
                                SyncEvent::Purge { installation_id, skill_id } => {
                                    let st = Arc::clone(&state);
                                    let reg_url = registry_url.clone();
                                    if handle_purge(installation_id, &skill_id, &st, &reg_url).await {
                                        fire_notification(&list_changed_tx, &mut notify_pending, &mut coalesce_deadline);
                                    }
                                }
                                SyncEvent::Snapshot { .. } => {
                                    // Nested snapshots not expected; ignore.
                                }
                            }
                        }
                    }
                }
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
        report_installation_status(installation_id, "installed", None, &reg_url).await;
        return Ok(());
    }

    // Report "installing" to backend.
    report_installation_status(installation_id, "installing", None, &reg_url).await;

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

    report_installation_status(installation_id, "installed", None, &reg_url).await;
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
            "UPDATE installed_skills SET active_version = ?1, deactivated = 0, deactivated_at = NULL \
             WHERE skill_id = ?2",
            rusqlite::params![version, skill_id],
        )
        .context("failed to update installed_skills after symlink flip")?;

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

/// Unpack a `.cskill` ZIP archive into `dest`.
///
/// `.cskill` files are ZIP archives (same as `.skill` bundles). We reuse the
/// zip crate already in the workspace.
fn unpack_cskill_archive(archive_path: &camino::Utf8Path, dest: &camino::Utf8Path) -> Result<()> {
    use std::io::Read;

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {archive_path}"))?;

    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive: {archive_path}"))?;

    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create unpack dir: {dest}"))?;

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
            report_installation_status(installation_id, "deactivated", None, &reg_url).await;
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
            report_installation_status(installation_id, "removed", None, &reg_url).await;
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
        match record.state.as_str() {
            "desired" => {
                // Should be installed and active.
                let locally_installed = local_state
                    .get(&record.skill_id)
                    .map(|(ver, deactivated)| ver == &record.version && !deactivated)
                    .unwrap_or(false);

                if !locally_installed {
                    events.push(SyncEvent::Install {
                        installation_id: record.installation_id,
                        skill_id: record.skill_id.clone(),
                        version: record.version.clone(),
                    });
                }
            }
            "deactivated" => {
                // Should be deactivated; if locally active, enqueue deactivate.
                let locally_active = local_state
                    .get(&record.skill_id)
                    .map(|(_, deactivated)| !deactivated)
                    .unwrap_or(false);

                if locally_active {
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
/// Fire-and-forget: failures are logged at WARN but do not affect local state.
async fn report_installation_status(
    installation_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    registry_url: &str,
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

    match client.patch(&url).json(&body).send().await {
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "reconciler_tests.rs"]
mod tests;
