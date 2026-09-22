//! Binary-replacement detection.
//!
//! The daemon notices when its own on-disk binary has been swapped out from
//! under it — a package-manager upgrade — and exits cleanly so the service
//! manager (`launchd`/`systemd`) restarts it onto the new version. See
//! `run_daemon` in `lib.rs` for how the periodic task built from this
//! module's pieces is wired into the accept loop / shutdown path.
//!
//! # Why this exists
//!
//! On Linux, a running process keeps executing its binary's original inode
//! even after the file on disk is replaced — `readlink /proc/<pid>/exe` on
//! a stale daemon after `brew upgrade` shows e.g.
//! `…/Cellar/vectorhawk/1.0.93/bin/vectorhawk (deleted)`. Upgrades were
//! therefore invisible to a running daemon: it kept serving the old build
//! forever, until something happened to restart it by other means.
//!
//! 1.0.93 added a check to `daemon install` that detects this and restarts,
//! but nothing reliably *calls* `daemon install` on upgrade: `brew upgrade`
//! does not restart services on its own, and Homebrew's `post_install` hook
//! is sandboxed on both macOS (seatbelt) and Linux (Landlock) — it cannot
//! write into the user's real home directory to trigger a restart itself.
//! The robust fix is for the daemon to notice its own binary changed and
//! exit; `KeepAlive`/`Restart=` bring it back on the new build. This works
//! identically for Homebrew, `brew services`, and the Linux shell installer.
//!
//! # Which path to watch
//!
//! `std::env::current_exe()` is deliberately NOT the watched path — see
//! [`resolve_watch_path`].

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Env var that disables the binary-replacement watch entirely: set to any
/// non-empty value to turn it off. Escape hatch for setups where the watch
/// path doesn't make sense — e.g. a container that intentionally overlays a
/// new binary without wanting a restart, or a debug build run repeatedly
/// out of `target/` where every `cargo build` would otherwise trigger one.
pub const NO_BINARY_WATCH_ENV: &str = "VH_NO_BINARY_WATCH";

/// How often (in seconds) the binary-watch task re-stats the watch path.
/// Coarse on purpose: an upgrade landing up to a minute late costs nothing
/// (the old binary keeps serving fine in the meantime, it just hasn't
/// noticed yet), and even a cheap `stat()` is still blocking I/O that must
/// go through `spawn_blocking` on the daemon's single-threaded executor —
/// see the spawn_blocking discipline note at the top of `lib.rs`.
pub const BINARY_WATCH_INTERVAL_SECS: u64 = 60;

/// `(device, inode)` identity of a resolved binary on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExeIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Pure decision: has the binary at the watched path changed since we
/// recorded its identity at startup?
///
/// - `current == Some(id)` and `id != recorded` → `true` — replaced.
/// - `current == None` → **`false`**, unconditionally. Package managers
///   briefly unlink and relink the target during an upgrade (remove the old
///   symlink/file, then write the new one); a stat landing in that gap is
///   not evidence of an upgrade, just unlucky timing, and treating it as a
///   change would make the daemon exit on its own transient — a
///   self-inflicted restart loop, not a response to a real upgrade. Wait
///   for the path to resolve again. If it never does, that is a broken
///   install to be surfaced some other way (`doctor`), not this watch's
///   job — see [`stat_identity`].
/// - `current == Some(id)` and `id == recorded` → `false` — unchanged.
pub fn binary_changed(recorded: ExeIdentity, current: Option<ExeIdentity>) -> bool {
    match current {
        Some(id) => id != recorded,
        None => false,
    }
}

