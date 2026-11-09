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
fn compiled_roots_are_real_keys() {
    for o in [
        "https://app.scenery.dev",
        "https://dev.scenery.dev",
        "https://demo.scenery.dev",
    ] {
        assert!(is_compiled_origin(o));
        assert_eq!(compiled_keys(o).len(), 2, "{o}");
    }
    let (_d, st) = state();
    assert!(has_trust_root(&st, "https://app.scenery.dev"));
    assert!(!has_trust_root(&st, "https://other.example"));
    assert!(TRUST_ROOTS
        .iter()
        .flat_map(|(_, ks)| ks.iter())
        .all(|(_, k)| *k != PLACEHOLDER_KEY));
}

/// Pins the KMS public keys: a changed byte here changes who the runner trusts.
#[test]
fn compiled_key_fingerprints_are_pinned() {
    let want = [
        ("scn-prod-a", "33a4e02f6692f54a"),
        ("scn-prod-b", "bc8da11f4901dd15"),
        ("scn-dev-a", "cc18702ef3a98ded"),
        ("scn-dev-b", "91e22561bd04aeaf"),
        ("scn-demo-a", "27592e02b1ac31c5"),
        ("scn-demo-b", "bc85e592e02ef57a"),
    ];
    let all: Vec<&RootKey> = TRUST_ROOTS.iter().flat_map(|(_, ks)| ks.iter()).collect();
    assert_eq!(all.len(), want.len());
    for (kid, prefix) in want {
        let (_, k) = all.iter().find(|(k, _)| *k == kid).unwrap();
        assert!(fingerprint(k).starts_with(prefix), "{kid}");
    }
}

/// Real AWS KMS `Sign` (ED25519_SHA_512, RAW) output, made on the dev box with
/// `alias/scenery-sign-dev-a`, must pass dalek `verify_strict` under the
/// compiled-in `scn-dev-a` key.
#[test]
fn kms_vector_verifies_under_compiled_dev_key() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::{Signature, VerifyingKey};
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/kms_signing_vector.json")).unwrap();
    let dec = |k: &str| URL_SAFE_NO_PAD.decode(v[k].as_str().unwrap()).unwrap();
    let (stmt, sig, pk) = (dec("statement"), dec("sig"), dec("public_key"));
    assert!(stmt.len() <= 4096);
    let key = compiled_keys("https://dev.scenery.dev")
        .into_iter()
        .find(|k| k.kid == v["kid"].as_str().unwrap())
        .unwrap();
    assert_eq!(key.pubkey.as_slice(), pk.as_slice());
    let vk = VerifyingKey::from_bytes(&key.pubkey).unwrap();
    let sig = Signature::from_slice(&sig).unwrap();
    vk.verify_strict(&stmt, &sig).unwrap();
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
