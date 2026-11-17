//! Symlink- and special-file-safe tree copy and fingerprint.
//!
//! Every place that backs up, restores or stages a user's directory goes
//! through here. The naive recursive copy these replace used `metadata()`
//! (which follows links) and `fs::copy` (which opens FIFOs and blocks
//! forever), so a symlink loop recursed until the stack overflowed (an abort,
//! not a panic: the daemon died, and since the skill was retried every tick it
//! crash-looped) and a FIFO hung the thread.
//!
//! Rules:
//! - Walk with `symlink_metadata`. Never follow a link.
//! - A symlink is copied as a symlink (same target text), never as the
//!   content it points at. Backup and restore therefore round-trip links.
//! - Anything that is not a regular file, directory or symlink (FIFO, socket,
//!   device) is skipped and reported in [`CopyStats::skipped_special`].
//! - File count, total bytes and depth are capped ([`CopyLimits`]); exceeding
//!   a cap is an error, so a backup that could not be completed is never
//!   mistaken for a full one.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Caps applied to every tree walk.
#[derive(Debug, Clone, Copy)]
pub struct CopyLimits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_depth: usize,
}

impl Default for CopyLimits {
    fn default() -> Self {
        Self {
            max_files: 50_000,
            max_bytes: 1024 * 1024 * 1024,
            max_depth: 64,
        }
    }
}

/// What a copy did.
#[derive(Debug, Default, Clone)]
pub struct CopyStats {
    /// Regular files copied.
    pub files: usize,
    /// Bytes of regular-file content copied.
    pub bytes: u64,
    /// Symlinks recreated as symlinks.
    pub symlinks: usize,
    /// FIFOs, sockets and devices that were left out.
    pub skipped_special: Vec<PathBuf>,
}

/// A short token that is unique across processes and calls: nanoseconds, pid
/// and a process-wide counter. Used for staging dirs and backup dir names so
/// two attempts in the same second never share a directory.
pub fn unique_token() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}-{}-{}",
        nanos,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Copy `src` (file, directory or symlink) to `dest` with the default caps.
pub fn copy_tree(src: &Path, dest: &Path) -> Result<CopyStats> {
    copy_tree_with(src, dest, &CopyLimits::default())
}

/// [`copy_tree`] with explicit caps.
pub fn copy_tree_with(src: &Path, dest: &Path, limits: &CopyLimits) -> Result<CopyStats> {
    let mut stats = CopyStats::default();
    let meta = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let ft = meta.file_type();
    if ft.is_dir() {
        copy_dir(src, dest, limits, 0, &mut stats)?;
    } else if ft.is_symlink() || ft.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        copy_leaf(src, dest, &meta, limits, &mut stats)?;
    } else {
        bail!(
            "refusing to copy {}: not a regular file, directory or symlink",
            src.display()
        );
    }
    Ok(stats)
}

fn copy_dir(
    src: &Path,
    dest: &Path,
    limits: &CopyLimits,
    depth: usize,
    stats: &mut CopyStats,
) -> Result<()> {
    if depth > limits.max_depth {
        bail!(
            "refusing to copy {}: directory nesting deeper than {}",
            src.display(),
            limits.max_depth
        );
    }
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry.with_context(|| format!("reading entry in {}", src.display()))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let meta =
            fs::symlink_metadata(&from).with_context(|| format!("stat {}", from.display()))?;
        if meta.file_type().is_dir() {
            copy_dir(&from, &to, limits, depth + 1, stats)?;
        } else {
            copy_leaf(&from, &to, &meta, limits, stats)?;
        }
    }
    Ok(())
}

fn copy_leaf(
    from: &Path,
    to: &Path,
    meta: &fs::Metadata,
    limits: &CopyLimits,
    stats: &mut CopyStats,
) -> Result<()> {
    let ft = meta.file_type();
    if !ft.is_symlink() && !ft.is_file() {
        tracing::warn!(path = %from.display(), "safe_copy: skipping special file (FIFO/socket/device)");
        stats.skipped_special.push(from.to_path_buf());
        return Ok(());
    }
    if stats.files + stats.symlinks + 1 > limits.max_files {
        bail!(
            "refusing to copy {}: more than {} files",
            from.display(),
            limits.max_files
        );
    }
    if ft.is_symlink() {
        let target = fs::read_link(from).with_context(|| format!("readlink {}", from.display()))?;
        replace_with_symlink(&target, to)?;
        stats.symlinks += 1;
        return Ok(());
    }
    if stats.bytes.saturating_add(meta.len()) > limits.max_bytes {
        bail!(
            "refusing to copy {}: more than {} bytes",
            from.display(),
            limits.max_bytes
        );
    }
    // `to` may already be a symlink (merge into an existing tree): `fs::copy`
    // would write THROUGH it.
    if let Ok(existing) = fs::symlink_metadata(to) {
        if existing.file_type().is_symlink() {
            fs::remove_file(to).with_context(|| format!("removing {}", to.display()))?;
        }
    }
    fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    stats.files += 1;
    stats.bytes += meta.len();
    Ok(())
}

