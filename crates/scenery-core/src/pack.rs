//! Safe packing of a skill directory for upload to the registry.
//!
//! Used by every upload path (daemon adopt/publish, CLI `skill publish`).
//! `tar::Builder::append_dir_all` follows symlinks by default, so a skill
//! containing `creds -> ~/.aws` would upload those files; it also packs
//! `.git`, `.env` and private keys. This module walks the tree itself.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::{debug, warn};

/// A skill directory that must not be uploaded. Carried inside the
/// `anyhow::Error` so callers can recognise it (`downcast_ref`) and log the
/// skill as *held* rather than as a transient failure.
#[derive(Debug)]
pub struct PackRefused {
    pub dir: std::path::PathBuf,
    pub path: std::path::PathBuf,
    pub reason: String,
}

impl std::fmt::Display for PackRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "skill at {} held, not uploaded: {} ({})",
            self.dir.display(),
            self.reason,
            self.path.display()
        )
    }
}

impl std::error::Error for PackRefused {}

/// Upload caps: a skill is a handful of text files, not a project checkout.
const MAX_PACK_FILES: usize = 10_000;
const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PACK_DEPTH: usize = 32;

/// Directories that never belong in an upload: `.git` and every other
/// dot-directory, dependency trees and compiled-Python caches.
fn is_excluded_dir(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules" || name == "__pycache__"
}

/// Files that commonly hold secrets: `.env*`, `*.pem`, `*.key`, and SSH
/// private keys (`id_rsa`, `id_ed25519`, ... but not their `.pub` halves),
/// plus editor swap files and OS litter ([`crate::skill_tree::is_noise_name`]).
fn is_excluded_file(name: &str) -> bool {
    if crate::skill_tree::is_noise_name(name) {
        return true;
    }
    let n = name.to_ascii_lowercase();
    if n.starts_with(".env") || n.ends_with(".pem") || n.ends_with(".key") {
        return true;
    }
    const SSH_KEYS: [&str; 5] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "id_xmss"];
    SSH_KEYS.iter().any(|k| n.starts_with(k)) && !n.ends_with(".pub")
}

/// Lexically resolve `target` (a symlink's text) against `parent_rel` (the
/// link's directory relative to the pack root). `None` if it climbs above the
/// root or is absolute.
fn lexical_target_inside(parent_rel: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = parent_rel.components().count() as i64;
    for c in target.components() {
        match c {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

struct PackState {
    files: usize,
    bytes: u64,
    /// Refuse ANY symlink, not only one that escapes the tree (auto-manage
    /// uploads: a link cannot be shown to be safe once it leaves this machine).
    refuse_links: bool,
}

/// Append `dir`'s tree to `tar` under `./`, safely: links are never followed
/// (this sets `follow_symlinks(false)` on the builder), a link that resolves
/// outside `dir` makes the whole pack fail with [`PackRefused`], likely
/// secrets and VCS/dependency dirs are skipped, and size/depth are capped.
///
/// Shared by the daemon's adopt/publish uploads and the CLI's publish.
pub fn append_skill_tree<W: std::io::Write>(tar: &mut tar::Builder<W>, dir: &Path) -> Result<()> {
    append_skill_tree_with(tar, dir, false)
}

/// [`append_skill_tree`], optionally refusing every symlink (`refuse_links`).
pub fn append_skill_tree_with<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    dir: &Path,
    refuse_links: bool,
) -> Result<()> {
    let root = std::fs::canonicalize(dir)
        .with_context(|| format!("failed to resolve skill directory: {}", dir.display()))?;
    // Never read through a link: a skill containing `creds -> ~/.aws` must not
    // upload those files. Links are archived as links (and only if they stay
    // inside the tree; see `pack_walk`).
    tar.follow_symlinks(false);
    tar.append_dir(".", &root)
        .with_context(|| format!("failed to pack directory: {}", dir.display()))?;
    let mut state = PackState {
        files: 0,
        bytes: 0,
        refuse_links,
    };
    pack_walk(tar, dir, &root, Path::new(""), 0, &mut state)
}

/// Pack the directory at `dir` into an in-memory gzipped tar archive (see
/// [`append_skill_tree`] for the safety rules).
pub fn pack_dir_to_tar_gz(dir: &Path) -> Result<Vec<u8>> {
    pack_dir_to_tar_gz_with(dir, false)
}

/// [`pack_dir_to_tar_gz`] that refuses a tree containing ANY symlink with
/// [`PackRefused`]. Used by the auto-manage upload: a skill with a link in it
/// is held, never uploaded.
pub fn pack_dir_to_tar_gz_no_links(dir: &Path) -> Result<Vec<u8>> {
    pack_dir_to_tar_gz_with(dir, true)
}

fn pack_dir_to_tar_gz_with(dir: &Path, refuse_links: bool) -> Result<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};

    let mut gz_buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(&mut gz_buf, Compression::default());
    let mut tar = tar::Builder::new(enc);
    append_skill_tree_with(&mut tar, dir, refuse_links)?;
    tar.into_inner()
        .context("failed to finalize tar")?
        .finish()
        .context("failed to finalize gzip")?;
    Ok(gz_buf)
}

