//! Handle-relative writes for scaffolding a skill bundle (SEC1/F3 follow-up).
//!
//! Checking a path for symlinks and then writing to it leaves a window in
//! which another local user can swap a directory for a symlink. On Unix a
//! [`BundleDir`] closes it: the directory is opened once with `O_NOFOLLOW`,
//! and every later create, open and rename goes through that file descriptor
//! (`mkdirat`, `openat`, `renameat`), so swapping the *path* afterwards cannot
//! redirect a write. Elsewhere it falls back to path checks.

use anyhow::Result;
use camino::Utf8Path;

#[cfg(unix)]
pub use unix::BundleDir;

#[cfg(not(unix))]
pub use fallback::BundleDir;

/// `name` must be one plain path component.
fn check_component(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        anyhow::bail!("invalid path component {name:?}");
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::Context;
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// An open directory. Operations are relative to the handle.
    pub struct BundleDir {
        fd: OwnedFd,
        label: String,
    }

    fn cstr(s: &str) -> Result<CString> {
        CString::new(s).map_err(|_| anyhow::anyhow!("path contains a NUL byte"))
    }

    fn errno() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    impl BundleDir {
        /// Open `path` as a directory, refusing a symlink in the final
        /// component. A missing directory is created first.
        pub fn open_or_create(path: &Utf8Path) -> Result<Self> {
            let c = cstr(path.as_str())?;
            match std::fs::symlink_metadata(path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // SAFETY: `c` is a valid NUL-terminated path.
                    let rc = unsafe { libc::mkdir(c.as_ptr(), 0o755) };
                    if rc != 0 && errno().kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(errno())
                            .with_context(|| format!("failed to create directory {path}"));
                    }
                }
                Err(e) => return Err(e).with_context(|| format!("failed to inspect {path}")),
            }
            // SAFETY: valid path; the returned fd is owned below.
            let fd = unsafe {
                libc::open(
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(errno()).with_context(|| {
                    format!("refusing to write through {path} (not a real directory)")
                });
            }
            // SAFETY: `fd` was just returned by open and is not used elsewhere.
            Ok(Self {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                label: path.to_string(),
            })
        }

        /// Open (creating if missing) the child directory `name`, refusing a
        /// symlink.
        pub fn subdir(&self, name: &str) -> Result<Self> {
            super::check_component(name)?;
            let c = cstr(name)?;
            // SAFETY: valid dir fd and path.
            let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o755) };
            if rc != 0 && errno().kind() != std::io::ErrorKind::AlreadyExists {
                return Err(errno())
                    .with_context(|| format!("failed to create directory {}/{name}", self.label));
            }
            // SAFETY: valid dir fd and path.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(errno()).with_context(|| {
                    format!(
                        "refusing to write through {}/{name} (not a real directory)",
                        self.label
                    )
                });
            }
            Ok(Self {
                // SAFETY: fresh fd from openat.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                label: format!("{}/{name}", self.label),
            })
        }

        /// Write `name` in this directory: refuse an existing symlink or
        /// directory, write a fresh `O_EXCL` temp file, rename it over the
        /// target (so a hard-linked inode is never written through).
        pub fn write_file(&self, name: &str, content: &[u8]) -> Result<()> {
            super::check_component(name)?;
            let target = cstr(name)?;

            // SAFETY: zeroed stat is a valid out-parameter.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: valid dir fd, path and out pointer.
            let rc = unsafe {
                libc::fstatat(
                    self.fd.as_raw_fd(),
                    target.as_ptr(),
                    &mut st,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc == 0 {
                let kind = st.st_mode & libc::S_IFMT;
                if kind == libc::S_IFLNK {
                    anyhow::bail!("refusing to write through symlink {}/{name}", self.label);
                }
                if kind == libc::S_IFDIR {
                    anyhow::bail!("{}/{name} exists and is a directory", self.label);
                }
            } else if errno().kind() != std::io::ErrorKind::NotFound {
                return Err(errno())
                    .with_context(|| format!("failed to inspect {}/{name}", self.label));
            }

            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp_name = format!(".{name}.scn-import-{}-{nanos}.tmp", std::process::id());
            let tmp = cstr(&tmp_name)?;
            // SAFETY: valid dir fd and path.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    tmp.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(errno())
                    .with_context(|| format!("failed to create {}/{tmp_name}", self.label));
            }
            // SAFETY: fresh fd from openat.
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };

            let result = (|| -> Result<()> {
                f.write_all(content)
                    .with_context(|| format!("failed to write {}/{tmp_name}", self.label))?;
                drop(f);
                // SAFETY: valid dir fd and paths.
                let rc = unsafe {
                    libc::renameat(
                        self.fd.as_raw_fd(),
                        tmp.as_ptr(),
                        self.fd.as_raw_fd(),
                        target.as_ptr(),
                    )
                };
                if rc != 0 {
                    return Err(errno())
                        .with_context(|| format!("failed to write {}/{name}", self.label));
                }
                Ok(())
            })();
            if result.is_err() {
                // SAFETY: valid dir fd and path.
                unsafe { libc::unlinkat(self.fd.as_raw_fd(), tmp.as_ptr(), 0) };
            }
            result
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use super::*;
    use anyhow::Context;
    use camino::Utf8PathBuf;
    use std::fs;

    /// Path-based fallback: checks, then writes (no handle to hold).
    pub struct BundleDir {
        path: Utf8PathBuf,
    }

    impl BundleDir {
        pub fn open_or_create(path: &Utf8Path) -> Result<Self> {
            match fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_symlink() => {
                    anyhow::bail!("refusing to write through symlink {path}")
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => anyhow::bail!("{path} exists and is not a directory"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)
                    .with_context(|| format!("failed to create directory {path}"))?,
                Err(e) => return Err(e).with_context(|| format!("failed to inspect {path}")),
            }
            Ok(Self {
                path: path.to_path_buf(),
            })
        }

        pub fn subdir(&self, name: &str) -> Result<Self> {
            super::check_component(name)?;
            Self::open_or_create(&self.path.join(name))
        }

        pub fn write_file(&self, name: &str, content: &[u8]) -> Result<()> {
            super::check_component(name)?;
            let path = self.path.join(name);
            match fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    anyhow::bail!("refusing to write through symlink {path}")
                }
                Ok(m) if m.is_dir() => anyhow::bail!("{path} exists and is a directory"),
                _ => {}
            }
            let tmp = self
                .path
                .join(format!(".{name}.scn-import-{}.tmp", std::process::id()));
            fs::write(&tmp, content).with_context(|| format!("failed to write {tmp}"))?;
            fs::rename(&tmp, &path).with_context(|| format!("failed to write {path}"))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = camino::Utf8PathBuf::from_path_buf(d.path().canonicalize().unwrap()).unwrap();
        (d, p)
    }

    #[test]
    fn write_goes_through_the_open_handle_even_if_the_path_is_swapped() {
        let (_g, root) = tmp();
        let bundle = root.join("bundle");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();

        let dir = BundleDir::open_or_create(&bundle).unwrap();
        // The race: after the directory was verified, the path is swapped for
        // a symlink to somewhere else.
        fs::rename(&bundle, root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &bundle).unwrap();

        dir.write_file("workflow.yaml", b"x").unwrap();
        let sub = dir.subdir("prompts").unwrap();
        sub.write_file("system.txt", b"y").unwrap();

        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing escaped"
        );
        assert_eq!(fs::read(root.join("moved/workflow.yaml")).unwrap(), b"x");
        assert_eq!(
            fs::read(root.join("moved/prompts/system.txt")).unwrap(),
            b"y"
        );
    }

    #[test]
    fn refuses_symlinked_dir_subdir_and_file() {
        let (_g, root) = tmp();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(BundleDir::open_or_create(&link).is_err());

        let real = root.join("real");
        let dir = BundleDir::open_or_create(&real).unwrap();
        std::os::unix::fs::symlink(&outside, real.join("prompts")).unwrap();
        assert!(dir.subdir("prompts").is_err());
        std::os::unix::fs::symlink(outside.join("victim"), real.join("f")).unwrap();
        assert!(dir.write_file("f", b"z").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn rejects_non_component_names() {
        let (_g, root) = tmp();
        let dir = BundleDir::open_or_create(&root.join("d")).unwrap();
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(dir.write_file(bad, b"x").is_err(), "{bad}");
            assert!(dir.subdir(bad).is_err(), "{bad}");
        }
    }
}
