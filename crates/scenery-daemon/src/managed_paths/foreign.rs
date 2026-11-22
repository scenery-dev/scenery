//! Local classifier: what should Scenery do with a skill it found on disk?
//!
//! Three treatments (plan 4.1), decided on the machine before anything is
//! reported, so the backend only ever sees a runner-side verdict:
//!
//! - **Convert**: an ordinary skill. Uploaded privately, then converted in
//!   place (backup first).
//! - **Track**: governed but never rewritten. Skills under active development
//!   (symlinked, inside any git work tree, managed by a dotfiles tool) and
//!   skills owned by another installer that keeps updating them. Uploaded
//!   once privately, no marker written, kill-switchable by neutralizing.
//! - **Hold**: reported but not touched: a marker that claims Scenery
//!   ownership it cannot prove (R4), a secret-looking file or pattern, a slug
//!   that would collide with a different skill already at the canonical path,
//!   or two sources for one slug.
//!
//! The classifier is a pure function of the filesystem plus the canonical
//! root, so it is tested with plain temp directories.

use scenery_core::skill_tree::TreeSurvey;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime},
};
use unicode_normalization::UnicodeNormalization;

/// A skill whose newest file changed this recently is probably still being
/// written; it is deferred one tick.
pub const QUIESCENCE: Duration = Duration::from_secs(10 * 60);

/// Marker files another installer writes into a skill directory it keeps
/// updating. Closed list, deliberately short.
///
/// `.aws-skill-metadata` is confirmed against a real install. The `npx
/// skills` lock file lives beside the skills, not inside one, so it is
/// handled separately in [`npx_skills_lock_lists`] (UNCONFIRMED against a
/// real install; see the release notes).
pub const FOREIGN_MARKERS: &[&str] = &[".aws-skill-metadata"];

/// What to do with a discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Treatment {
    Convert,
    /// Carries `tracked_reason`.
    Track(String),
    /// Carries `hold_reason`.
    Hold(String),
}

impl Treatment {
    pub fn is_convert(&self) -> bool {
        matches!(self, Treatment::Convert)
    }

    /// The `detail` object the discovery report carries for this treatment.
    /// `Convert` is the absence of a detail ("absent = convert").
    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            Treatment::Convert => None,
            Treatment::Track(reason) => Some(serde_json::json!({
                "treatment": "track",
                "tracked_reason": reason,
            })),
            Treatment::Hold(reason) => Some(serde_json::json!({
                "treatment": "hold",
                "hold_reason": reason,
            })),
        }
    }
}

/// Result of classifying one skill directory.
#[derive(Debug, Clone)]
pub struct Classification {
    pub treatment: Treatment,
    /// Whole-tree hash at classification time (empty if the walk failed).
    pub tree_hash: String,
    /// The newest file in the tree changed within [`QUIESCENCE`]: act on it
    /// next tick.
    pub recently_modified: bool,
}

