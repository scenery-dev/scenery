//! Verification of backend-signed statements (design 2026-11-08 §1).
//!
//! The backend signs the exact statement bytes with Ed25519. The runner
//! verifies over the decoded bytes (never re-canonicalising), then parses
//! them as JSON and checks issuer, org, type, sequence and the detached
//! payload hash. Nothing here does I/O; trust roots and the per-subject
//! sequence high-water mark are passed in.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Envelope format version understood by this runner.
pub const ENVELOPE_VERSION: u32 = 1;
/// Statements are small by design; the backend enforces the same cap.
pub const MAX_STATEMENT_BYTES: usize = 4096;
/// Cap on a detached payload (plugin file lists are the largest).
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

pub const TYP_MCP_SERVER: &str = "scenery.mcp_server.v1";
pub const TYP_ORG_POLICY: &str = "scenery.org_policy.v1";
pub const TYP_SKILL_ARTIFACT: &str = "scenery.skill_artifact.v1";
pub const TYP_PLUGIN_VERSION: &str = "scenery.plugin_version.v1";
pub const TYP_REVOCATION: &str = "scenery.revocation.v1";

pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_REVOKED: &str = "revoked";
pub const STATUS_CLEARED: &str = "cleared";

/// The `signed` object carried on wire records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub kid: String,
    /// base64url (no padding) statement bytes.
    pub statement: String,
    /// base64url (no padding) 64-byte Ed25519 signature.
    pub sig: String,
    /// base64url (no padding) detached payload bytes, when the statement
    /// carries a `payload_sha256`.
    #[serde(default)]
    pub payload: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignError {
    #[error("unsupported envelope version {0}")]
    UnsupportedVersion(u32),
    #[error("malformed envelope: {0}")]
    Malformed(String),
    #[error("statement larger than {MAX_STATEMENT_BYTES} bytes")]
    StatementTooLarge,
    #[error("payload larger than {MAX_PAYLOAD_BYTES} bytes")]
    PayloadTooLarge,
    #[error("key id '{0}' is not trusted for this registry")]
    UnknownKid(String),
    #[error("signature verification failed")]
    BadSignature,
    #[error("statement issuer '{got}' does not match the pinned registry '{want}'")]
    WrongIssuer { got: String, want: String },
    #[error("statement is for org '{got}', not this device's org")]
    WrongOrg { got: String },
    #[error("device org is not known yet; cannot accept an org-bound statement")]
    OrgUnknown,
    #[error("statement type '{got}' is not '{want}'")]
    WrongType { got: String, want: String },
    #[error("statement seq {got} is older than the last accepted {max} for this subject")]
    StaleSeq { got: u64, max: u64 },
    #[error("payload does not match the statement's payload_sha256")]
    PayloadMismatch,
    #[error("statement requires a payload but none was sent")]
    PayloadMissing,
    #[error("payload sent without a payload_sha256 in the statement")]
    PayloadUnbound,
    #[error("unknown statement status '{0}'")]
    UnknownStatus(String),
}

#[derive(Debug, Deserialize)]
struct RawStatement {
    typ: String,
    iss: String,
    org: String,
    sub: String,
    seq: u64,
    #[serde(default)]
    iat: i64,
    status: String,
    #[serde(default)]
    payload_sha256: Option<String>,
}

/// A statement that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedStatement {
    pub typ: String,
    pub iss: String,
    pub org: String,
    pub sub: String,
    pub seq: u64,
    pub iat: i64,
    pub status: String,
    pub kid: String,
    /// Decoded payload bytes, hash-checked. `None` when the statement has no
    /// detached payload.
    pub payload: Option<Vec<u8>>,
}

impl VerifiedStatement {
    /// Parse the verified payload as `T`.
    pub fn payload_json<T: serde::de::DeserializeOwned>(&self) -> Result<T, SignError> {
        let bytes = self.payload.as_deref().ok_or(SignError::PayloadMissing)?;
        serde_json::from_slice(bytes).map_err(|e| SignError::Malformed(format!("payload: {e}")))
    }
}

/// A trusted public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    pub kid: String,
    pub pubkey: [u8; 32],
}

/// What the caller expects of the statement.
#[derive(Debug, Clone)]
pub struct VerifyParams<'a> {
    /// Keys trusted for the pinned registry origin.
    pub keys: &'a [TrustedKey],
    /// Pinned registry origin (`https://app.scenery.dev`).
    pub iss: &'a str,
    /// The device's org id; `None` while unknown.
    pub org: Option<&'a str>,
    /// Required `typ`.
    pub typ: &'a str,
    /// Highest `seq` already accepted for this subject, if any.
    pub max_seq: Option<u64>,
}

