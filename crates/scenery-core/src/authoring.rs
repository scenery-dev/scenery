//! Authoring with Scenery (plan 4.7): shared types and the hook the MCP tool
//! `scenery_create_skill` uses to register and convert a freshly scaffolded
//! skill.
//!
//! The MCP crate (which hosts the tools) sits below the daemon crate (which
//! owns the conversion machinery: the transactional install, markers, client
//! links), so the daemon registers [`CreateHook`] at start, the same way it
//! registers the skill-uninstall removers in [`crate::installer`]. A process
//! without the daemon (the in-process fallback) has no hook; the tool then
//! leaves the scaffolded skill for the daemon's discovery pass, which assigns
//! its ID.

use crate::state::AppState;
use std::sync::OnceLock;

/// Reserved slug of the built-in skill. Never a private or catalog skill name.
pub const BUILTIN_SLUG: &str = "authoring-with-scenery";

/// A skill registered and converted by [`CreateHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSkill {
    /// Registry text id (`<owner_namespace>~<slug>`).
    pub skill_id: String,
    /// Stable registry identity (`skills.id`).
    pub skill_uuid: String,
    /// The private version the folder now holds.
    pub version: String,
    pub owner_namespace: Option<String>,
    pub scan_verdict: Option<String>,
}

/// Why a skill could not be registered right now. The folder is kept in every
/// case; discovery finishes the job later where that makes sense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// Signed out, no device identity, or the registry is unreachable.
    Offline(String),
    /// The registry said no (`code` is its reason: `reserved_slug`,
    /// `secrets_detected`, `scan_rejected`, `runner_upgrade_required`, ...).
    Refused { code: String, message: String },
    /// Anything else (the local conversion failed, a bad response).
    Failed(String),
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateError::Offline(m) | CreateError::Failed(m) => write!(f, "{m}"),
            CreateError::Refused { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

/// `(state, registry_url, slug)`: register `~/.agents/skills/<slug>` as the
/// user's own private skill and convert it in place. Blocking.
pub type CreateHook = fn(&AppState, &str, &str) -> Result<CreatedSkill, CreateError>;

static CREATE_HOOK: OnceLock<CreateHook> = OnceLock::new();

/// Register the hook. First registration wins.
pub fn register_create_hook(hook: CreateHook) {
    let _ = CREATE_HOOK.set(hook);
}

/// Run the hook, or `None` when this process has none.
pub fn run_create_hook(
    state: &AppState,
    registry_url: &str,
    slug: &str,
) -> Option<Result<CreatedSkill, CreateError>> {
    CREATE_HOOK.get().map(|h| h(state, registry_url, slug))
}

// ── Slugs ───────────────────────────────────────────────────────────────────

/// NFC + lowercase + trim: APFS treats `PDF` and `pdf`, and NFC/NFD spellings,
/// as one folder, so the registry key must too. Same rule as the backend's
/// `auto_manage.normalize_slug`.
pub fn normalize_slug(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    raw.trim().nfc().collect::<String>().to_lowercase()
}

/// Why `slug` cannot name a new skill, or `None`. Mirrors the backend's
/// `auto_manage.slug_error` (a single path component, not hidden, not
/// reserved), plus the filename-safe rule the scaffold needs.
pub fn slug_error(slug: &str) -> Option<&'static str> {
    let s = normalize_slug(slug);
    if s.is_empty() {
        return Some("the name is empty");
    }
    if s.chars().count() > 128 {
        return Some("the name is too long (128 characters at most)");
    }
    if s.contains(['/', '\\', '\0']) || s.chars().any(|c| (c as u32) < 32) {
        return Some("the name must be a single folder name (no slashes)");
    }
    if s == "." || s == ".." || s.starts_with('.') {
        return Some("the name must not start with a dot");
    }
    if s.contains('~') {
        return Some("the name must not contain '~'");
    }
    if s.chars()
        .any(|c| c == '"' || c == '\'' || c == ':' || c == '#')
    {
        return Some("the name must not contain quotes, ':' or '#'");
    }
    if s == BUILTIN_SLUG {
        return Some("that name is reserved by Scenery");
    }
    None
}

