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
/// Idempotent, and idempotent *without churn*: an existing correct link is left
/// alone, and so is a real directory left behind by a prior `LinkMode::Copy`
/// fallback whose content already matches `canonical` — that returns
/// [`LinkMode::Copy`] having touched nothing. Only a link pointing elsewhere,
/// or a copy that has actually diverged, is replaced; such a directory is moved
/// into `~/.claude/.vectorhawk-backup/<ts>/links/` before removal, so the
/// replacement is always recoverable — see [`backup_and_remove`].
///
/// Ownership of a real directory is decided by the `.vectorhawk-managed.json`
/// marker the pusher writes into the canonical directory (which `copy_tree`
/// necessarily carries along). Refuses to touch a real directory that is not
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
        // canonical.
        //
        // If it is already byte-identical to canonical there is nothing to
        // do, and doing something is actively harmful: on a copy-mode host
        // (Windows without Developer Mode) the symlink below always fails, so
        // tearing the directory down and re-copying it would mint a fresh
        // timestamped backup on *every* call — and this function is called on
        // every `push_skill` and every daemon start. That is unbounded disk
        // growth on a recurring trigger. Recognise the settled state and stop.
        //
        // This does not suppress healing where it matters: any divergence
        // (including one introduced by the very next push, which rewrites
        // canonical) falls through to the relink below, so a copy can never
        // persist past a content change. What we skip is converting a copy
        // whose content is already correct — which costs nothing.
        if dirs_identical(canonical, link_path) {
            return Ok(LinkMode::Copy);
        }
        // Diverged. Back it up, then remove it and re-materialise below so the
        // call stays idempotent and heals into a symlink if possible.
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

// ── Content comparison ────────────────────────────────────────────────────────

/// Chunk size for the streaming file comparison. Fixed, so comparing a tree
/// costs O(1) memory regardless of how large the files in it are.
const COMPARE_CHUNK: usize = 64 * 1024;

/// True iff `a` and `b` are identical directory trees.
///
/// Bounded by construction: entries are compared pairwise with an early return
/// on the first difference, and file contents stream through a fixed-size
/// buffer rather than being read into memory. A tree that differs in its first
/// entry costs one `read_dir` pair.
///
/// Never follows links — `symlink_metadata` classifies, and a symlink is
/// compared by its *target*, so a link is never confused with the file it
/// points at. Any read error yields `false`: "cannot prove they match" must
/// never be mistaken for "they match", because a false match suppresses the
/// relink.
///
/// `pub(crate)` so `drift::check_link_integrity` can reuse the same bounded
/// comparison to recognise a healthy `LinkMode::Copy` steady state, rather
/// than duplicating this walk.
pub(crate) fn dirs_identical(a: &Path, b: &Path) -> bool {
    dirs_identical_inner(a, b).unwrap_or(false)
}

fn dirs_identical_inner(a: &Path, b: &Path) -> Result<bool> {
    let mut names_a = read_dir_names(a)?;
    let mut names_b = read_dir_names(b)?;
    if names_a.len() != names_b.len() {
        return Ok(false);
    }
    names_a.sort();
    names_b.sort();
    if names_a != names_b {
        return Ok(false);
    }

    for name in &names_a {
        let pa = a.join(name);
        let pb = b.join(name);
        let ma = fs::symlink_metadata(&pa)
            .with_context(|| format!("links: failed to stat {}", pa.display()))?;
        let mb = fs::symlink_metadata(&pb)
            .with_context(|| format!("links: failed to stat {}", pb.display()))?;

        if ma.is_symlink() || mb.is_symlink() {
            // Compare nested links by target; never traverse them.
            if !(ma.is_symlink() && mb.is_symlink()) {
                return Ok(false);
            }
            let ta = fs::read_link(&pa)
                .with_context(|| format!("links: failed to read link {}", pa.display()))?;
            let tb = fs::read_link(&pb)
                .with_context(|| format!("links: failed to read link {}", pb.display()))?;
            if ta != tb {
                return Ok(false);
            }
        } else if ma.is_dir() || mb.is_dir() {
            if !(ma.is_dir() && mb.is_dir()) {
                return Ok(false);
            }
            if !dirs_identical_inner(&pa, &pb)? {
                return Ok(false);
            }
        } else {
            if ma.len() != mb.len() {
                return Ok(false);
            }
            if !files_identical(&pa, &pb)? {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

fn read_dir_names(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("links: failed to read {}", dir.display()))?
    {
        let entry = entry.context("links: failed to read dir entry")?;
        names.push(entry.file_name());
    }
    Ok(names)
}

/// Stream two same-sized files through fixed buffers, returning on the first
/// differing chunk. Never holds more than `2 * COMPARE_CHUNK` bytes.
fn files_identical(a: &Path, b: &Path) -> Result<bool> {
    let mut fa =
        fs::File::open(a).with_context(|| format!("links: failed to open {}", a.display()))?;
    let mut fb =
        fs::File::open(b).with_context(|| format!("links: failed to open {}", b.display()))?;

    let mut buf_a = vec![0u8; COMPARE_CHUNK];
    let mut buf_b = vec![0u8; COMPARE_CHUNK];

    loop {
        let n = read_full(&mut fa, &mut buf_a)
            .with_context(|| format!("links: failed to read {}", a.display()))?;
        let m = read_full(&mut fb, &mut buf_b)
            .with_context(|| format!("links: failed to read {}", b.display()))?;
        if n != m {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
        if buf_a[..n] != buf_b[..n] {
            return Ok(false);
        }
    }
}

/// Fill `buf` as far as EOF allows, so the two sides stay chunk-aligned even
/// when a single `read` returns short.
fn read_full(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
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
