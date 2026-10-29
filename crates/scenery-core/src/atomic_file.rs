//! Atomic, mode-preserving file replacement.
//!
//! Every rewrite of an AI-client config (`~/.claude.json` above all) goes
//! through [`write_atomic`]. Those files hold other MCP servers' env secrets,
//! Authorization headers and the H3 local key, and Claude Code keeps
//! `~/.claude.json` at 0600. A plain `fs::write` to a temp file followed by a
//! rename would create the temp file at the umask default (often 0644) and
//! carry that wider mode onto the real file, and a chmod after the write
//! would still leave the secrets readable for a moment. Here the temp file is
//! created with its final mode before any content is written.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with `contents` via a temp file in the same directory and
/// a rename, so readers see either the old or the new file, never a partial
/// one.
///
/// The new file gets the permission bits `path` has now. When `path` does
/// not exist yet it gets `new_file_mode`, or the umask default when that is
/// `None`. `path` is replaced as given: a symlink there is replaced, not
/// followed — resolve it first to write through it.
///
/// On failure the temp file is removed and `path` is left untouched. Windows
/// has no mode bits, so there the arguments only steer the write itself.
pub fn write_atomic(path: &Path, contents: &[u8], new_file_mode: Option<u32>) -> io::Result<()> {
    let tmp = temp_path_for(path);
    let result = write_new(&tmp, contents, mode_for(path, new_file_mode)?)
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `.<name>.vh-tmp-<pid>-<nanos>-<seq>` next to `path`. The sequence number
/// keeps two writers in one process (the pusher runs on several threads)
/// from sharing a temp file even within one clock tick.
fn temp_path_for(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".{file_name}.vh-tmp-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(unix)]
fn mode_for(path: &Path, new_file_mode: Option<u32>) -> io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.permissions().mode() & 0o777)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(new_file_mode),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn mode_for(_path: &Path, _new_file_mode: Option<u32>) -> io::Result<Option<u32>> {
    Ok(None)
}

/// Create `tmp` (which must not exist) with `mode` and write `contents`.
#[cfg(unix)]
fn write_new(tmp: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        options.mode(mode);
    }
    let mut file = options.open(tmp)?;
    if let Some(mode) = mode {
        // `mode()` above is filtered by the umask; set the exact bits.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.write_all(contents)
}

#[cfg(not(unix))]
fn write_new(tmp: &Path, contents: &[u8], _mode: Option<u32>) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    file.write_all(contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vh-atomic-file-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn replaces_content_and_leaves_no_temp_file() {
        let dir = temp_dir("basic");
        let path = dir.join("claude.json");
        write_atomic(&path, b"first", None).unwrap();
        write_atomic(&path, b"second, longer", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second, longer");
        assert_eq!(entries(&dir), vec!["claude.json".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_rename_leaves_target_and_no_temp_file() {
        let dir = temp_dir("fail");
        let path = dir.join("claude.json");
        fs::create_dir_all(&path).unwrap();
        assert!(write_atomic(&path, b"new", Some(0o600)).is_err());
        assert!(path.is_dir());
        assert_eq!(entries(&dir), vec!["claude.json".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    mod modes {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn keeps_an_existing_owner_only_mode() {
            let dir = temp_dir("keep600");
            let path = dir.join("claude.json");
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            write_atomic(&path, b"new", None).unwrap();
            assert_eq!(mode(&path), 0o600);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn keeps_an_existing_wider_mode_rather_than_imposing_the_default() {
            let dir = temp_dir("keep664");
            let path = dir.join("settings.json");
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
            write_atomic(&path, b"new", Some(0o600)).unwrap();
            assert_eq!(mode(&path), 0o664);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn new_file_gets_the_requested_mode_regardless_of_umask() {
            let dir = temp_dir("new600");
            let path = dir.join("claude.json");
            write_atomic(&path, b"{}", Some(0o600)).unwrap();
            assert_eq!(mode(&path), 0o600);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn replaces_a_symlink_rather_than_following_it() {
            let dir = temp_dir("symlink");
            let real = dir.join("real.json");
            fs::write(&real, "real").unwrap();
            let link = dir.join("claude.json");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            write_atomic(&link, b"new", Some(0o600)).unwrap();
            assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(fs::read(&real).unwrap(), b"real");
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
