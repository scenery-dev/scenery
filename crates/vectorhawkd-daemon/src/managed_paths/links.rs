//! Directory link management for multi-client skill surfacing.
//!
//! VectorHawk writes each managed skill exactly once, as a real directory
//! under `~/.agents/skills/<slug>/`. Cursor, Codex, and Gemini CLI all scan
//! that path natively. Claude Code does not, so it gets a directory symlink
//! at `~/.claude/skills/<slug>` pointing at the canonical directory.
//!
//! # Why links and not copies
//!
//! One copy on disk means one hash to verify, one atomic update (repoint the
//! link), and no N-way divergence between clients.
//!
//! # Why nothing writes through a link
//!
//! A pre-v1.0.51 installer symlinked `~/.claude/skills/<slug>` at a versioned
//! install directory and then wrote *through* it, leaking modifications back
//! into the installer's state. Here the pusher writes only to the canonical
//! directory and links are strictly read-only aliases. Never open a file for
//! writing via a path under a link root.
//!
//! # Platform
//!
//! Unix uses `std::os::unix::fs::symlink`. Windows attempts
//! `std::os::windows::fs::symlink_dir`, which requires Developer Mode or
//! elevation; when that fails we fall back to a recursive copy and report
//! `LinkMode::Copy` so callers can select the hash-based drift path instead of
//! the link-integrity path.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use vectorhawkd_mcp::ownership;

/// How a link target was materialised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// A real symlink — the normal case.
    Symlink,
    /// A recursive copy, used where symlinks are unavailable (Windows without
    /// Developer Mode). Drift for these must use content hashing.
    Copy,
}

/// Point `link_path` at `canonical`.
///
/// Idempotent: an existing correct link is left alone; an existing link
/// pointing elsewhere is replaced. A real directory left behind by a prior
/// `LinkMode::Copy` fallback (identified by the `.vectorhawk-managed.json`
/// marker the pusher writes into the canonical directory, which `copy_tree`
/// necessarily carries along) is also replaced, so repeated calls stay
/// idempotent even on platforms where symlinks are unavailable. Such a
/// directory is moved into `~/.claude/.vectorhawk-backup/<ts>/links/` before
/// removal, so the replacement is always recoverable — see
/// [`backup_and_remove`]. Refuses to touch a real directory that is not
/// ours — that is user content.
pub fn link_dir(canonical: &Path, link_path: &Path) -> Result<LinkMode> {
    if !canonical.is_dir() {
        bail!(
            "links: canonical dir does not exist: {}",
            canonical.display()
        );
    }

    if link_path.is_symlink() {
        if link_is_intact(canonical, link_path)? {
            return Ok(LinkMode::Symlink);
        }
        fs::remove_file(link_path).with_context(|| {
            format!(
                "links: failed to remove stale link: {}",
                link_path.display()
            )
        })?;
    } else if link_path.exists() {
        if !ownership::is_vectorhawk_managed(link_path) {
            bail!(
                "links: refusing to replace a real directory at {} — \
                 not VectorHawk-managed",
                link_path.display()
            );
        }
        // A real directory we materialised ourselves — either a prior
        // `LinkMode::Copy` fallback, or (pre-pivot) a real managed skill dir
        // that lived at the Claude path before `~/.agents/skills` became
        // canonical. Back it up, then remove it and re-materialise below so
        // the call stays idempotent and heals into a symlink if possible.
        //
        // The backup is what makes this recoverable: the marker proves
        // VectorHawk *wrote* the directory, but not that its current content
        // is still reproducible — a restored backup or a hand-customised
        // skill dir carries the marker too. Every other destructive path in
        // this codebase has a ledger behind it (the migrator backs up before
        // each takeover, `push_skill` journals its own push); this one now
        // does as well.
        backup_and_remove(link_path)?;
    }

    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("links: failed to create link parent: {}", parent.display())
        })?;
    }

    match symlink_dir(canonical, link_path) {
        Ok(()) => Ok(LinkMode::Symlink),
        Err(e) => {
            tracing::warn!(
                error = %e,
                link = %link_path.display(),
                "links: symlink failed, falling back to copy"
            );
            copy_tree(canonical, link_path)?;
            Ok(LinkMode::Copy)
        }
    }
}