fn b64(what: &str, s: &str) -> Result<Vec<u8>, SignError> {
    URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|e| SignError::Malformed(format!("{what}: {e}")))
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Verify an envelope. See module docs for the order of checks.
pub fn verify(env: &Envelope, p: &VerifyParams<'_>) -> Result<VerifiedStatement, SignError> {
    if env.v != ENVELOPE_VERSION {
        return Err(SignError::UnsupportedVersion(env.v));
    }
    let statement = b64("statement", &env.statement)?;
    if statement.len() > MAX_STATEMENT_BYTES {
        return Err(SignError::StatementTooLarge);
    }
    let sig_bytes = b64("sig", &env.sig)?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SignError::Malformed("sig is not 64 bytes".into()))?;
    let key = p
        .keys
        .iter()
        .find(|k| k.kid == env.kid)
        .ok_or_else(|| SignError::UnknownKid(env.kid.clone()))?;
    let vk = VerifyingKey::from_bytes(&key.pubkey).map_err(|_| SignError::BadSignature)?;
    vk.verify_strict(&statement, &Signature::from_bytes(&sig_arr))
        .map_err(|_| SignError::BadSignature)?;

    let raw: RawStatement = serde_json::from_slice(&statement)
        .map_err(|e| SignError::Malformed(format!("statement: {e}")))?;

    if raw.iss != p.iss {
        return Err(SignError::WrongIssuer {
            got: raw.iss,
            want: p.iss.to_string(),
        });
    }
    if raw.typ != p.typ {
        return Err(SignError::WrongType {
            got: raw.typ,
            want: p.typ.to_string(),
        });
    }
    check_org(&raw, p)?;
    if !matches!(
        raw.status.as_str(),
        STATUS_ACTIVE | STATUS_REVOKED | STATUS_CLEARED
    ) {
        return Err(SignError::UnknownStatus(raw.status));
    }
    // Equal seq is fine: redelivery is idempotent.
    if let Some(max) = p.max_seq {
        if raw.seq < max {
            return Err(SignError::StaleSeq { got: raw.seq, max });
        }
    }
    let payload = check_payload(&raw, env)?;

    Ok(VerifiedStatement {
        typ: raw.typ,
        iss: raw.iss,
        org: raw.org,
        sub: raw.sub,
        seq: raw.seq,
        iat: raw.iat,
        status: raw.status,
        kid: env.kid.clone(),
        payload,
    })
}

fn check_org(raw: &RawStatement, p: &VerifyParams<'_>) -> Result<(), SignError> {
    if raw.org == "*" {
        // Platform-wide statements exist for revocation only.
        if raw.typ == TYP_REVOCATION {
            return Ok(());
        }
        return Err(SignError::WrongOrg { got: raw.org.clone() });
    }
    match p.org {
        None => Err(SignError::OrgUnknown),
        Some(org) if org == raw.org => Ok(()),
        Some(_) => Err(SignError::WrongOrg { got: raw.org.clone() }),
    }
}

fn check_payload(raw: &RawStatement, env: &Envelope) -> Result<Option<Vec<u8>>, SignError> {
    match (&raw.payload_sha256, &env.payload) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(SignError::PayloadUnbound),
        (Some(_), None) => Err(SignError::PayloadMissing),
        (Some(want), Some(enc)) => {
            // Bound the decode before allocating: base64 expands by 4/3.
            if enc.len() > MAX_PAYLOAD_BYTES / 3 * 4 + 4 {
                return Err(SignError::PayloadTooLarge);
            }
            let bytes = b64("payload", enc)?;
            if bytes.len() > MAX_PAYLOAD_BYTES {
                return Err(SignError::PayloadTooLarge);
            }
            if !hex_sha256(&bytes).eq_ignore_ascii_case(want) {
                return Err(SignError::PayloadMismatch);
            }
            Ok(Some(bytes))
        }
    }
}

#[cfg(test)]
#[path = "signed_tests.rs"]
mod tests;

/// Test support: build and sign envelopes with a deterministic keypair.
/// Compiled into the library (not just `cfg(test)`) so the daemon crate's
/// tests can sign fixtures; it holds no secret that matters (fixed seed) and
/// is not wired to any production trust root.
pub mod testing {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub const TEST_ISS: &str = "https://test.scenery.invalid";
    pub const TEST_KID: &str = "scn-test-a";

    pub fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn trusted(seed: u8, kid: &str) -> TrustedKey {
        TrustedKey {
            kid: kid.to_string(),
            pubkey: signing_key(seed).verifying_key().to_bytes(),
        }
    }

    pub fn statement_json(
        typ: &str,
        iss: &str,
        org: &str,
        sub: &str,
        seq: u64,
        status: &str,
        payload: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut v = serde_json::json!({
            "typ": typ, "iss": iss, "org": org, "sub": sub, "seq": seq,
            "iat": 1_762_560_000u64, "status": status,
        });
        if let Some(p) = payload {
            v["payload_sha256"] = serde_json::Value::String(hex_sha256(p));
        }
        serde_json::to_vec(&v).unwrap_or_default()
    }

    /// Sign `statement` bytes verbatim and wrap them in an envelope.
    pub fn envelope(seed: u8, kid: &str, statement: &[u8], payload: Option<&[u8]>) -> Envelope {
        let sig = signing_key(seed).sign(statement);
        Envelope {
            v: ENVELOPE_VERSION,
            kid: kid.to_string(),
            statement: URL_SAFE_NO_PAD.encode(statement),
            sig: URL_SAFE_NO_PAD.encode(sig.to_bytes()),
            payload: payload.map(|p| URL_SAFE_NO_PAD.encode(p)),
        }
    }

    /// Convenience: a fully valid envelope for the test issuer.
    pub fn signed_envelope(
        typ: &str,
        org: &str,
        sub: &str,
        seq: u64,
        status: &str,
        payload: Option<&[u8]>,
    ) -> Envelope {
        let st = statement_json(typ, TEST_ISS, org, sub, seq, status, payload);
        envelope(1, TEST_KID, &st, payload)
    }
}
