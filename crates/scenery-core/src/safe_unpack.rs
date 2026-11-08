//! Hardened `.cskill` unpacking (H2).
//!
//! Skill bundles are publisher-controlled. A tar symlink followed by a regular
//! file under it would write through the link to anywhere the user can write,
//! so this unpacker accepts only regular files and directories, writes every
//! file itself (never via the tar crate's link-following paths), refuses any
//! symlink among the parents it creates under `dest`, and enforces entry-count
//! and byte caps counted on bytes actually written.

use anyhow::{Context, Result};
use camino::Utf8Path;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub const MAX_ENTRIES: usize = 2_000;
pub const MAX_ENTRY_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;

/// Running totals for one archive.
#[derive(Default)]
struct Budget {
    entries: usize,
    total: u64,
}

impl Budget {
    fn take_entry(&mut self) -> Result<()> {
        self.entries += 1;
        anyhow::ensure!(
            self.entries <= MAX_ENTRIES,
            "archive has more than {MAX_ENTRIES} entries"
        );
        Ok(())
    }
}

/// Unpack a `.cskill` archive (tar.gz or legacy zip, detected by magic) into
/// `dest`, which the caller should have just created empty.
pub fn unpack_cskill_archive(archive_path: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    let mut magic = [0u8; 2];
    {
        let mut f = fs::File::open(archive_path).with_context(|| {
            format!("failed to open archive for magic detection: {archive_path}")
        })?;
        f.read_exact(&mut magic)
            .with_context(|| format!("archive too small to detect format: {archive_path}"))?;
    }
    fs::create_dir_all(dest).with_context(|| format!("failed to create unpack dir: {dest}"))?;
    if magic == [0x1f, 0x8b] {
        unpack_tar_gz(archive_path, dest)
    } else if magic == [0x50, 0x4b] {
        unpack_zip(archive_path, dest)
    } else {
        anyhow::bail!(
            "unrecognised archive format (magic bytes {:02x}{:02x}): {archive_path}",
            magic[0],
            magic[1]
        )
    }
}

/// Normalise an archive member path to its `Normal` components, dropping `.`
/// and rejecting `..`, roots and prefixes.
fn normal_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("unsafe path in archive: {}", path.display())
            }
        }
    }
    Ok(out)
}

/// Create `dest/<comps[..n]>` directories one at a time, refusing any
/// existing symlink on the way.
fn ensure_dirs(dest: &Path, comps: &[std::ffi::OsString]) -> Result<PathBuf> {
    let mut cur = dest.to_path_buf();
    for c in comps {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                anyhow::bail!("symlink in unpack path: {}", cur.display())
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => anyhow::bail!("path component is not a directory: {}", cur.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&cur)
                .with_context(|| format!("failed to create dir: {}", cur.display()))?,
            Err(e) => return Err(e).context("failed to stat unpack path"),
        }
    }
    Ok(cur)
}

/// Write one regular file under `dest`, counting bytes actually written.
fn write_file<R: Read>(
    dest: &Path,
    comps: &[std::ffi::OsString],
    reader: &mut R,
    budget: &mut Budget,
) -> Result<()> {
    let (name, parents) = comps
        .split_last()
        .context("archive file entry has an empty path")?;
    let dir = ensure_dirs(dest, parents)?;
    let target = dir.join(name);
    match fs::symlink_metadata(&target) {
        Ok(m) if m.is_file() => fs::remove_file(&target).context("failed to replace file")?,
        Ok(_) => anyhow::bail!("unpack target exists and is not a file: {}", target.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("failed to stat unpack target"),
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .with_context(|| format!("failed to create: {}", target.display()))?;
    let mut buf = [0u8; 65536];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("error reading archive entry")?;
        if n == 0 {
            break;
        }
        written += n as u64;
        budget.total += n as u64;
        anyhow::ensure!(
            written <= MAX_ENTRY_BYTES,
            "archive entry exceeds {MAX_ENTRY_BYTES} bytes"
        );
        anyhow::ensure!(
            budget.total <= MAX_TOTAL_BYTES,
            "archive expands beyond {MAX_TOTAL_BYTES} bytes"
        );
        out.write_all(&buf[..n])
            .context("error writing archive entry")?;
    }
    Ok(())
}

fn open_tar(path: &Utf8Path) -> Result<tar::Archive<flate2::read::GzDecoder<fs::File>>> {
    let file = fs::File::open(path).with_context(|| format!("failed to open tar.gz: {path}"))?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(file)))
}