/// Resolve the **stable** path whose identity should be watched.
///
/// Deliberately not `std::env::current_exe()`: under Homebrew that resolves
/// to the *versioned* Cellar path (e.g.
/// `/opt/homebrew/Cellar/vectorhawk/1.0.94/bin/vectorhawk`). An upgrade
/// deletes that exact path outright rather than replacing it in place, so
/// stat'ing it after an upgrade always returns "not found" — which
/// [`binary_changed`] correctly refuses to treat as a change (see its
/// `None` case). A daemon watching the versioned path would therefore never
/// notice an upgrade at all, not even late.
///
/// Instead this applies the same Cellar→symlink rewrite the CLI installer
/// uses for the unit's `ExecStart`/`ProgramArguments`
/// (`vectorhawkd_core::binary_path::rewrite_homebrew_cellar_to_symlink`):
/// `<prefix>/Cellar/<formula>/<version>/bin/<name>` becomes
/// `<prefix>/bin/<name>`, the unversioned symlink Homebrew relinks in place
/// on every upgrade. That path itself never disappears, but the *inode* it
/// resolves to (after following the symlink) changes on every relink —
/// exactly the signal `binary_changed` needs. For a non-Homebrew install
/// (`cargo install`, the Linux shell installer) the rewrite is a no-op and
/// the watch path is `current_exe()` itself; the shell installer replaces
/// that file directly, which also changes its inode.
pub fn resolve_watch_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(vectorhawkd_core::binary_path::rewrite_homebrew_cellar_to_symlink(&exe))
}

/// Stat `path`, **following symlinks** (`std::fs::metadata`, not
/// `symlink_metadata`) — the watch path is often itself a symlink (e.g.
/// Homebrew's `<prefix>/bin/vectorhawk`), and it is the *target's* identity
/// that changes on relink, not necessarily the symlink's own inode. Returns
/// `None` on any stat failure (missing path, permission error, dangling
/// symlink mid-relink, ...) — callers must treat that as "unknown, try
/// again later," never as "changed": see [`binary_changed`]'s `None` case.
pub fn stat_identity(path: &Path) -> Option<ExeIdentity> {
    let meta = std::fs::metadata(path).ok()?;
    Some(ExeIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// `true` when [`NO_BINARY_WATCH_ENV`] is set to a non-empty value.
pub fn watch_disabled() -> bool {
    std::env::var_os(NO_BINARY_WATCH_ENV).is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(dev: u64, ino: u64) -> ExeIdentity {
        ExeIdentity { dev, ino }
    }

    #[test]
    fn unchanged_is_not_a_change() {
        assert!(!binary_changed(id(1, 100), Some(id(1, 100))));
    }

    #[test]
    fn same_dev_different_ino_is_a_change() {
        assert!(binary_changed(id(1, 100), Some(id(1, 200))));
    }

    #[test]
    fn different_dev_same_ino_is_a_change() {
        // Inode numbers are only unique within a single filesystem; two
        // different devices can each have an inode 100 that are completely
        // unrelated files. Comparing `ino` alone would miss this case;
        // comparing `dev` alone would miss the same-device case above.
        assert!(binary_changed(id(1, 100), Some(id(2, 100))));
    }

    #[test]
    fn missing_current_is_never_a_change() {
        // The transient unlink/relink window during an upgrade must not,
        // on its own, trigger an exit — see `binary_changed`'s doc comment.
        assert!(!binary_changed(id(1, 100), None));
    }

    #[test]
    fn stat_identity_returns_none_for_missing_path() {
        assert_eq!(
            stat_identity(Path::new("/nonexistent/path/vectorhawk")),
            None
        );
    }

    #[test]
    fn stat_identity_follows_symlinks_to_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-binary");
        std::fs::write(&target, b"pretend binary").unwrap();
        let link = dir.path().join("bin-symlink");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let via_link = stat_identity(&link).unwrap();
        let via_target = stat_identity(&target).unwrap();
        assert_eq!(
            via_link, via_target,
            "stat_identity must follow the symlink to the target's identity"
        );
    }

    #[test]
    fn watch_disabled_reads_the_env_var() {
        // Serialize against other tests mutating process env: this is a
        // single assertion-per-state test, not a fixture, so a plain
        // save/restore around it is enough — no other test in this module
        // touches `NO_BINARY_WATCH_ENV`.
        let saved = std::env::var_os(NO_BINARY_WATCH_ENV);

        std::env::remove_var(NO_BINARY_WATCH_ENV);
        assert!(!watch_disabled());

        std::env::set_var(NO_BINARY_WATCH_ENV, "1");
        assert!(watch_disabled());

        std::env::set_var(NO_BINARY_WATCH_ENV, "");
        assert!(!watch_disabled(), "empty value must not count as set");

        match saved {
            Some(v) => std::env::set_var(NO_BINARY_WATCH_ENV, v),
            None => std::env::remove_var(NO_BINARY_WATCH_ENV),
        }
    }
}
