//! Crash records: what the daemon leaves behind when it dies, and how that
//! reaches the backend.
//!
//! Release builds use `panic = "abort"`, so nothing can be reported from the
//! dying process itself beyond a local file. Two writers produce records in
//! `<root>/crashes/<report_id>.json` (mode 0600):
//!
//! 1. A panic hook ([`install_panic_hook`]) writes a `panic` record
//!    (message, location, backtrace) and then lets the abort proceed.
//! 2. A run marker ([`mark_run_started`] / [`clear_run_marker`]) detects
//!    deaths no hook sees (SIGKILL, OOM kill, power loss). The marker exists
//!    while a daemon runs and is removed on clean shutdown, so finding one at
//!    the next start (after the instance lock proves its owner is gone) is
//!    reported as an `unclean_shutdown` record with no message.
//!
//! The next daemon start uploads pending records (see
//! `RegistryClient::upload_crash_reports`) and deletes them on success.
//!
//! Privacy: records carry no environment variables and no tokens. Anything
//! under `$HOME` is rewritten to `~`, and token-shaped words are replaced
//! with `[REDACTED]`. Text is length-capped to the backend's limits.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::atomic_file::write_atomic;

pub const CRASH_DIR: &str = "crashes";
pub const RUN_MARKER: &str = "running.marker";
/// Records kept on disk while the backend is unreachable; oldest dropped first.
pub const MAX_PENDING: usize = 20;
/// Records per upload request (the backend caps a request at 10).
pub const UPLOAD_BATCH: usize = 10;
const MAX_MESSAGE: usize = 2_000;
const MAX_LOCATION: usize = 500;
const MAX_BACKTRACE: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    Panic,
    UncleanShutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrashRecord {
    pub report_id: String,
    pub kind: CrashKind,
    pub runner_version: String,
    pub os: String,
    pub arch: String,
    /// RFC 3339. For `unclean_shutdown` this is when the dead run started
    /// (the true time of death is unknown).
    pub occurred_at: String,
    pub message: Option<String>,
    pub location: Option<String>,
    pub backtrace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunMarker {
    pid: u32,
    runner_version: String,
    started_at: String,
}

fn new_record(kind: CrashKind, runner_version: &str, occurred_at: String) -> CrashRecord {
    CrashRecord {
        report_id: uuid::Uuid::new_v4().simple().to_string(),
        kind,
        runner_version: runner_version.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        occurred_at,
        message: None,
        location: None,
        backtrace: None,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Cut `s` to at most `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn is_secret_word(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    w.starts_with("scn_pat_") || w.starts_with("eyJ") || w.starts_with("sk-")
}

/// Rewrite `$HOME` to `~` and mask token-shaped words. Pure.
pub fn redact(text: &str, home: Option<&str>) -> String {
    let mut out = text.to_string();
    if let Some(h) = home {
        if h.len() > 1 {
            out = out.replace(h, "~");
        }
    }
    let mut result = String::with_capacity(out.len());
    let mut prev_bearer = false;
    for (i, word) in out.split(' ').enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if prev_bearer || is_secret_word(word) {
            result.push_str("[REDACTED]");
        } else {
            result.push_str(word);
        }
        prev_bearer = word.eq_ignore_ascii_case("bearer");
    }
    result
}

fn home_dir_string() -> Option<String> {
    dirs::home_dir().map(|p| p.to_string_lossy().into_owned())
}

fn clean(text: &str, max: usize) -> String {
    truncate(&redact(text, home_dir_string().as_deref()), max)
}

/// Build a `panic` record from the raw panic facts. Pure apart from the
/// process-wide constants (OS, arch, `$HOME` lookup).
pub fn build_panic_record(
    runner_version: &str,
    message: &str,
    location: Option<&str>,
    backtrace: Option<&str>,
) -> CrashRecord {
    let mut rec = new_record(CrashKind::Panic, runner_version, now_rfc3339());
    rec.message = Some(clean(message, MAX_MESSAGE));
    rec.location = location.map(|l| clean(l, MAX_LOCATION));
    rec.backtrace = backtrace.map(|b| clean(b, MAX_BACKTRACE));
    rec
}

fn crash_dir(root: &Utf8Path) -> Utf8PathBuf {
    root.join(CRASH_DIR)
}

fn marker_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(RUN_MARKER)
}

/// Persist `rec` and trim the directory to [`MAX_PENDING`] files.
pub fn write_record(root: &Utf8Path, rec: &CrashRecord) -> Result<()> {
    let dir = crash_dir(root);
    std::fs::create_dir_all(dir.as_std_path())
        .with_context(|| format!("creating crash dir {dir}"))?;
    let path = dir.join(format!("{}.json", rec.report_id));
    let body = serde_json::to_vec(rec).context("serializing crash record")?;
    write_atomic(path.as_std_path(), &body, Some(0o600))
        .with_context(|| format!("writing crash record {path}"))?;
    prune(&dir);
    Ok(())
}

fn prune(dir: &Utf8Path) {
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((modified, path));
    }
    if files.len() <= MAX_PENDING {
        return;
    }
    files.sort();
    let excess = files.len() - MAX_PENDING;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Pending records, oldest first. Unreadable or corrupt files are deleted.
pub fn list_pending(root: &Utf8Path) -> Vec<CrashRecord> {
    let dir = crash_dir(root);
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return Vec::new();
    };
    let mut records: Vec<CrashRecord> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CrashRecord>(&b).ok());
        match parsed {
            Some(rec) => records.push(rec),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    records.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
    records
}

/// Delete the records with these ids (already uploaded).
pub fn delete_records(root: &Utf8Path, report_ids: &[String]) {
    let dir = crash_dir(root);
    for id in report_ids {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")).as_std_path());
    }
}

/// Install the process-wide panic hook. Chains to the previous hook, so the
/// normal panic message (and the abort that follows in release) is unchanged.
/// Best-effort: a failure to write never masks the panic.
pub fn install_panic_hook(root: Utf8PathBuf, runner_version: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with non-string payload".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let rec = build_panic_record(
            runner_version,
            &message,
            location.as_deref(),
            Some(&backtrace),
        );
        if write_record(&root, &rec).is_ok() {
            // A recorded panic is this run's report: no unclean-shutdown
            // record on top of it at the next start.
            let _ = std::fs::remove_file(marker_path(&root).as_std_path());
        }
        previous(info);
    }));
}

/// Call once the single-instance lock is held (so any existing marker belongs
/// to a dead run). Records that run as `unclean_shutdown`, then writes a fresh
/// marker for this run. Returns the record written, if any.
pub fn mark_run_started(root: &Utf8Path, runner_version: &str) -> Result<Option<CrashRecord>> {
    let path = marker_path(root);
    let mut written = None;
    if let Ok(bytes) = std::fs::read(path.as_std_path()) {
        let (version, started) = match serde_json::from_slice::<RunMarker>(&bytes) {
            Ok(m) => (m.runner_version, m.started_at),
            Err(_) => ("unknown".to_string(), now_rfc3339()),
        };
        let rec = new_record(CrashKind::UncleanShutdown, &version, started);
        write_record(root, &rec)?;
        written = Some(rec);
    }
    let marker = RunMarker {
        pid: std::process::id(),
        runner_version: runner_version.to_string(),
        started_at: now_rfc3339(),
    };
    let body = serde_json::to_vec(&marker).context("serializing run marker")?;
    write_atomic(path.as_std_path(), &body, Some(0o600))
        .with_context(|| format!("writing run marker {path}"))?;
    Ok(written)
}

/// Remove the run marker: this run ended cleanly.
pub fn clear_run_marker(root: &Utf8Path) {
    let _ = std::fs::remove_file(marker_path(root).as_std_path());
}

/// Clears the run marker when dropped. Held for the daemon's lifetime so an
/// ordinary `Err` return (e.g. a refused registry URL at startup) is not
/// mistaken for a crash. Never runs on abort, `kill -9` or `process::exit`,
/// which is the point.
pub struct RunMarkerGuard {
    root: Utf8PathBuf,
}

impl RunMarkerGuard {
    pub fn new(root: &Utf8Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl Drop for RunMarkerGuard {
    fn drop(&mut self) {
        clear_run_marker(&self.root);
    }
}

/// Upload pending records and delete the accepted ones. Returns how many were
/// sent. Errors (network, 401/403/404/429/5xx) leave every record on disk.
pub fn upload_pending(
    root: &Utf8Path,
    registry: &crate::registry::RegistryClient,
) -> Result<usize> {
    let pending = list_pending(root);
    if pending.is_empty() {
        return Ok(0);
    }
    let mut sent = 0;
    for batch in pending.chunks(UPLOAD_BATCH) {
        registry.upload_crash_reports(batch)?;
        let ids: Vec<String> = batch.iter().map(|r| r.report_id.clone()).collect();
        delete_records(root, &ids);
        sent += batch.len();
    }
    Ok(sent)
}

#[cfg(test)]
#[path = "crash_tests.rs"]
mod tests;
