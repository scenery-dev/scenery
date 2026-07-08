//! F3 drift detection + quarantine.
//!
//! Periodic scanner that reads every `managed_path_markers` row, recomputes the
//! current on-disk canonical hash, and compares it to the stored marker. Three
//! outcomes per item:
//!
//! - **CLEAN**: hashes match — no-op.
//! - **DRIFTED**: file exists but hash differs from the marker — emit event,
//!   take action per current `managed_paths_mode` policy.
//! - **DELETED**: marker present but file gone — emit `deleted` event; the
//!   marker is cleaned up so the next backend snapshot can re-trigger an install.
//!
//! ## Policy → action matrix
//!
//! | Mode               | DRIFTED action              | DELETED action |
//! |--------------------|-----------------------------|----------------|
//! | audit_only (def.)  | `reported`                  | `deleted`      |
//! | warn               | `reported` (UI badges it)   | `deleted`      |
//! | quarantine         | move aside + `quarantined`  | `deleted`      |
//! | approve_required   | `awaiting_approval`         | `deleted`      |
//!
//! Quarantine moves the drifted file/dir to
//! `~/.claude/.vectorhawk-quarantine/<ISO8601>/<kind>/<slug>/` and drops the
//! marker so the next desired-state snapshot triggers a fresh install from the
//! registry.
//!
//! The killswitch `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER=1` makes every
//! public entry-point a no-op.

use super::marker::ManagedPathMarker;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::interval;
use tracing::{debug, info, warn};
use vectorhawkd_core::auth::load_all_tokens;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::ownership;

const DEFAULT_INTERVAL_SECS: u64 = 300;
const SCAN_BUDGET_SECS: u64 = 30;
const ENV_INTERVAL: &str = "VECTORHAWK_DRIFT_SCAN_INTERVAL_SECS";
const ENV_DISABLE: &str = "VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER";

// ── Public types ──────────────────────────────────────────────────────────────

/// One outcome row produced by `scan_once`.
#[derive(Debug, Clone)]
pub struct DriftOutcome {
    pub slug: String,
    pub kind: String,
    pub path: String,
    pub expected_hash: String,
    pub current_hash: Option<String>,
    pub status: DriftStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftStatus {
    Clean,
    Drifted,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    AuditOnly,
    Warn,
    Quarantine,
    ApproveRequired,
}

impl PolicyMode {
    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "warn" => PolicyMode::Warn,
            "quarantine" => PolicyMode::Quarantine,
            "approve_required" => PolicyMode::ApproveRequired,
            _ => PolicyMode::AuditOnly,
        }
    }
}

#[derive(Debug, Serialize)]
struct DriftReportItem {
    slug: String,
    kind: String,
    current_hash: String,
    expected_hash: String,
    action: String,
    detail: Option<Value>,
}

#[derive(Debug, Serialize)]
struct DriftReportBody {
    device_id: Option<String>,
    events: Vec<DriftReportItem>,
}

// ── Scanner ───────────────────────────────────────────────────────────────────

pub struct DriftScanner {
    state: Arc<AppState>,
    registry_url: String,
    http_client: reqwest::Client,
}

