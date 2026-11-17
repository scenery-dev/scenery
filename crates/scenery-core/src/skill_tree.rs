//! Whole-tree survey of a skill directory: one walk that yields the tree hash,
//! the size/shape numbers the auto-manage classifier needs, the newest mtime
//! (for the "still being edited" deferral), every symlink, and everything the
//! upload would drop. Also the secret-pattern scan run before any upload.
//!
//! Rules shared with [`crate::pack`] and [`crate::safe_copy`]: walk with
//! `symlink_metadata`, never follow a link, skip FIFOs/sockets/devices, and
//! stop at hard caps so a hostile tree cannot make the daemon hang.
//!
//! # The tree hash
//!
//! SHA-256 over the sorted entries: `(relative path, content hash, exec bit)`
//! for files, `(relative path, target text)` for symlinks (never followed),
//! and the relative path for directories. Excluded from the hash, so they can
//! come and go without changing it:
//!
//! - editor swap and OS noise (`*.swp`, `*~`, `.#*`, `#*#`, `.DS_Store`),
//! - `__pycache__` and `*.pyc`,
//! - the Scenery marker `.scenery-managed.json`,
//! - everything the upload drops (`.git` and other dot-directories,
//!   `node_modules`, secret-looking files). Those are reported in
//!   [`TreeSurvey::dropped`] instead, because converting a skill that holds
//!   them would lose them from the live directory.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::Path,
    time::{Duration, SystemTime},
};

/// Conversion caps (plan 4.1): over any of these a skill is tracked in place
/// instead of converted, so it stays governed without being rewritten.
pub const CONVERT_MAX_BYTES: u64 = 5 * 1024 * 1024;
pub const CONVERT_MAX_FILES: usize = 200;
pub const CONVERT_MAX_DEPTH: usize = 8;

/// Hard walking caps (a survey above these is an error, not a classification).
const HARD_MAX_FILES: usize = 10_000;
const HARD_MAX_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_DEPTH: usize = 32;

/// The marker file Scenery writes into a managed directory.
const MARKER: &str = ".scenery-managed.json";

/// Everything one walk learned about a skill tree.
#[derive(Debug, Clone, Default)]
pub struct TreeSurvey {
    /// See the module docs.
    pub tree_hash: String,
    /// Regular files and symlinks that were hashed.
    pub files: usize,
    /// Total size of the hashed regular files.
    pub bytes: u64,
    /// Deepest directory nesting (the skill root is depth 0).
    pub depth: usize,
    /// Newest mtime of any entry, editor swap files included (an open swap
    /// file means somebody is editing). `.git` and other dropped trees are
    /// not looked into.
    pub newest_mtime: Option<SystemTime>,
    /// Relative paths of every symlink in the tree.
    pub symlinks: Vec<String>,
    /// Relative paths of entries the upload drops and the hash ignores,
    /// excluding pure noise. A converted copy would lose these.
    pub dropped: Vec<String>,
    /// The subset of `dropped` that are secret-looking files (`.env*`,
    /// `*.pem`, `*.key`, SSH keys). Names only, never contents.
    pub secret_files: Vec<String>,
}

impl TreeSurvey {
    /// Over the conversion caps (bytes, files or depth)?
    pub fn exceeds_convert_caps(&self) -> bool {
        self.bytes > CONVERT_MAX_BYTES
            || self.files > CONVERT_MAX_FILES
            || self.depth > CONVERT_MAX_DEPTH
    }

    /// Was anything in the tree modified within `window` of `now`?
    pub fn modified_within(&self, now: SystemTime, window: Duration) -> bool {
        match self.newest_mtime {
            Some(m) => now.duration_since(m).map(|d| d < window).unwrap_or(true),
            None => false,
        }
    }
}

/// Editor swap files, OS litter and compiled-Python caches: never part of a
/// skill's content, never hashed, never worth reporting.
pub fn is_noise_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == ".ds_store"
        || n == "__pycache__"
        || n == MARKER
        || n.ends_with(".pyc")
        || n.ends_with(".swp")
        || n.ends_with(".swo")
        || n.ends_with(".swn")
        || name.ends_with('~')
        || name.starts_with(".#")
        || (name.len() > 2 && name.starts_with('#') && name.ends_with('#'))
}

/// Walk `dir` once. See the module docs for what is hashed and what is not.
pub fn survey(dir: &Path) -> Result<TreeSurvey> {
    let mut out = TreeSurvey::default();
    let mut entries: Vec<String> = Vec::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    walk(
        dir,
        Path::new(""),
        0,
        &mut out,
        &mut entries,
        &mut files,
        &mut bytes,
    )?;
    entries.sort();
    let mut hasher = Sha256::new();
    for e in &entries {
        hasher.update(e.as_bytes());
        hasher.update(b"\n");
    }
    out.tree_hash = hex::encode(hasher.finalize());
    out.symlinks.sort();
    out.dropped.sort();
    out.secret_files.sort();
    Ok(out)
}

