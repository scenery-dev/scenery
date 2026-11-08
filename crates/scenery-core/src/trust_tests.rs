#![allow(clippy::unwrap_used)]
use super::*;
use crate::signed::testing::*;
use camino::Utf8PathBuf;

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let st = AppState::bootstrap_in(root).unwrap();
    (dir, st)
}

#[test]
fn placeholders_are_not_trust_roots() {
    assert!(is_compiled_origin("https://app.scenery.dev"));
    assert!(compiled_keys("https://app.scenery.dev").is_empty());
    let (_d, st) = state();
    assert!(!has_trust_root(&st, "https://app.scenery.dev"));
    assert!(!has_trust_root(&st, "https://other.example"));
}

#[test]
fn pin_and_reset_roundtrip() {
    let (_d, st) = state();
    let k = trusted(1, "scn-x-a");
    pin_key(&st, "https://r.example", "scn-x-a", &k.pubkey).unwrap();
    let keys = keys_for_origin(&st, "https://r.example");
    assert_eq!(keys, vec![k]);
    assert!(keys_for_origin(&st, "https://other.example").is_empty());
    assert_eq!(reset_origin(&st, "https://r.example").unwrap(), 1);
    assert!(!has_trust_root(&st, "https://r.example"));
}

#[test]
fn fingerprint_selection_matches_only_exact_key() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let k1 = trusted(1, "a");
    let k2 = trusted(2, "b");
    let resp = SigningKeysResponse {
        iss: "https://r.example".into(),
        keys: vec![
            SigningKeyInfo {
                kid: "a".into(),
                alg: "ed25519".into(),
                public_key: URL_SAFE_NO_PAD.encode(k1.pubkey),
                status: "active".into(),
            },
            SigningKeyInfo {
                kid: "b".into(),
                alg: "ed25519".into(),
                public_key: URL_SAFE_NO_PAD.encode(k2.pubkey),
                status: "standby".into(),
            },
        ],
    };
    let (kid, pk) =
        select_key_by_fingerprint(&resp, "https://r.example", &fingerprint(&k2.pubkey)).unwrap();
    assert_eq!((kid.as_str(), pk), ("b", k2.pubkey));
    assert!(select_key_by_fingerprint(&resp, "https://r.example", &"0".repeat(64)).is_err());
    assert!(
        select_key_by_fingerprint(&resp, "https://evil.example", &fingerprint(&k1.pubkey)).is_err()
    );
}
