#![allow(clippy::unwrap_used)]

use super::*;

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn hash_is_stable_vector() {
    let (h, basis) = hash_identity(Some(" abc-def "), "Host", "alice");
    assert_eq!(h, sha(b"scenery-scan-identity-v1\0ABC-DEF\0alice"));
    assert_eq!(basis, IdentityBasis::Hardware);
    assert_eq!(h.len(), 64);
    assert!(h
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
}

#[test]
fn hostname_fallback() {
    let want = sha(b"scenery-scan-identity-v1\0host:host\0alice");
    for hw in [None, Some("  ")] {
        let (h, basis) = hash_identity(hw, "Host", "alice");
        assert_eq!(basis, IdentityBasis::Hostname);
        assert_eq!(h, want);
    }
}

#[test]
fn user_changes_hash() {
    let (a, _) = hash_identity(Some("X"), "h", "alice");
    let (b, _) = hash_identity(Some("X"), "h", "bob");
    assert_ne!(a, b);
}

#[test]
fn parse_ioreg_line() {
    assert_eq!(
        parse_ioreg("    \"IOPlatformUUID\" = \"1234-AB\"\n"),
        Some("1234-AB".to_string())
    );
    assert_eq!(parse_ioreg("nothing here\n"), None);
}

#[test]
fn basis_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&IdentityBasis::Hardware).unwrap(),
        "\"hardware\""
    );
    assert_eq!(
        serde_json::to_string(&IdentityBasis::Hostname).unwrap(),
        "\"hostname\""
    );
}
