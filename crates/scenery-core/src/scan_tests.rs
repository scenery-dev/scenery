//! Unit tests for `scan.rs`.
//!
//! Kept in a separate file so `#[allow(clippy::unwrap_used)]` does not leak
//! into production code.
#![allow(clippy::unwrap_used)]

use super::*;

// ── NoOpScanClient ────────────────────────────────────────────────────────────

#[test]
fn noop_scan_client_returns_unknown_without_errors() {
    let client = NoOpScanClient;
    let verdict = client.scan(b"some content", "skill_md").unwrap();
    assert_eq!(verdict.verdict, Severity::Unknown);
    assert!(verdict.findings.is_empty());
    assert!(!verdict.cached);
    assert!(verdict.content_hash.starts_with("sha256:"));
}

#[test]
fn noop_scan_client_hash_is_deterministic() {
    let client = NoOpScanClient;
    let content = b"deterministic content";
    let v1 = client.scan(content, "skill_md").unwrap();
    let v2 = client.scan(content, "skill_md").unwrap();
    assert_eq!(v1.content_hash, v2.content_hash);
}

#[test]
fn noop_scan_client_different_content_different_hash() {
    let client = NoOpScanClient;
    let v1 = client.scan(b"aaa", "skill_md").unwrap();
    let v2 = client.scan(b"bbb", "skill_md").unwrap();
    assert_ne!(v1.content_hash, v2.content_hash);
}

// ── ScanVerdict::is_risky ─────────────────────────────────────────────────────

#[test]
fn is_risky_returns_false_for_clean() {
    let v = make_verdict(Severity::Clean);
    assert!(!v.is_risky());
}

#[test]
fn is_risky_returns_false_for_info() {
    let v = make_verdict(Severity::Info);
    assert!(!v.is_risky());
}

#[test]
fn is_risky_returns_false_for_unknown() {
    let v = make_verdict(Severity::Unknown);
    assert!(!v.is_risky());
}

#[test]
fn is_risky_returns_true_for_low() {
    let v = make_verdict(Severity::Low);
    assert!(v.is_risky());
}

#[test]
fn is_risky_returns_true_for_medium() {
    let v = make_verdict(Severity::Medium);
    assert!(v.is_risky());
}

#[test]
fn is_risky_returns_true_for_high() {
    let v = make_verdict(Severity::High);
    assert!(v.is_risky());
}

#[test]
fn is_risky_returns_true_for_critical() {
    let v = make_verdict(Severity::Critical);
    assert!(v.is_risky());
}

// ── ScanVerdict::requires_confirmation ───────────────────────────────────────

#[test]
fn requires_confirmation_false_for_clean() {
    assert!(!make_verdict(Severity::Clean).requires_confirmation());
}

#[test]
fn requires_confirmation_false_for_info() {
    assert!(!make_verdict(Severity::Info).requires_confirmation());
}

#[test]
fn requires_confirmation_false_for_unknown() {
    assert!(!make_verdict(Severity::Unknown).requires_confirmation());
}

#[test]
fn requires_confirmation_false_for_low() {
    assert!(!make_verdict(Severity::Low).requires_confirmation());
}

#[test]
fn requires_confirmation_true_for_medium() {
    assert!(make_verdict(Severity::Medium).requires_confirmation());
}

#[test]
fn requires_confirmation_true_for_high() {
    assert!(make_verdict(Severity::High).requires_confirmation());
}

#[test]
fn requires_confirmation_true_for_critical() {
    assert!(make_verdict(Severity::Critical).requires_confirmation());
}

// ── Severity ordering ─────────────────────────────────────────────────────────

#[test]
fn severity_ordering_is_correct() {
    assert!(Severity::Unknown < Severity::Clean);
    assert!(Severity::Clean < Severity::Info);
    assert!(Severity::Info < Severity::Low);
    assert!(Severity::Low < Severity::Medium);
    assert!(Severity::Medium < Severity::High);
    assert!(Severity::High < Severity::Critical);
}

// ── Severity::from_str ────────────────────────────────────────────────────────