fn track_mtime(out: &mut TreeSurvey, meta: &fs::Metadata) {
    if let Ok(m) = meta.modified() {
        if out.newest_mtime.map_or(true, |cur| m > cur) {
            out.newest_mtime = Some(m);
        }
    }
}

fn is_secret_file_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with(".env") || n.ends_with(".pem") || n.ends_with(".key") {
        return true;
    }
    const SSH_KEYS: [&str; 5] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "id_xmss"];
    SSH_KEYS.iter().any(|k| n.starts_with(k)) && !n.ends_with(".pub")
}

fn exec_bit(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o100 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    rel: &Path,
    depth: usize,
    out: &mut TreeSurvey,
    entries: &mut Vec<String>,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    anyhow::ensure!(
        depth <= HARD_MAX_DEPTH,
        "survey: {} nests deeper than {HARD_MAX_DEPTH}",
        root.display()
    );
    out.depth = out.depth.max(depth);
    let dir = root.join(rel);
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        let name_str = name.to_string_lossy().to_string();
        let child_rel = rel.join(&name);
        let rel_str = child_rel.to_string_lossy().to_string();
        let abs = root.join(&child_rel);
        let meta = fs::symlink_metadata(&abs).with_context(|| format!("stat {}", abs.display()))?;
        let ft = meta.file_type();

        // Noise: ignored entirely, except that an open swap file still says
        // "someone is editing this".
        if is_noise_name(&name_str) {
            if name_str != MARKER {
                track_mtime(out, &meta);
            }
            continue;
        }

        if ft.is_dir() {
            // `.git`, other dot-directories and `node_modules` are dropped
            // from uploads: not hashed, not walked, reported.
            if name_str.starts_with('.') || name_str == "node_modules" {
                track_mtime(out, &meta);
                out.dropped.push(rel_str);
                continue;
            }
            track_mtime(out, &meta);
            entries.push(format!("d\0{rel_str}"));
            walk(root, &child_rel, depth + 1, out, entries, files, bytes)?;
        } else if ft.is_symlink() {
            track_mtime(out, &meta);
            *files += 1;
            out.files += 1;
            let target = fs::read_link(&abs)
                .with_context(|| format!("read link {}", abs.display()))?
                .to_string_lossy()
                .to_string();
            out.symlinks.push(rel_str.clone());
            entries.push(format!("l\0{rel_str}\0{target}"));
        } else if ft.is_file() {
            track_mtime(out, &meta);
            if is_secret_file_name(&name_str) {
                out.dropped.push(rel_str.clone());
                out.secret_files.push(rel_str);
                continue;
            }
            *files += 1;
            *bytes = bytes.saturating_add(meta.len());
            anyhow::ensure!(
                *files <= HARD_MAX_FILES && *bytes <= HARD_MAX_BYTES,
                "survey: {} exceeds the size cap",
                root.display()
            );
            out.files += 1;
            out.bytes = *bytes;
            let h = sha256_file(&abs)?;
            entries.push(format!("f\0{rel_str}\0{}\0{h}", u8::from(exec_bit(&meta))));
        }
        // FIFOs, sockets and devices are ignored, as everywhere else.
    }
    Ok(())
}

// ── Secret patterns ─────────────────────────────────────────────────────────

/// Largest file the secret scan reads. Bigger files are skipped (a skill is
/// text; a multi-megabyte blob is data and is covered by the size caps).
const SECRET_SCAN_MAX_FILE: u64 = 1024 * 1024;

