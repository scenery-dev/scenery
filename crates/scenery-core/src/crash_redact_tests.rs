#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn r(s: &str) -> String {
    redact(s, Some("/Users/alice"), Some("alice"))
}

#[test]
fn home_and_other_users_paths_collapse() {
    let out = r("a /Users/alice/x.rs b /Users/bob/y.rs c /home/carol/z d C:\\Users\\dave\\w.rs");
    for name in ["alice", "bob", "carol", "dave"] {
        assert!(!out.contains(name), "{out}");
    }
    assert!(out.contains("~/x.rs") && out.contains("~/y.rs") && out.contains("~/z"));
    assert!(out.contains("~\\w.rs"));
}

#[test]
fn known_prefixes_masked() {
    let out = r("scn_pat_abc123 ghp_abcdef sk-live-1 AKIA1234");
    assert_eq!(out.matches(REDACTED).count(), 4, "{out}");
}

#[test]
fn newline_separated_text_is_masked() {
    let out = r("line one\nscn_pat_abc123\nnext eyJhbGciOi.payload.sig\nend");
    assert!(
        !out.contains("scn_pat_abc123") && !out.contains("eyJhbGciOi"),
        "{out}"
    );
    assert!(out.contains("line one") && out.contains("end"));
}

#[test]
fn key_equals_value_is_masked() {
    let out = r("failed token=eyJabc.def.ghi and password=hunter2 ok");
    assert!(!out.contains("eyJabc") && !out.contains("hunter2"), "{out}");
}

#[test]
fn quoted_json_is_masked() {
    let out = r(r#"body {"token":"sk-abc","name":"visible","api_key":"short"}"#);
    assert!(!out.contains("sk-abc") && !out.contains("short"), "{out}");
    assert!(out.contains("visible"));
}

#[test]
fn authorization_headers_are_masked() {
    let out = r("Authorization: Bearer abc\nauthorization: xyz\nProxy bearer qqq");
    for s in ["abc", "xyz", "qqq"] {
        assert!(!out.contains(s), "{out}");
    }
}

#[test]
fn url_query_and_userinfo_are_masked() {
    let out = r("GET https://user:pw@example.com/path?code=123&state=abc failed");
    assert!(
        !out.contains("pw") && !out.contains("code=123") && !out.contains("user:"),
        "{out}"
    );
    assert!(out.contains("example.com/path"));
}

#[test]
fn emails_are_masked() {
    let out = r("owner bob@example.com rejected");
    assert!(!out.contains("bob@"), "{out}");
}

#[test]
fn long_opaque_runs_masked_but_paths_survive() {
    let out = r("blob 0123456789abcdef0123456789abcdef and crates/scenery-core/src/state.rs");
    assert!(!out.contains("0123456789abcdef0123"), "{out}");
    assert!(out.contains("crates/scenery-core/src/state.rs"));
}

#[test]
fn backtrace_keeps_only_symbols() {
    let raw = "   0: scenery_core::state::open\n             at /Users/alice/code/state.rs:10:5\n   1: main\n             at ./x.rs:1\n";
    let out = backtrace_symbols(raw);
    assert!(out.contains("scenery_core::state::open") && out.contains("1: main"));
    assert!(!out.contains("alice") && !out.contains("at "));
}