impl DriftScanner {
    pub fn new(state: Arc<AppState>, registry_url: String) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("drift: failed to build HTTP client")?;
        Ok(Self {
            state,
            registry_url,
            http_client,
        })
    }

    /// Spawn the drift loop on the current tokio runtime.  Returns the task
    /// handle so callers can abort on shutdown.
    pub fn spawn_loop(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if reconciler_disabled() {
                info!("drift: VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER set — scanner disabled");
                return;
            }
            let secs = std::env::var(ENV_INTERVAL)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(DEFAULT_INTERVAL_SECS);
            let mut ticker = interval(Duration::from_secs(secs));
            // First tick fires immediately; skip it so we don't scan before the
            // daemon's pairing flow has completed.
            ticker.tick().await;
            info!(interval_secs = secs, "drift: scanner started");
            loop {
                ticker.tick().await;
                let started = std::time::Instant::now();
                match tokio::time::timeout(Duration::from_secs(SCAN_BUDGET_SECS), self.run_cycle())
                    .await
                {
                    Ok(Ok(stats)) => {
                        debug!(
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            scanned = stats.scanned,
                            drifted = stats.drifted,
                            deleted = stats.deleted,
                            "drift: cycle complete"
                        );
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "drift: cycle failed");
                    }
                    Err(_) => {
                        warn!("drift: cycle exceeded {SCAN_BUDGET_SECS}s budget — abandoning");
                    }
                }
            }
        })
    }

    /// Run one drift-scan cycle: scan, apply policy, report to backend.
    pub async fn run_cycle(&self) -> Result<CycleStats> {
        let outcomes = self.scan_once()?;
        let mode = read_policy_mode(&self.state);
        let mut stats = CycleStats {
            scanned: outcomes.len(),
            ..Default::default()
        };

        let mut to_report: Vec<DriftReportItem> = Vec::new();

        for outcome in outcomes {
            match outcome.status {
                DriftStatus::Clean => {}
                DriftStatus::Drifted => {
                    stats.drifted += 1;
                    let current_hash = outcome.current_hash.clone().unwrap_or_default();
                    let action = match mode {
                        PolicyMode::AuditOnly | PolicyMode::Warn => "reported",
                        PolicyMode::Quarantine => match quarantine_item(&outcome) {
                            Ok(quarantined_to) => {
                                drop_marker(&self.state, &outcome.path).ok();
                                info!(
                                    slug = %outcome.slug,
                                    kind = %outcome.kind,
                                    dest = %quarantined_to.display(),
                                    "drift: quarantined drifted item"
                                );
                                "quarantined"
                            }
                            Err(e) => {
                                warn!(
                                    slug = %outcome.slug,
                                    error = %e,
                                    "drift: quarantine failed; downgrading to reported"
                                );
                                "reported"
                            }
                        },
                        PolicyMode::ApproveRequired => "awaiting_approval",
                    };
                    to_report.push(DriftReportItem {
                        slug: outcome.slug,
                        kind: outcome.kind,
                        current_hash,
                        expected_hash: outcome.expected_hash,
                        action: action.to_string(),
                        detail: Some(serde_json::json!({"path": outcome.path})),
                    });
                }
                DriftStatus::Deleted => {
                    stats.deleted += 1;
                    drop_marker(&self.state, &outcome.path).ok();
                    to_report.push(DriftReportItem {
                        slug: outcome.slug,
                        kind: outcome.kind,
                        current_hash: String::new(),
                        expected_hash: outcome.expected_hash,
                        action: "deleted".to_string(),
                        detail: Some(serde_json::json!({"path": outcome.path})),
                    });
                }
            }
        }

        if !to_report.is_empty() {
            self.report(to_report).await?;
        }
        Ok(stats)
    }

    /// Pure scan — read every marker, compute current hash, classify outcome.
    /// Exposed so tests can drive it without spawning the loop.
    pub fn scan_once(&self) -> Result<Vec<DriftOutcome>> {
        let conn = Connection::open(self.state.db_path.as_std_path())
            .context("drift: failed to open state DB")?;
        let markers = list_markers(&conn)?;
        let mut out = Vec::with_capacity(markers.len());
        for marker in markers {
            out.push(classify(&marker));
        }
        Ok(out)
    }

    async fn report(&self, events: Vec<DriftReportItem>) -> Result<()> {
        let token = self.access_token().await;
        if token.is_empty() {
            debug!("drift: no auth token — skipping report (will retry next cycle)");
            return Ok(());
        }
        let body = DriftReportBody {
            device_id: load_device_id(&self.state),
            events,
        };
        let url = format!("{}/portal/managed-paths/drift", self.registry_url);
        let resp = self
            .http_client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("drift: failed to POST drift report")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            warn!(%status, body = %text, "drift: backend rejected drift report");
        }
        Ok(())
    }

    async fn access_token(&self) -> String {
        let reg = self.registry_url.clone();
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            load_all_tokens(&state)
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|r| r.registry_url == reg)
                        .map(|r| r.access_token)
                })
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct CycleStats {
    pub scanned: usize,
    pub drifted: usize,
    pub deleted: usize,
}

