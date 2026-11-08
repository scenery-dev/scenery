//! Test helpers: pair a temp device against a fake registry origin with a
//! test trust root, and mint correctly signed statements for it.

use scenery_core::signed::testing::{envelope, statement_json, trusted, TEST_KID};
use scenery_core::signed::{Envelope, STATUS_ACTIVE, TYP_MCP_SERVER, TYP_PLUGIN_VERSION};
use scenery_core::state::AppState;
use scenery_core::{trust, verifier};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const TEST_ORG: &str = "org-test";

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Strictly increasing sequence numbers across the whole test binary.
pub(crate) fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Canonical origin of a registry URL as the daemon computes it.
pub(crate) fn origin_of(registry_url: &str) -> String {
    scenery_core::registry_url::validate_origin(registry_url)
        .unwrap_or_else(|_| registry_url.trim_end_matches('/').to_string())
}

/// Pin `registry_url`'s origin, a test org and the test signing key.
pub(crate) fn pair_device(state: &AppState, registry_url: &str) {
    let origin = origin_of(registry_url);
    state
        .set_sync_state(verifier::REGISTRY_ORIGIN_KEY, &origin)
        .unwrap_or_default();
    state
        .set_sync_state(verifier::ORG_ID_KEY, TEST_ORG)
        .unwrap_or_default();
    let key = trusted(1, TEST_KID);
    trust::pin_key(state, &origin, TEST_KID, &key.pubkey).unwrap_or_default();
}

/// A signed `mcp_server.v1` envelope for the given definition.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_mcp(
    registry_url: &str,
    mcp_server_id: &str,
    name: &str,
    package_source: &str,
    version_pin: Option<&str>,
    server_config: Option<&serde_json::Value>,
    auth_type: &str,
    gateway_server_id: Option<&str>,
    gateway_url: Option<&str>,
    seq: u64,
) -> Envelope {
    let payload = serde_json::to_vec(&serde_json::json!({
        "mcp_server_id": mcp_server_id,
        "name": name,
        "package_source": package_source,
        "version_pin": version_pin,
        "server_config": server_config,
        "auth_type": auth_type,
        "gateway_server_id": gateway_server_id,
        "gateway_url": gateway_url,
    }))
    .unwrap_or_default();
    let st = statement_json(
        TYP_MCP_SERVER,
        &origin_of(registry_url),
        TEST_ORG,
        &format!("mcp_server:{mcp_server_id}"),
        seq,
        STATUS_ACTIVE,
        Some(&payload),
    );
    envelope(1, TEST_KID, &st, Some(&payload))
}

/// A signed `plugin_version.v1` envelope listing `files` as
/// `(path, bytes, executable)`.
pub(crate) fn sign_plugin(
    registry_url: &str,
    slug: &str,
    version: &str,
    files: &[(&str, &[u8], bool)],
) -> Envelope {
    use sha2::{Digest, Sha256};
    let listed: Vec<_> = files
        .iter()
        .map(|(p, b, x)| {
            serde_json::json!({
                "path": p, "sha256": hex::encode(Sha256::digest(b)),
                "size": b.len(), "executable": x,
            })
        })
        .collect();
    let payload = serde_json::to_vec(&serde_json::json!({
        "slug": slug, "version": version, "files": listed,
    }))
    .unwrap_or_default();
    let st = statement_json(
        TYP_PLUGIN_VERSION,
        &origin_of(registry_url),
        TEST_ORG,
        &format!("plugin:{slug}@{version}"),
        next_seq(),
        STATUS_ACTIVE,
        Some(&payload),
    );
    envelope(1, TEST_KID, &st, Some(&payload))
}

/// Approve the stdio definition of every stored MCP row (tests that model an
/// "already approved" install).
pub(crate) fn approve_stored_stdio(state: &AppState) {
    for row in state.list_mcp_installs().unwrap_or_default() {
        if let Some(def) = crate::launch_gate::stdio_definition(&row) {
            scenery_core::approvals::insert_approval(state, &def, &row.mcp_server_name)
                .unwrap_or_default();
        }
    }
}

