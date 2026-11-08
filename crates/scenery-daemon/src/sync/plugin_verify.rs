//! Verification of an imported plugin's source tree against its signed
//! `plugin_version.v1` statement (M2).
//!
//! The runner writes only files the verified payload lists, with a matching
//! SHA-256 and size; `executables` come only from `executable: true` entries.
//! Anything unlisted, missing, mismatched or over the caps rejects the whole
//! plugin, so nothing is written on failure.

use anyhow::{Context, Result};
use base64::Engine as _;
use scenery_core::signed::{self, Envelope};
use scenery_core::state::AppState;
use scenery_core::verifier::{self, PluginFileEntry, PluginVersionPayload};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(crate) const MAX_PLUGIN_FILES: usize = 2_000;
pub(crate) const MAX_PLUGIN_BYTES: u64 = 50 * 1024 * 1024;

/// An imported plugin's source tree, verified.
#[derive(Debug, Default)]
pub(crate) struct ImportedPluginContent {
    /// `(relative_path, bytes)` sorted by path.
    pub files: Vec<(String, Vec<u8>)>,
    /// Paths whose verified entry says `executable: true`.
    pub executables: Vec<String>,
}

fn text_entries(components: &serde_json::Value, key: &str) -> serde_json::Map<String, serde_json::Value> {
    components
        .get(key)
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Decode `components.files` and `components.binary_files` into byte blobs,
/// enforcing the file-count and byte caps as it goes.
fn received_files(components: &serde_json::Value) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for (path, content) in text_entries(components, "files") {
        let s = content
            .as_str()
            .with_context(|| format!("plugin file {path} is not a string"))?;
        total += s.len() as u64;
        out.push((path, s.as_bytes().to_vec()));
    }
    for (path, content) in text_entries(components, "binary_files") {
        let b64 = content
            .as_str()
            .with_context(|| format!("plugin binary file {path} is not a string"))?;
        // Bound before decoding: base64 is 4/3 larger than its bytes.
        anyhow::ensure!(
            (b64.len() as u64) / 4 * 3 <= MAX_PLUGIN_BYTES,
            "plugin file {path} exceeds the size cap"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .with_context(|| format!("plugin binary file {path} is not valid base64"))?;
        total += bytes.len() as u64;
        out.push((path, bytes));
    }
    anyhow::ensure!(
        out.len() <= MAX_PLUGIN_FILES,
        "plugin has more than {MAX_PLUGIN_FILES} files"
    );
    anyhow::ensure!(
        total <= MAX_PLUGIN_BYTES,
        "plugin content exceeds {MAX_PLUGIN_BYTES} bytes"
    );
    Ok(out)
}

/// Check received files against the listed entries (both directions).
fn check_against_listing(
    received: &[(String, Vec<u8>)],
    listed: &[PluginFileEntry],
) -> Result<()> {
    anyhow::ensure!(
        listed.len() <= MAX_PLUGIN_FILES,
        "signed plugin listing has more than {MAX_PLUGIN_FILES} files"
    );
    let mut by_path: HashMap<&str, &PluginFileEntry> = HashMap::new();
    for e in listed {
        scenery_core::safe_path::safe_join_rel(std::path::Path::new("/plugin"), &e.path)
            .map_err(|err| anyhow::anyhow!("signed plugin listing has an unsafe path: {err}"))?;
        anyhow::ensure!(
            by_path.insert(e.path.as_str(), e).is_none(),
            "signed plugin listing repeats {}",
            e.path
        );
    }
    let mut seen = std::collections::HashSet::new();
    for (path, bytes) in received {
        let entry = by_path
            .get(path.as_str())
            .with_context(|| format!("plugin file {path} is not in the signed listing"))?;
        anyhow::ensure!(
            seen.insert(path.as_str()),
            "plugin file {path} was sent twice"
        );
        anyhow::ensure!(
            bytes.len() as u64 == entry.size,
            "plugin file {path} has the wrong size"
        );
        anyhow::ensure!(
            hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(&entry.sha256),
            "plugin file {path} does not match its signed hash"
        );
    }
    for e in listed {
        anyhow::ensure!(
            seen.contains(e.path.as_str()),
            "signed plugin file {} was not delivered",
            e.path
        );
    }
    Ok(())
}

/// Verify `body` (the `/runner/plugins/{slug}/versions/{version}` response)
/// and return the content to write.
pub(crate) fn verify_plugin_content(
    state: &AppState,
    slug: &str,
    version: &str,
    body: &serde_json::Value,
) -> Result<ImportedPluginContent> {
    let env: Option<Envelope> = body
        .get("signed")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("plugin `signed` envelope is malformed")?;
    let components = body.get("components").cloned().unwrap_or(serde_json::Value::Null);

    let Some(env) = env else {
        if scenery_core::trust::insecure_unsigned() {
            return unsigned_content(&components);
        }
        anyhow::bail!("plugin content is not signed");
    };
    let sub = format!("plugin:{slug}@{version}");
    let v = verifier::verify_for_device(state, signed::TYP_PLUGIN_VERSION, &env, Some(&sub))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        v.status == signed::STATUS_ACTIVE,
        "plugin statement status is '{}', not active",
        v.status
    );
    let payload: PluginVersionPayload = v.payload_json().map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        payload.slug == slug && payload.version == version,
        "plugin payload is for {}@{}, not {slug}@{version}",
        payload.slug,
        payload.version
    );

    let mut received = received_files(&components)?;
    check_against_listing(&received, &payload.files)?;
    received.sort_by(|a, b| a.0.cmp(&b.0));
    let executables = payload
        .files
        .iter()
        .filter(|f| f.executable)
        .map(|f| f.path.clone())
        .collect();
    Ok(ImportedPluginContent {
        files: received,
        executables,
    })
}

/// Debug-only unsigned path (`SCENERY_INSECURE_UNSIGNED=1`).
fn unsigned_content(components: &serde_json::Value) -> Result<ImportedPluginContent> {
    let files = received_files(components)?;
    let executables = components
        .get("executables")
        .and_then(|e| e.as_array())
        .map(|p| p.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Ok(ImportedPluginContent { files, executables })
}

#[cfg(test)]
#[path = "plugin_verify_tests.rs"]
mod tests;
