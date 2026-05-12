//! Scan client for the VectorHawk registry's `/runner/scan` endpoint.
//!
//! Provides a `ScanClient` trait with two implementations:
//! - `HttpScanClient` — POSTs to the registry and returns the verdict.
//! - `NoOpScanClient` — Returns `Unknown` without any network call (offline/unauthenticated mode).
//!
//! The HTTP implementation is **fail-open**: any network or parse error yields
//! `ScanVerdict { verdict: Severity::Unknown, ... }` rather than `Err`. This ensures
//! that a registry outage never blocks an import.

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

// ── Severity ─────────────────────────────────────────────────────────────────

/// Normalized severity levels — maps both registry and local scanner output.
///
/// The ordering `Unknown < Clean < Info < Low < Medium < High < Critical` is
/// intentional: `Ord` derives allow `severity >= Severity::Low` comparisons.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Unknown,
    Clean,
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Convert a registry verdict string to a `Severity`.
    ///
    /// Unrecognised strings map to `Unknown` so schema additions are fail-open.
    /// Named `parse_verdict` rather than `from_str` to avoid clippy's
    /// `should_implement_trait` lint — implementing `std::str::FromStr` would
    /// require returning `Result` which adds noise at every call site.
    pub fn parse_verdict(s: &str) -> Self {
        match s {
            "clean" => Self::Clean,
            "info" => Self::Info,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "critical" => Self::Critical,
            _ => Self::Unknown,
        }
    }

    /// ANSI-colored badge label for terminal output.
    pub fn badge_label(&self) -> &'static str {
        match self {
            Self::Clean => "[CLEAN]",
            Self::Info => "[OK]",
            Self::Unknown => "[OK]",
            Self::Low => "[LOW RISK]",
            Self::Medium => "[MEDIUM RISK - review findings]",
            Self::High => "[HIGH RISK]",
            Self::Critical => "[CRITICAL RISK]",
        }
    }

    /// ANSI color code prefix for `badge_label()`.
    ///
    /// Returns an empty string when the severity does not warrant coloring
    /// (clean / info / unknown).
    pub fn ansi_color(&self) -> &'static str {
        match self {
            Self::Clean => "\x1b[32m",      // green
            Self::Info => "\x1b[2m",        // dim
            Self::Unknown => "\x1b[2m",     // dim
            Self::Low => "\x1b[33m",        // yellow
            Self::Medium => "\x1b[33m",     // yellow-orange (no true orange in ANSI)
            Self::High => "\x1b[31m",       // red
            Self::Critical => "\x1b[31;1m", // bold red
        }
    }
}

// ── ScanFinding ───────────────────────────────────────────────────────────────

/// A single finding from the scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanFinding {
    pub rule_id: Option<String>,
    pub severity: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

// ── ScanVerdict ───────────────────────────────────────────────────────────────

/// Aggregated scan verdict returned by the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanVerdict {
    pub verdict: Severity,
    pub max_severity: Option<String>,
    pub findings: Vec<ScanFinding>,
    pub scanner_version: Option<String>,
    pub cached: bool,
    pub content_hash: String,
}

impl ScanVerdict {
    /// Returns `true` if the verdict warrants a warning (Low and above).
    pub fn is_risky(&self) -> bool {
        matches!(
            self.verdict,
            Severity::Low | Severity::Medium | Severity::High | Severity::Critical
        )
    }

    /// Returns `true` if the verdict requires explicit `--confirm-risky` before proceeding.
    pub fn requires_confirmation(&self) -> bool {
        matches!(
            self.verdict,
            Severity::Medium | Severity::High | Severity::Critical
        )
    }

    /// Build a human-readable summary of all findings for display in the terminal.
    pub fn format_findings(&self) -> String {
        if self.findings.is_empty() {
            return String::new();
        }
        let mut out = String::from("Findings:\n");
        for f in &self.findings {
            let rule = f.rule_id.as_deref().unwrap_or("unknown");
            let title = f.title.as_deref().unwrap_or("(no title)");
            out.push_str(&format!("  [{sev}] {rule}: {title}\n", sev = f.severity));
        }
        out
    }
}

