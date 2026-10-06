//! Platform process inspection for daemon-takeover verification.
//!
//! Everything here answers questions about an *arbitrary* pid without
//! shelling out to `ps`/`lsof` and without signaling anything beyond the
//! liveness/permission probe `kill(pid, 0)` already requires. See
//! `daemon_takeover`'s module docs for how these answers feed the decision
//! of whether another `vectorhawk daemon run` process is safe to take over.
//!
//! # Why not `/proc` uniformly
//!
//! Linux has `/proc/<pid>/cmdline` and `/proc/<pid>/exe`, which this reuses
//! directly. macOS has no `/proc`; the equivalents are:
//! - argv: `sysctl(CTL_KERN, KERN_PROCARGS2, pid)` — documented, stable,
//!   used by `ps`/`top`/every open-source reimplementation of them.
//! - running executable path: `proc_pidpath` (libproc, part of libSystem —
//!   always linked, no extra build config).
//! - enumerate all pids: `proc_listpids` (same family).
//!
//! Both platforms produce argv as a single NUL-separated buffer so the one
//! positional matcher (`daemon_takeover::cmdline_is_daemon_run`) works
//! identically regardless of which platform produced the bytes.

use std::io;

/// What we can learn about an arbitrary pid, and the one way we're allowed
/// to act on it (signal it) — injected so tests never touch a real process
/// they didn't spawn themselves. See this module's and `daemon_takeover`'s
/// docs for the safety rule this exists to uphold.
pub trait ProcessInspector {
    /// `argv` for `pid`, NUL-separated exactly like Linux's
    /// `/proc/<pid>/cmdline` (`argv[0]` NUL `argv[1]` NUL ...), regardless
    /// of which platform produced it. `None` when unreadable (process gone,
    /// permission denied, platform call failed).
    fn argv(&self, pid: u32) -> Option<Vec<u8>>;

    /// `Some(true)`: `pid` is alive and we have permission to signal it —
    /// i.e. it shares our effective uid (or we are root, which this
    /// process never runs as). `Some(false)`: `pid` is alive but signaling
    /// it was denied — a different uid. `None`: `pid` does not exist, or
    /// its state could not be determined; never treat this as a match.
    fn same_uid_alive(&self, pid: u32) -> Option<bool>;

    /// Has the on-disk executable backing `pid` been replaced or deleted
    /// since that process started running it? `None` when undeterminable
    /// (be conservative — never treat `None` as "yes, stale").
    fn exe_replaced_or_deleted(&self, pid: u32) -> Option<bool>;

    /// All pids currently visible to us, best effort. Used only to locate
    /// the holder of an advisory lock (`flock`) when no live socket
    /// connection exists to read peer credentials from instead — see
    /// `daemon_takeover::resolve_unique_daemon_run_pid`.
    fn all_pids(&self) -> Vec<u32>;
}

/// The only action a verified takeover is allowed to take: signal a pid.
/// Separate from [`ProcessInspector`] so a caller that only needs to look
/// (e.g. logging, dry runs) never holds something that can kill.
pub trait ProcessKiller {
    fn terminate(&self, pid: u32) -> io::Result<()>;
    fn kill(&self, pid: u32) -> io::Result<()>;
}

/// Real, OS-backed implementation. Stateless — safe to share behind a
/// single `&'static` or stack value.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessOps;

impl ProcessKiller for SystemProcessOps {
    fn terminate(&self, pid: u32) -> io::Result<()> {
        send_signal(pid, libc::SIGTERM)
    }

    fn kill(&self, pid: u32) -> io::Result<()> {
        send_signal(pid, libc::SIGKILL)
    }
}

fn send_signal(pid: u32, sig: libc::c_int) -> io::Result<()> {
    // SAFETY: `kill` with a valid pid and signal number is always safe to
    // call; it either succeeds or sets `errno`, never undefined behaviour.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl ProcessInspector for SystemProcessOps {
    fn argv(&self, pid: u32) -> Option<Vec<u8>> {
        platform::argv(pid)
    }

    fn same_uid_alive(&self, pid: u32) -> Option<bool> {
        same_uid_alive(pid)
    }

    fn exe_replaced_or_deleted(&self, pid: u32) -> Option<bool> {
        platform::exe_replaced_or_deleted(pid)
    }

    fn all_pids(&self) -> Vec<u32> {
        platform::all_pids()
    }
}

