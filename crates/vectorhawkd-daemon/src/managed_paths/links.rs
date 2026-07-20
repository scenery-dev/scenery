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
/// pointing elsewhere is replaced. Refuses to touch a real directory that is
/// not ours — that is user content.
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
        bail!(
            "links: refusing to replace a real directory at {} — \
             not VectorHawk-managed",
            link_path.display()
        );
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

#[cfg(unix)]
fn symlink_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn symlink_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
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