fn pack_walk<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    top: &Path,
    root: &Path,
    rel: &Path,
    depth: usize,
    state: &mut PackState,
) -> Result<()> {
    let refuse = |path: &Path, reason: String| -> anyhow::Error {
        warn!(
            skill_dir = %top.display(),
            path = %path.display(),
            %reason,
            "publish: HOLDING skill, it will not be uploaded"
        );
        anyhow::Error::new(PackRefused {
            dir: top.to_path_buf(),
            path: path.to_path_buf(),
            reason,
        })
    };
    if depth > MAX_PACK_DEPTH {
        return Err(refuse(
            &root.join(rel),
            format!("directory nesting deeper than {MAX_PACK_DEPTH}"),
        ));
    }
    let abs_dir = root.join(rel);
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&abs_dir)
        .with_context(|| format!("failed to read {}", abs_dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();

    for name in names {
        let name_str = name.to_string_lossy().to_string();
        let child_rel = rel.join(&name);
        let abs = root.join(&child_rel);
        let archive_name = Path::new(".").join(&child_rel);
        let meta = std::fs::symlink_metadata(&abs)
            .with_context(|| format!("failed to stat {}", abs.display()))?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            let target = std::fs::read_link(&abs)
                .with_context(|| format!("failed to read link {}", abs.display()))?;
            if state.refuse_links {
                return Err(refuse(
                    &abs,
                    format!("contains a symbolic link ({})", target.display()),
                ));
            }
            let inside = match std::fs::canonicalize(&abs) {
                Ok(resolved) => resolved.starts_with(root),
                // Dangling or looping: judge the link text alone.
                Err(_) => lexical_target_inside(rel, &target),
            };
            if !inside {
                return Err(refuse(
                    &abs,
                    format!("symlink points outside the skill ({})", target.display()),
                ));
            }
            state.files += 1;
            tar.append_path_with_name(&abs, &archive_name)
                .with_context(|| format!("failed to pack link {}", abs.display()))?;
        } else if ft.is_dir() {
            if is_excluded_dir(&name_str) {
                debug!(path = %abs.display(), "publish: excluding directory from pack");
                continue;
            }
            tar.append_dir(&archive_name, &abs)
                .with_context(|| format!("failed to pack {}", abs.display()))?;
            pack_walk(tar, top, root, &child_rel, depth + 1, state)?;
        } else if ft.is_file() {
            if is_excluded_file(&name_str) {
                debug!(path = %abs.display(), "publish: excluding likely secret from pack");
                continue;
            }
            state.files += 1;
            state.bytes = state.bytes.saturating_add(meta.len());
            if state.files > MAX_PACK_FILES || state.bytes > MAX_PACK_BYTES {
                return Err(refuse(
                    &abs,
                    format!("skill exceeds the upload cap ({MAX_PACK_FILES} files / {MAX_PACK_BYTES} bytes)"),
                ));
            }
            tar.append_path_with_name(&abs, &archive_name)
                .with_context(|| format!("failed to pack {}", abs.display()))?;
        } else {
            warn!(path = %abs.display(), "publish: skipping special file (FIFO/socket/device)");
        }
    }
    Ok(())
}

// ── Reading a skill tree back for installation ──────────────────────────────

