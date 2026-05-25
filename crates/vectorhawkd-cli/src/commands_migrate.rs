//! `vectorhawk migrate` — inspect and roll back F1 managed-paths uplifts.
//!
//! Subcommands:
//!
//! ```text
//! vectorhawk migrate list-backups
//!     List all backup runs with timestamp, item count, and a slug preview.
//!
//! vectorhawk migrate rollback --ts <ts> [--slug <slug>] [--yes]
//!     Restore items from a backup run.  Prompts for confirmation unless --yes.
//! ```

use anyhow::{Context, Result};
use clap::Subcommand;
use vectorhawkd_core::state::AppState;
use vectorhawkd_daemon::managed_paths::{list_backups, rollback};

// ── CLI types ─────────────────────────────────────────────────────────────────

/// `vectorhawk migrate` subcommand tree.
#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// List all managed-paths backup runs with timestamp and item count.
    ListBackups,

    /// Restore items from a specific backup run to their original paths.
    ///
    /// Removes the F2 marker from SQLite and notifies the backend to delete
    /// the catalog and installation rows (non-fatal if the backend is
    /// unreachable).
    Rollback {
        /// The backup timestamp to roll back (ISO-8601 directory name, e.g. 2025-06-01T120000Z).
        #[arg(long, value_name = "TS")]
        ts: String,

        /// Limit rollback to a single slug (skill/plugin name or MCP key).
        /// When omitted all items in the backup are restored.
        #[arg(long, value_name = "SLUG")]
        slug: Option<String>,

        /// Skip the interactive confirmation prompt.
        #[arg(long, default_value_t = false)]
        yes: bool,

        /// Registry base URL — used to call backend cleanup endpoints.
        #[arg(
            long,
            env = "VECTORHAWK_REGISTRY_URL",
            default_value = "https://app.vectorhawk.ai"
        )]
        registry_url: String,
    },
}

/// Entry point called from `main.rs`.
pub async fn run(cmd: MigrateCommand) -> Result<()> {
    match cmd {
        MigrateCommand::ListBackups => cmd_list_backups().await,
        MigrateCommand::Rollback {
            ts,
            slug,
            yes,
            registry_url,
        } => cmd_rollback(&ts, slug.as_deref(), yes, &registry_url).await,
    }
}

// ── list-backups ──────────────────────────────────────────────────────────────

async fn cmd_list_backups() -> Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("migrate list-backups: HOME directory not resolvable"))?;

    let summaries =
        list_backups(&home).context("migrate list-backups: failed to enumerate backups")?;

    if summaries.is_empty() {
        println!("No managed-paths backups found.");
        println!("Backups are created when the daemon performs a first-run migration (F1).");
        return Ok(());
    }

    println!("{:<26} {:>6}  SLUGS", "TIMESTAMP", "ITEMS");
    println!("{}", "-".repeat(70));

    for s in &summaries {
        // Show up to 3 slugs as a preview.
        let preview: Vec<&str> = s.items.iter().take(3).map(|i| i.slug.as_str()).collect();
        let preview_str = if s.item_count > 3 {
            format!("{}, ... (+{})", preview.join(", "), s.item_count - 3)
        } else {
            preview.join(", ")
        };
        println!("{:<26} {:>6}  {}", s.ts, s.item_count, preview_str);
    }

    println!("\nTo roll back: vectorhawk migrate rollback --ts <TIMESTAMP>");
    Ok(())
}

// ── rollback ──────────────────────────────────────────────────────────────────

async fn cmd_rollback(ts: &str, slug: Option<&str>, yes: bool, registry_url: &str) -> Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("migrate rollback: HOME directory not resolvable"))?;

    // Confirm before proceeding unless --yes.
    if !yes {
        let scope = match slug {
            Some(s) => format!("slug '{s}' from backup '{ts}'"),
            None => format!("all items from backup '{ts}'"),
        };

        eprint!(
            "This will restore {scope} to their original paths and remove them from VectorHawk governance.\n\
             Proceed? [y/N] "
        );

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("migrate rollback: failed to read confirmation input")?;

        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let state = AppState::bootstrap().context("migrate rollback: failed to bootstrap state")?;

    println!("Rolling back backup '{ts}'...");

    let report = rollback(&state, registry_url, &home, ts, slug)
        .await
        .context("migrate rollback: rollback operation failed")?;

    // ── Print report ──────────────────────────────────────────────────────────
    if report.restored.is_empty() && report.errors.is_empty() {
        println!("Nothing to roll back (no items matched the filter).");
        return Ok(());
    }

    if !report.restored.is_empty() {
        println!("\nRestored ({}):", report.restored.len());
        for slug_restored in &report.restored {
            println!("  + {slug_restored}");
        }
    }

    if !report.errors.is_empty() {
        println!("\nErrors ({}):", report.errors.len());
        for err in &report.errors {
            println!("  ! {} — {}", err.slug, err.message);
        }
        eprintln!(
            "\nwarning: {} item(s) could not be fully rolled back — see errors above.",
            report.errors.len()
        );
    }

    if report.errors.is_empty() {
        println!("\nRollback complete.");
    } else {
        println!("\nRollback finished with errors.");
    }

    Ok(())
}