// ── SSE resolution handler ────────────────────────────────────────────────────

/// Apply an admin-side resolution received via SSE.
///
/// `accept_local`     — bump marker's source_sha256 to the current on-disk hash.
/// `restore_canonical` — drop the file + marker; next snapshot will re-install.
/// `reject`           — no filesystem change.
///
/// Always POSTs `/portal/managed-paths/drift/{id}/applied` so the admin queue
/// reflects the daemon's progress.
pub async fn handle_drift_resolution(
    state: Arc<AppState>,
    registry_url: String,
    drift_id: String,
    slug: String,
    kind: String,
    resolution: String,
) -> Result<()> {
    if reconciler_disabled() {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("drift: failed to build HTTP client for resolution")?;

    let result = match resolution.as_str() {
        "accept_local" => accept_local(&state, &slug, &kind),
        "restore_canonical" => restore_canonical(&state, &slug, &kind),
        "reject" => Ok(()),
        other => {
            warn!(resolution = %other, "drift: unknown resolution — treating as no-op");
            Ok(())
        }
    };

    if let Err(e) = &result {
        warn!(slug = %slug, error = %e, "drift: resolution apply failed; ack-ing anyway so admin queue progresses");
    }

    // Ack the apply regardless — the row stays in 'applied' status and the
    // admin can re-investigate if the filesystem step failed.
    let token = {
        let reg = registry_url.clone();
        let state_clone = Arc::clone(&state);
        tokio::task::spawn_blocking(move || {
            load_all_tokens(&state_clone)
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|r| r.registry_url == reg)
                        .map(|r| r.access_token)
                })
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    };
    let url = format!(
        "{}/portal/managed-paths/drift/{}/applied",
        registry_url, drift_id
    );
    if !token.is_empty() {
        let resp = client.post(&url).bearer_auth(token).send().await;
        if let Err(e) = resp {
            warn!(error = %e, "drift: failed to ack applied to backend");
        }
    }
    result
}

fn accept_local(state: &AppState, slug: &str, kind: &str) -> Result<()> {
    let marker = find_marker_by_slug_kind(state, slug, kind)?
        .ok_or_else(|| anyhow::anyhow!("drift: no marker for slug={slug} kind={kind}"))?;
    let current = match current_hash_for(&marker)? {
        Some(h) => h,
        None => {
            return Err(anyhow::anyhow!(
                "drift: cannot accept local for missing file ({slug})"
            ))
        }
    };
    let conn = Connection::open(state.db_path.as_std_path())?;
    conn.execute(
        "UPDATE managed_path_markers SET source_sha256 = ?1 WHERE path = ?2",
        rusqlite::params![current, marker.path],
    )?;
    info!(slug, "drift: accepted local edit as new canonical");
    Ok(())
}

fn restore_canonical(state: &AppState, slug: &str, kind: &str) -> Result<()> {
    let marker = find_marker_by_slug_kind(state, slug, kind)?
        .ok_or_else(|| anyhow::anyhow!("drift: no marker for slug={slug} kind={kind}"))?;
    // Drop the file and the marker. The next desired-state snapshot from the
    // backend will trigger a fresh install via the F2 pusher.
    remove_path_for(&marker)?;
    drop_marker(state, &marker.path)?;
    info!(
        slug,
        "drift: restored canonical (removed local; awaiting re-install via snapshot)"
    );
    Ok(())
}

// ── Classification ────────────────────────────────────────────────────────────

fn classify(marker: &ManagedPathMarker) -> DriftOutcome {
    let outcome_status;
    let current_hash = match current_hash_for(marker) {
        Ok(Some(h)) => {
            outcome_status = if h == marker.source_sha256 {
                DriftStatus::Clean
            } else {
                DriftStatus::Drifted
            };
            Some(h)
        }
        Ok(None) => {
            outcome_status = DriftStatus::Deleted;
            None
        }
        Err(e) => {
            warn!(slug = %marker.slug, error = %e, "drift: classify failed; treating as clean");
            outcome_status = DriftStatus::Clean;
            None
        }
    };
    DriftOutcome {
        slug: marker.slug.clone(),
        kind: marker.kind.clone(),
        path: marker.path.clone(),
        expected_hash: marker.source_sha256.clone(),
        current_hash,
        status: outcome_status,
    }
}