/// One file of a skill tree other than its root `SKILL.md`, read for writing
/// into a managed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    /// Path relative to the skill root, `/`-separated.
    pub rel: String,
    pub bytes: Vec<u8>,
    /// The owner-execute bit was set. Skills ship `scripts/` that the AI
    /// client runs; an install that drops the bit breaks them.
    pub executable: bool,
}

/// Read every regular file under `root` except the root `SKILL.md` (written
/// separately) and the root `.scenery-managed.json` marker.
///
/// Same safety rules as the pack: walks with `symlink_metadata` and never
/// follows a link (a symlinked file or directory is skipped, so a
/// `creds -> ~/.aws` cannot be copied in and a directory loop cannot recurse),
/// skips FIFOs/sockets/devices (reading a FIFO blocks forever), and caps file
/// count, total bytes and depth. Best-effort: an unreadable file is skipped.
/// Sorted, so the result is deterministic.
pub fn collect_tree_files(root: &Path) -> Vec<TreeFile> {
    let mut out = Vec::new();
    let mut bytes = 0u64;
    collect_tree_files_inner(root, root, &mut out, 0, &mut bytes);
    out
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
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

fn collect_tree_files_inner(
    root: &Path,
    dir: &Path,
    out: &mut Vec<TreeFile>,
    depth: usize,
    bytes: &mut u64,
) {
    if depth > MAX_PACK_DEPTH {
        warn!(dir = %dir.display(), "skill tree: nesting too deep, not descending");
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = read.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir == root && (name == "SKILL.md" || name == ".scenery-managed.json") {
                continue;
            }
            if out.len() >= MAX_PACK_FILES || bytes.saturating_add(meta.len()) > MAX_PACK_BYTES {
                warn!(dir = %dir.display(), "skill tree: size cap reached, remaining files not read");
                return;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let parts: Option<Vec<&str>> =
                rel.components().map(|c| c.as_os_str().to_str()).collect();
            let Some(parts) = parts else {
                warn!(path = %path.display(), "skill tree: skipping file with a non-UTF-8 name");
                continue;
            };
            if let Ok(data) = std::fs::read(&path) {
                *bytes += data.len() as u64;
                out.push(TreeFile {
                    rel: parts.join("/"),
                    bytes: data,
                    executable: is_executable(&meta),
                });
            }
        } else if ft.is_dir() {
            collect_tree_files_inner(root, &path, out, depth + 1, bytes);
        } else if ft.is_symlink() {
            debug!(path = %path.display(), "skill tree: skipping symlink");
        } else {
            warn!(path = %path.display(), "skill tree: skipping special file");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;

    // ── pack safety ───────────────────────────────────────────────────────────

    fn tar_names(gz: &[u8]) -> Vec<String> {
        let dec = flate2::read::GzDecoder::new(gz);
        let mut ar = tar::Archive::new(dec);
        let mut v: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn pack_includes_a_normal_skill_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("prompts")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("prompts/a.md"), "a").unwrap();
        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        assert!(names.contains(&"SKILL.md".to_string()), "{names:?}");
        assert!(names.contains(&"prompts/a.md".to_string()), "{names:?}");
    }

    #[test]
    fn pack_excludes_secrets_vcs_and_dependency_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::create_dir_all(d.join("node_modules/x")).unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join(".git/config"), "git").unwrap();
        fs::write(d.join("node_modules/x/i.js"), "js").unwrap();
        for f in [
            ".env",
            ".env.local",
            ".envrc",
            "server.pem",
            "tls.KEY",
            "id_rsa",
            "id_ed25519",
            "sub/id_ecdsa",
            "sub/.env.production",
        ] {
            fs::write(d.join(f), "secret").unwrap();
        }
        // Not secrets: must still travel.
        fs::write(d.join("id_rsa.pub"), "pub").unwrap();
        fs::write(d.join("id_helper.py"), "print(1)").unwrap();
        fs::write(d.join("keyboard.md"), "k").unwrap();
        fs::write(d.join("environment.md"), "e").unwrap();

        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        for banned in [
            ".git",
            "node_modules",
            ".env",
            ".envrc",
            "server.pem",
            "tls.KEY",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            ".env.production",
        ] {
            assert!(
                !names
                    .iter()
                    .any(|n| n.contains(banned) && !n.contains("id_rsa.pub")),
                "{banned} must not be packed: {names:?}"
            );
        }
        for kept in [
            "SKILL.md",
            "id_rsa.pub",
            "id_helper.py",
            "keyboard.md",
            "environment.md",
        ] {
            assert!(
                names.contains(&kept.to_string()),
                "{kept} missing: {names:?}"
            );
        }
    }

    /// `creds -> <dir outside the skill>`: the files behind it must not be
    /// uploaded. The skill is refused outright and the error says why.
    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn no_links_pack_refuses_even_a_link_that_stays_inside_the_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("real.md"), "r").unwrap();
        std::os::unix::fs::symlink("real.md", d.join("alias.md")).unwrap();
        // The default pack keeps inside links...
        assert!(pack_dir_to_tar_gz(d).is_ok());
        // ...the auto-manage pack holds the skill.
        let err = pack_dir_to_tar_gz_no_links(d).unwrap_err();
        let held = err.downcast_ref::<PackRefused>().expect("PackRefused");
        assert!(held.reason.contains("symbolic link"), "{}", held.reason);
    }

    #[test]
    fn pack_drops_swap_files_pycache_and_other_dot_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join(".SKILL.md.swp"), "vim").unwrap();
        fs::write(d.join("a.md~"), "emacs").unwrap();
        fs::create_dir_all(d.join("__pycache__")).unwrap();
        fs::write(d.join("__pycache__/a.pyc"), "x").unwrap();
        fs::create_dir_all(d.join(".claude")).unwrap();
        fs::write(d.join(".claude/settings.json"), "{}").unwrap();
        let names = tar_names(&pack_dir_to_tar_gz(d).unwrap());
        assert!(names.iter().any(|n| n.ends_with("SKILL.md")));
        for gone in [".swp", "a.md~", "__pycache__", ".claude"] {
            assert!(
                !names.iter().any(|n| n.contains(gone)),
                "{gone} packed: {names:?}"
            );
        }
    }

    #[test]
    fn pack_refuses_a_symlink_that_points_outside_the_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("aws");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("credentials"), "AKIAFAKE").unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("creds")).unwrap();

        let err = pack_dir_to_tar_gz(&d).unwrap_err();
        let refused = err.downcast_ref::<PackRefused>().expect("PackRefused");
        assert!(refused.reason.contains("outside"), "{}", refused.reason);
        assert!(refused.path.ends_with("creds"));
    }

    #[cfg(unix)]
    #[test]
    fn pack_refuses_relative_and_absolute_escapes_and_file_links() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("outside.txt"), "o").unwrap();
        for (label, target) in [
            ("rel-dir", "../../"),
            ("rel-file", "../../outside.txt"),
            ("abs-file", "/etc/hosts"),
            ("dangling-up", "../../../nowhere"),
        ] {
            let d = tmp.path().join(label);
            fs::create_dir_all(d.join("sub")).unwrap();
            fs::write(d.join("SKILL.md"), "x").unwrap();
            std::os::unix::fs::symlink(target, d.join("sub/link")).unwrap();
            let err = pack_dir_to_tar_gz(&d).unwrap_err();
            assert!(
                err.downcast_ref::<PackRefused>().is_some(),
                "{label}: {err:#}"
            );
        }
    }

    /// A link that stays inside the tree is archived AS A LINK (never as the
    /// content it points at).
    #[cfg(unix)]
    #[test]
    fn pack_keeps_inside_symlinks_as_links_and_never_follows_them() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("real")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("real/data.md"), "data").unwrap();
        std::os::unix::fs::symlink("real", d.join("alias")).unwrap();
        std::os::unix::fs::symlink("real/data.md", d.join("data-link.md")).unwrap();
        // A loop that stays inside the tree must not recurse either.
        std::os::unix::fs::symlink(".", d.join("real/self")).unwrap();

        let gz = pack_dir_to_tar_gz(&d).unwrap();
        let dec = flate2::read::GzDecoder::new(&gz[..]);
        let mut ar = tar::Archive::new(dec);
        let mut links = Vec::new();
        let mut names = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            names.push(e.path().unwrap().to_string_lossy().to_string());
            if e.header().entry_type().is_symlink() {
                links.push(e.path().unwrap().to_string_lossy().to_string());
            }
        }
        links.sort();
        assert_eq!(links, vec!["alias", "data-link.md", "real/self"]);
        assert!(
            !names.iter().any(|n| n.starts_with("alias/")),
            "alias contents must not be duplicated: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pack_skips_fifos_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(d.join("pipe"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return;
        }
        let names = tar_names(&pack_dir_to_tar_gz(&d).unwrap());
        assert!(!names.iter().any(|n| n.contains("pipe")), "{names:?}");
        assert!(names.contains(&"SKILL.md".to_string()));
    }

    /// The shared builder entry point used by the CLI: same exclusions and
    /// refusals, and the caller can still append its own entries afterwards.
    #[test]
    fn append_skill_tree_applies_the_same_rules_for_cli_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join(".env"), "SECRET=1").unwrap();

        let mut tar = tar::Builder::new(Vec::new());
        append_skill_tree(&mut tar, &d).unwrap();
        let raw = tar.into_inner().unwrap();
        let mut ar = tar::Archive::new(&raw[..]);
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"SKILL.md".to_string()));
        assert!(!names.iter().any(|n| n.contains(".env")));
    }

    #[cfg(unix)]
    #[test]
    fn append_skill_tree_refuses_an_escaping_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("aws");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("credentials"), "AKIAFAKE").unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("creds")).unwrap();

        let mut tar = tar::Builder::new(Vec::new());
        let err = append_skill_tree(&mut tar, &d).unwrap_err();
        assert!(err.downcast_ref::<PackRefused>().is_some());
    }

    #[test]
    fn pack_enforces_the_file_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(&d).unwrap();
        for i in 0..(MAX_PACK_FILES + 1) {
            fs::write(d.join(format!("f{i}")), "").unwrap();
        }
        let err = pack_dir_to_tar_gz(&d).unwrap_err();
        assert!(err.downcast_ref::<PackRefused>().is_some());
    }

    // ── collect_tree_files ────────────────────────────────────────────────────

    #[test]
    fn collect_reads_extras_with_modes_and_skips_root_skill_md_and_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("scripts")).unwrap();
        fs::create_dir_all(d.join("references/deep")).unwrap();
        fs::write(d.join("SKILL.md"), "root").unwrap();
        fs::write(d.join(".scenery-managed.json"), "{}").unwrap();
        fs::write(d.join("notes.md"), "notes").unwrap();
        fs::write(d.join("scripts/run.sh"), "#!/bin/sh\n").unwrap();
        // A nested SKILL.md is content, not the skill's manifest.
        fs::write(d.join("references/SKILL.md"), "nested").unwrap();
        fs::write(d.join("references/deep/a.bin"), [0u8, 255, 1, 254]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(d.join("scripts/run.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let files = collect_tree_files(&d);
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "notes.md",
                "references/SKILL.md",
                "references/deep/a.bin",
                "scripts/run.sh"
            ]
        );
        let by = |n: &str| files.iter().find(|f| f.rel == n).unwrap();
        assert_eq!(by("references/deep/a.bin").bytes, vec![0u8, 255, 1, 254]);
        #[cfg(unix)]
        {
            assert!(by("scripts/run.sh").executable);
            assert!(!by("notes.md").executable);
        }
    }

    #[cfg(unix)]
    #[test]
    fn collect_never_follows_links_or_reads_fifos() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("skill");
        fs::create_dir_all(d.join("prompts")).unwrap();
        fs::write(d.join("SKILL.md"), "x").unwrap();
        fs::write(d.join("prompts/p.md"), "p").unwrap();
        let secret = tmp.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        fs::write(secret.join("creds"), "AKIA").unwrap();
        std::os::unix::fs::symlink(&secret, d.join("creds")).unwrap();
        std::os::unix::fs::symlink("prompts/p.md", d.join("alias.md")).unwrap();
        std::os::unix::fs::symlink(".", d.join("prompts/loop")).unwrap();

        let files = collect_tree_files(&d);
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["prompts/p.md"]);
    }
}
