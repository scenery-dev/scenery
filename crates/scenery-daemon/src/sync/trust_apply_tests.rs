#![allow(clippy::unwrap_used)]
use super::*;
use crate::sync::sse_client::{OrgPolicySnapshot, RevocationRecord};
use camino::Utf8PathBuf;
use scenery_core::revocation;
use scenery_core::signed::testing::*;
use scenery_core::signed::{
    STATUS_ACTIVE, STATUS_CLEARED, STATUS_REVOKED, TYP_ORG_POLICY, TYP_REVOCATION,
};

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

fn policy_env(seq: u64, mode: &str, block: bool) -> Envelope {
    let p = serde_json::to_vec(&serde_json::json!({
        "managed_paths_mode": mode, "block_third_party_inference": block
    }))
    .unwrap();
    let st = statement_json(
        TYP_ORG_POLICY,
        TEST_ISS,
        ORG,
        "org_policy",
        seq,
        STATUS_ACTIVE,
        Some(&p),
    );
    envelope(1, TEST_KID, &st, Some(&p))
}

#[test]
fn signed_org_policy_applies_payload_not_unsigned_siblings() {
    let (_d, st) = paired();
    let env = policy_env(1, "quarantine", true);
    // Unsigned siblings lie; only the signed payload counts.
    assert!(apply_org_policy(
        &st,
        Some(&env),
        Some("audit"),
        Some(false)
    ));
    assert_eq!(
        st.get_sync_state("managed_paths_mode").unwrap().as_deref(),
        Some("quarantine")
    );
    assert!(st
        .block_third_party_inference
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn unsigned_tampered_and_replayed_org_policy_is_ignored() {
    let (_d, st) = paired();
    assert!(!apply_org_policy(&st, None, Some("quarantine"), Some(true)));
    assert_eq!(st.get_sync_state("managed_paths_mode").unwrap(), None);

    let mut env = policy_env(5, "quarantine", true);
    let mut bytes = base64_decode(&env.statement);
    bytes[12] ^= 1;
    env.statement = base64_encode(&bytes);
    assert!(!apply_org_policy(&st, Some(&env), None, None));

    assert!(apply_org_policy(
        &st,
        Some(&policy_env(5, "audit", false)),
        None,
        None
    ));
    // Older seq: replay.
    assert!(!apply_org_policy(
        &st,
        Some(&policy_env(4, "quarantine", true)),
        None,
        None
    ));
    assert_eq!(
        st.get_sync_state("managed_paths_mode").unwrap().as_deref(),
        Some("audit")
    );
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.decode(s).unwrap()
}
fn base64_encode(b: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(b)
}

fn rev_env(org: &str, key: &str, seq: u64, status: &str) -> Envelope {
    signed_envelope(
        TYP_REVOCATION,
        org,
        &format!("revocation:mcp_server:{key}:"),
        seq,
        status,
        None,
    )
}

fn rec(key: &str, signed: Option<Envelope>) -> RevocationRecord {
    RevocationRecord {
        artifact_type: "mcp_server".into(),
        artifact_key: key.into(),
        version: None,
        signed,
    }
}

#[test]
fn only_a_verified_cleared_statement_lifts_a_revocation() {
    let (_d, st) = paired();
    let good = rev_env("*", "srv-1", 2, STATUS_CLEARED);
    assert!(verified_clear(
        &st,
        "mcp_server",
        "srv-1",
        None,
        Some(&good)
    ));
    // Missing, wrong subject and `revoked` statements do not clear.
    assert!(!verified_clear(&st, "mcp_server", "srv-1", None, None));
    assert!(!verified_clear(
        &st,
        "mcp_server",
        "srv-2",
        None,
        Some(&good)
    ));
    let revoked = rev_env("*", "srv-1", 3, STATUS_REVOKED);
    assert!(!verified_clear(
        &st,
        "mcp_server",
        "srv-1",
        None,
        Some(&revoked)
    ));
    // Forged: signed by an unknown key.
    let st_bytes = statement_json(
        TYP_REVOCATION,
        TEST_ISS,
        "*",
        "revocation:mcp_server:srv-1:",
        9,
        STATUS_CLEARED,
        None,
    );
    let forged = envelope(7, TEST_KID, &st_bytes, None);
    assert!(!verified_clear(
        &st,
        "mcp_server",
        "srv-1",
        None,
        Some(&forged)
    ));
}

#[test]
fn snapshot_drop_without_cleared_keeps_the_gate() {
    let (_d, st) = paired();
    // Locally revoked.
    revocation::upsert_revocation(&st, "mcp_server", "srv-1", None).unwrap();
    revocation::upsert_revocation(&st, "mcp_server", "srv-2", None).unwrap();
    // The next snapshot lists only srv-2 (srv-1 "dropped") and carries a
    // forged clear for srv-2: neither lifts anything.
    let forged = rev_env("*", "srv-2", 1, STATUS_ACTIVE);
    let plan = plan_revocations(&st, &[rec("srv-2", Some(forged))]);
    assert_eq!(plan.active.len(), 1);
    assert!(plan.cleared.is_empty());
    revocation::merge_snapshot(&st, &plan.active, &plan.cleared).unwrap();
    assert!(revocation::is_revoked(&st, "mcp_server", "srv-1", None).unwrap());
    assert!(revocation::is_revoked(&st, "mcp_server", "srv-2", None).unwrap());

    // A snapshot entry with a valid cleared statement does lift it.
    let ok = rev_env("*", "srv-1", 5, STATUS_CLEARED);
    let plan = plan_revocations(&st, &[rec("srv-1", Some(ok))]);
    assert_eq!(plan.cleared.len(), 1);
    revocation::merge_snapshot(&st, &plan.active, &plan.cleared).unwrap();
    assert!(!revocation::is_revoked(&st, "mcp_server", "srv-1", None).unwrap());
    assert!(revocation::is_revoked(&st, "mcp_server", "srv-2", None).unwrap());
}

#[test]
fn org_policy_snapshot_helper_uses_the_envelope() {
    let (_d, st) = paired();
    let p = OrgPolicySnapshot {
        managed_paths_mode: Some("audit".into()),
        block_third_party_inference: Some(false),
        signed: Some(policy_env(1, "quarantine", false)),
    };
    assert!(apply_org_policy_snapshot(&st, &p));
    assert_eq!(
        st.get_sync_state("managed_paths_mode").unwrap().as_deref(),
        Some("quarantine")
    );
}

#[test]
fn first_verified_snapshot_pins_the_org_once() {
    let (_d, st) = paired();
    st.delete_sync_state(verifier::ORG_ID_KEY).unwrap();
    let env = policy_env(1, "audit", false);
    // Wrong org in the snapshot: statement org does not match, no pin.
    pin_org_from_snapshot(&st, Some("other-org"), &[(TYP_ORG_POLICY, &env)]);
    assert_eq!(verifier::device_org(&st), None);
    pin_org_from_snapshot(&st, Some(ORG), &[(TYP_ORG_POLICY, &env)]);
    assert_eq!(verifier::device_org(&st).as_deref(), Some(ORG));
    // Once pinned, a snapshot naming another org does not change it.
    pin_org_from_snapshot(&st, Some("other-org"), &[(TYP_ORG_POLICY, &env)]);
    assert_eq!(verifier::device_org(&st).as_deref(), Some(ORG));
}