/// `kill(pid, 0)` is the portable way to ask "does this pid exist, and do
/// I have permission to signal it" without actually signaling anything —
/// it performs the permission/existence checks the kernel always does
/// before delivering a real signal, then stops. A deliberate gate on this
/// crate's own permission to act, never a directory of other users' uids.
///
/// This process never runs as root, so success here is a reliable proxy
/// for "same effective uid as us."
fn same_uid_alive(pid: u32) -> Option<bool> {
    // SAFETY: signal 0 sends nothing; `kill` only performs its permission
    // and existence checks and returns.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return Some(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => Some(false),
        _ => None, // ESRCH (gone) or anything else: undetermined, be conservative
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;

    pub(super) fn argv(pid: u32) -> Option<Vec<u8>> {
        fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .filter(|b| !b.is_empty())
    }

    /// Mirrors `crate::install`-equivalent logic (the CLI's `exe_is_stale`),
    /// applied to an arbitrary pid instead of only the systemd `MainPID`.
    pub(super) fn exe_replaced_or_deleted(pid: u32) -> Option<bool> {
        let link = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let exe_target = link.to_str()?;
        if exe_target.ends_with(" (deleted)") {
            return Some(true);
        }
        let current = crate::binary_watch::resolve_watch_path().ok()?;
        let canonical_current = fs::canonicalize(&current).ok()?;
        Some(canonical_current.to_str() != Some(exe_target))
    }

    pub(super) fn all_pids() -> Vec<u32> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .collect()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;

    /// Generous but bounded: macOS's own `ARG_MAX` is on this order; a
    /// `KERN_PROCARGS2` buffer is never larger than that. Guards against an
    /// unbounded allocation on a corrupt/unexpected sysctl result.
    const MAX_PROCARGS_BUFFER: usize = 1 << 20; // 1 MiB

    pub(super) fn argv(pid: u32) -> Option<Vec<u8>> {
        let mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut needed: libc::size_t = 0;
        // SAFETY: `mib` is a valid 3-element sysctl name for KERN_PROCARGS2;
        // passing a null oldp with a valid oldlenp just asks for the
        // required buffer size, per sysctl(3)'s documented two-call usage.
        let rc = unsafe {
            libc::sysctl(
                mib.as_ptr() as *mut libc::c_int,
                mib.len() as u32,
                std::ptr::null_mut(),
                &mut needed,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || needed == 0 || needed > MAX_PROCARGS_BUFFER {
            return None;
        }

        let mut buf = vec![0u8; needed];
        let mut actual = needed;
        // SAFETY: `buf` is sized to `needed` bytes as reported by the
        // sizing call above; `actual` is updated in place by the kernel to
        // the number of bytes actually written, which is always <= `needed`.
        let rc = unsafe {
            libc::sysctl(
                mib.as_ptr() as *mut libc::c_int,
                mib.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut actual,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || actual < 4 {
            return None;
        }
        buf.truncate(actual);
        parse_kern_procargs2(&buf)
    }

    /// Parse a `KERN_PROCARGS2` buffer into a NUL-separated `argv[0..argc]`
    /// buffer matching Linux's `/proc/<pid>/cmdline` shape.
    ///
    /// Layout (Apple's documented, widely-reimplemented shape):
    /// `argc: i32` native-endian, then the exec path (NUL-terminated), then
    /// zero or more NUL padding bytes, then `argc` NUL-terminated strings
    /// (argv), then the environment (ignored here).
    pub(super) fn parse_kern_procargs2(buf: &[u8]) -> Option<Vec<u8>> {
        if buf.len() < 4 {
            return None;
        }
        let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?);
        if argc < 0 {
            return None;
        }
        let argc = argc as usize;

        let mut pos = 4usize;
        // Skip the exec path (NUL-terminated).
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        // Skip the NUL padding that follows it, up to the first argv byte.
        while pos < buf.len() && buf[pos] == 0 {
            pos += 1;
        }

        let mut out = Vec::new();
        for i in 0..argc {
            if pos >= buf.len() {
                // Truncated/unexpected buffer — return whatever we parsed
                // rather than fabricate missing args. An incomplete argv
                // still safely fails the positional matcher.
                break;
            }
            let start = pos;
            while pos < buf.len() && buf[pos] != 0 {
                pos += 1;
            }
            out.extend_from_slice(&buf[start..pos]);
            if i + 1 < argc {
                out.push(0);
            }
            pos += 1; // skip the NUL terminator
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub(super) fn exe_replaced_or_deleted(pid: u32) -> Option<bool> {
        let running_path = proc_pidpath(pid)?;
        if std::fs::metadata(&running_path).is_err() {
            return Some(true); // the file backing the running exe is gone
        }
        let current = crate::binary_watch::resolve_watch_path().ok()?;
        let canonical_current = std::fs::canonicalize(&current).ok()?;
        let canonical_running = std::fs::canonicalize(&running_path).ok()?;
        Some(canonical_current != canonical_running)
    }

    /// `proc_pidpath(3)` — libproc, part of libSystem (always linked on
    /// macOS; no extra build configuration needed).
    fn proc_pidpath(pid: u32) -> Option<std::path::PathBuf> {
        const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
        let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        // SAFETY: `buf` is exactly `PROC_PIDPATHINFO_MAXSIZE` bytes, the
        // documented maximum this call ever writes.
        let len = unsafe {
            proc_pidpath_sys(
                pid as i32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )
        };
        if len <= 0 {
            return None;
        }
        buf.truncate(len as usize);
        String::from_utf8(buf).ok().map(std::path::PathBuf::from)
    }

    pub(super) fn all_pids() -> Vec<u32> {
        // First call sizes the buffer (number of bytes needed for all
        // pids), per `proc_listpids`'s documented two-call usage.
        // SAFETY: a null buffer with buffersize 0 only asks for the
        // required size, per libproc's documented contract.
        let needed = unsafe { proc_listpids_sys(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        // Pad generously: the process count can grow between the sizing
        // call and the real one.
        let capacity = (needed as usize) * 2;
        let mut buf = vec![0i32; capacity / 4 + 1];
        // SAFETY: `buf` has room for at least `capacity` bytes.
        let written = unsafe {
            proc_listpids_sys(
                PROC_ALL_PIDS,
                0,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as i32,
            )
        };
        if written <= 0 {
            return Vec::new();
        }
        let count = (written as usize) / 4;
        buf.truncate(count);
        buf.into_iter()
            .filter(|&p| p > 0)
            .map(|p| p as u32)
            .collect()
    }

    const PROC_ALL_PIDS: u32 = 1;

    extern "C" {
        #[link_name = "proc_pidpath"]
        fn proc_pidpath_sys(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
        #[link_name = "proc_listpids"]
        fn proc_listpids_sys(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: i32)
            -> i32;
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub(super) fn argv(_pid: u32) -> Option<Vec<u8>> {
        None
    }
    pub(super) fn exe_replaced_or_deleted(_pid: u32) -> Option<bool> {
        None
    }
    pub(super) fn all_pids() -> Vec<u32> {
        Vec::new()
    }
}

#[cfg(test)]
#[path = "process_probe_tests.rs"]
mod tests;
