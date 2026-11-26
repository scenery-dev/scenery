//! Storage-free collector for `scenery scan --once`.
//!
//! Runs the daemon's own discovery rules (skills via
//! [`discoveries::collect_discoveries_in`], MCP servers via
//! [`mcp_discovery::collect_mcp_discoveries_in`], plugins via the
//! [`scanner`] walkers) against an in-memory schema, so a scan leaves no
//! state dir, DB file or any other file behind. Every `source_path` is
//! `~`-masked, plugin details carry only the allowlisted manifest strings,
//! and the batch is cut to the backend's report limits.

use super::discoveries::{self, DiscoveryItem};
use super::{mcp_discovery, paths, scanner};
use anyhow::Result;
use std::path::Path;
use tracing::warn;

/// Most items the backend accepts in one scan report.
pub const MAX_REPORT_ITEMS: usize = 5000;
/// Most entries the backend accepts in an MCP item's `detail.args`.
pub const MAX_ARGS: usize = 256;
/// Longest single `detail.args` entry the backend accepts, in bytes.
pub const MAX_ARG_BYTES: usize = 4096;
/// Budget for the serialized item list. The backend caps the whole report
/// body at 8 MiB; 64 KiB is left for the envelope (format, identity, ...).
pub const MAX_ITEMS_BYTES: usize = 8 * 1024 * 1024 - 64 * 1024;

/// Collect every skill, shadow MCP server and plugin on this machine for a
/// one-shot scan report. `include_args` is the `auto_submit_mcp` answer from
/// `GET /scan/session`: redacted MCP args travel only when it is true.
///
/// Reads only. The marker DB is in memory and starts empty, so nothing is
/// treated as Scenery-managed and nothing is written anywhere.
pub fn collect_for_scan(
    home: &Path,
    registry_url: &str,
    include_args: bool,
) -> Result<Vec<DiscoveryItem>> {
    let conn = scenery_core::state::ephemeral_schema_connection()?;
    let mut items = discoveries::collect_discoveries_in(&conn, &discoveries::extra_roots());
    let clients = scenery_mcp::setup::detect_ai_clients();
    for mcp in
        mcp_discovery::collect_mcp_discoveries_in(&conn, &clients, registry_url, include_args)
    {
        items.push(discoveries::mcp_item_to_discovery_item(mcp));
    }
    if let Some(plugins_dir) = paths::plugins_dir() {
        items.extend(collect_plugins(&plugins_dir));
    }
    for item in &mut items {
        item.source_path = mask_home(&item.source_path, home);
        item.local = None;
    }
    enforce_limits(&mut items, MAX_REPORT_ITEMS, MAX_ITEMS_BYTES);
    Ok(items)
}

/// Top-level and marketplace plugins as `kind: "plugin"` items. `detail`
/// holds `name`, `version` and `description` from
/// [`scanner::sanitize_plugin_manifest`] only, plus the marketplace name for
/// the nested layout (`null` for a top-level plugin).
fn collect_plugins(plugins_dir: &Path) -> Vec<DiscoveryItem> {
    let mut found = Vec::new();
    for (label, scan) in [
        ("plugins", scanner::scan_plugins_dir(plugins_dir)),
        (
            "marketplaces",
            scanner::scan_plugin_marketplaces(plugins_dir),
        ),
    ] {
        match scan {
            Ok(list) => found.extend(list),
            Err(e) => warn!(error = %e, label, "scan_collect: plugin scan failed — skipping"),
        }
    }
    let marketplaces = plugins_dir.join("marketplaces");
    let mut items = Vec::with_capacity(found.len());
    for m in found {
        let manifest = m.payload.get("manifest_json").cloned().unwrap_or_default();
        let clean = scanner::sanitize_plugin_manifest(&manifest);
        let marketplace = m
            .source_path
            .strip_prefix(&marketplaces)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_string_lossy().to_string());
        let detail = serde_json::json!({
            "name": clean.get("name"),
            "version": clean.get("version"),
            "description": clean.get("description"),
            "marketplace": marketplace,
        });
        items.push(DiscoveryItem {
            kind: "plugin".to_string(),
            slug: m.slug,
            source_path: m.source_path.to_string_lossy().to_string(),
            canonical_hash: m.canonical_hash,
            detail: Some(detail),
            local: None,
        });
    }
    items
}

/// `~`-mask `value`. A path under `home` is rewritten component-wise (so a
/// sibling such as `/home/alice2` is never mistaken for `/home/alice`);
/// anything else falls back to [`mcp_discovery::mask_home_dir`]. Values
/// already starting with `~` are left alone.
fn mask_home(value: &str, home: &Path) -> String {
    if value.starts_with('~') {
        return value.to_string();
    }
    match Path::new(value).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
        Err(_) => mcp_discovery::mask_home_dir(value),
    }
}

/// Fit `items` to the backend's limits: at most `max_items` items, MCP
/// `detail.args` cut to [`MAX_ARGS`] entries of at most [`MAX_ARG_BYTES`]
/// bytes, and the serialized list within `max_bytes`. Over the byte budget,
/// every item's `args` is dropped first (the inventory survives without
/// them), then items are dropped from the end.
pub fn enforce_limits(items: &mut Vec<DiscoveryItem>, max_items: usize, max_bytes: usize) {
    if items.len() > max_items {
        warn!(
            found = items.len(),
            max_items, "scan_collect: too many items — report truncated"
        );
        items.truncate(max_items);
    }
    for item in items.iter_mut() {
        if let Some(args) = args_mut(item) {
            cap_args(args);
        }
    }
    if serialized_len(items) <= max_bytes {
        return;
    }
    warn!("scan_collect: report over the size budget — dropping MCP args");
    for item in items.iter_mut() {
        if let Some(obj) = item.detail.as_mut().and_then(|d| d.as_object_mut()) {
            obj.remove("args");
        }
    }
    let mut lens: Vec<usize> = Vec::with_capacity(items.len());
    for item in items.iter() {
        lens.push(serde_json::to_vec(item).map(|v| v.len()).unwrap_or(0));
    }
    // `[` + `]` + one comma between neighbours.
    let mut total: usize = 2 + lens.iter().sum::<usize>() + lens.len().saturating_sub(1);
    while total > max_bytes {
        let Some(len) = lens.pop() else { break };
        items.pop();
        total -= len + usize::from(!lens.is_empty());
        warn!("scan_collect: report over the size budget — dropping an item");
    }
}

fn args_mut(item: &mut DiscoveryItem) -> Option<&mut Vec<serde_json::Value>> {
    item.detail.as_mut()?.get_mut("args")?.as_array_mut()
}

fn cap_args(args: &mut Vec<serde_json::Value>) {
    args.truncate(MAX_ARGS);
    for arg in args.iter_mut() {
        if let Some(s) = arg.as_str() {
            if s.len() > MAX_ARG_BYTES {
                let mut cut = MAX_ARG_BYTES;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                *arg = serde_json::Value::String(s[..cut].to_string());
            }
        }
    }
}

fn serialized_len(items: &[DiscoveryItem]) -> usize {
    serde_json::to_vec(items)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
#[path = "scan_collect_tests.rs"]
mod scan_collect_tests;
