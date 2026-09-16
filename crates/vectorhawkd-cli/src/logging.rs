//! Tracing initialization for the `vectorhawk` process.
//!
//! Two logging profiles, chosen by `main()` based on the parsed subcommand
//! (see `is_daemon_foreground` in `main.rs`):
//!
//! - **Interactive CLI subcommands** ([`init_cli`]) — human-readable output
//!   to stderr, WARN by default. The AI client reads stdout for MCP
//!   JSON-RPC and ignores stderr, so this is safe on every subcommand.
//! - **`vectorhawk daemon run`** ([`init_daemon`]) — the exact command line
//!   the installed LaunchAgent (macOS) and systemd user unit (Linux) both
//!   invoke — INFO by default, written to a size-bounded rotating file
//!   under the daemon's state directory (`<root_dir>/logs/vectorhawkd.log`).
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
//! deleted by the `rolling-file` crate as new ones roll in — see
//! `rotates_and_bounds_total_log_size` below for a direct test of that
//! behavior with the same condition/cap shape this module configures.

use std::path::Path;

use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
use tracing_subscriber::EnvFilter;

/// Size of each rotated log segment.
const LOG_SEGMENT_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated backups kept in addition to the active segment (10 segments
/// total * 5 MB = ~50 MB daemon log footprint cap).
const LOG_MAX_BACKUPS: usize = 9;
const LOG_FILE_BASENAME: &str = "vectorhawkd.log";

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

/// `vectorhawk daemon run`: size-bounded rotating file under `log_dir`, INFO
/// by default.
///
/// Falls back to stderr (still INFO by default) if the log directory or the
/// rotating file can't be opened, so the daemon still starts and still logs
/// *something* rather than failing silently on a read-only or unwritable
/// state directory.
pub fn init_daemon(log_dir: &Path) -> LoggingGuard {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "vectorhawk: warning: failed to create log dir {}: {e:#} \
             — falling back to stderr logging",
            log_dir.display()
        );
        return init_daemon_stderr_fallback();
    }

    let condition = RollingConditionBasic::new().max_size(LOG_SEGMENT_BYTES);
    let appender = match BasicRollingFileAppender::new(
        log_dir.join(LOG_FILE_BASENAME),
        condition,
        LOG_MAX_BACKUPS,
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "vectorhawk: warning: failed to open rotating log file under {}: {e:#} \
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
    // vectorhawkd-daemon/src/lib.rs) and `info!`/`warn!` are called directly
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Direct test of the exact condition/cap shape `init_daemon` configures
    /// (scaled down so the test runs fast): writes far more data than an
    /// unrotated file would ever be allowed to hold, then asserts the
    /// on-disk footprint stayed bounded and old segments were deleted.
    ///
    /// This is the D1 acceptance check ("generate volume, confirm rotation
    /// kicks in and old segments are deleted, size stays under cap") applied
    /// directly to the appender construction this module uses.
    #[test]
    fn rotates_and_bounds_total_log_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log_path = tmp.path().join(LOG_FILE_BASENAME);
        // 1 KiB segments, 3 rotated backups => 4 segments total, ~4 KiB cap.
        let condition = RollingConditionBasic::new().max_size(1024);
        let mut appender = BasicRollingFileAppender::new(&log_path, condition, 3)
            .expect("failed to construct rolling appender");

        // 200 lines * ~191 bytes =~ 38 KB if nothing ever rotated — about
        // 10x the ~4 KB total cap this configuration allows.
        let line = vec![b'x'; 190];
        for _ in 0..200 {
            appender.write_all(&line).expect("write");
            appender.write_all(b"\n").expect("write newline");
        }
        appender.flush().expect("flush");
        drop(appender);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();

        let total_bytes: u64 = entries
            .iter()
            .map(|e| e.metadata().expect("metadata").len())
            .sum();

        assert!(
            entries.len() <= 4,
            "expected at most 4 log segments (1 active + 3 backups), found {} ({:?})",
            entries.len(),
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
        assert!(
            total_bytes < 8 * 1024,
            "expected bounded total log size well under the ~38 KB an \
             unrotated file would reach, got {total_bytes} bytes"
        );
    }
}
