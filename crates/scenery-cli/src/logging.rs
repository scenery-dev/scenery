//! Tracing initialization for the `scenery` process.
//!
//! Two logging profiles, chosen by `main()` based on the parsed subcommand
//! (see `is_daemon_foreground` in `main.rs`):
//!
//! - **Interactive CLI subcommands** ([`init_cli`]) — human-readable output
//!   to stderr, WARN by default. The AI client reads stdout for MCP
//!   JSON-RPC and ignores stderr, so this is safe on every subcommand.
//! - **`scenery daemon run`** ([`init_daemon`]) — the exact command line
//!   the installed LaunchAgent (macOS) and systemd user unit (Linux) both
//!   invoke — INFO by default, written to a size-bounded rotating file
//!   under the daemon's state directory (`<root_dir>/logs/scenery.log`).
//!   This is the *durable* daemon log: launchd does not rotate
//!   `StandardOutPath`, so the plist no longer depends on it for the log
//!   that matters (see `install/macos.rs`); rotation is self-contained and
//!   identical on every platform instead of relying on external log
//!   management (journald vacuuming, logrotate, ...).
//!
//! `RUST_LOG` overrides the default level on both profiles — existing
//! behavior, unchanged.
//!
//! # Bound
//!
//! `LOG_SEGMENT_BYTES` * (`LOG_MAX_BACKUPS` + 1 active segment) caps the
//! daemon's durable log footprint at ~50 MB total. Oldest segments are
//! deleted by [`crate::rotating_file::RotatingFile`] as new ones roll in — see
//! its tests for the rotation/bound behavior.

use std::path::Path;

use crate::rotating_file::RotatingFile;
use tracing_subscriber::EnvFilter;

/// Size of each rotated log segment.
const LOG_SEGMENT_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated backups kept in addition to the active segment (10 segments
/// total * 5 MB = ~50 MB daemon log footprint cap).
const LOG_MAX_BACKUPS: usize = 9;
const LOG_FILE_BASENAME: &str = "scenery.log";

/// Keeps the background writer thread (when file logging is active) alive.
/// Must be held for the lifetime of `main()` — dropping it early stops the
/// worker thread and silently drops subsequent log lines.
#[must_use]
pub struct LoggingGuard(#[allow(dead_code)] Option<tracing_appender::non_blocking::WorkerGuard>);

/// Interactive CLI subcommands: stderr, WARN by default.
pub fn init_cli() -> LoggingGuard {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter_or("warn"))
        .init();
    LoggingGuard(None)
}

/// `scenery daemon run`: size-bounded rotating file under `log_dir`, INFO
/// by default.
///
/// Falls back to stderr (still INFO by default) if the log directory or the
/// rotating file can't be opened, so the daemon still starts and still logs
/// *something* rather than failing silently on a read-only or unwritable
/// state directory.
pub fn init_daemon(log_dir: &Path) -> LoggingGuard {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "scenery: warning: failed to create log dir {}: {e:#} \
             — falling back to stderr logging",
            log_dir.display()
        );
        return init_daemon_stderr_fallback();
    }

    let appender = match RotatingFile::new(
        log_dir.join(LOG_FILE_BASENAME),
        LOG_SEGMENT_BYTES,
        LOG_MAX_BACKUPS,
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "scenery: warning: failed to open rotating log file under {}: {e:#} \
                 — falling back to stderr logging",
                log_dir.display()
            );
            return init_daemon_stderr_fallback();
        }
    };

    let (writer, guard) = tracing_appender::non_blocking(appender);

    // Writes go through the non-blocking channel to a dedicated worker
    // thread: the daemon runs a single-threaded Tokio executor (see the
    // spawn_blocking discipline documented at the top of
    // scenery-daemon/src/lib.rs) and `info!`/`warn!` are called directly
    // from async code on that executor, so a synchronous file write here
    // would stall every concurrent shim connection.
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(env_filter_or("info"))
        .init();

    LoggingGuard(Some(guard))
}

fn init_daemon_stderr_fallback() -> LoggingGuard {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter_or("info"))
        .init();
    LoggingGuard(None)
}

fn env_filter_or(default_level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level))
}
