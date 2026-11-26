//! Stable per-user, per-machine identity used to link a runnerless scan to the
//! runner that later registers on the same machine.
//!
//! The hash is computed only here. The backend validates `^[0-9a-f]{64}$` and
//! never recomputes it, so the exact input text below is canonical.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DOMAIN: &str = "scenery-scan-identity-v1";

/// What the identity hash was built from. Serialized as the backend's
/// `identity_basis` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityBasis {
    Hardware,
    Hostname,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanIdentity {
    pub hash: String,
    pub basis: IdentityBasis,
    pub hostname: String,
    pub os_username: String,
}

/// Pure hash. Hardware id is trimmed and uppercased; the hostname fallback is
/// lowercased. Output is lowercase hex SHA-256.
pub fn hash_identity(
    hardware_id: Option<&str>,
    hostname: &str,
    os_username: &str,
) -> (String, IdentityBasis) {
    let hw = hardware_id.map(str::trim).filter(|s| !s.is_empty());
    let (input, basis) = match hw {
        Some(hw) => (
            format!("{DOMAIN}\0{}\0{os_username}", hw.to_uppercase()),
            IdentityBasis::Hardware,
        ),
        None => (
            format!("{DOMAIN}\0host:{}\0{os_username}", hostname.to_lowercase()),
            IdentityBasis::Hostname,
        ),
    };
    let digest = Sha256::digest(input.as_bytes());
    (hex::encode(digest), basis)
}

/// Extract `IOPlatformUUID` from `ioreg -rd1 -c IOPlatformExpertDevice` output.
pub(crate) fn parse_ioreg(output: &str) -> Option<String> {
    for line in output.lines() {
        if !line.contains("\"IOPlatformUUID\"") {
            continue;
        }
        let value = line.split_once('=')?.1.trim();
        let value = value.trim_matches('"').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn hardware_id() -> Option<String> {
    // `output()` waits for the child, so nothing is left running.
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ioreg(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn hardware_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let id = text.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn hardware_id() -> Option<String> {
    None
}

/// Effective user's login name: `getpwuid(geteuid())`, then `$USER`.
pub fn os_username() -> String {
    #[cfg(unix)]
    {
        if let Some(name) = passwd_name() {
            return name;
        }
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

#[cfg(unix)]
fn passwd_name() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers refer to live locals; buf length is passed; the
    // reentrant variant is thread-safe. The name is copied before return.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .into_owned();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Kernel hostname via `gethostname`, never `$HOSTNAME`, so a shell scan and
/// the launchd daemon hash the same value.
fn syscall_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        // SAFETY: buf is valid for buf.len() bytes.
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    "unknown".to_string()
}

/// Hostname as reported to the backend on register: `$HOSTNAME`, then
/// `gethostname`, then `"unknown"`.
pub fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| {
        #[cfg(unix)]
        {
            let mut buf = vec![0u8; 256];
            // SAFETY: buf is valid for buf.len() bytes.
            if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
        }
        "unknown".to_string()
    })
}

/// Derive this machine's scan identity. May spawn `ioreg` on macOS (waited on).
pub fn derive() -> ScanIdentity {
    let hostname = syscall_hostname();
    let os_username = os_username();
    let (hash, basis) = hash_identity(hardware_id().as_deref(), &hostname, &os_username);
    ScanIdentity {
        hash,
        basis,
        hostname,
        os_username,
    }
}

#[cfg(test)]
#[path = "scan_identity_tests.rs"]
mod tests;
