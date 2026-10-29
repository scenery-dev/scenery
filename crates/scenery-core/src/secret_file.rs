//! Owner-only files for secrets that must not live in `state.db`.
//!
//! `state.db` gets copied, backed up and attached to bug reports, and an
//! earlier build left it world-readable, so it holds no secrets any more.
//! Auth tokens go to the OS keychain where one is usable and to this store
//! otherwise; the H3 local MCP key always comes here, because the daemon
//! checks it on every request and the same value already sits in the
//! client's config file.
//!
//! Each secret is one file in `<root>/secrets/`. The directory is forced to
//! 0700 on every write and each file is created 0600 through
//! [`write_atomic`], so the content is never readable by anyone else, even
//! for a moment. One file per secret means the daemon and the CLI never
//! read-modify-write a shared file.

use crate::atomic_file::write_atomic;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Name of the secrets directory under the data root.
pub const SECRETS_DIR: &str = "secrets";

/// The secrets directory for a data root.
pub fn secrets_dir(root: &Path) -> PathBuf {
    root.join(SECRETS_DIR)
}

/// Read secret `name`, or `None` when it has never been written.
pub fn read(root: &Path, name: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(secrets_dir(root).join(name)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write secret `name`, replacing any previous value atomically.
pub fn write(root: &Path, name: &str, value: &str) -> io::Result<()> {
    let dir = secrets_dir(root);
    ensure_private_dir(&dir)?;
    write_atomic(&dir.join(name), value.as_bytes(), Some(0o600))
}

/// Delete secret `name`. Deleting a secret that is not there is not an error.
pub fn remove(root: &Path, name: &str) -> io::Result<()> {
    match fs::remove_file(secrets_dir(root).join(name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Also tightens a directory created earlier, or under a loose umask.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "vh-secret-file-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn write_read_remove_roundtrip() {
        let root = temp_root("roundtrip");
        assert_eq!(read(&root, "k").unwrap(), None);
        write(&root, "k", "v1").unwrap();
        write(&root, "k", "v2").unwrap();
        assert_eq!(read(&root, "k").unwrap().as_deref(), Some("v2"));
        remove(&root, "k").unwrap();
        remove(&root, "k").unwrap();
        assert_eq!(read(&root, "k").unwrap(), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn dir_is_0700_and_file_is_0600_even_when_the_dir_was_wider() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("modes");
        fs::create_dir_all(secrets_dir(&root)).unwrap();
        fs::set_permissions(secrets_dir(&root), fs::Permissions::from_mode(0o755)).unwrap();

        write(&root, "k", "v").unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&secrets_dir(&root)), 0o700);
        assert_eq!(mode(&secrets_dir(&root).join("k")), 0o600);
        let _ = fs::remove_dir_all(&root);
    }
}
