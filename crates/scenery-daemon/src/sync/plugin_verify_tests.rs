#![allow(clippy::unwrap_used)]
use super::*;
use camino::Utf8PathBuf;
use scenery_core::signed::testing::*;
use scenery_core::signed::{STATUS_ACTIVE, TYP_PLUGIN_VERSION};
use scenery_core::{trust, verifier};
use serde_json::json;

const ORG: &str = "org-1";

fn paired() -> (tempfile::TempDir, AppState) {
    let d = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(d.path().to_path_buf()).unwrap())
        .unwrap();
    st.set_sync_state(verifier::REGISTRY_ORIGIN_KEY, TEST_ISS)
        .unwrap();
    st.set_sync_state(verifier::ORG_ID_KEY, ORG).unwrap();
    trust::pin_key(&st, TEST_ISS, TEST_KID, &trusted(1, TEST_KID).pubkey).unwrap();
    (d, st)
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn envelope_for(slug: &str, ver: &str, seq: u64, files: serde_json::Value) -> serde_json::Value {
    let payload =
        serde_json::to_vec(&json!({"slug": slug, "version": ver, "files": files})).unwrap();
    let st = statement_json(
        TYP_PLUGIN_VERSION,
        TEST_ISS,
        ORG,
        &format!("plugin:{slug}@{ver}"),
        seq,
        STATUS_ACTIVE,
        Some(&payload),
    );
    serde_json::to_value(envelope(1, TEST_KID, &st, Some(&payload))).unwrap()
}

fn body(
    files: serde_json::Value,
    text: serde_json::Value,
    bin: serde_json::Value,
    seq: u64,
) -> serde_json::Value {
    json!({
        "components": {"files": text, "binary_files": bin, "executables": ["evil.sh"]},
        "signed": envelope_for("plug", "1.0.0", seq, files),
    })
}

#[test]
fn valid_listing_installs_and_executables_come_from_signed_entries() {
    let (_d, st) = paired();
    let a = b"hello";
    let b = [1u8, 2, 3];
    let files = json!([
        {"path": ".claude-plugin/plugin.json", "sha256": sha(a), "size": a.len(), "executable": false},
        {"path": "bin/run", "sha256": sha(&b), "size": b.len(), "executable": true},
    ]);
    use base64::Engine as _;
    let bin_b64 = base64::engine::general_purpose::STANDARD.encode(b);
    let out = verify_plugin_content(
        &st,
        "plug",
        "1.0.0",
        &body(
            files,
            json!({".claude-plugin/plugin.json": "hello"}),
            json!({"bin/run": bin_b64}),
            1,
        ),
    )
    .unwrap();
    assert_eq!(out.files.len(), 2);
    // The unsigned `executables: ["evil.sh"]` is ignored.
    assert_eq!(out.executables, vec!["bin/run".to_string()]);
}

#[test]
fn unsigned_unlisted_mismatched_and_missing_files_are_rejected() {
    let (_d, st) = paired();
    let files = json!([{"path": "a.txt", "sha256": sha(b"x"), "size": 1, "executable": false}]);

    // Unsigned.
    let no_sig = json!({"components": {"files": {"a.txt": "x"}}});
    assert!(verify_plugin_content(&st, "plug", "1.0.0", &no_sig).is_err());
    // Unlisted extra file.
    let b = body(
        files.clone(),
        json!({"a.txt": "x", "extra.sh": "boom"}),
        json!({}),
        1,
    );
    let e = verify_plugin_content(&st, "plug", "1.0.0", &b).unwrap_err();
    assert!(
        format!("{e:#}").contains("not in the signed listing"),
        "{e:#}"
    );
    // Hash mismatch.
    let b = body(files.clone(), json!({"a.txt": "y"}), json!({}), 2);
    assert!(format!(
        "{:#}",
        verify_plugin_content(&st, "plug", "1.0.0", &b).unwrap_err()
    )
    .contains("hash"));
    // Listed but not delivered.
    let b = body(files.clone(), json!({}), json!({}), 3);
    assert!(format!(
        "{:#}",
        verify_plugin_content(&st, "plug", "1.0.0", &b).unwrap_err()
    )
    .contains("not delivered"));
    // Wrong plugin / version.
    let b = body(files, json!({"a.txt": "x"}), json!({}), 4);
    assert!(verify_plugin_content(&st, "other", "1.0.0", &b).is_err());
    assert!(verify_plugin_content(&st, "plug", "2.0.0", &b).is_err());
}

#[test]
fn unsafe_listed_paths_and_caps_are_rejected() {
    let (_d, st) = paired();
    let files =
        json!([{"path": "/Users/u/.zshrc", "sha256": sha(b"x"), "size": 1, "executable": false}]);
    let b = body(files, json!({"/Users/u/.zshrc": "x"}), json!({}), 1);
    assert!(verify_plugin_content(&st, "plug", "1.0.0", &b).is_err());

    let many: Vec<_> = (0..=MAX_PLUGIN_FILES)
        .map(|i| json!({"path": format!("f{i}"), "sha256": sha(b""), "size": 0, "executable": false}))
        .collect();
    let text: serde_json::Map<String, serde_json::Value> = (0..=MAX_PLUGIN_FILES)
        .map(|i| (format!("f{i}"), json!("")))
        .collect();
    let b = body(json!(many), json!(text), json!({}), 2);
    assert!(format!(
        "{:#}",
        verify_plugin_content(&st, "plug", "1.0.0", &b).unwrap_err()
    )
    .contains("more than"));
}

#[test]
fn tampered_envelope_is_rejected() {
    let (_d, st) = paired();
    let files = json!([]);
    let mut b = body(files, json!({}), json!({}), 1);
    b["signed"]["sig"] = json!("A".repeat(86));
    assert!(verify_plugin_content(&st, "plug", "1.0.0", &b).is_err());
}
