//! Adopt auto-upload + takeover — SSE-driven handler for `discovery_adopted`.
//!
//! Uploads the discovered SKILL.md tree through the org's normal scan/
//! approval gate (`POST /runner/skills/adopt-publish`) and, once a real
//! registry artifact exists, installs the VectorHawk-managed copy and takes
//! over from the original discovered `source_path`.
//!
//! # Outcomes (see `context/adopt-publish-contract.md`)
//!
//! - `published`: a real artifact exists now. Installs it directly via
//!   [`crate::sync::reconciler::install_verified_version`] — bypassing the
//!   normal reconciler's phantom-artifact check entirely, since the F2 marker
//!   the sibling `discovery_adopted` local-copy push already wrote for this
//!   slug (see `pusher::push_adopted_discovery`) would otherwise make that
//!   check misfire even though the backend just confirmed real bytes exist.
//!   Takeover (removing `source_path`) happens as a side effect of that
//!   install once the managed copy is confirmed on disk — see
//!   `managed_paths::takeover`.
//! - `pending_review` / HTTP 422 `rejected`: no real artifact yet. The
//!   original `source_path` (and whatever local copy the sibling
//!   `discovery_adopted` push already wrote) is left untouched. The
//!   pending-takeover record recorded here converges later: when IT approves
//!   and a normal `Install` SSE event eventually installs the real version,
//!   `sync::reconciler::push_skill_to_claude` performs the takeover.
//!
//! # Idempotency
//!
//! Safe under SSE redelivery, retried adopts, and daemon restarts:
//! - If `source_path` is already gone, a prior takeover already completed —
//!   returns `Ok(())` immediately without re-uploading.
//! - The pending-takeover record is recorded *before* the upload, so a crash
//!   mid-flight still leaves enough state for the deferred-approval
//!   convergence path to finish the job later.
//!
//! # Killswitch
//!
//! Honors `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` — set, this is a no-op.

use anyhow::{Context, Result};
use std::{path::PathBuf, sync::Arc};
use tracing::{info, warn};
use uuid::Uuid;
use vectorhawkd_core::{
    registry::{AdoptPublishOutcome, RegistryClient},
    state::AppState,
};

use super::publish::{build_tar_gz, load_token_for_registry};
use super::pusher::ManagedPathsPusher;

const ENV_DISABLE: &str = "VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER";

fn reconciler_disabled() -> bool {
    std::env::var_os(ENV_DISABLE).is_some()
}

/// Handle a `discovery_adopted` SSE event's auto-upload + takeover leg.
///
/// Runs alongside (not instead of) the existing immediate local-copy push
/// (`pusher::push_adopted_discovery`) — that call still gives the user an
/// instant, usable copy at the canonical `~/.agents/skills/<slug>/` (linked at
/// `~/.claude/skills/<slug>` for Claude Code) regardless of how long the
/// scan/approval gate takes. This function's job is solely to route the bytes
/// through that gate and, once a real artifact exists, complete the takeover.
///
/// Note that when the discovery was itself found in `~/.agents/skills`, the
/// "copy" is a rewrite in place and there is nothing left to take over — see
/// `takeover::source_is_canonical_dest`.
pub async fn handle_discovery_adopted(
    state: Arc<AppState>,
    registry_url: String,
    slug: String,
    kind: String,
    source_path: String,
) -> Result<()> {
    if reconciler_disabled() {
        info!(slug, "adopt-publish: killswitch set — skipping");
        return Ok(());
    }

    if kind != "skill" {
        tracing::debug!(
            slug,
            kind,
            "adopt-publish: kind is not 'skill' — skipping upload"
        );
        return Ok(());
    }

    let source = PathBuf::from(&source_path);
    if !source.exists() {
        info!(
            slug,
            source = %source_path,
            "adopt-publish: source_path already absent — takeover already complete (idempotent no-op)"
        );
        return Ok(());
    }

    // Record before uploading so a crash mid-flight (or a backend that never
    // resolves to `published`) still leaves enough state for the
    // deferred-approval convergence path (a later normal `Install` SSE event)
    // to complete the takeover.
    state
        .record_pending_adopt_takeover(&slug, &source_path)
        .context("adopt-publish: failed to record pending takeover")?;

    let token = load_token_for_registry(&state, &registry_url)?;
    let gz_buf = build_tar_gz(source.clone()).await.with_context(|| {
        format!(
            "adopt-publish: failed to pack skill directory: {}",
            source.display()
        )
    })?;

    info!(
        slug,
        bytes = gz_buf.len(),
        registry_url,
        "adopt-publish: uploading discovered skill"
    );

    let registry = RegistryClient::new(registry_url.clone()).with_auth(token);
    let slug_for_upload = slug.clone();
    let outcome =
        tokio::task::spawn_blocking(move || registry.adopt_publish(gz_buf, &slug_for_upload))
            .await
            .context("adopt-publish: upload task panicked")?
            .with_context(|| format!("adopt-publish: upload failed for slug={slug}"))?;

    match outcome {
        AdoptPublishOutcome::Published(resp) => {
            info!(
                slug,
                version = resp.version,
                "adopt-publish: published — installing managed copy on origin device"
            );

            // The endpoint does not create install rows; fabricate a local
            // installation_id when the backend has none on file. Status
            // reporting to the backend degrades to a harmless no-op (404,
            // swallowed — see reconciler::patch_state) if this id is not
            // backend-recognized; the local install itself is unaffected.
            let installation_id = resp
                .installation_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(Uuid::new_v4);
            if resp.installation_id.is_none() {
                warn!(
                    slug,
                    "adopt-publish: backend returned no installation_id — using a local id; \
                     backend status reporting for this install will be a best-effort no-op"
                );
            }

            let pusher = ManagedPathsPusher::new(&state);
            crate::sync::reconciler::install_verified_version(
                installation_id,
                &slug,
                &resp.version,
                &state,
                &registry_url,
                Some(&pusher),
            )
            .await
            .with_context(|| {
                format!(
                    "adopt-publish: install of published version failed for {slug}@{}",
                    resp.version
                )
            })?;

            // install_verified_version's push_skill_to_claude already performs
            // the takeover as soon as the managed copy is confirmed on disk;
            // this call is a cheap, idempotent belt-and-suspenders in case
            // that inner push failed non-fatally (it logs WARN and returns
            // without touching source_path in that case — leaving it pending
            // is the safe default either way).
            if let Err(e) = super::takeover::perform_if_pending(&state, &slug) {
                warn!(slug, error = %e, "adopt-publish: takeover check failed (non-fatal)");
            }
        }
        AdoptPublishOutcome::PendingReview(resp) => {
            info!(
                slug,
                version = resp.version,
                "adopt-publish: queued for IT review — local copy retained; \
                 takeover deferred until the version is approved and installed"
            );
        }
        AdoptPublishOutcome::Rejected(rej) => {
            warn!(
                slug,
                verdict = rej.verdict,
                reason = rej.reason,
                message = rej.message,
                "adopt-publish: rejected by strict-mode policy — local copy retained"
            );
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "adopt_publish_tests.rs"]
mod tests;