/// Does `token` (a maximal run of `[A-Za-z0-9_.\-]`) look like a credential?
/// Shape-based: known issuer prefixes with a plausible tail, never a
/// denylist of values. Shared with the MCP argument redactor.
pub fn looks_like_secret_token(token: &str) -> bool {
    let tail_ok = |t: &str, min: usize| {
        t.len() >= min
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    // GitHub: ghp_ gho_ ghu_ ghs_ ghr_ + 36+; fine-grained github_pat_.
    for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if let Some(t) = token.strip_prefix(p) {
            if tail_ok(t, 30) {
                return true;
            }
        }
    }
    if let Some(t) = token.strip_prefix("github_pat_") {
        if tail_ok(t, 20) {
            return true;
        }
    }
    // Anthropic / OpenAI / Stripe / GitLab / npm / Slack / Google / AWS.
    if let Some(t) = token.strip_prefix("sk-ant-") {
        if tail_ok(t, 16) {
            return true;
        }
    }
    if let Some(t) = token.strip_prefix("sk-") {
        if tail_ok(t, 32) {
            return true;
        }
    }
    for p in ["sk_live_", "rk_live_", "sk_test_", "pk_live_"] {
        if let Some(t) = token.strip_prefix(p) {
            if tail_ok(t, 16) {
                return true;
            }
        }
    }
    if let Some(t) = token.strip_prefix("glpat-") {
        if tail_ok(t, 18) {
            return true;
        }
    }
    if let Some(t) = token.strip_prefix("npm_") {
        if tail_ok(t, 30) {
            return true;
        }
    }
    if token.len() >= 14
        && token.starts_with("xox")
        && matches!(
            token.as_bytes().get(3),
            Some(b'a' | b'b' | b'p' | b'o' | b'r' | b's')
        )
        && token.as_bytes().get(4) == Some(&b'-')
    {
        return true;
    }
    if let Some(t) = token.strip_prefix("AIza") {
        if tail_ok(t, 35) {
            return true;
        }
    }
    for p in ["AKIA", "ASIA"] {
        if let Some(t) = token.strip_prefix(p) {
            if t.len() == 16
                && t.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                return true;
            }
        }
    }
    // JWT: three dot-separated base64url parts, the first two starting eyJ.
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts[1].starts_with("eyJ")
        && parts.iter().all(|p| p.len() >= 10)
    {
        return true;
    }
    false
}

/// Does this text hold a private-key header or a credential-shaped token?
pub fn text_has_secret(text: &str) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    let mut start = None;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let tok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.';
        match (tok, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if looks_like_secret_token(&text[s..i]) {
                    return true;
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        if looks_like_secret_token(&text[s..]) {
            return true;
        }
    }
    false
}

/// Relative paths (names only, never contents) of regular text files under
/// `dir` that hold a secret pattern. Walks like [`survey`]: no links
/// followed, dropped directories not entered, special files skipped.
pub fn scan_secrets(dir: &Path) -> Result<Vec<String>> {
    let mut hits = Vec::new();
    scan_dir(dir, Path::new(""), 0, &mut hits)?;
    hits.sort();
    Ok(hits)
}

