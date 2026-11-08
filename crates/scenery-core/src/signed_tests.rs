#![allow(clippy::unwrap_used)]
use super::testing::*;
use super::*;

const ORG: &str = "11111111-1111-1111-1111-111111111111";

fn keys() -> Vec<TrustedKey> {
    vec![trusted(1, TEST_KID)]
}

fn params<'a>(keys: &'a [TrustedKey], typ: &'a str) -> VerifyParams<'a> {
    VerifyParams {
        keys,
        iss: TEST_ISS,
        org: Some(ORG),
        typ,
        max_seq: None,
    }
}

fn good(payload: Option<&[u8]>) -> Envelope {
    signed_envelope(
        TYP_MCP_SERVER,
        ORG,
        "mcp_server:abc",
        5,
        STATUS_ACTIVE,
        payload,
    )
}

#[test]
fn valid_envelope_verifies_and_returns_payload() {
    let payload = br#"{"a":1}"#;
    let k = keys();
    let v = verify(&good(Some(payload)), &params(&k, TYP_MCP_SERVER)).unwrap();
    assert_eq!(v.sub, "mcp_server:abc");
    assert_eq!(v.seq, 5);
    assert_eq!(v.status, STATUS_ACTIVE);
    assert_eq!(v.payload.as_deref(), Some(&payload[..]));
}

#[test]
fn tampered_statement_is_rejected() {
    let k = keys();
    let mut e = good(None);
    let mut st = URL_SAFE_NO_PAD.decode(&e.statement).unwrap();
    st[10] ^= 1;
    e.statement = URL_SAFE_NO_PAD.encode(st);
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::BadSignature
    );
}

#[test]
fn wrong_key_is_rejected() {
    let k = vec![trusted(2, TEST_KID)];
    assert_eq!(
        verify(&good(None), &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::BadSignature
    );
}

#[test]
fn wrong_kid_is_rejected() {
    let k = vec![trusted(1, "scn-other")];
    assert!(matches!(
        verify(&good(None), &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::UnknownKid(_)
    ));
}

#[test]
fn wrong_issuer_is_rejected() {
    let k = keys();
    let st = statement_json(
        TYP_MCP_SERVER,
        "https://evil.example",
        ORG,
        "s",
        1,
        "active",
        None,
    );
    let e = envelope(1, TEST_KID, &st, None);
    assert!(matches!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::WrongIssuer { .. }
    ));
}

#[test]
fn wrong_org_is_rejected_and_unknown_org_too() {
    let k = keys();
    let e = signed_envelope(TYP_MCP_SERVER, "other-org", "s", 1, "active", None);
    assert!(matches!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::WrongOrg { .. }
    ));
    let mut p = params(&k, TYP_MCP_SERVER);
    p.org = None;
    assert_eq!(verify(&good(None), &p).unwrap_err(), SignError::OrgUnknown);
}

#[test]
fn wrong_type_is_rejected() {
    let k = keys();
    assert!(matches!(
        verify(&good(None), &params(&k, TYP_ORG_POLICY)).unwrap_err(),
        SignError::WrongType { .. }
    ));
}

#[test]
fn lower_seq_rejected_equal_seq_accepted() {
    let k = keys();
    let mut p = params(&k, TYP_MCP_SERVER);
    p.max_seq = Some(6);
    assert_eq!(
        verify(&good(None), &p).unwrap_err(),
        SignError::StaleSeq { got: 5, max: 6 }
    );
    p.max_seq = Some(5);
    assert!(verify(&good(None), &p).is_ok());
}

#[test]
fn payload_mismatch_missing_and_unbound() {
    let k = keys();
    // Statement commits to payload A, envelope carries payload B.
    let st = statement_json(TYP_MCP_SERVER, TEST_ISS, ORG, "s", 1, "active", Some(b"A"));
    let e = envelope(1, TEST_KID, &st, Some(b"B"));
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::PayloadMismatch
    );
    let e = envelope(1, TEST_KID, &st, None);
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::PayloadMissing
    );
    let st = statement_json(TYP_MCP_SERVER, TEST_ISS, ORG, "s", 1, "active", None);
    let e = envelope(1, TEST_KID, &st, Some(b"B"));
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::PayloadUnbound
    );
}

#[test]
fn star_org_only_for_revocation() {
    let k = keys();
    let e = signed_envelope(
        TYP_REVOCATION,
        "*",
        "revocation:mcp_server:x:",
        1,
        STATUS_REVOKED,
        None,
    );
    let v = verify(&e, &params(&k, TYP_REVOCATION)).unwrap();
    assert_eq!(v.org, "*");
    let e = signed_envelope(TYP_MCP_SERVER, "*", "mcp_server:x", 1, STATUS_ACTIVE, None);
    assert!(matches!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::WrongOrg { .. }
    ));
    // `*` revocation verifies even before the device org is known.
    let e = signed_envelope(
        TYP_REVOCATION,
        "*",
        "revocation:x:y:",
        1,
        STATUS_CLEARED,
        None,
    );
    let mut p = params(&k, TYP_REVOCATION);
    p.org = None;
    assert!(verify(&e, &p).is_ok());
}

#[test]
fn bad_version_oversize_and_status() {
    let k = keys();
    let mut e = good(None);
    e.v = 2;
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::UnsupportedVersion(2)
    );
    let big = vec![b' '; MAX_STATEMENT_BYTES + 1];
    let e = envelope(1, TEST_KID, &big, None);
    assert_eq!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::StatementTooLarge
    );
    let e = signed_envelope(TYP_MCP_SERVER, ORG, "s", 1, "weird", None);
    assert!(matches!(
        verify(&e, &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::UnknownStatus(_)
    ));
}

#[test]
fn weak_zero_key_never_verifies() {
    let k = vec![TrustedKey {
        kid: TEST_KID.into(),
        pubkey: [0u8; 32],
    }];
    assert_eq!(
        verify(&good(None), &params(&k, TYP_MCP_SERVER)).unwrap_err(),
        SignError::BadSignature
    );
}