fn replace_with_symlink(target: &Path, link: &Path) -> Result<()> {
    if let Ok(existing) = fs::symlink_metadata(link) {
        if existing.file_type().is_dir() {
            bail!(
                "cannot create symlink {}: a directory is in the way",
                link.display()
            );
        }
        fs::remove_file(link).with_context(|| format!("removing {}", link.display()))?;
    }
    make_symlink(target, link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let resolved = link.parent().map(|p| p.join(target));
    let is_dir = resolved
        .as_deref()
        .and_then(|p| fs::metadata(p).ok())
        .is_some_and(|m| m.is_dir());
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// SHA-256 over a directory tree: relative paths, entry types, file bytes and
/// symlink targets, in sorted order. Names listed in `skip_root` are ignored
/// at the top level only (pass the managed-marker filename: it carries a
/// timestamp). Never follows links, skips special files, honours the default
/// caps.
pub fn tree_fingerprint(dir: &Path, skip_root: &[&str]) -> Result<String> {
    let limits = CopyLimits::default();
    let mut hasher = Sha256::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    hash_dir(
        dir,
        Path::new(""),
        skip_root,
        &limits,
        0,
        &mut files,
        &mut bytes,
        &mut hasher,
    )?;
    Ok(hex::encode(hasher.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn hash_dir(
    root: &Path,
    rel: &Path,
    skip_root: &[&str],
    limits: &CopyLimits,
    depth: usize,
    files: &mut usize,
    bytes: &mut u64,
    hasher: &mut Sha256,
) -> Result<()> {
    if depth > limits.max_depth {
        bail!("fingerprint: {} nests deeper than the cap", root.display());
    }
    let dir = root.join(rel);
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        if depth == 0 && skip_root.iter().any(|s| name == *s) {
            continue;
        }
        let rel_child = rel.join(&name);
        let abs = root.join(&rel_child);
        let meta = fs::symlink_metadata(&abs).with_context(|| format!("stat {}", abs.display()))?;
        let ft = meta.file_type();
        let rel_bytes = rel_child.to_string_lossy();
        if ft.is_dir() {
            hasher.update(b"d\0");
            hasher.update(rel_bytes.as_bytes());
            hasher.update(b"\0");
            hash_dir(
                root,
                &rel_child,
                skip_root,
                limits,
                depth + 1,
                files,
                bytes,
                hasher,
            )?;
        } else if ft.is_symlink() {
            *files += 1;
            let target = fs::read_link(&abs)?;
            hasher.update(b"l\0");
            hasher.update(rel_bytes.as_bytes());
            hasher.update(b"\0");
            hasher.update(target.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        } else if ft.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(meta.len());
            if *files > limits.max_files || *bytes > limits.max_bytes {
                bail!("fingerprint: {} exceeds the size cap", root.display());
            }
            hasher.update(b"f\0");
            hasher.update(rel_bytes.as_bytes());
            hasher.update(b"\0");
            let mut f = fs::File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
            let mut buf = [0u8; 16 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            hasher.update(b"\0");
        }
        // Special files are ignored, matching `copy_tree`.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_symlinks_and_does_not_follow_loops() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("SKILL.md"), "x").unwrap();
        // Loop: sub/up -> .. (would recurse forever if followed).
        std::os::unix::fs::symlink("..", src.join("sub/up")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", src.join("abs")).unwrap();

        let dest = tmp.path().join("dest");
        let stats = copy_tree(&src, &dest).unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.symlinks, 2);
        assert_eq!(
            fs::read_link(dest.join("sub/up")).unwrap(),
            PathBuf::from("..")
        );
        assert_eq!(
            fs::read_link(dest.join("abs")).unwrap(),
            PathBuf::from("/etc/hosts")
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_skips_fifos_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("SKILL.md"), "x").unwrap();
        let fifo = src.join("pipe");
        // Use the `mkfifo` binary to avoid a libc dependency.
        let ok = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no mkfifo on this box
        }
        let dest = tmp.path().join("dest");
        let stats = copy_tree(&src, &dest).unwrap();
        assert_eq!(stats.skipped_special.len(), 1);
        assert!(dest.join("SKILL.md").exists());
        assert!(!dest.join("pipe").exists());
    }

    #[test]
    fn copy_tree_enforces_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..5 {
            fs::write(src.join(format!("f{i}")), "abc").unwrap();
        }
        let tight = CopyLimits {
            max_files: 3,
            ..CopyLimits::default()
        };
        assert!(copy_tree_with(&src, &tmp.path().join("d1"), &tight).is_err());
        let tiny = CopyLimits {
            max_bytes: 4,
            ..CopyLimits::default()
        };
        assert!(copy_tree_with(&src, &tmp.path().join("d2"), &tiny).is_err());
        let shallow = CopyLimits {
            max_depth: 1,
            ..CopyLimits::default()
        };
        let deep = tmp.path().join("deep");
        fs::create_dir_all(deep.join("a/b/c")).unwrap();
        assert!(copy_tree_with(&deep, &tmp.path().join("d3"), &shallow).is_err());
    }

    #[test]
    fn fingerprint_changes_with_content_and_ignores_root_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("SKILL.md"), "a").unwrap();
        fs::write(d.join("sub/x"), "b").unwrap();
        fs::write(d.join(".marker"), "t1").unwrap();
        let h1 = tree_fingerprint(&d, &[".marker"]).unwrap();
        fs::write(d.join(".marker"), "t2").unwrap();
        assert_eq!(h1, tree_fingerprint(&d, &[".marker"]).unwrap());
        fs::write(d.join("sub/x"), "c").unwrap();
        assert_ne!(h1, tree_fingerprint(&d, &[".marker"]).unwrap());
    }

    #[test]
    fn unique_token_never_repeats() {
        let a = unique_token();
        let b = unique_token();
        assert_ne!(a, b);
    }
}