/// The `SKILL.md` a new skill starts as: standard frontmatter (`name`,
/// `description`) and the instructions as the body. The registry's compile step
/// adds `metadata.scenery.publisher/version` when the skill is registered.
pub fn skill_md(slug: &str, description: &str, instructions: &str) -> String {
    // A YAML double-quoted scalar: escape backslashes and quotes, fold newlines.
    let desc = description
        .trim()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ");
    format!(
        "---\nname: {slug}\ndescription: \"{desc}\"\n---\n\n{}\n",
        instructions.trim()
    )
}

// ── Checking a skill folder ─────────────────────────────────────────────────

/// What [`check_tree`] found. `ok()` is true when nothing blocks sharing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeCheck {
    pub tree_hash: String,
    pub files: usize,
    pub bytes: u64,
    /// Layout problems that must be fixed (missing SKILL.md, bad frontmatter,
    /// TODO description, symlinks, over the size caps).
    pub problems: Vec<String>,
    /// Things worth a look that do not block.
    pub warnings: Vec<String>,
    /// File NAMES (never contents) that look like they hold a secret.
    pub secrets: Vec<String>,
}

impl TreeCheck {
    pub fn ok(&self) -> bool {
        self.problems.is_empty() && self.secrets.is_empty()
    }
}

/// Validate a skill folder's layout and look for secrets, offline. The same
/// rules the upload applies (`pack` / `skill_tree`): no symlinks, no secret
/// files or patterns, within the size caps.
pub fn check_tree(dir: &std::path::Path) -> TreeCheck {
    let mut out = TreeCheck::default();
    let survey = match crate::skill_tree::survey(dir) {
        Ok(s) => s,
        Err(e) => {
            out.problems
                .push(format!("could not read the folder: {e:#}"));
            return out;
        }
    };
    out.tree_hash = survey.tree_hash.clone();
    out.files = survey.files;
    out.bytes = survey.bytes;
    if survey.exceeds_convert_caps() {
        out.problems
            .push("the folder is too large for a skill (5 MB, 200 files, 8 levels)".into());
    }
    for l in &survey.symlinks {
        out.problems.push(format!(
            "'{l}' is a symbolic link; skills cannot contain links"
        ));
    }
    for f in &survey.secret_files {
        out.secrets.push(f.clone());
    }
    if let Ok(hits) = crate::skill_tree::scan_secrets(dir) {
        for h in hits {
            if !out.secrets.contains(&h) {
                out.secrets.push(h);
            }
        }
    }
    out.secrets.sort();
    for d in &survey.dropped {
        if !survey.secret_files.contains(d) {
            out.warnings
                .push(format!("'{d}' will not be uploaded (hidden or excluded)"));
        }
    }

    let md_path = dir.join("SKILL.md");
    match std::fs::read_to_string(&md_path) {
        Err(_) => out.problems.push("SKILL.md is missing".into()),
        Ok(text) => check_frontmatter(&text, &mut out),
    }
    out
}