/// Folded form used to compare slugs: Unicode NFC, then lowercase. APFS and
/// default HFS+ volumes are case-insensitive and normalization-insensitive,
/// so `Pdf`, `pdf` and the NFD spelling of an accented name are one folder.
pub fn fold_slug(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// Everything the classifier reads besides the skill directory itself.
#[derive(Debug, Clone, Default)]
pub struct ClassifyContext {
    /// `~/.agents/skills`: the canonical root conversions write into.
    pub canonical_root: Option<PathBuf>,
    /// `~`, for the chezmoi source and the `npx skills` lock.
    pub home: Option<PathBuf>,
    /// Clock, injectable for tests.
    pub now: Option<SystemTime>,
    /// `canonical_root`'s entries by folded name, read once per scan (see
    /// [`ClassifyContext::for_scan`]). `None`: each classification lists the
    /// root itself.
    pub canonical_index: Option<Arc<HashMap<String, Vec<PathBuf>>>>,
}

impl ClassifyContext {
    pub fn from_env() -> Self {
        Self {
            canonical_root: super::paths::agents_skills_dir(),
            home: dirs::home_dir(),
            now: None,
            canonical_index: None,
        }
    }

    /// The context for one scan over many skills: the canonical root is listed
    /// (and every name folded) once here, instead of once per skill, which
    /// made a scan quadratic in the number of skills.
    pub fn for_scan(mut self) -> Self {
        self.canonical_index = self
            .canonical_root
            .as_deref()
            .map(|root| Arc::new(index_root(root)));
        self
    }
}

/// `root`'s non-hidden entries keyed by folded name.
fn index_root(root: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut idx: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let Ok(rd) = fs::read_dir(root) else {
        return idx;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        idx.entry(fold_slug(&name)).or_default().push(e.path());
    }
    idx
}

/// Classify the skill at `dir` (`slug` is its folder name).
pub fn classify_skill(dir: &Path, slug: &str, ctx: &ClassifyContext) -> Classification {
    let now = ctx.now.unwrap_or_else(SystemTime::now);
    // One stamp walk serves both the survey and the secret scan.
    let settled = scenery_core::skill_tree_cache::settle(dir);
    let survey = match scenery_core::skill_tree_cache::survey_at(dir, &settled) {
        Ok(s) => s,
        Err(e) => {
            // A walk that cannot finish (a hard cap, an unreadable file) is
            // too big or too odd to convert: keep it governed in place.
            tracing::warn!(slug, error = %e, "classifier: tree survey failed — tracking in place");
            return Classification {
                treatment: Treatment::Track("over_caps".into()),
                tree_hash: String::new(),
                recently_modified: false,
            };
        }
    };
    let recently_modified = survey.modified_within(now, QUIESCENCE);
    let treatment = decide(dir, slug, ctx, &survey, &settled);
    Classification {
        treatment,
        tree_hash: survey.tree_hash,
        recently_modified,
    }
}

fn decide(
    dir: &Path,
    slug: &str,
    ctx: &ClassifyContext,
    survey: &TreeSurvey,
    settled: &scenery_core::skill_tree_cache::Settled,
) -> Treatment {
    // ── Hold: never touched, shown to the user ───────────────────────────
    if !survey.secret_files.is_empty() {
        return Treatment::Hold("secrets_detected".into());
    }
    match scenery_core::skill_tree_cache::scan_secrets_at(dir, settled) {
        Ok(hits) if !hits.is_empty() => return Treatment::Hold("secrets_detected".into()),
        Ok(_) => {}
        // Unreadable means unverifiable: do not upload what cannot be checked.
        Err(_) => return Treatment::Hold("unreadable".into()),
    }
    if let Some(reason) = slug_collision(dir, slug, ctx) {
        return Treatment::Hold(reason);
    }

    // ── Track: governed in place ─────────────────────────────────────────
    if fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Treatment::Track("dev_symlink".into());
    }
    if in_git_work_tree(dir) {
        return Treatment::Track("git_tracked".into());
    }
    if let Some(home) = &ctx.home {
        if symlinked_ancestor_below(dir, home) {
            return Treatment::Track("dotfiles_managed:symlinked_ancestor".into());
        }
        if bare_dotfiles_repo_tracks(dir, home) {
            return Treatment::Track("git_tracked:dotfiles_repo".into());
        }
    }
    for marker in FOREIGN_MARKERS {
        if fs::symlink_metadata(dir.join(marker)).is_ok() {
            return Treatment::Track(format!("foreign_installer:{marker}"));
        }
    }
    if let Some(home) = &ctx.home {
        if npx_skills_lock_lists(home, slug) {
            return Treatment::Track("foreign_installer:npx_skills_lock".into());
        }
        if chezmoi_manages(home, slug) {
            return Treatment::Track("dotfiles_managed:chezmoi".into());
        }
    }
    if !survey.symlinks.is_empty() {
        return Treatment::Track("symlink_in_tree".into());
    }
    if survey.exceeds_convert_caps() {
        return Treatment::Track("over_caps".into());
    }
    if let Some(first) = survey.dropped.first() {
        // `.git` is caught above; anything else the upload drops (a
        // dot-directory, node_modules) would vanish from a converted copy.
        return Treatment::Track(format!("excluded_content:{first}"));
    }
    if name_differs_cached(dir, slug, &survey.tree_hash) {
        return Treatment::Track("name_differs_from_folder".into());
    }

    Treatment::Convert
}

