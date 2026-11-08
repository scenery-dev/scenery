//! Verification glue for the sync stream: org policy, revocation clears and
//! the first-snapshot org pin. Shared by the reconciler and by the periodic
//! snapshot tick in `lib.rs`, so both converge through the same rules.
//!
//! Fail-safe rule (design 2026-11-08 §1): anything that grants (install,
//! re-activate, clear a revocation, change org policy) needs a valid
//! envelope; anything that takes away (deactivate, purge, revoke) is accepted
//! unsigned.

use super::sse_client::{OrgPolicySnapshot, RevocationRecord};
use scenery_core::revocation::RevocationEntry;
use scenery_core::signed::{self, Envelope};
use scenery_core::state::AppState;
use scenery_core::{trust, verifier};
use tracing::{info, warn};

#[derive(Debug, serde::Deserialize)]
struct OrgPolicyPayload {
    #[serde(default)]
    managed_paths_mode: Option<String>,
    #[serde(default)]
    block_third_party_inference: Option<bool>,
}

/// Apply org policy from a signed `org_policy.v1` envelope. The unsigned
/// siblings are used only by debug builds with `SCENERY_INSECURE_UNSIGNED=1`
/// and no envelope. Returns whether anything was applied.
pub(crate) fn apply_org_policy(
    state: &AppState,
    signed_env: Option<&Envelope>,
    unsigned_mode: Option<&str>,
    unsigned_block: Option<bool>,
) -> bool {
    let Some(env) = signed_env else {
        if trust::insecure_unsigned() {
            state.apply_org_policy_snapshot(unsigned_mode, unsigned_block);
            return true;
        }
        warn!("sync: unsigned org policy ignored");
        return false;
    };
    let v = match verifier::verify_for_device(
        state,
        signed::TYP_ORG_POLICY,
        env,
        Some("org_policy"),
    ) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "sync: org policy statement rejected");
            return false;
        }
    };
    if v.status != signed::STATUS_ACTIVE {
        warn!(status = %v.status, "sync: org policy statement is not active");
        return false;
    }
    match v.payload_json::<OrgPolicyPayload>() {
        Ok(p) => {
            state.apply_org_policy_snapshot(
                p.managed_paths_mode.as_deref(),
                p.block_third_party_inference,
            );
            true
        }
        Err(e) => {
            warn!(error = %e, "sync: org policy payload rejected");
            false
        }
    }
}

/// Apply a snapshot's `org_policy` block.
pub(crate) fn apply_org_policy_snapshot(state: &AppState, policy: &OrgPolicySnapshot) -> bool {
    apply_org_policy(
        state,
        policy.signed.as_ref(),
        policy.managed_paths_mode.as_deref(),
        policy.block_third_party_inference,
    )
}

fn revocation_sub(artifact_type: &str, key: &str, version: Option<&str>) -> String {
    format!(
        "revocation:{artifact_type}:{key}:{}",
        version.map(str::trim).unwrap_or("")
    )
}

/// Does the statement's subject name this artifact? Skill keys are compared
/// after the local-folder-name mapping the reconciler applies before us.
fn sub_matches(state: &AppState, sub: &str, artifact_type: &str, key: &str, version: Option<&str>) -> bool {
    let mut parts = sub.splitn(4, ':');
    let (Some("revocation"), Some(t), Some(k), Some(ver)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let key_ok = k == key || (artifact_type == "skill" && state.skill_local_name(k) == key);
    t == artifact_type && key_ok && ver == version.map(str::trim).unwrap_or("")
}

/// True only when `env` is a valid `revocation.v1` `status=cleared`
/// statement for exactly this artifact.
pub(crate) fn verified_clear(
    state: &AppState,
    artifact_type: &str,
    key: &str,
    version: Option<&str>,
    env: Option<&Envelope>,
) -> bool {
    let Some(env) = env else {
        if trust::insecure_unsigned() {
            return true;
        }
        return false;
    };
    match verifier::verify_for_device(state, signed::TYP_REVOCATION, env, None) {
        Ok(v) => {
            let ok = v.status == signed::STATUS_CLEARED
                && sub_matches(state, &v.sub, artifact_type, key, version);
            if !ok {
                warn!(
                    artifact_type, key, sub = %v.sub, status = %v.status,
                    "sync: revocation statement does not clear this artifact"
                );
            }
            ok
        }
        Err(e) => {
            warn!(error = %e, artifact_type, key, "sync: revocation clear rejected");
            false
        }
    }
}

/// Entries to keep revoked and entries a verified `cleared` statement lifts.
#[derive(Debug, Default)]
pub(crate) struct RevocationPlan {
    pub active: Vec<RevocationEntry>,
    pub cleared: Vec<RevocationEntry>,
}

pub(crate) fn plan_revocations(state: &AppState, revs: &[RevocationRecord]) -> RevocationPlan {
    let mut plan = RevocationPlan::default();
    for r in revs {
        let entry = RevocationEntry {
            artifact_type: r.artifact_type.clone(),
            artifact_key: r.artifact_key.clone(),
            version: r.version.clone(),
        };
        // A list entry is "revoked" unless it carries a verified clear.
        if r.signed.is_some()
            && verified_clear(
                state,
                &r.artifact_type,
                &r.artifact_key,
                r.version.as_deref(),
                r.signed.as_ref(),
            )
        {
            plan.cleared.push(entry);
        } else {
            plan.active.push(entry);
        }
    }
    plan
}

/// First verified snapshot pins the org (TOFU, logged) for devices paired
/// before 2.2.0. Needs at least one statement that verifies against the
/// snapshot's org id.
pub(crate) fn pin_org_from_snapshot(
    state: &AppState,
    snapshot_org: Option<&str>,
    statements: &[(&str, &Envelope)],
) {
    let Some(org) = snapshot_org else { return };
    if let Some(stored) = verifier::device_org(state) {
        if stored != org {
            warn!(stored = %stored, snapshot = %org, "sync: snapshot org differs from this device's org");
        }
        return;
    }
    for (typ, env) in statements {
        if verifier::verify_with_org(state, typ, env, None, Some(org)).is_ok() {
            match state.set_sync_state(verifier::ORG_ID_KEY, org) {
                Ok(()) => info!(org_id = org, "sync: pinned device org on first verified snapshot (TOFU)"),
                Err(e) => warn!(error = %e, "sync: failed to store org id"),
            }
            return;
        }
    }
}

/// Convenience used by tests and the tick: subject string for a revocation.
#[allow(dead_code)]
pub(crate) fn subject_for(artifact_type: &str, key: &str, version: Option<&str>) -> String {
    revocation_sub(artifact_type, key, version)
}

#[cfg(test)]
#[path = "trust_apply_tests.rs"]
mod tests;