fn check_frontmatter(text: &str, out: &mut TreeCheck) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        out.problems
            .push("SKILL.md must start with a '---' frontmatter block".into());
        return;
    };
    let end = rest
        .find("\n---")
        .map(|i| (i, i + 4))
        .or_else(|| rest.strip_suffix("---").map(|r| (r.len(), rest.len())));
    let Some((fm_end, _)) = end else {
        out.problems
            .push("the SKILL.md frontmatter block is not closed with '---'".into());
        return;
    };
    let fm: serde_yaml::Value = match serde_yaml::from_str(&rest[..fm_end]) {
        Ok(v) => v,
        Err(e) => {
            out.problems
                .push(format!("the SKILL.md frontmatter is not valid YAML: {e}"));
            return;
        }
    };
    let get = |k: &str| fm.get(k).and_then(|v| v.as_str()).map(str::trim);
    match get("name") {
        None | Some("") => out.problems.push("frontmatter `name` is missing".into()),
        Some(n) if n == BUILTIN_SLUG => out
            .problems
            .push(format!("the name '{BUILTIN_SLUG}' is reserved by Scenery")),
        Some(_) => {}
    }
    match get("description") {
        None | Some("") => out
            .problems
            .push("frontmatter `description` is missing".into()),
        Some(d) if d.to_ascii_uppercase().starts_with("TODO") => out
            .problems
            .push("frontmatter `description` is still a TODO placeholder".into()),
        Some(d) if d.len() < 20 => out
            .warnings
            .push("the description is very short; say when the skill should be used".into()),
        Some(_) => {}
    }
    if fm
        .as_mapping()
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str())
                .any(|k| k.starts_with("vh_"))
        })
        .unwrap_or(false)
    {
        out.problems
            .push("top-level `vh_*` keys are not allowed; use metadata.scenery.*".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slug_error("meeting-notes"), None);
        assert_eq!(normalize_slug("  PDF-Tool "), "pdf-tool");
        assert!(slug_error("").is_some());
        assert!(slug_error("a/b").is_some());
        assert!(slug_error("..").is_some());
        assert!(slug_error(".hidden").is_some());
        assert!(slug_error("a~b").is_some());
        assert!(slug_error("Authoring-With-Scenery").is_some());
        assert!(slug_error("a: b").is_some());
        assert!(slug_error(&"x".repeat(129)).is_some());
        // NFC and NFD spellings are the same folder
        assert_eq!(normalize_slug("e\u{301}"), normalize_slug("\u{e9}"));
    }

    #[test]
    fn scaffolded_skill_md_passes_the_check() {
        let d = tmp();
        std::fs::write(
            d.path().join("SKILL.md"),
            skill_md(
                "notes",
                "Use when the user asks to \"summarize\" meeting notes",
                "# Do it\n",
            ),
        )
        .unwrap();
        let c = check_tree(d.path());
        assert!(c.ok(), "{c:?}");
        assert!(!c.tree_hash.is_empty());
    }

    #[test]
    fn missing_skill_md_todo_description_and_reserved_name_are_problems() {
        let d = tmp();
        assert!(check_tree(d.path())
            .problems
            .iter()
            .any(|p| p.contains("SKILL.md is missing")));
        std::fs::write(
            d.path().join("SKILL.md"),
            "---\nname: authoring-with-scenery\ndescription: TODO describe\n---\nbody\n",
        )
        .unwrap();
        let c = check_tree(d.path());
        assert!(c.problems.iter().any(|p| p.contains("reserved")));
        assert!(c.problems.iter().any(|p| p.contains("TODO")));
        std::fs::write(d.path().join("SKILL.md"), "no frontmatter").unwrap();
        assert!(!check_tree(d.path()).ok());
    }

    #[test]
    fn secrets_are_named_never_quoted_and_block() {
        let d = tmp();
        std::fs::write(
            d.path().join("SKILL.md"),
            skill_md("x", "A long enough description of this skill", "body"),
        )
        .unwrap();
        std::fs::write(d.path().join(".env"), "TOKEN=abc").unwrap();
        std::fs::write(
            d.path().join("notes.md"),
            "-----BEGIN RSA PRIVATE KEY-----\nabc\n",
        )
        .unwrap();
        let c = check_tree(d.path());
        assert!(!c.ok());
        assert!(c.secrets.contains(&".env".to_string()), "{c:?}");
        assert!(c.secrets.contains(&"notes.md".to_string()), "{c:?}");
        assert!(!format!("{c:?}").contains("abc"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_a_problem() {
        let d = tmp();
        std::fs::write(
            d.path().join("SKILL.md"),
            skill_md("x", "A long enough description of this skill", "body"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc/hosts", d.path().join("link")).unwrap();
        assert!(check_tree(d.path())
            .problems
            .iter()
            .any(|p| p.contains("symbolic link")));
    }

    #[test]
    fn hash_changes_with_content() {
        let d = tmp();
        std::fs::write(
            d.path().join("SKILL.md"),
            skill_md("x", "A long enough description of x", "one"),
        )
        .unwrap();
        let a = check_tree(d.path()).tree_hash;
        std::fs::write(
            d.path().join("SKILL.md"),
            skill_md("x", "A long enough description of x", "two"),
        )
        .unwrap();
        assert_ne!(a, check_tree(d.path()).tree_hash);
    }
}