/// Recompute the canonical hash for the file backing this marker.
/// Returns `Ok(None)` when the file is gone — that's a DELETED outcome.
pub fn current_hash_for(marker: &ManagedPathMarker) -> Result<Option<String>> {
    match marker.kind.as_str() {
        "skill" => {
            let skill_md = PathBuf::from(&marker.path).join("SKILL.md");
            if !skill_md.exists() {
                return Ok(None);
            }
            let bytes = fs::read(&skill_md).with_context(|| {
                format!("drift: failed to read skill at {}", skill_md.display())
            })?;
            Ok(Some(hex_sha256(&bytes)))
        }
        "plugin" => {
            let manifest = PathBuf::from(&marker.path)
                .join(".claude-plugin")
                .join("plugin.json");
            if !manifest.exists() {
                return Ok(None);
            }
            let bytes = fs::read(&manifest).with_context(|| {
                format!(
                    "drift: failed to read plugin manifest at {}",
                    manifest.display()
                )
            })?;
            Ok(Some(hex_sha256(&bytes)))
        }
        "mcp" => {
            // marker.path is virtual: `<claude_json>:<slug>`. Pull the entry out
            // of the JSON object and hash its serialised form. We re-serialise
            // exactly how F2's pusher did so hashes stay comparable.
            let (json_path, slug) = split_mcp_path(&marker.path)?;
            if !json_path.exists() {
                return Ok(None);
            }
            let bytes = fs::read(&json_path)
                .with_context(|| format!("drift: failed to read {}", json_path.display()))?;
            let root: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("drift: malformed JSON at {}", json_path.display()))?;
            let entry = root.get("mcpServers").and_then(|s| s.get(&slug));
            match entry {
                None => Ok(None),
                Some(v) => Ok(Some(hex_sha256(v.to_string().as_bytes()))),
            }
        }
        other => {
            warn!(kind = %other, "drift: unknown marker kind — treating as clean");
            Ok(None)
        }
    }
}

fn split_mcp_path(path: &str) -> Result<(PathBuf, String)> {
    // Virtual path is "<absolute claude json path>:<slug>". The last colon is
    // the separator (paths on Unix can't contain colons in the segments F2
    // writes — claude.json is in $HOME).
    let idx = path
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("drift: malformed mcp path: {path}"))?;
    let (json_str, slug_part) = path.split_at(idx);
    let slug = slug_part.trim_start_matches(':').to_string();
    Ok((PathBuf::from(json_str), slug))
}