#[test]
fn severity_from_str_known_values() {
    assert_eq!(Severity::parse_verdict("clean"), Severity::Clean);
    assert_eq!(Severity::parse_verdict("info"), Severity::Info);
    assert_eq!(Severity::parse_verdict("low"), Severity::Low);
    assert_eq!(Severity::parse_verdict("medium"), Severity::Medium);
    assert_eq!(Severity::parse_verdict("high"), Severity::High);
    assert_eq!(Severity::parse_verdict("critical"), Severity::Critical);
    assert_eq!(Severity::parse_verdict("unknown"), Severity::Unknown);
}

#[test]
fn severity_from_str_unrecognised_maps_to_unknown() {
    assert_eq!(Severity::parse_verdict("bogus"), Severity::Unknown);
    assert_eq!(Severity::parse_verdict("CLEAN"), Severity::Unknown); // case-sensitive
    assert_eq!(Severity::parse_verdict(""), Severity::Unknown);
}

// ── HttpScanClient: fail-open on network error ────────────────────────────────

#[test]
fn http_scan_client_returns_unknown_when_registry_unreachable() {
    // Point at a port that won't respond.
    let client = HttpScanClient::new("http://127.0.0.1:1", "test-token");
    let verdict = client.scan(b"hello", "skill_md").unwrap();
    assert_eq!(verdict.verdict, Severity::Unknown);
}

// ── HttpScanClient: mockito integration ──────────────────────────────────────

#[test]
fn http_scan_client_parses_clean_verdict() {
    use mockito::Server;

    let mut server = Server::new();
    let mock = server
        .mock("POST", "/runner/scan")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "content_hash": "sha256:abc123",
                "verdict": "clean",
                "max_severity": "clean",
                "findings": [],
                "scanner_version": "1.0.2",
                "cached": true
            }"#,
        )
        .create();

    let client = HttpScanClient::new(server.url(), "test-token");
    let verdict = client.scan(b"safe content", "skill_md").unwrap();

    assert_eq!(verdict.verdict, Severity::Clean);
    assert!(verdict.findings.is_empty());
    assert_eq!(verdict.scanner_version.as_deref(), Some("1.0.2"));
    assert!(verdict.cached);
    assert_eq!(verdict.content_hash, "sha256:abc123");
    mock.assert();
}

#[test]
fn http_scan_client_parses_high_verdict_with_findings() {
    use mockito::Server;

    let mut server = Server::new();
    let mock = server
        .mock("POST", "/runner/scan")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "content_hash": "sha256:def456",
                "verdict": "high",
                "max_severity": "high",
                "findings": [
                    {
                        "rule_id": "SEC001",
                        "severity": "high",
                        "title": "Sensitive data exfiltration",
                        "description": "Skill sends data to unknown endpoint."
                    }
                ],
                "scanner_version": "1.0.2",
                "cached": false
            }"#,
        )
        .create();

    let client = HttpScanClient::new(server.url(), "test-token");
    let verdict = client.scan(b"malicious content", "skill_md").unwrap();

    assert_eq!(verdict.verdict, Severity::High);
    assert_eq!(verdict.findings.len(), 1);
    assert_eq!(verdict.findings[0].rule_id.as_deref(), Some("SEC001"));
    assert_eq!(
        verdict.findings[0].title.as_deref(),
        Some("Sensitive data exfiltration")
    );
    assert!(verdict.is_risky());
    assert!(verdict.requires_confirmation());
    mock.assert();
}

#[test]
fn http_scan_client_returns_unknown_on_5xx() {
    use mockito::Server;

    let mut server = Server::new();
    let mock = server
        .mock("POST", "/runner/scan")
        .with_status(500)
        .with_body("internal error")
        .create();

    let client = HttpScanClient::new(server.url(), "test-token");
    let verdict = client.scan(b"content", "skill_md").unwrap();

    assert_eq!(verdict.verdict, Severity::Unknown);
    mock.assert();
}

#[test]
fn http_scan_client_returns_unknown_on_malformed_json() {
    use mockito::Server;

    let mut server = Server::new();
    let mock = server
        .mock("POST", "/runner/scan")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("not json at all")
        .create();

    let client = HttpScanClient::new(server.url(), "test-token");
    let verdict = client.scan(b"content", "skill_md").unwrap();

    assert_eq!(verdict.verdict, Severity::Unknown);
    mock.assert();
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn make_verdict(severity: Severity) -> ScanVerdict {
    ScanVerdict {
        verdict: severity,
        max_severity: None,
        findings: vec![],
        scanner_version: None,
        cached: false,
        content_hash: "sha256:0000".to_string(),
    }
}
