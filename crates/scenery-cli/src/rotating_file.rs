//! Minimal size-based rotating log writer (replaces the `rolling-file` crate,
//! which dragged in `chrono`).
//!
//! Layout matches what `rolling-file` produced, so existing logs keep
//! working across the upgrade: the active segment is `<path>`, rotated
//! backups are `<path>.1` (newest) through `<path>.N` (oldest, deleted when
//! the next rotation happens).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Appends to `path`, rolling to `path.1` once the active segment would
/// exceed `max_bytes`, keeping at most `max_backups` rotated segments.
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    max_backups: usize,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    /// Open (creating if needed, appending to any existing segment).
    pub fn new(path: PathBuf, max_bytes: u64, max_backups: usize) -> io::Result<Self> {
        let (file, size) = open_append(&path)?;
        Ok(Self {
            path,
            max_bytes,
            max_backups,
            file: Some(file),
            size,
        })
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Close the handle first: Windows cannot rename an open file.
        if let Some(mut f) = self.file.take() {
            f.flush()?;
        }
        if self.max_backups == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            let oldest = self.backup_path(self.max_backups);
            if oldest.exists() {
                std::fs::remove_file(&oldest)?;
            }
            for i in (1..self.max_backups).rev() {
                let from = self.backup_path(i);
                if from.exists() {
                    std::fs::rename(&from, self.backup_path(i + 1))?;
                }
            }
            std::fs::rename(&self.path, self.backup_path(1))?;
        }
        let (file, size) = open_append(&self.path)?;
        self.file = Some(file);
        self.size = size;
        Ok(())
    }

    fn active(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let (file, size) = open_append(&self.path)?;
            self.file = Some(file);
            self.size = size;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file not open"))
    }
}

fn open_append(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.active()?.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "rotating_file_tests.rs"]
mod tests;