/// Approve a raw `server_config` JSON stdio definition.
pub(crate) fn approve_config(state: &AppState, config: &serde_json::Value, name: &str) {
    if let Some(def) = scenery_core::approvals::definition_from_config(config) {
        scenery_core::approvals::insert_approval(state, &def, name).unwrap_or_default();
    }
}

/// Pair the device against `registry_url`, then sign any `InstallMcp` event
/// (and approve its stdio definition) so reconciler tests exercise install
/// behavior rather than signature plumbing. Other events pass through.
pub(crate) fn prepare_event(
    event: crate::sync::SyncEvent,
    state: &AppState,
    registry_url: &str,
) -> crate::sync::SyncEvent {
    pair_device(state, registry_url);
    match event {
        crate::sync::SyncEvent::InstallMcp {
            installation_id,
            mcp_server_id,
            mcp_server_name,
            package_source,
            version_pin,
            server_config,
            auth_type,
            gateway_server_id,
            gateway_url,
            signed: None,
        } => {
            if let Some(cfg) = server_config.as_ref() {
                approve_config(state, cfg, &mcp_server_name);
            }
            let signed = sign_mcp(
                registry_url,
                &mcp_server_id.to_string(),
                &mcp_server_name,
                &package_source,
                version_pin.as_deref(),
                server_config.as_ref(),
                &auth_type,
                gateway_server_id.as_deref(),
                gateway_url.as_deref(),
                next_seq(),
            );
            crate::sync::SyncEvent::InstallMcp {
                installation_id,
                mcp_server_id,
                mcp_server_name,
                package_source,
                version_pin,
                server_config,
                auth_type,
                gateway_server_id,
                gateway_url,
                signed: Some(signed),
            }
        }
        crate::sync::SyncEvent::Unrevoke {
            artifact_type,
            artifact_key,
            version,
            signed: None,
        } => {
            let signed = sign_revocation(registry_url, &artifact_type, &artifact_key, version.as_deref(), "cleared");
            crate::sync::SyncEvent::Unrevoke {
                artifact_type,
                artifact_key,
                version,
                signed: Some(signed),
            }
        }
        other => other,
    }
}

/// A platform-wide `revocation.v1` statement with the given status.
pub(crate) fn sign_revocation(
    registry_url: &str,
    artifact_type: &str,
    artifact_key: &str,
    version: Option<&str>,
    status: &str,
) -> Envelope {
    let st = statement_json(
        scenery_core::signed::TYP_REVOCATION,
        &origin_of(registry_url),
        "*",
        &format!(
            "revocation:{artifact_type}:{artifact_key}:{}",
            version.unwrap_or("")
        ),
        next_seq(),
        status,
        None,
    );
    envelope(1, TEST_KID, &st, None)
}

/// Serve a signed, empty-listing plugin content response for any
/// `slug`/`version` (the shape of a Scenery-published plugin that only
/// bundles registry skills).
pub(crate) fn mock_plugin_content(server: &mut mockito::ServerGuard) -> mockito::Mock {
    let origin = origin_of(&server.url());
    server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api/runner/plugins/[^/]+/versions/[^/]+$".to_string()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let path = req.path().to_string();
            let parts: Vec<&str> = path.split('/').collect();
            let (slug, version) = (parts[4], parts[6]);
            let signed = sign_plugin(&origin, slug, version, &[]);
            serde_json::json!({"components": {}, "signed": signed})
                .to_string()
                .into_bytes()
        })
        .create()
}

/// A signed `org_policy.v1` envelope (JSON string) for the given flags.
pub(crate) fn sign_org_policy(
    registry_url: &str,
    managed_paths_mode: &str,
    block_third_party_inference: bool,
) -> String {
    let payload = serde_json::to_vec(&serde_json::json!({
        "managed_paths_mode": managed_paths_mode,
        "block_third_party_inference": block_third_party_inference,
    }))
    .unwrap_or_default();
    let st = statement_json(
        scenery_core::signed::TYP_ORG_POLICY,
        &origin_of(registry_url),
        TEST_ORG,
        "org_policy",
        next_seq(),
        STATUS_ACTIVE,
        Some(&payload),
    );
    serde_json::to_string(&envelope(1, TEST_KID, &st, Some(&payload))).unwrap_or_default()
}