/// First pass: type-check every entry, count entries, and find a single
/// top-level wrapper directory to strip.
fn scan_tar(path: &Utf8Path) -> Result<Option<std::ffi::OsString>> {
    let mut archive = open_tar(path)?;
    let mut budget = Budget::default();
    let mut tops: std::collections::HashSet<std::ffi::OsString> = Default::default();
    let mut has_root_file = false;
    for entry in archive.entries().context("failed to iterate tar entries")? {
        let entry = entry.context("failed to read tar entry")?;
        budget.take_entry()?;
        let ty = entry.header().entry_type();
        if ty.is_pax_global_extensions() {
            continue;
        }
        check_entry_type(ty, &entry.path().context("invalid tar entry path")?)?;
        let comps = normal_components(&entry.path().context("invalid tar entry path")?)?;
        if let Some(first) = comps.first() {
            if comps.len() == 1 && !ty.is_dir() {
                has_root_file = true;
            }
            tops.insert(first.clone());
        }
    }
    if !has_root_file && tops.len() == 1 {
        Ok(tops.into_iter().next())
    } else {
        Ok(None)
    }
}

fn check_entry_type(ty: tar::EntryType, path: &Path) -> Result<()> {
    if ty.is_hard_link() {
        anyhow::bail!("hard link in tar archive: {}", path.display());
    }
    if ty.is_symlink() {
        anyhow::bail!("symlink in tar archive: {}", path.display());
    }
    if !(ty.is_file() || ty.is_dir()) {
        anyhow::bail!(
            "unsupported entry type in tar archive: {} ({ty:?})",
            path.display()
        );
    }
    Ok(())
}

/// Unpack a tar.gz archive into `dest`, stripping a single top-level wrapper
/// directory when every entry shares one.
fn unpack_tar_gz(archive_path: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    let strip = scan_tar(archive_path)?;
    let mut archive = open_tar(archive_path)?;
    let mut budget = Budget::default();
    for entry in archive.entries().context("failed to iterate tar entries")? {
        let mut entry = entry.context("failed to read tar entry")?;
        budget.take_entry()?;
        let ty = entry.header().entry_type();
        if ty.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path().context("invalid tar entry path")?.into_owned();
        check_entry_type(ty, &path)?;
        let mut comps = normal_components(&path)?;
        if strip.is_some() {
            if comps.len() <= 1 && ty.is_dir() {
                continue; // the wrapper directory itself
            }
            comps.remove(0);
        }
        if comps.is_empty() {
            continue;
        }
        if ty.is_dir() {
            ensure_dirs(dest.as_std_path(), &comps)?;
        } else {
            write_file(dest.as_std_path(), &comps, &mut entry, &mut budget)?;
        }
    }
    Ok(())
}

/// Unpack a ZIP archive into `dest`.
fn unpack_zip(archive_path: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("failed to open ZIP archive: {archive_path}"))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive: {archive_path}"))?;
    anyhow::ensure!(
        zip.len() <= MAX_ENTRIES,
        "archive has more than {MAX_ENTRIES} entries"
    );
    let mut budget = Budget::default();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .with_context(|| format!("failed to access ZIP entry {i}"))?;
        budget.take_entry()?;
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            anyhow::bail!("symlink in zip archive (entry {i})");
        }
        let name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("ZIP entry has unsafe path"))?;
        let comps = normal_components(&name)?;
        if comps.is_empty() {
            continue;
        }
        if entry.is_dir() {
            ensure_dirs(dest.as_std_path(), &comps)?;
        } else {
            write_file(dest.as_std_path(), &comps, &mut entry, &mut budget)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "safe_unpack_tests.rs"]
mod tests;