// ── Wire type for the registry response ──────────────────────────────────────

/// JSON shape from `POST /runner/scan`.
#[derive(Debug, Deserialize)]
struct ScanApiResponse {
    content_hash: String,
    verdict: String,
    max_severity: Option<String>,
    #[serde(default)]
    findings: Vec<ScanFindingWire>,
    scanner_version: Option<String>,
    #[serde(default)]
    cached: bool,
}

#[derive(Debug, Deserialize)]
struct ScanFindingWire {
    rule_id: Option<String>,
    severity: String,
    title: Option<String>,
    description: Option<String>,
}

impl ScanFindingWire {
    fn into_finding(self) -> ScanFinding {
        ScanFinding {
            rule_id: self.rule_id,
            severity: self.severity,
            title: self.title,
            description: self.description,
        }
    }
}

// ── ScanClient trait ─────────────────────────────────────────────────────────

/// Abstraction over the scan backend so tests can inject mock implementations.
pub trait ScanClient: Send + Sync {
    /// Scan `content` with the given `content_type` hint.
    ///
    /// Implementations **must** be fail-open: network failures should return
    /// `Ok(ScanVerdict { verdict: Severity::Unknown, ... })` rather than `Err`.
    fn scan(&self, content: &[u8], content_type: &str) -> Result<ScanVerdict>;
}

// ── HttpScanClient ────────────────────────────────────────────────────────────

/// Scans content via the registry's `POST /runner/scan` endpoint.
///
/// Uses `reqwest::blocking` (already present in vectorhawkd-core) with the same
/// timeouts as other `RegistryClient` calls. Always fail-open on errors.
pub struct HttpScanClient {
    registry_url: String,
    access_token: String,
}

impl HttpScanClient {
    pub fn new(registry_url: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self {
            registry_url: registry_url.into(),
            access_token: access_token.into(),
        }
    }
}

impl ScanClient for HttpScanClient {
    fn scan(&self, content: &[u8], content_type: &str) -> Result<ScanVerdict> {
        let unknown_verdict = unknown_verdict(content);

        let url = format!("{}/runner/scan", self.registry_url.trim_end_matches('/'));

        let content_b64 = base64::engine::general_purpose::STANDARD.encode(content);
        let body = serde_json::json!({
            "content_b64":  content_b64,
            "content_type": content_type,
            "content_hash": null,
        });

        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "scan: failed to build HTTP client — returning unknown");
                return Ok(unknown_verdict);
            }
        };

        let resp = match client
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, url, "scan: HTTP request failed — returning unknown");
                return Ok(unknown_verdict);
            }
        };

        if !resp.status().is_success() {
            warn!(
                status = %resp.status(),
                url,
                "scan: registry returned non-2xx — returning unknown"
            );
            return Ok(unknown_verdict);
        }

        let wire: ScanApiResponse = match resp.json() {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "scan: failed to parse scan response — returning unknown");
                return Ok(unknown_verdict);
            }
        };

        let verdict = Severity::parse_verdict(&wire.verdict);
        let findings = wire
            .findings
            .into_iter()
            .map(ScanFindingWire::into_finding)
            .collect();

        Ok(ScanVerdict {
            verdict,
            max_severity: wire.max_severity,
            findings,
            scanner_version: wire.scanner_version,
            cached: wire.cached,
            content_hash: wire.content_hash,
        })
    }
}

// ── NoOpScanClient ────────────────────────────────────────────────────────────

/// A scan client that never makes network calls; returns `Unknown` for all content.
///
/// Used when no registry URL or auth token is available.
pub struct NoOpScanClient;

impl ScanClient for NoOpScanClient {
    fn scan(&self, content: &[u8], _content_type: &str) -> Result<ScanVerdict> {
        Ok(unknown_verdict(content))
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn unknown_verdict(content: &[u8]) -> ScanVerdict {
    ScanVerdict {
        verdict: Severity::Unknown,
        max_severity: None,
        findings: vec![],
        scanner_version: None,
        cached: false,
        content_hash: sha256_hex(content),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("sha256:{}", hex::encode(hash))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
