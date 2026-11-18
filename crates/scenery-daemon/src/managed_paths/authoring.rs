//! `scenery_create_skill` back end (plan 4.7): register a freshly scaffolded
//! skill and convert it in place at once, so it is managed from its first save.
//!
//! The folder `~/.agents/skills/<slug>/` was just written by the MCP tool. This
//! is exactly what the discovery tick would do to it eventually, minus the wait:
//! upload privately (`POST /runner/skills/register`), then install the registry
//! copy with the transactional in-place conversion ([`super::conversion`]), which
//! writes the marker (`origin = auto_managed`, `skill_id`, `skill_uuid`) and the
//! client links. Nothing on disk changes unless the registry accepted the upload.

use super::{
    auto_manage::{record_attempt, AttemptStatus, OWNER_NAMESPACE_KEY},
    pusher::ManagedPathsPusher,
};
use scenery_core::{
    authoring::{CreateError, CreatedSkill},
    registry::{AuthoringReply, AutoManageOutcomeKind, RegisterSkillRequest, RegistryClient},
    state::AppState,
};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};
use tracing::{info, warn};
use uuid::Uuid;

static RUNTIME: OnceLock<(Arc<AppState>, tokio::runtime::Handle)> = OnceLock::new();

/// Wire the create hook. Call once from the daemon's async start-up, so the
/// blocking MCP tool thread can drive the async install on the daemon runtime.
pub fn init(state: Arc<AppState>) {
    let _ = RUNTIME.set((state, tokio::runtime::Handle::current()));
    scenery_core::authoring::register_create_hook(create_hook);
}

fn create_hook(
    _state: &AppState,
    registry_url: &str,
    slug: &str,
) -> Result<CreatedSkill, CreateError> {
    let Some((state, handle)) = RUNTIME.get() else {
        return Err(CreateError::Failed("daemon runtime not initialised".into()));
    };
    handle.block_on(register_and_convert(
        Arc::clone(state),
        registry_url.to_string(),
        slug.to_string(),
    ))
}

/// See the module docs.
pub async fn register_and_convert(
    state: Arc<AppState>,
    registry_url: String,
    slug: String,
) -> Result<CreatedSkill, CreateError> {
    let canonical = super::paths::agents_skills_dir()
        .map(|d| d.join(&slug))
        .ok_or_else(|| CreateError::Failed("home directory not resolvable".into()))?;
    let source_path = canonical.to_string_lossy().to_string();

    let token = scenery_core::auth::load_tokens(&state, &registry_url)
        .ok()
        .flatten()
        .map(|t| t.access_token)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| CreateError::Offline("not signed in to the Scenery registry".into()))?;
    let device_id = state
        .get_sync_state("device_id")
        .ok()
        .flatten()
        .ok_or_else(|| CreateError::Offline("this device is not paired yet".into()))?;

    // Pack and hash the tree as it is now (no symlinks, no secrets).
    let dir = canonical.clone();
    let packed = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<u8>, String)> {
        let hash = scenery_core::skill_tree::survey(&dir)?.tree_hash;
        Ok((scenery_core::pack::pack_dir_to_tar_gz_no_links(&dir)?, hash))
    })
    .await
    .map_err(|e| CreateError::Failed(format!("pack task failed: {e}")))?;
    let (gz, tree_hash) = packed.map_err(|e| CreateError::Failed(format!("{e:#}")))?;

    let (url, up_slug, up_hash, up_dev) = (
        registry_url.clone(),
        slug.clone(),
        tree_hash.clone(),
        device_id,
    );
    let reply = tokio::task::spawn_blocking(move || {
        RegistryClient::new(url)
            .with_auth(token)
            .register_skill(RegisterSkillRequest {
                tar_gz: gz,
                slug: &up_slug,
                tree_hash: &up_hash,
                device_id: &up_dev,
                mode: "manage",
            })
    })
    .await
    .map_err(|e| CreateError::Failed(format!("register task failed: {e}")))?
    .map_err(|e| CreateError::Offline(format!("{e:#}")))?;

    let resp = match reply {
        AuthoringReply::Done(r) => r,
        AuthoringReply::Refused(r) => {
            return Err(CreateError::Refused {
                code: r.code,
                message: r.message,
            })
        }
    };
    match resp.outcome {
        AutoManageOutcomeKind::Managed => {}
        AutoManageOutcomeKind::Blocked | AutoManageOutcomeKind::Revoked => {
            let reason = resp.reason.clone().unwrap_or_else(|| "blocked".into());
            return Err(CreateError::Refused {
                message: format!("the registry did not accept the skill ({reason})"),
                code: reason,
            });
        }
    }
    let (Some(skill_id), Some(version)) = (resp.skill_id.clone(), resp.version.clone()) else {
        return Err(CreateError::Failed(
            "the registry answered managed without a skill id".into(),
        ));
    };
    let skill_uuid = resp.skill_uuid.clone().unwrap_or_default();
    if let Some(ns) = resp.owner_namespace.as_deref() {
        let _ = state.set_sync_state(OWNER_NAMESPACE_KEY, ns);
    }

    // Convert: the same steps as the discovery tick's `convert`.
    if skill_id != slug {
        state
            .record_skill_local_name(&skill_id, &slug)
            .map_err(|e| CreateError::Failed(format!("could not record the folder name: {e:#}")))?;
    }
    scenery_core::own_uninstall_outbox::clear(&state, &skill_id);
    scenery_core::own_uninstall_outbox::clear(&state, &slug);
    state
        .record_pending_auto_managed_takeover(&slug, &source_path, &skill_id, &skill_uuid)
        .map_err(|e| CreateError::Failed(format!("could not record the takeover: {e:#}")))?;
    let installation_id = resp
        .installation_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::new_v4);
    let pusher = ManagedPathsPusher::new(&state);
    if let Err(e) = crate::sync::reconciler::install_verified_version(
        installation_id,
        &slug,
        &version,
        &state,
        &registry_url,
        Some(&pusher),
    )
    .await
    {
        let _ = state.clear_pending_adopt_takeover(&slug);
        warn!(slug, error = %e, "create-skill: install failed; the folder is untouched and discovery will retry");
        return Err(CreateError::Failed(format!(
            "registered, but the local conversion failed: {e:#}"
        )));
    }
    if !scenery_mcp::ownership::is_scenery_managed(Path::new(&source_path)) {
        let _ = state.clear_pending_adopt_takeover(&slug);
        return Err(CreateError::Failed(
            "registered, but the conversion did not complete".into(),
        ));
    }
    record_attempt(
        &state,
        &slug,
        &source_path,
        &tree_hash,
        AttemptStatus::Managed,
        None,
    );
    info!(
        slug,
        skill_id, version, "create-skill: registered and managed"
    );
    Ok(CreatedSkill {
        skill_id,
        skill_uuid,
        version,
        owner_namespace: resp.owner_namespace,
        scan_verdict: resp.scan.verdict,
    })
}

#[cfg(test)]
#[path = "authoring_tests.rs"]
mod tests;
