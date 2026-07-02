//! Managed-paths filesystem reconciler — Phase F1.
//!
//! On daemon startup, scans three watched paths, migrates existing items into
//! VectorHawk's DB as locally-owned submissions, and backs up originals before
//! taking ownership.  No quarantine, no drift detection, no write-on-install
//! yet — those are F3 and F2 respectively.
//!
//! # Watched paths (macOS + Linux)
//!
//! | Path                  | What we scan                              |
//! |-----------------------|-------------------------------------------|
//! | `~/.claude/skills/`   | Immediate subdirs that contain `SKILL.md` |
//! | `~/.claude/plugins/`  | Immediate subdirs, excluding Anthropic    |
//! |                       | plumbing dirs (marketplaces/cache/data)   |
//! | `~/.claude.json`      | `mcpServers` object (excluding `vectorhawk`) |
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

pub mod adopt_publish;
pub mod discoveries;
pub mod drift;
pub mod marker;
pub mod migrator;
pub mod paths;
pub mod plugin_marketplace;
pub mod publish;
pub mod pusher;
pub mod rollback;
pub mod scanner;
pub mod takeover;

use anyhow::Result;
use std::{path::PathBuf, sync::Arc};
use tracing::{info, warn};
use vectorhawkd_core::state::AppState;

pub use discoveries::DiscoveriesScanner;
pub use drift::DriftScanner;
pub use marker::ManagedPathMarker;
pub use plugin_marketplace::{
    install_plugin_bundle, uninstall_plugin_bundle, BundledSkill, PluginBundle,
};
pub use pusher::{push_missing_active_skills, reclaim_active_skills, ManagedPathsPusher};
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
    Skill,
    Plugin,
    Mcp,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemKind::Skill => write!(f, "skill"),
            ItemKind::Plugin => write!(f, "plugin"),
            ItemKind::Mcp => write!(f, "mcp"),
        }
    }
}

/// Summary produced by one `migrate_existing` run.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub skills_migrated: usize,
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
        let http_client = reqwest::Client::builder()
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

        let skills_dir = home.join(".claude").join("skills");
        let plugins_dir = home.join(".claude").join("plugins");
        let claude_json = home.join(".claude.json");

        // Compute a single ISO-8601 timestamp for this migration run's backup dir.
        let now_ts = chrono::Utc::now().format("%Y-%m-%dT%H%M%SZ").to_string();
        let backup_root = home
            .join(".claude")
            .join(".vectorhawk-backup")
            .join(&now_ts);

        let mut report = MigrationReport {
            backed_up_to: Some(backup_root.clone()),
            ..Default::default()
        };

        // ── Skills ────────────────────────────────────────────────────────────
        let skill_items = match scanner::scan_skills_dir(&skills_dir) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = %e, "managed_paths: failed to scan skills dir; skipping skills");
                vec![]
            }
        };

        for item in skill_items {
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
                    report.skills_migrated += 1;
                }
                Ok(false) => {
                    // Already tracked — idempotent skip.
                }
                Err(e) => {
                    warn!(slug = %slug, error = %e, "managed_paths: migration failed for skill");
                    report.errors.push(MigrationError {
                        slug,
                        kind: ItemKind::Skill,
                        message: e.to_string(),
                    });
                }
            }
        }

        // ── Plugins ───────────────────────────────────────────────────────────
        let plugin_items = match scanner::scan_plugins_dir(&plugins_dir) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = %e, "managed_paths: failed to scan plugins dir; skipping plugins");
                vec![]
            }
        };

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