/// [`name_differs_from_folder`], memoised by tree hash (the answer only
/// depends on the tree's `SKILL.md`): parsing every skill's manifest on every
/// scan tick was a steady idle cost. Bounded like the tree cache.
fn name_differs_cached(dir: &Path, slug: &str, tree_hash: &str) -> bool {
    type Memo = Mutex<HashMap<PathBuf, (String, String, bool)>>;
    static MEMO: OnceLock<Memo> = OnceLock::new();
    if tree_hash.is_empty() {
        return name_differs_from_folder(dir, slug);
    }
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = memo.lock() {
        if let Some((h, s, v)) = map.get(dir) {
            if h == tree_hash && s == slug {
                return *v;
            }
        }
    }
    let v = name_differs_from_folder(dir, slug);
    if let Ok(mut map) = memo.lock() {
        if map.len() >= 1024 && !map.contains_key(dir) {
            map.clear();
        }
        map.insert(
            dir.to_path_buf(),
            (tree_hash.to_string(), slug.to_string(), v),
        );
    }
    v
}

/// The skill's registry-facing id (derived from the frontmatter `name`) is not
/// its folder name. The install pipeline keys everything by that id, so such a
/// skill cannot be converted without renaming something the user wrote;
/// tracking keeps it governed instead.
fn name_differs_from_folder(dir: &Path, slug: &str) -> bool {
    use camino::Utf8PathBuf;
    let Ok(root) = Utf8PathBuf::from_path_buf(dir.to_path_buf()) else {
        return false;
    };
    match scenery_manifest::SkillPackage::load_from_dir(root) {
        Ok(pkg) => pkg.manifest.id != slug,
        // An invalid skill is the backend's call (`blocked: invalid_skill`).
        Err(_) => false,
    }
}

/// Is `.git` (a directory, or a file for a worktree/submodule) present in
/// `dir` or any ancestor up to `/`? Catches a repo checkout, a dotfiles repo
/// rooted at `~`, and stow targets. A symlinked `dir` is also checked through
/// its resolved location.
pub fn in_git_work_tree(dir: &Path) -> bool {
    let has_git = |start: &Path| {
        start
            .ancestors()
            .any(|a| fs::symlink_metadata(a.join(".git")).is_ok())
    };
    if has_git(dir) {
        return true;
    }
    match fs::canonicalize(dir) {
        Ok(resolved) if resolved != dir => has_git(&resolved),
        _ => false,
    }
}

/// Is any ancestor of `dir` that lies strictly BELOW `home` a symlink? That is
/// how stow folds a directory (`~/.agents -> ~/dotfiles/agents`): the skill
/// then lives inside the user's dotfiles repo and must not be rewritten.
/// Ancestors at or above `home` (`/var` -> `/private/var`) never count.
pub fn symlinked_ancestor_below(dir: &Path, home: &Path) -> bool {
    let Ok(rel) = dir.strip_prefix(home) else {
        return false;
    };
    let mut cur = home.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    // Every component except the skill directory itself (checked as dev_symlink).
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Is `dir` tracked by a BARE dotfiles repository whose work tree is `home`
/// (yadm; the `git --git-dir=$HOME/.cfg --work-tree=$HOME` pattern; a repo
/// pointed at by `GIT_DIR`)? No `.git` exists in any ancestor for those, so
/// the ancestor walk misses them. Candidates are the well-known locations plus
/// `GIT_DIR`; each is asked `git ls-files` for this exact path. Best effort: no
/// `git` on the machine, or no candidate, means `false`.
pub fn bare_dotfiles_repo_tracks(dir: &Path, home: &Path) -> bool {
    let Ok(rel) = dir.strip_prefix(home) else {
        return false;
    };
    let mut candidates: Vec<PathBuf> = vec![
        home.join(".local/share/yadm/repo.git"),
        home.join(".config/yadm/repo.git"),
        home.join(".yadm/repo.git"),
        home.join(".cfg"),
        home.join(".dotfiles"),
        home.join(".dotfiles.git"),
        home.join(".cfg.git"),
        home.join(".mydotfiles"),
    ];
    if let Some(g) = std::env::var_os("GIT_DIR") {
        candidates.push(PathBuf::from(g));
    }
    for repo in candidates {
        // A bare git directory has HEAD and objects/.
        if !(repo.join("HEAD").is_file() && repo.join("objects").is_dir()) {
            continue;
        }
        let tracked = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&repo)
            .arg("--work-tree")
            .arg(home)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(rel)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if tracked {
            return true;
        }
    }
    false
}

