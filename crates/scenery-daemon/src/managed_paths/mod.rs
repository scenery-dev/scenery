//! Managed-paths filesystem reconciler — Phase F1.
//!
//! On daemon startup, scans two watched paths, migrates existing plugins and
//! MCP entries into Scenery's DB as locally-owned submissions, and backs up
//! originals before taking ownership.
//!
//! # Watched paths (macOS + Linux)
//!
//! | Path                  | What we scan                              |
//! |-----------------------|-------------------------------------------|
//! | `~/.claude/plugins/`  | Immediate subdirs, excluding Anthropic    |
//! |                       | plumbing dirs (marketplaces/cache/data)   |
//! | `~/.claude.json`      | `mcpServers` object (excluding `scenery`) |
//!
//! Skills are NOT migrated here. Since AUTO-MANAGE Phase 8 (plan
//! `context/2026-11-16-auto-manage-adoption-plan.md`) every skill found on the
//! device, including those under `~/.claude/skills/`, goes through
//! `discoveries.rs` and the auto-manage converter (`auto_manage.rs`); the old
//! F1 skill migrator re-adopted skills that auto-manage already tracked.
//!
//! # Idempotency
//!
//! Every item is keyed by its absolute path in the `managed_path_markers`
//! SQLite table.  A second run of `migrate_existing` is a no-op for already-
//! marked items.
//!
//! # Failure handling
//!
//! `migrate_existing` logs per-item failures and continues.  The function
//! itself returns `Ok(MigrationReport)` unless something catastrophic prevents
//! even opening the DB.  Migration failure must NOT crash the daemon.

pub mod authoring;
pub mod auto_manage;
pub mod builtin;
pub mod conversion;
pub mod discoveries;
pub mod drift;
pub mod foreign;
pub mod links;
pub mod local_edits;
pub mod marker;
pub mod mcp_args;
pub mod mcp_discovery;
pub mod migrator;
pub mod native_cleanup;
pub mod own_uninstall;
pub mod paths;
pub mod plugin_marketplace;
pub mod projection_outcome;
pub mod pusher;
pub mod rollback;
pub mod scanner;
pub mod skill_merge;
pub mod takeover;
pub mod tracked;

/// Serializes every test that mutates process-global state (`$HOME`,
/// `SCENERY_DISABLE_FILESYSTEM_RECONCILER`).
///
/// This MUST be a single shared static. A `static` is per-module, so a
/// per-file `ENV_MUTEX` in each test file yields independent locks and no
/// cross-file serialization — which is the bug this replaces.
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

use anyhow::Result;
use scenery_core::state::AppState;
use std::{path::PathBuf, sync::Arc};
use tracing::{info, warn};

pub use conversion::recover_interrupted as recover_interrupted_conversions;
pub use discoveries::DiscoveriesScanner;
pub use drift::DriftScanner;
pub use marker::ManagedPathMarker;
pub use mcp_discovery::{collect_mcp_discoveries, McpDiscoveryDetail, McpDiscoveryItem};
pub use plugin_marketplace::{
    has_bundled_skill_record, install_plugin_bundle, list_governed_plugins, plugin_has_local_state,
    plugins_bundling_skill, remove_bundled_skill, uninstall_marketplace, uninstall_plugin_bundle,
    BundledSkill, PartialTeardown, PluginBundle,
};
pub use pusher::{push_missing_active_skills, ManagedPathsPusher};

/// Wire the F2 pusher into `scenery_core::installer::uninstall_skill`, so every
/// uninstall path (CLI, MCP `scenery_uninstall`, registry-sync cleanup) also
/// removes the native `~/.agents/skills/<slug>` dir and the
/// `~/.claude/skills/<slug>` link. Idempotent; call once at process start.
pub fn register_core_hooks() {
    scenery_core::installer::register_skill_projection_remover(|state, slug| {
        ManagedPathsPusher::new(state)
            .remove_skill(slug)
            .map(|_| ())
    });
    scenery_core::installer::register_skill_user_remover(|state, slug| {
        own_uninstall::uninstall_projection_detailed(state, slug).map(|(_, own)| own)
    });
}
pub use rollback::{list_backups, rollback, BackupSummary, RollbackReport};
pub use scanner::MigrationItem;