/// Remove `link_path`, whether it is a symlink or a copied directory.
/// Idempotent — absent is success. Never touches the canonical directory.
pub fn unlink_dir(link_path: &Path) -> Result<()> {
    if link_path.is_symlink() {
        fs::remove_file(link_path)
            .with_context(|| format!("links: failed to remove link: {}", link_path.display()))?;
    } else if link_path.is_dir() {
        fs::remove_dir_all(link_path).with_context(|| {
            format!(
                "links: failed to remove copied dir: {}",
                link_path.display()
            )
        })?;
    }
    Ok(())
}

/// True iff `link_path` is a symlink resolving to `canonical`.
///
/// Both sides are canonicalized so `/var` vs `/private/var` on macOS and any
/// intermediate links compare equal.
pub fn link_is_intact(canonical: &Path, link_path: &Path) -> Result<bool> {
    if !link_path.is_symlink() {
        return Ok(false);
    }
    let Ok(resolved) = fs::canonicalize(link_path) else {
        return Ok(false);
    };
    let Ok(target) = fs::canonicalize(canonical) else {
        return Ok(false);
    };
    Ok(resolved == target)
}

/// Whether a directory symlink can actually be created inside `dir`.
///
/// Windows without Developer Mode (and some network/FAT mounts) refuse
/// `symlink_dir`, which is why [`link_dir`] has a copy fallback at all.
/// Callers that must distinguish "materialised as a copy because symlinks are
/// unavailable" from "materialised as a copy for some other reason" need to
/// know this *before* they tear anything down, so the check is an explicit
/// probe: create a throwaway link, observe, remove it.
///
/// Best-effort — any failure to even set up the probe reports `false`, which
/// is the conservative answer (callers then treat the copy as deliberate).
pub fn symlinks_supported(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".vectorhawk-symlink-probe-{}", std::process::id()));
    let _ = fs::remove_file(&probe);
    match symlink_dir(dir, &probe) {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn symlink_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn symlink_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

/// Move `dir` into a timestamped run directory under
/// `~/.claude/.vectorhawk-backup/<ts>/links/<name>`, then ensure `dir` is gone.
///
/// Uses the same backup root and `%Y-%m-%dT%H%M%SZ` run-directory convention as
/// the F1 migrator (`managed_paths::migrate_existing`), so recovered content
/// sits alongside every other VectorHawk backup rather than in a new location
/// of its own. No `manifest.json` is written, so `rollback::list_backups`
/// deliberately skips these runs — `migrate rollback` restores *takeovers* of
/// user content, and re-materialising a link's stale copy over the canonical
/// directory is not that. The copy is here to be recoverable by hand.
///
/// Prefers a rename (cheap, atomic within a filesystem) and falls back to a
/// recursive copy when the backup root is on a different device. If neither
/// succeeds the error propagates and the caller must NOT delete: an
/// unrecoverable removal is worse than a failed relink, which leaves Claude
/// Code pointed at the pre-existing directory.
fn backup_and_remove(dir: &Path) -> Result<()> {
    let name = dir
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("links: path has no final component: {}", dir.display()))?;

    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("links: HOME directory not resolvable for backup"))?;
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H%M%SZ").to_string();
    let backup_root = home
        .join(".claude")
        .join(".vectorhawk-backup")
        .join(&ts)
        .join("links");

    fs::create_dir_all(&backup_root).with_context(|| {
        format!(
            "links: failed to create backup dir: {}",
            backup_root.display()
        )
    })?;

    let dest = backup_root.join(name);

    match fs::rename(dir, &dest) {
        Ok(()) => {
            tracing::info!(
                from = %dir.display(),
                to = %dest.display(),
                "links: backed up pre-existing managed dir before relinking"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "links: rename into backup failed, falling back to copy"
            );
        }
    }

    copy_tree(dir, &dest).with_context(|| {
        format!(
            "links: refusing to remove {} — could not back it up to {}",
            dir.display(),
            dest.display()
        )
    })?;
    tracing::info!(
        from = %dir.display(),
        to = %dest.display(),
        "links: backed up pre-existing managed dir before relinking"
    );

    fs::remove_dir_all(dir)
        .with_context(|| format!("links: failed to remove stale copy: {}", dir.display()))?;

    Ok(())
}

/// Recursive directory copy used only by the Windows fallback.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("links: failed to create {}", dst.display()))?;
    for entry in
        fs::read_dir(src).with_context(|| format!("links: failed to read {}", src.display()))?
    {
        let entry = entry.context("links: failed to read dir entry")?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("links: failed to copy {}", from.display()))?;
        }
    }
    Ok(())
}
