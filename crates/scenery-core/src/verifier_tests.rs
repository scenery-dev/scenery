#![allow(clippy::unwrap_used)]
use super::*;
use crate::signed::testing::*;
use crate::signed::{STATUS_ACTIVE, TYP_MCP_SERVER};
use camino::Utf8PathBuf;

const ORG: &str = "org-1";

fn paired_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap())
        .unwrap();
    st.set_sync_state(REGISTRY_ORIGIN_KEY, TEST_ISS).unwrap();
    st.set_sync_state(ORG_ID_KEY, ORG).unwrap();
    (dir, st)
}

#[test]
fn no_origin_and_no_trust_root_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap())
        .unwrap();
    let e = signed_envelope(TYP_MCP_SERVER, ORG, "s", 1, STATUS_ACTIVE, None);
    assert!(matches!(
        verify_for_device(&st, TYP_MCP_SERVER, &e, None),
        Err(VerifyError::NoRegistryOrigin)
    ));
    st.set_sync_state(REGISTRY_ORIGIN_KEY, TEST_ISS).unwrap();
    assert!(matches!(
        verify_for_device(&st, TYP_MCP_SERVER, &e, None),
        Err(VerifyError::NoTrustRoot(_))
    ));
}

#[test]
fn seq_high_water_mark_blocks_replay() {
    let (_d, st) = paired_state();
    trust::pin_key(&st, TEST_ISS, TEST_KID, &trusted(1, TEST_KID).pubkey).unwrap();
    let new = signed_envelope(TYP_MCP_SERVER, ORG, "mcp_server:x", 9, STATUS_ACTIVE, None);
    let old = signed_envelope(TYP_MCP_SERVER, ORG, "mcp_server:x", 4, STATUS_ACTIVE, None);
    verify_for_device(&st, TYP_MCP_SERVER, &new, Some("mcp_server:x")).unwrap();
    assert_eq!(max_seq(&st, "mcp_server:x").unwrap(), Some(9));
    // Redelivery of the same seq is fine; an older one is a replay.
    verify_for_device(&st, TYP_MCP_SERVER, &new, None).unwrap();
    assert!(matches!(
        verify_for_device(&st, TYP_MCP_SERVER, &old, None),
        Err(VerifyError::Sign(SignError::StaleSeq { got: 4, max: 9 }))
    ));
    // A different subject has its own mark.
    let other = signed_envelope(TYP_MCP_SERVER, ORG, "mcp_server:y", 1, STATUS_ACTIVE, None);
    verify_for_device(&st, TYP_MCP_SERVER, &other, None).unwrap();
}

#[test]
fn subject_mismatch_is_rejected() {
    let (_d, st) = paired_state();
    trust::pin_key(&st, TEST_ISS, TEST_KID, &trusted(1, TEST_KID).pubkey).unwrap();
    let e = signed_envelope(TYP_MCP_SERVER, ORG, "mcp_server:x", 1, STATUS_ACTIVE, None);
    assert!(verify_for_device(&st, TYP_MCP_SERVER, &e, Some("mcp_server:other")).is_err());
}