fn scan_dir(root: &Path, rel: &Path, depth: usize, hits: &mut Vec<String>) -> Result<()> {
    if depth > HARD_MAX_DEPTH {
        return Ok(());
    }
    let dir = root.join(rel);
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        let name_str = name.to_string_lossy().to_string();
        if is_noise_name(&name_str) {
            continue;
        }
        let child_rel = rel.join(&name);
        let abs = root.join(&child_rel);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            if name_str.starts_with('.') || name_str == "node_modules" {
                continue;
            }
            scan_dir(root, &child_rel, depth + 1, hits)?;
        } else if ft.is_file() && meta.len() <= SECRET_SCAN_MAX_FILE {
            let Ok(data) = fs::read(&abs) else { continue };
            if data.contains(&0) {
                continue; // binary
            }
            if let Ok(text) = std::str::from_utf8(&data) {
                if text_has_secret(text) {
                    hits.push(child_rel.to_string_lossy().to_string());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn skill(dir: &Path) {
        fs::create_dir_all(dir.join("references")).unwrap();
        fs::write(dir.join("SKILL.md"), "---\nname: demo\n---\nbody\n").unwrap();
        fs::write(dir.join("references/x.md"), "ref").unwrap();
    }

    #[test]
    fn hash_covers_the_whole_tree_not_just_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        let a = survey(tmp.path()).unwrap().tree_hash;
        fs::write(tmp.path().join("references/x.md"), "changed").unwrap();
        let b = survey(tmp.path()).unwrap().tree_hash;
        assert_ne!(a, b, "a reference-file edit changes the hash");
        fs::write(tmp.path().join("added.md"), "new").unwrap();
        let c = survey(tmp.path()).unwrap().tree_hash;
        assert_ne!(b, c, "an added file changes the hash");
        fs::remove_file(tmp.path().join("added.md")).unwrap();
        assert_eq!(b, survey(tmp.path()).unwrap().tree_hash);
    }

    #[test]
    fn hash_ignores_swap_files_pycache_and_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        let base = survey(tmp.path()).unwrap().tree_hash;
        fs::write(tmp.path().join(".SKILL.md.swp"), "vim").unwrap();
        fs::write(tmp.path().join("notes.md~"), "emacs").unwrap();
        fs::write(tmp.path().join(".#notes.md"), "emacs lock").unwrap();
        fs::write(tmp.path().join(".DS_Store"), "mac").unwrap();
        fs::create_dir_all(tmp.path().join("__pycache__")).unwrap();
        fs::write(tmp.path().join("__pycache__/a.pyc"), "x").unwrap();
        fs::write(tmp.path().join(MARKER), "{}").unwrap();
        let s = survey(tmp.path()).unwrap();
        assert_eq!(s.tree_hash, base);
        assert!(
            s.dropped.is_empty(),
            "noise is not reported: {:?}",
            s.dropped
        );
    }

    #[test]
    fn hash_sees_the_exec_bit() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            skill(tmp.path());
            fs::write(tmp.path().join("run.sh"), "#!/bin/sh").unwrap();
            let a = survey(tmp.path()).unwrap().tree_hash;
            fs::set_permissions(tmp.path().join("run.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
            assert_ne!(a, survey(tmp.path()).unwrap().tree_hash);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_hashed_by_target_and_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        std::os::unix::fs::symlink("/etc", tmp.path().join("etc-link")).unwrap();
        std::os::unix::fs::symlink(".", tmp.path().join("references/loop")).unwrap();
        let s = survey(tmp.path()).unwrap();
        assert_eq!(s.symlinks, vec!["etc-link", "references/loop"]);
        std::fs::remove_file(tmp.path().join("etc-link")).unwrap();
        std::os::unix::fs::symlink("/usr", tmp.path().join("etc-link")).unwrap();
        assert_ne!(s.tree_hash, survey(tmp.path()).unwrap().tree_hash);
    }

    #[test]
    fn dropped_content_and_secret_files_are_reported_not_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        let base = survey(tmp.path()).unwrap().tree_hash;
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        fs::create_dir_all(tmp.path().join("node_modules/x")).unwrap();
        fs::create_dir_all(tmp.path().join(".hidden")).unwrap();
        fs::write(tmp.path().join(".env"), "A=1").unwrap();
        fs::write(tmp.path().join("id_ed25519"), "k").unwrap();
        fs::write(tmp.path().join("id_ed25519.pub"), "pub").unwrap();
        let s = survey(tmp.path()).unwrap();
        assert_eq!(s.secret_files, vec![".env", "id_ed25519"]);
        for want in [".git", "node_modules", ".hidden", ".env", "id_ed25519"] {
            assert!(s.dropped.contains(&want.to_string()), "missing {want}");
        }
        assert_ne!(s.tree_hash, base, "the .pub file is content and is hashed");
    }

    #[test]
    fn caps_and_depth_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        assert!(!survey(tmp.path()).unwrap().exceeds_convert_caps());
        for i in 0..=CONVERT_MAX_FILES {
            fs::write(tmp.path().join(format!("f{i}.md")), "x").unwrap();
        }
        assert!(survey(tmp.path()).unwrap().exceeds_convert_caps());

        let deep = tempfile::tempdir().unwrap();
        skill(deep.path());
        let mut p = deep.path().to_path_buf();
        for i in 0..=CONVERT_MAX_DEPTH {
            p = p.join(format!("d{i}"));
        }
        fs::create_dir_all(&p).unwrap();
        let s = survey(deep.path()).unwrap();
        assert!(s.depth > CONVERT_MAX_DEPTH && s.exceeds_convert_caps());
    }

    #[test]
    fn newest_mtime_includes_swap_files_and_modified_within_works() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        let s = survey(tmp.path()).unwrap();
        let m = s.newest_mtime.unwrap();
        assert!(s.modified_within(m + Duration::from_secs(60), Duration::from_secs(600)));
        assert!(!s.modified_within(m + Duration::from_secs(3600), Duration::from_secs(600)));
    }

    #[test]
    fn secret_scan_finds_key_headers_and_token_shapes_by_name_only() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path());
        fs::write(
            tmp.path().join("a.md"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("b.md"),
            format!("token ghp_{}", "a".repeat(36)),
        )
        .unwrap();
        fs::write(tmp.path().join("c.md"), "AKIAABCDEFGHIJKLMNOP is aws").unwrap();
        fs::write(tmp.path().join("ok.md"), "sk-short and ghp_ and prose").unwrap();
        let hits = scan_secrets(tmp.path()).unwrap();
        assert_eq!(hits, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn secret_token_shapes() {
        assert!(looks_like_secret_token(&format!(
            "sk-ant-{}",
            "x".repeat(20)
        )));
        assert!(looks_like_secret_token("xoxb-1234567890-abcdef"));
        assert!(looks_like_secret_token(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk"
        ));
        assert!(!looks_like_secret_token("npx"));
        assert!(!looks_like_secret_token(
            "@modelcontextprotocol/server-everything"
        ));
        assert!(!looks_like_secret_token("sk-short"));
    }
}
