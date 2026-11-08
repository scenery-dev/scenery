//! Path-component validation for backend- and publisher-supplied names (M1, L1).
//!
//! Anything that reaches the filesystem from the sync stream (`skill_id`,
//! `local_slug`, `version`, `plugin_slug`) must be exactly one plain path
//! component. `Path::join` replaces the base for an absolute argument and
//! happily follows `..`, so these helpers are the only sanctioned way to join
//! such a name under a root.

use std::path::{Component, Path, PathBuf};

/// Longest accepted name component, in bytes.
pub const MAX_COMPONENT_LEN: usize = 128;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SafePathError {
    #[error("{what}: must not be empty")]
    Empty { what: String },
    #[error("{what}: longer than {MAX_COMPONENT_LEN} bytes")]
    TooLong { what: String },
    #[error("{what}: contains a character outside [A-Za-z0-9._-]")]
    BadChar { what: String },
    #[error("{what}: must not start with '.'")]
    LeadingDot { what: String },
    #[error("{what}: is not a single plain path component")]
    NotNormal { what: String },
    #[error("{what}: resolves outside the install root")]
    Escapes { what: String },
}

/// Validate one name component: 1-128 bytes of `[A-Za-z0-9._-]`, no leading
/// `.`, and exactly one `Component::Normal` when parsed as a path.
pub fn validate_component(what: &str, s: &str) -> Result<(), SafePathError> {
    let w = || what.to_string();
    if s.is_empty() {
        return Err(SafePathError::Empty { what: w() });
    }
    if s.len() > MAX_COMPONENT_LEN {
        return Err(SafePathError::TooLong { what: w() });
    }
    if s.starts_with('.') {
        return Err(SafePathError::LeadingDot { what: w() });
    }
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-';
        if !ok {
            return Err(SafePathError::BadChar { what: w() });
        }
    }
    let mut comps = Path::new(s).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(SafePathError::NotNormal { what: w() }),
    }
}

/// Permissive sibling of [`validate_component`] for names that originate on
/// this machine (local skill folders adopted from disk): exactly one plain
/// path component, any characters otherwise. Still refuses `.`, `..`,
/// separators, roots and NUL, so it can never aim a join outside its parent.
pub fn validate_single_component(what: &str, s: &str) -> Result<(), SafePathError> {
    if s.is_empty() || s.contains('\0') || s.contains(['/', '\\']) {
        return Err(SafePathError::NotNormal {
            what: what.to_string(),
        });
    }
    let mut comps = Path::new(s).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(c)), None) if c == std::ffi::OsStr::new(s) => Ok(()),
        _ => Err(SafePathError::NotNormal {
            what: what.to_string(),
        }),
    }
}

/// Validate `comp` and join it under `root`, asserting the result is still
/// lexically under `root`.
pub fn safe_join(root: &Path, comp: &str) -> Result<PathBuf, SafePathError> {
    validate_component("path component", comp)?;
    let joined = root.join(comp);
    if !joined.starts_with(root) {
        return Err(SafePathError::Escapes {
            what: comp.to_string(),
        });
    }
    Ok(joined)
}

/// Join a relative multi-component path (plugin file keys, archive members)
/// under `root`. Every component must be `Component::Normal`: no absolute
/// paths, no `..`, no `.`, no prefixes. Names like `a..b` are fine.
pub fn safe_join_rel(root: &Path, rel: &str) -> Result<PathBuf, SafePathError> {
    let w = || rel.to_string();
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return Err(SafePathError::NotNormal { what: w() });
    }
    if rel
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(SafePathError::NotNormal { what: w() });
    }
    let mut out = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(SafePathError::NotNormal { what: w() }),
        }
    }
    if out == root || !out.starts_with(root) {
        return Err(SafePathError::Escapes { what: w() });
    }
    Ok(out)
}

/// Percent-encode a validated or unvalidated name for use as one URL path
/// segment (M1: slug and version in registry URLs).
pub fn encode_segment(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[cfg(test)]
#[path = "safe_path_tests.rs"]
mod tests;
