//! Learn who is on the other end of the daemon's Unix socket, and what
//! version they're running.
//!
//! # Peer credentials
//!
//! `SO_PEERCRED` (Linux) and `LOCAL_PEERPID`/`getpeereid` (macOS) are
//! kernel-verified facts about the process on the other end of an
//! already-connected `AF_UNIX` socket — not something the peer can lie
//! about by crafting its argv or environment. This is the most trustworthy
//! signal `daemon_takeover` ever has access to.
//!
//! # Version
//!
//! Every build of this daemon since M0 answers an `initialize` JSON-RPC
//! request over this socket with its own `CARGO_PKG_VERSION` in
//! `serverInfo.version` (see `vectorhawkd-mcp`'s `backend.rs`) — this is
//! the wire protocol's oldest, most stable surface, so even a daemon too
//! old to have `instance_lock` or `binary_watch` answers it correctly.
//! [`query_version`] exploits exactly that.
//!
//! # Why blocking, not tokio
//!
//! This is only ever called once, very early in `run_daemon` — before the
//! tokio accept loop exists or any background task has been spawned — or
//! from the fully synchronous CLI installer, which has no tokio runtime at
//! all. A short, bounded-timeout blocking call costs nothing at either call
//! site and keeps this module usable, unmodified, from both.

use crate::protocol_frame;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Kernel-reported identity of the process on the other end of a connected
/// `AF_UNIX` socket. Either field may be `None` if the platform call
/// failed — never treat `None` as "verified absent."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerCreds {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
}

#[cfg(target_os = "linux")]
pub fn peer_creds(stream: &UnixStream) -> PeerCreds {
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a valid, connected AF_UNIX socket for the duration
    // of this call; `cred`/`len` are sized exactly for `SO_PEERCRED`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        PeerCreds {
            pid: Some(cred.pid as u32),
            uid: Some(cred.uid),
        }
    } else {
        PeerCreds::default()
    }
}

#[cfg(target_os = "macos")]
pub fn peer_creds(stream: &UnixStream) -> PeerCreds {
    let fd = stream.as_raw_fd();

    let mut pid: libc::pid_t = 0;
    let mut pid_len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `fd` is a valid, connected AF_UNIX socket; `pid`/`pid_len`
    // are sized exactly for `LOCAL_PEERPID`.
    let pid_rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut pid_len,
        )
    };

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, connected AF_UNIX socket; `uid`/`gid` are
    // plain out-parameters `getpeereid` writes through.
    let uid_rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };

    PeerCreds {
        pid: (pid_rc == 0).then_some(pid as u32),
        uid: (uid_rc == 0).then_some(uid),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_creds(_stream: &UnixStream) -> PeerCreds {
    PeerCreds::default()
}

/// Send a minimal `initialize` request over an already-connected stream
/// and parse the peer's reported version from `serverInfo.version`.
/// Returns `None` on any I/O error, timeout, malformed response, or
/// unparseable version string — callers must treat `None` as "unknown,"
/// never as a signal either way about staleness.
pub fn query_version(stream: &mut UnixStream, timeout: Duration) -> Option<semver::Version> {
    stream.set_write_timeout(Some(timeout)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "vh-takeover-probe",
        "method": "initialize",
        "params": {},
    });
    let body = serde_json::to_vec(&request).ok()?;
    protocol_frame::write_framed_blocking(stream, &body).ok()?;

    let response = protocol_frame::read_framed_blocking(stream).ok()??;
    let value: serde_json::Value = serde_json::from_slice(&response).ok()?;
    let version_str = value
        .get("result")?
        .get("serverInfo")?
        .get("version")?
        .as_str()?;
    semver::Version::parse(version_str).ok()
}

/// Connect to `socket_path`, read peer credentials, and query its version
/// — the one-shot combined probe `daemon_takeover`'s callers actually use.
/// Every field of the result may independently be unknown; a connect
/// failure (nothing listening) yields all-`None`, which callers correctly
/// treat as "nothing to take over" rather than "ambiguous."
pub fn connect_and_probe(
    socket_path: &camino::Utf8Path,
    timeout: Duration,
) -> (PeerCreds, Option<semver::Version>) {
    let Ok(mut stream) = UnixStream::connect(socket_path.as_std_path()) else {
        return (PeerCreds::default(), None);
    };
    let creds = peer_creds(&stream);
    let version = query_version(&mut stream, timeout);
    (creds, version)
}

#[cfg(test)]
#[path = "peer_handshake_tests.rs"]
mod tests;
