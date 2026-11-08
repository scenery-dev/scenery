//! Glue between [`crate::signed`] and device state: which registry origin is
//! pinned, which org this device belongs to, which keys are trusted, and the
//! per-subject sequence high-water mark.

use crate::signed::{self, Envelope, SignError, VerifiedStatement, VerifyParams};
use crate::state::AppState;
use crate::trust;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// `sync_state` key: the registry origin this device was paired against.
pub const REGISTRY_ORIGIN_KEY: &str = "registry_origin";
/// `sync_state` key: the org id this device belongs to (from the backend,
/// pinned at pairing or TOFU on the first verified snapshot).
pub const ORG_ID_KEY: &str = "org_id";

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("no registry origin is pinned for this device; run `scenery auth pair` again")]
    NoRegistryOrigin,
    #[error(
        "no trust root for {0}: signed definitions are refused. Run `scenery trust pin {0} --fingerprint <sha256>`"
    )]
    NoTrustRoot(String),
    #[error("{0}")]
    Sign(#[from] SignError),
    #[error("state error: {0}")]
    State(String),
}

pub fn registry_origin(state: &AppState) -> Option<String> {
    state.get_sync_state(REGISTRY_ORIGIN_KEY).ok().flatten()
}

pub fn device_org(state: &AppState) -> Option<String> {
    state
        .get_sync_state(ORG_ID_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Highest accepted `seq` for `sub`.
pub fn max_seq(state: &AppState, sub: &str) -> Result<Option<u64>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let v: Option<i64> = conn
        .query_row("SELECT max_seq FROM signed_seq WHERE sub = ?1", [sub], |r| r.get(0))
        .optional()
        .context("failed to read signed_seq")?;
    Ok(v.and_then(|n| u64::try_from(n).ok()))
}

/// Raise the high-water mark for `sub` to `seq` (never lowers it).
pub fn record_seq(state: &AppState, sub: &str, seq: u64) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let seq = i64::try_from(seq).context("seq out of range")?;
    conn.execute(
        "INSERT INTO signed_seq (sub, max_seq) VALUES (?1, ?2)
         ON CONFLICT(sub) DO UPDATE SET max_seq = MAX(max_seq, excluded.max_seq)",
        rusqlite::params![sub, seq],
    )
    .context("failed to write signed_seq")?;
    Ok(())
}

/// Verify `env` as `typ` against this device's pinned origin, trust set, org
/// and sequence mark. `expected_sub`, when given, must equal the statement's
/// `sub` (routing ids must match). On success the sequence mark is advanced.
pub fn verify_for_device(
    state: &AppState,
    typ: &str,
    env: &Envelope,
    expected_sub: Option<&str>,
) -> Result<VerifiedStatement, VerifyError> {
    verify_with_org(state, typ, env, expected_sub, device_org(state).as_deref())
}

/// As [`verify_for_device`] with an explicit org (used for the first-snapshot
/// TOFU pin, before `sync_state.org_id` is stored).
pub fn verify_with_org(
    state: &AppState,
    typ: &str,
    env: &Envelope,
    expected_sub: Option<&str>,
    org: Option<&str>,
) -> Result<VerifiedStatement, VerifyError> {
    let origin = registry_origin(state).ok_or(VerifyError::NoRegistryOrigin)?;
    let keys = trust::keys_for_origin(state, &origin);
    if keys.is_empty() {
        return Err(VerifyError::NoTrustRoot(origin));
    }
    // The sub is only known after parsing, so read the mark for the claimed
    // subject lazily: verify once without it, then compare.
    let params = VerifyParams {
        keys: &keys,
        iss: &origin,
        org,
        typ,
        max_seq: None,
    };
    let v = signed::verify(env, &params)?;
    if let Some(want) = expected_sub {
        if v.sub != want {
            return Err(SignError::Malformed(format!(
                "statement subject '{}' does not match '{want}'",
                v.sub
            ))
            .into());
        }
    }
    let max = max_seq(state, &v.sub).map_err(|e| VerifyError::State(e.to_string()))?;
    if let Some(max) = max {
        if v.seq < max {
            return Err(SignError::StaleSeq { got: v.seq, max }.into());
        }
    }
    record_seq(state, &v.sub, v.seq).map_err(|e| VerifyError::State(e.to_string()))?;
    Ok(v)
}

/// `skill_artifact.v1` payload.
#[derive(Debug, serde::Deserialize)]
struct SkillArtifactPayload {
    skill_id: String,
    version: String,
    sha256: String,
    size_bytes: u64,
    org: String,
}

/// Verify the signed `skill_artifact.v1` statement on artifact metadata and
/// return the trusted `(sha256, size_bytes)`. The unsigned `sha256` and
/// `size_bytes` fields next to the envelope are never used. Debug builds with
/// `SCENERY_INSECURE_UNSIGNED=1` fall back to them when no envelope is sent.
pub fn verify_skill_artifact(
    state: &AppState,
    skill_id: &str,
    version: &str,
    meta: &crate::registry::ArtifactMetadata,
) -> Result<(String, u64)> {
    let Some(env) = meta.signed.as_ref() else {
        if crate::trust::insecure_unsigned() {
            tracing::warn!(skill_id, "SCENERY_INSECURE_UNSIGNED: accepting unsigned artifact");
            let size = meta.size_bytes.unwrap_or(crate::registry::MAX_ARTIFACT_BYTES);
            return Ok((meta.sha256.clone(), size));
        }
        anyhow::bail!("artifact metadata is not signed");
    };
    let sub = format!("skill:{skill_id}@{version}");
    let v = verify_for_device(state, signed::TYP_SKILL_ARTIFACT, env, Some(&sub))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        v.status == signed::STATUS_ACTIVE,
        "artifact statement status is '{}', not active",
        v.status
    );
    let p: SkillArtifactPayload = v.payload_json().map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        p.skill_id == skill_id && p.version == version,
        "artifact payload is for {}@{}, not {skill_id}@{version}",
        p.skill_id,
        p.version
    );
    anyhow::ensure!(p.org == v.org, "artifact payload org does not match its statement");
    Ok((p.sha256, p.size_bytes))
}

/// One file entry of a `plugin_version.v1` payload.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginFileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub executable: bool,
}

/// `plugin_version.v1` payload.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginVersionPayload {
    pub slug: String,
    pub version: String,
    pub files: Vec<PluginFileEntry>,
}

#[cfg(test)]
#[path = "verifier_tests.rs"]
mod tests;