/// Does the `npx skills` global lock file (`~/.agents/.skill-lock.json`) list
/// `slug`? UNCONFIRMED against a real install: the shape assumed is
/// `{"skills": {"<name>": {...}}}`. A missing or differently shaped file
/// simply never matches.
fn npx_skills_lock_lists(home: &Path, slug: &str) -> bool {
    let Ok(text) = fs::read_to_string(home.join(".agents").join(".skill-lock.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let folded = fold_slug(slug);
    v.get("skills")
        .and_then(|s| s.as_object())
        .is_some_and(|m| m.keys().any(|k| fold_slug(k) == folded))
}

/// Is there a chezmoi source entry for a skill folder named `slug`? The
/// source tree (`$CHEZMOI_SOURCE_DIR` or `~/.local/share/chezmoi`) spells
/// names with attribute prefixes (`dot_`, `private_`, `exact_`...), which are
/// stripped before comparing. Bounded depth; best effort.
fn chezmoi_manages(home: &Path, slug: &str) -> bool {
    let src = std::env::var_os("CHEZMOI_SOURCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("share").join("chezmoi"));
    if !src.is_dir() {
        return false;
    }
    fn strip(name: &str) -> String {
        let mut n = name;
        loop {
            let before = n;
            for p in [
                "dot_",
                "private_",
                "exact_",
                "readonly_",
                "executable_",
                "empty_",
                "symlink_",
            ] {
                if let Some(rest) = n.strip_prefix(p) {
                    n = rest;
                }
            }
            if n == before {
                return n.to_string();
            }
        }
    }
    fn search(dir: &Path, parent_is_skills: bool, folded: &str, depth: usize) -> bool {
        if depth > 7 {
            return false;
        }
        let Ok(rd) = fs::read_dir(dir) else {
            return false;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let raw = e.file_name().to_string_lossy().to_string();
            if raw == ".git" {
                continue;
            }
            let name = strip(&raw);
            if ft.is_dir() {
                if parent_is_skills && fold_slug(&name) == folded {
                    return true;
                }
                if search(&e.path(), name == "skills", folded, depth + 1) {
                    return true;
                }
            }
        }
        false
    }
    search(&src, false, &fold_slug(slug), 0)
}

/// `Some(reason)` when converting `dir` would land on a *different* skill at
/// `~/.agents/skills/<slug>` (compared case-folded and NFC-normalized).
///
/// A skill that IS the canonical entry never collides with itself. A skill in
/// another root (`~/.pi/agent/skills/pdf`) collides when the canonical root
/// already holds something with the same folded name that is not this very
/// directory: converting would overwrite a catalog install or another skill.
fn slug_collision(dir: &Path, slug: &str, ctx: &ClassifyContext) -> Option<String> {
    let root = ctx.canonical_root.as_ref()?;
    let folded = fold_slug(slug);
    let listed;
    let candidates: &[PathBuf] = match &ctx.canonical_index {
        Some(idx) => idx.get(&folded).map(Vec::as_slice).unwrap_or(&[]),
        None => {
            listed = index_root(root).remove(&folded).unwrap_or_default();
            &listed
        }
    };
    let own = if candidates.is_empty() {
        None
    } else {
        fs::canonicalize(dir).ok()
    };
    for other in candidates {
        if other == dir {
            continue;
        }
        let same = match (&own, fs::canonicalize(other).ok()) {
            (Some(a), Some(b)) => *a == b,
            _ => false,
        };
        if !same {
            return Some("slug_collision".into());
        }
    }
    None
}

/// Duplicate sources: more than one *non-canonical* item with the same folded
/// slug would race to convert into the same canonical folder. Returns the
/// indices (into `slugs_and_paths`) that must be held.
pub fn duplicate_non_canonical(
    slugs_and_paths: &[(String, PathBuf)],
    canonical_root: Option<&Path>,
) -> Vec<usize> {
    use std::collections::HashMap;
    let mut by_slug: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (slug, path)) in slugs_and_paths.iter().enumerate() {
        let canonical = canonical_root.is_some_and(|r| path.parent() == Some(r));
        if !canonical {
            by_slug.entry(fold_slug(slug)).or_default().push(i);
        }
    }
    let mut out: Vec<usize> = by_slug
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
#[path = "foreign_tests.rs"]
mod tests;
