//! Stamp-keyed memo of [`skill_tree::survey`] and [`skill_tree::scan_secrets`]
//! for the daemon's periodic scanners.
//!
//! The discoveries and drift scanners classify every skill tree every 300 s.
//! Re-reading, SHA-256 hashing and secret-scanning megabytes of unchanged
//! files on every tick was the daemon's dominant idle CPU cost (2026-11-21
//! release-gate profile: ~0.85 s per tick, 98% of it in the survey and the
//! secret scan). A tree is now only re-read when its *stamp* changes.
//!
//! # The stamp
//!
//! One metadata-only walk (`symlink_metadata`, no file is opened) over exactly
//! the entries the survey and the secret scan look at, folded into a SHA-256
//! digest of, per entry: relative path, file type, size, mtime (ns), and on
//! Unix ctime (ns), inode, device and mode. Directories the survey and scan
//! never enter (`.git` and other dot-directories, `node_modules`, noise like
//! `__pycache__`) contribute their own metadata but are not walked, mirroring
//! [`skill_tree::survey`].
//!
//! Any content write, rename, chmod, link retarget, add or delete changes a
//! stamped field: the kernel updates ctime on every one of them, and ctime
//! cannot be set from userspace, so even a tool that restores mtime is seen.
//!
//! # Racy-clean guard
//!
//! A filesystem with coarse timestamps (HFS+, FAT: 1-2 s) could let a
//! same-size edit inside the same tick of the clock keep the old stamp. As in
//! git's "racily clean" rule, a result is only memoised when every stamped
//! timestamp is at least [`RACY_WINDOW`] older than the moment the stamp was
//! taken; a tree touched more recently than that is re-read every time until
//! it settles. The stamp is taken *before* the tree is read, so an edit that
//! lands during the read leaves the cached stamp stale and forces a re-read
//! next time.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, SystemTime},
};

use crate::skill_tree::{self, TreeSurvey};

/// See the module docs.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Stamp walking caps. Above these the tree is not stamped (and therefore
/// never memoised); the uncached survey reports the real error.
const STAMP_MAX_ENTRIES: usize = 20_000;
const STAMP_MAX_DEPTH: usize = 32;

/// A cache that outgrows this many directories is simply cleared: entries
/// for skills that were deleted would otherwise accumulate for the daemon's
/// lifetime.
const MAX_DIRS: usize = 1024;

#[derive(Clone)]
struct Entry {
    stamp: [u8; 32],
    survey: Option<TreeSurvey>,
    secrets: Option<Vec<String>>,
}

/// A memo of tree results keyed by directory and stamp. The daemon uses the
/// process-wide [`global`] instance; tests build their own.
pub struct TreeCache {
    racy_window: Duration,
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// How many times a tree was actually read (survey or secret scan run).
    reads: AtomicUsize,
}

/// The daemon's shared cache.
pub fn global() -> &'static TreeCache {
    static CACHE: OnceLock<TreeCache> = OnceLock::new();
    CACHE.get_or_init(|| TreeCache::new(RACY_WINDOW))
}

/// [`skill_tree::survey`], memoised in the [`global`] cache.
pub fn survey_cached(dir: &Path) -> Result<TreeSurvey> {
    global().survey(dir)
}

/// [`skill_tree::scan_secrets`], memoised in the [`global`] cache.
pub fn scan_secrets_cached(dir: &Path) -> Result<Vec<String>> {
    global().scan_secrets(dir)
}

impl TreeCache {
    pub fn new(racy_window: Duration) -> Self {
        Self {
            racy_window,
            entries: Mutex::new(HashMap::new()),
            reads: AtomicUsize::new(0),
        }
    }