fn remove_path_for(marker: &ManagedPathMarker) -> Result<()> {
    match marker.kind.as_str() {
        "skill" | "plugin" => {
            let dir = PathBuf::from(&marker.path);
            // Defense-in-depth: never delete Anthropic-native content, even if a
            // stale marker row points at it.
            ownership::ensure_not_native(&dir)?;
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .with_context(|| format!("drift: failed to remove {}", dir.display()))?;
            }
            Ok(())
        }
        "mcp" => {
            let (json_path, slug) = split_mcp_path(&marker.path)?;
            // Never remove our own aggregator entry via drift.
            if ownership::is_vectorhawk_mcp_key(&slug) {
                anyhow::bail!("drift: refusing to remove the vectorhawk aggregator entry");
            }
            if !json_path.exists() {
                return Ok(());
            }
            let bytes = fs::read(&json_path)?;
            let mut root: Value = serde_json::from_slice(&bytes)?;
            if let Some(Value::Object(map)) = root.get_mut("mcpServers") {
                map.remove(&slug);
            }
            let tmp = json_path.with_extension("tmp.drift");
            fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
            fs::rename(&tmp, &json_path)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Quarantine ────────────────────────────────────────────────────────────────

fn quarantine_item(outcome: &DriftOutcome) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("drift: HOME not resolvable"))?;
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H%M%SZ").to_string();
    let dest_root = home
        .join(".claude")
        .join(".vectorhawk-quarantine")
        .join(ts)
        .join(&outcome.kind);
    fs::create_dir_all(&dest_root).with_context(|| {
        format!(
            "drift: failed to create quarantine root {}",
            dest_root.display()
        )
    })?;

    match outcome.kind.as_str() {
        "skill" | "plugin" => {
            let src = PathBuf::from(&outcome.path);
            // Defense-in-depth: never move/delete Anthropic-native content.
            ownership::ensure_not_native(&src)?;
            if !src.exists() {
                return Ok(dest_root);
            }
            let dest = dest_root.join(&outcome.slug);
            // Try rename first; fall back to copy+delete on cross-FS.
            if fs::rename(&src, &dest).is_err() {
                copy_dir_recursive(&src, &dest)?;
                fs::remove_dir_all(&src).ok();
            }
            Ok(dest)
        }
        "mcp" => {
            let (json_path, slug) = split_mcp_path(&outcome.path)?;
            if ownership::is_vectorhawk_mcp_key(&slug) {
                anyhow::bail!("drift: refusing to quarantine the vectorhawk aggregator entry");
            }
            if !json_path.exists() {
                return Ok(dest_root);
            }
            let bytes = fs::read(&json_path)?;
            let mut root: Value = serde_json::from_slice(&bytes)?;
            // Snapshot the entry into the quarantine dir before mutating.
            let entry = root
                .get("mcpServers")
                .and_then(|s| s.get(&slug))
                .cloned()
                .unwrap_or(Value::Null);
            let dest = dest_root.join(format!("{slug}.json"));
            fs::write(&dest, serde_json::to_vec_pretty(&entry)?)?;
            // Remove the entry from claude.json.
            if let Some(Value::Object(map)) = root.get_mut("mcpServers") {
                map.remove(&slug);
            }
            let tmp = json_path.with_extension("tmp.drift");
            fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
            fs::rename(&tmp, &json_path)?;
            Ok(dest)
        }
        _ => Ok(dest_root),
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let typ = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if typ.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

// ── DB helpers ────────────────────────────────────────────────────────────────

fn list_markers(conn: &Connection) -> Result<Vec<ManagedPathMarker>> {
    let mut stmt = conn.prepare(
        "SELECT path, kind, slug, installation_id, source_sha256, migrated_at FROM managed_path_markers"
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ManagedPathMarker {
                path: row.get(0)?,
                kind: row.get(1)?,
                slug: row.get(2)?,
                installation_id: row.get(3)?,
                source_sha256: row.get(4)?,
                migrated_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn find_marker_by_slug_kind(
    state: &AppState,
    slug: &str,
    kind: &str,
) -> Result<Option<ManagedPathMarker>> {
    let conn = Connection::open(state.db_path.as_std_path())?;
    let mut stmt = conn.prepare(
        "SELECT path, kind, slug, installation_id, source_sha256, migrated_at \
         FROM managed_path_markers WHERE slug = ?1 AND kind = ?2 LIMIT 1",
    )?;
    let result = stmt
        .query_row(rusqlite::params![slug, kind], |row| {
            Ok(ManagedPathMarker {
                path: row.get(0)?,
                kind: row.get(1)?,
                slug: row.get(2)?,
                installation_id: row.get(3)?,
                source_sha256: row.get(4)?,
                migrated_at: row.get(5)?,
            })
        })
        .ok();
    Ok(result)
}

fn drop_marker(state: &AppState, path: &str) -> Result<()> {
    let conn = Connection::open(state.db_path.as_std_path())?;
    conn.execute(
        "DELETE FROM managed_path_markers WHERE path = ?1",
        rusqlite::params![path],
    )?;
    Ok(())
}

fn read_policy_mode(state: &AppState) -> PolicyMode {
    match state.get_sync_state("managed_paths_mode") {
        Ok(Some(s)) => PolicyMode::from_str_or_default(&s),
        _ => PolicyMode::AuditOnly,
    }
}

fn load_device_id(state: &AppState) -> Option<String> {
    state.get_sync_state("device_id").ok().flatten()
}

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "drift_tests.rs"]
mod drift_tests;
