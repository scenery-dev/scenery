//! The scan report: the backend's `ScanReport` wire shape, and the one file
//! the scan may write (a failed upload, or `--out`).

use anyhow::{Context, Result};
use scenery_daemon::managed_paths::discoveries::DiscoveryItem;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub const REPORT_FORMAT: &str = "scenery-scan-report/1";

#[derive(Debug, Clone, Serialize)]
pub struct ReportDevice {
    pub scan_identity: String,
    pub identity_basis: scenery_core::scan_identity::IdentityBasis,
    pub hostname: String,
    pub os_username: String,
    pub platform: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub format: String,
    pub scanner_version: String,
    pub scanned_at: String,
    pub device: ReportDevice,
    pub items: Vec<DiscoveryItem>,
}

/// Build a report for this machine. `platform` and `arch` come from
/// `std::env::consts` (`macos` / `linux` / `windows` match the backend enum).
pub fn build(items: Vec<DiscoveryItem>, now: OffsetDateTime) -> Result<ScanReport> {
    let id = scenery_core::scan_identity::derive();
    Ok(ScanReport {
        format: REPORT_FORMAT.to_string(),
        scanner_version: env!("CARGO_PKG_VERSION").to_string(),
        scanned_at: now.format(&Rfc3339).context("formatting scan time")?,
        device: ReportDevice {
            scan_identity: id.hash,
            identity_basis: id.basis,
            hostname: id.hostname,
            os_username: id.os_username,
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        },
        items,
    })
}

/// A new report file name in `dir`. The random suffix makes the name
/// unguessable, so another local user cannot pre-create it in a shared
/// temp dir (the `create_new` open would then fail instead of saving).
pub fn fresh_path(dir: &Path, now: OffsetDateTime) -> PathBuf {
    let ts = now.unix_timestamp();
    let suffix = uuid::Uuid::new_v4().simple();
    dir.join(format!("scenery-scan-{ts}-{suffix}.json"))
}

/// Write the report with `create_new` and mode 0600. Never overwrites.
pub fn save(report: &ScanReport, out: Option<&Path>) -> Result<PathBuf> {
    let path = match out {
        Some(p) => p.to_path_buf(),
        None => fresh_path(&std::env::temp_dir(), OffsetDateTime::now_utc()),
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .with_context(|| format!("creating report file {}", path.display()))?;
    let body = serde_json::to_vec_pretty(report).context("serializing scan report")?;
    file.write_all(&body)
        .with_context(|| format!("writing report file {}", path.display()))?;
    Ok(path)
}