    /// Number of real tree reads so far (cache misses).
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// [`skill_tree::survey`], skipped when the tree's stamp is unchanged.
    pub fn survey(&self, dir: &Path) -> Result<TreeSurvey> {
        let stamp = self.settled_stamp(dir);
        if let Some(s) = &stamp {
            if let Some(hit) = self.lookup(dir, s, |e| e.survey.clone()) {
                return Ok(hit);
            }
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let fresh = skill_tree::survey(dir)?;
        if let Some(s) = stamp {
            self.store(dir, s, |e| e.survey = Some(fresh.clone()));
        }
        Ok(fresh)
    }

    /// [`skill_tree::scan_secrets`], skipped when the tree's stamp is unchanged.
    pub fn scan_secrets(&self, dir: &Path) -> Result<Vec<String>> {
        let stamp = self.settled_stamp(dir);
        if let Some(s) = &stamp {
            if let Some(hit) = self.lookup(dir, s, |e| e.secrets.clone()) {
                return Ok(hit);
            }
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let fresh = skill_tree::scan_secrets(dir)?;
        if let Some(s) = stamp {
            self.store(dir, s, |e| e.secrets = Some(fresh.clone()));
        }
        Ok(fresh)
    }

    fn lookup<T, F: Fn(&Entry) -> Option<T>>(
        &self,
        dir: &Path,
        stamp: &[u8; 32],
        pick: F,
    ) -> Option<T> {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.get(dir)?;
        if &entry.stamp != stamp {
            return None;
        }
        pick(entry)
    }

    fn store<F: FnOnce(&mut Entry)>(&self, dir: &Path, stamp: [u8; 32], fill: F) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_DIRS && !map.contains_key(dir) {
            map.clear();
        }
        let entry = map.entry(dir.to_path_buf()).or_insert_with(|| Entry {
            stamp,
            survey: None,
            secrets: None,
        });
        if entry.stamp != stamp {
            // The tree changed: whatever else was memoised is stale.
            *entry = Entry {
                stamp,
                survey: None,
                secrets: None,
            };
        }
        fill(entry);
    }

    /// The tree's stamp, or `None` when it cannot be stamped or was touched
    /// within the racy window (then nothing is memoised).
    fn settled_stamp(&self, dir: &Path) -> Option<[u8; 32]> {
        let now = SystemTime::now();
        let (digest, newest) = stamp(dir)?;
        let settled = match newest {
            Some(t) => now
                .duration_since(t)
                .map(|age| age >= self.racy_window)
                .unwrap_or(false),
            None => true,
        };
        settled.then_some(digest)
    }
}

/// Digest of the tree's metadata plus the newest timestamp in it.
pub(crate) fn stamp(dir: &Path) -> Option<([u8; 32], Option<SystemTime>)> {
    let mut walker = StampWalk {
        hasher: Sha256::new(),
        newest: None,
        count: 0,
    };
    let root_meta = fs::metadata(dir).ok()?;
    walker.add("", &root_meta);
    walker.walk(dir, Path::new(""), 0).ok()?;
    Some((walker.hasher.finalize().into(), walker.newest))
}

struct StampWalk {
    hasher: Sha256,
    newest: Option<SystemTime>,
    count: usize,
}

impl StampWalk {
    fn walk(&mut self, root: &Path, rel: &Path, depth: usize) -> std::io::Result<()> {
        if depth > STAMP_MAX_DEPTH {
            return Err(std::io::Error::other("stamp: too deep"));
        }
        let dir = root.join(rel);
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        for e in fs::read_dir(&dir)? {
            names.push(e?.file_name());
        }
        names.sort();
        for name in names {
            self.count += 1;
            if self.count > STAMP_MAX_ENTRIES {
                return Err(std::io::Error::other("stamp: too many entries"));
            }
            let child_rel = rel.join(&name);
            let meta = fs::symlink_metadata(root.join(&child_rel))?;
            self.add(&child_rel.to_string_lossy(), &meta);
            let name_str = name.to_string_lossy();
            let walked_by_survey = !(skill_tree::is_noise_name(&name_str)
                || name_str.starts_with('.')
                || name_str == "node_modules");
            if meta.file_type().is_dir() && walked_by_survey {
                self.walk(root, &child_rel, depth + 1)?;
            }
        }
        Ok(())
    }

    fn add(&mut self, rel: &str, meta: &fs::Metadata) {
        let ft = meta.file_type();
        let kind: u8 = if ft.is_symlink() {
            b'l'
        } else if ft.is_dir() {
            b'd'
        } else if ft.is_file() {
            b'f'
        } else {
            b'o'
        };
        self.hasher.update(rel.as_bytes());
        self.hasher.update([0, kind]);
        self.hasher.update(meta.len().to_le_bytes());
        let mtime = meta.modified().ok();
        self.hasher.update(nanos(mtime).to_le_bytes());
        self.note(mtime);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ctime_ns = i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec());
            self.hasher.update(ctime_ns.to_le_bytes());
            self.hasher.update(meta.ino().to_le_bytes());
            self.hasher.update(meta.dev().to_le_bytes());
            self.hasher.update(meta.mode().to_le_bytes());
            if let Ok(secs) = u64::try_from(meta.ctime()) {
                let nsec = u32::try_from(meta.ctime_nsec()).unwrap_or(0);
                self.note(Some(
                    SystemTime::UNIX_EPOCH + Duration::new(secs, nsec.min(999_999_999)),
                ));
            }
        }
    }

    fn note(&mut self, t: Option<SystemTime>) {
        if let Some(t) = t {
            if self.newest.map_or(true, |cur| t > cur) {
                self.newest = Some(t);
            }
        }
    }
}

fn nanos(t: Option<SystemTime>) -> i128 {
    match t.map(|t| t.duration_since(SystemTime::UNIX_EPOCH)) {
        Some(Ok(d)) => i128::try_from(d.as_nanos()).unwrap_or(i128::MAX),
        Some(Err(e)) => -i128::try_from(e.duration().as_nanos()).unwrap_or(i128::MAX),
        None => i128::MIN,
    }
}

#[cfg(test)]
#[path = "skill_tree_cache_tests.rs"]
mod tests;