/// Error recorded in `MigrationReport` for a single item that failed.
#[derive(Debug)]
pub struct MigrationError {
    pub slug: String,
    pub kind: ItemKind,
    pub message: String,
}

/// Which category of managed path item this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Plugin,
    Mcp,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemKind::Plugin => write!(f, "plugin"),
            ItemKind::Mcp => write!(f, "mcp"),
        }
    }
}

/// Summary produced by one `migrate_existing` run.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub plugins_migrated: usize,
    pub mcps_migrated: usize,
    pub backed_up_to: Option<PathBuf>,
    pub errors: Vec<MigrationError>,
}

/// Top-level reconciler.  Cheaply cloneable via `Arc<AppState>`.
pub struct ManagedPathsReconciler {
    state: Arc<AppState>,
    registry_url: String,
    http_client: reqwest::Client,
}

impl ManagedPathsReconciler {
    pub fn new(state: Arc<AppState>, registry_url: String) -> Result<Self> {
        let http_client = scenery_core::http::async_client_builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client for managed_paths: {e}"))?;
        Ok(Self {
            state,
            registry_url,
            http_client,
        })
    }

    /// Discover all three watched paths, migrate anything not already tracked.
    /// Idempotent — safe to call multiple times.
    pub async fn migrate_existing(&self) -> Result<MigrationReport> {
        // Resolve all watched paths via the home dir.
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("managed_paths: HOME directory not resolvable"))?;

        let plugins_dir = home.join(".claude").join("plugins");
        let claude_json = home.join(".claude.json");

        // Compute a single ISO-8601 timestamp for this migration run's backup dir.
        let now_ts = scenery_core::timefmt::stamp_compact(scenery_core::timefmt::now());
        let backup_root = home.join(".claude").join(".scenery-backup").join(&now_ts);

        let mut report = MigrationReport {
            backed_up_to: Some(backup_root.clone()),
            ..Default::default()
        };

        // ── Plugins ───────────────────────────────────────────────────────────
        // Two layouts: legacy immediate-child dirs under ~/.claude/plugins/, and
        // the real nested marketplace tree (marketplaces/<mp>/{plugins,external_plugins}/).
        // The marketplace walk classifies each plugin and skips Anthropic-native ones.
        let mut plugin_items = match scanner::scan_plugins_dir(&plugins_dir) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = %e, "managed_paths: failed to scan plugins dir; skipping plugins");
                vec![]
            }
        };
        match scanner::scan_plugin_marketplaces(&plugins_dir) {
            Ok(items) => plugin_items.extend(items),
            Err(e) => {
                warn!(error = %e, "managed_paths: failed to scan plugin marketplaces; skipping nested plugins");
            }
        }

        for item in plugin_items {
            let slug = item.slug.clone();
            match migrator::migrate_item(
                &item,
                &backup_root,
                &self.state,
                &self.registry_url,
                &self.http_client,
            )
            .await
            {
                Ok(true) => {
                    info!(slug = %slug, kind = %item.kind, "managed_paths: migrated");
                    report.plugins_migrated += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(slug = %slug, error = %e, "managed_paths: migration failed for plugin");
                    report.errors.push(MigrationError {
                        slug,
                        kind: ItemKind::Plugin,
                        message: e.to_string(),
                    });
                }
            }
        }

        // ── MCP servers from ~/.claude.json ───────────────────────────────────
        let mcp_items = match scanner::scan_claude_json(&claude_json) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = %e, "managed_paths: failed to scan ~/.claude.json; skipping mcpServers");
                vec![]
            }
        };

        for item in mcp_items {
            let slug = item.slug.clone();
            match migrator::migrate_item(
                &item,
                &backup_root,
                &self.state,
                &self.registry_url,
                &self.http_client,
            )
            .await
            {
                Ok(true) => {
                    info!(slug = %slug, kind = %item.kind, "managed_paths: migrated");
                    report.mcps_migrated += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(slug = %slug, error = %e, "managed_paths: migration failed for mcp");
                    report.errors.push(MigrationError {
                        slug,
                        kind: ItemKind::Mcp,
                        message: e.to_string(),
                    });
                }
            }
        }

        Ok(report)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;

#[cfg(test)]
#[path = "links_tests.rs"]
mod links_tests;

#[cfg(test)]
#[path = "uninstall_hook_tests.rs"]
mod uninstall_hook_tests;
