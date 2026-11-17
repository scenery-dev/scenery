//! Crash- and disk-full-safe in-place conversion of a user's skill directory
//! to a Scenery-managed one.
//!
//! # The problem this replaces
//!
//! Converting `~/.agents/skills/<slug>` (the user's own, unmarked directory)
//! used to be: back up, `remove_dir_all`, write files one by one, write the
//! marker. A crash or `ENOSPC` midway left an unmarked, half-written directory
//! at the canonical path. Discovery then re-found it as a "user skill" and
//! re-uploaded the half-written content, and a second backup of the
//! half-written directory shadowed the original.
//!
//! # The protocol
//!
//! 1. Write an *intent* file (the conversion-in-progress journal entry) under
//!    `<root>/conversions-in-progress/<slug>.json`.
//! 2. Build the new tree in a sibling staging directory on the same
//!    filesystem (`.<slug>.scenery-staging-<token>`). The original is untouched.
//! 3. Back the original up (symlink-safe copy into a unique
//!    `restore-backups/<ts>/adopted/<slug>` plus an `adopted` restore-journal
//!    entry). The destination is recorded in the intent *before* copying.
//! 4. `rename(original -> .<slug>.scenery-old-<token>)`, then
//!    `rename(staging -> original)`. Both are same-filesystem renames, so the
//!    canonical path is always either the complete original or the complete
//!    new tree, never a mixture.
//! 5. The caller writes the marker; [`Conversion::commit`] then removes the
//!    old directory and the intent.
//!
//! At daemon startup [`recover_interrupted`] rolls back any intent that was
//! never committed: if the original was moved aside it is put back, the
//! staging directory is removed, and the now-pointless backup and its
//! journal entry are dropped. The pending adopt-takeover record is untouched,
//! so the conversion is simply retried on a later install.
//!
//! Hidden (dot-prefixed) sibling names keep the staging and old directories
//! out of discovery (`discoveries::scan_root` skips dot directories).

use anyhow::{Context, Result};
use scenery_core::{
    atomic_file, restore_journal::RestoreJournal, safe_copy::unique_token, state::AppState,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{info, warn};

const INTENT_DIR: &str = "conversions-in-progress";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    /// Staging directory being built; the original has not been touched.
    Staging,
    /// Backup of the original being taken; the original is still in place.
    Backup,
    /// Original being swapped for the staged tree.
    Swapping,
    /// Marker written; only cleanup of the old directory remains.
    Marked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Intent {
    slug: String,
    target: String,
    staging: String,
    old: String,
    #[serde(default)]
    backup_path: Option<String>,
    phase: Phase,
    started: String,
}

fn intent_dir(root_dir: &Path) -> PathBuf {
    root_dir.join(INTENT_DIR)
}

fn intent_path(root_dir: &Path, slug: &str) -> PathBuf {
    intent_dir(root_dir).join(format!("{slug}.json"))
}

fn write_intent(path: &Path, intent: &Intent) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(intent).context("conversion: serialise intent")?;
    atomic_file::write_atomic(path, &bytes, Some(0o600))
        .with_context(|| format!("conversion: write intent {}", path.display()))
}

fn exists_no_follow(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_dir() => {
            fs::remove_dir_all(p).with_context(|| format!("conversion: removing {}", p.display()))
        }
        Ok(_) => {
            fs::remove_file(p).with_context(|| format!("conversion: removing {}", p.display()))
        }
        Err(_) => Ok(()),
    }
}

/// A conversion in flight. Dropping it without [`commit`](Self::commit) or
/// [`rollback`](Self::rollback) leaves the intent on disk for startup recovery.
pub(crate) struct Conversion {
    root_dir: PathBuf,
    intent_path: PathBuf,
    intent: Intent,
}

impl Conversion {
    /// Start converting `target` (the user's unmarked real directory).
    /// Creates the empty staging directory and records the intent. Any stale
    /// intent for the same slug is recovered first; if that fails the new
    /// conversion is refused.
    pub(crate) fn begin(root_dir: &Path, slug: &str, target: &Path) -> Result<Self> {
        let parent = target.parent().ok_or_else(|| {
            anyhow::anyhow!("conversion: target has no parent: {}", target.display())
        })?;
        let ipath = intent_path(root_dir, slug);
        if ipath.exists() {
            recover_one(root_dir, &ipath)
                .context("conversion: could not recover an earlier interrupted conversion")?;
        }
        fs::create_dir_all(intent_dir(root_dir))
            .context("conversion: creating intent directory")?;

        let token = unique_token();
        let staging = parent.join(format!(".{slug}.scenery-staging-{token}"));
        let old = parent.join(format!(".{slug}.scenery-old-{token}"));
        let intent = Intent {
            slug: slug.to_string(),
            target: target.to_string_lossy().to_string(),
            staging: staging.to_string_lossy().to_string(),
            old: old.to_string_lossy().to_string(),
            backup_path: None,
            phase: Phase::Staging,
            started: chrono::Utc::now().to_rfc3339(),
        };
        // Intent first, so a crash at any later point is recoverable.
        write_intent(&ipath, &intent)?;
        if let Err(e) = fs::create_dir(&staging) {
            let _ = fs::remove_file(&ipath);
            return Err(e).with_context(|| {
                format!("conversion: creating staging dir {}", staging.display())
            });
        }
        Ok(Self {
            root_dir: root_dir.to_path_buf(),
            intent_path: ipath,
            intent,
        })
    }

    /// The directory the new tree must be written into.
    pub(crate) fn staging(&self) -> &Path {
        Path::new(&self.intent.staging)
    }

    /// Back the original up, then atomically swap the staged tree in.
    /// On error nothing is half-done: the original is back in place (this
    /// rolls itself back) and the caller must not call `commit`.
    pub(crate) fn swap(&mut self, journal: &RestoreJournal) -> Result<()> {
        let target = PathBuf::from(&self.intent.target);
        let dest = journal.new_backup_dest("adopted", &self.intent.slug);
        self.intent.backup_path = Some(dest.to_string());
        self.intent.phase = Phase::Backup;
        write_intent(&self.intent_path, &self.intent)?;

        super::takeover::back_up_before_delete_to(journal, &self.intent.slug, &target, &dest)
            .context("conversion: refusing to replace an un-backed-up directory")?;

        self.intent.phase = Phase::Swapping;
        write_intent(&self.intent_path, &self.intent)?;

        let old = PathBuf::from(&self.intent.old);
        fs::rename(&target, &old)
            .with_context(|| format!("conversion: could not move {} aside", target.display()))?;
        if let Err(e) = fs::rename(&self.intent.staging, &target) {
            // Put the original back right now.
            if let Err(e2) = fs::rename(&old, &target) {
                warn!(
                    error = %e2,
                    old = %old.display(),
                    "conversion: could not restore the original after a failed swap; \
                     startup recovery will retry"
                );
            }
            return Err(e).with_context(|| {
                format!(
                    "conversion: could not move staged tree into {}",
                    target.display()
                )
            });
        }
        Ok(())
    }

    /// The marker is written: drop the old directory and the intent.
    pub(crate) fn commit(mut self) {
        self.intent.phase = Phase::Marked;
        if let Err(e) = write_intent(&self.intent_path, &self.intent) {
            warn!(error = %e, "conversion: could not record committed phase");
            return;
        }
        if let Err(e) = remove_any(Path::new(&self.intent.old)) {
            warn!(error = %e, "conversion: could not remove the old directory (recovery will retry)");
            return;
        }
        let _ = fs::remove_file(&self.intent_path);
    }

    /// Undo everything: original back in place, staging gone, backup dropped.
    /// Failures are logged; the intent stays for startup recovery.
    pub(crate) fn rollback(self) {
        if let Err(e) = roll_back_intent(&self.root_dir, &self.intent_path, &self.intent) {
            warn!(slug = %self.intent.slug, error = %e, "conversion: rollback failed; startup recovery will retry");
        }
    }
}

/// Summary of what startup recovery did.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub rolled_back: Vec<String>,
    pub finished: Vec<String>,
    pub failed: Vec<String>,
}

/// Roll back (or finish cleaning up) every conversion that was interrupted by
/// a crash or power loss. Called once at daemon startup, before anything can
/// push. Never fails: problems are logged and reported.
pub fn recover_interrupted(state: &AppState) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let root = state.root_dir.as_std_path();
    let Ok(entries) = fs::read_dir(intent_dir(root)) else {
        return report;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match recover_one(root, &path) {
            Ok(Phase::Marked) => report.finished.push(name),
            Ok(_) => report.rolled_back.push(name),
            Err(e) => {
                warn!(intent = %path.display(), error = %e, "conversion: recovery failed");
                report.failed.push(name);
            }
        }
    }
    if !report.rolled_back.is_empty() || !report.finished.is_empty() {
        info!(
            rolled_back = ?report.rolled_back,
            finished = ?report.finished,
            "conversion: recovered interrupted in-place conversions"
        );
    }
    report
}

fn recover_one(root_dir: &Path, intent_file: &Path) -> Result<Phase> {
    let text = fs::read_to_string(intent_file)
        .with_context(|| format!("conversion: reading {}", intent_file.display()))?;
    let intent: Intent = serde_json::from_str(&text)
        .with_context(|| format!("conversion: parsing {}", intent_file.display()))?;
    validate_intent(root_dir, &intent)?;
    roll_back_intent(root_dir, intent_file, &intent)?;
    Ok(intent.phase)
}

/// An intent file is data on disk: never let it point deletion or renames at
/// anything but the three paths a conversion creates.
fn validate_intent(root_dir: &Path, intent: &Intent) -> Result<()> {
    scenery_core::safe_path::validate_single_component("skill slug", &intent.slug)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let target = Path::new(&intent.target);
    let parent = target.parent();
    anyhow::ensure!(
        target.file_name().and_then(|n| n.to_str()) == Some(intent.slug.as_str()),
        "intent target does not match its slug"
    );
    for (p, prefix) in [
        (
            &intent.staging,
            format!(".{}.scenery-staging-", intent.slug),
        ),
        (&intent.old, format!(".{}.scenery-old-", intent.slug)),
    ] {
        let p = Path::new(p);
        anyhow::ensure!(
            p.parent() == parent
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix)),
            "intent path {} is not a conversion sibling of the target",
            p.display()
        );
    }
    if let Some(bp) = &intent.backup_path {
        anyhow::ensure!(
            Path::new(bp).starts_with(root_dir.join("restore-backups")),
            "intent backup path is outside restore-backups"
        );
    }
    Ok(())
}

fn roll_back_intent(root_dir: &Path, intent_file: &Path, intent: &Intent) -> Result<()> {
    let target = Path::new(&intent.target);
    let old = Path::new(&intent.old);
    let staging = Path::new(&intent.staging);

    if intent.phase == Phase::Marked {
        // Committed; only the old directory is left to clean up.
        remove_any(old)?;
        remove_any(staging)?;
        let _ = fs::remove_file(intent_file);
        return Ok(());
    }

    if exists_no_follow(old) {
        // The original was moved aside. What is at `target` (if anything) is
        // our own staged tree.
        remove_any(target)?;
        fs::rename(old, target).with_context(|| {
            format!(
                "conversion: could not put the original back at {}",
                target.display()
            )
        })?;
    }
    remove_any(staging)?;

    // The original is intact again, so its backup and journal entry only
    // add noise (and a second, later backup could shadow it).
    if let Some(bp) = &intent.backup_path {
        let journal = RestoreJournal::new(
            camino::Utf8PathBuf::from_path_buf(root_dir.to_path_buf())
                .map_err(|p| anyhow::anyhow!("non-UTF-8 data dir: {}", p.display()))?,
        );
        if let Err(e) = journal.remove_where(|e| e.backup_path.as_deref() == Some(bp.as_str())) {
            warn!(error = %e, "conversion: could not drop the rolled-back journal entry");
        }
        // `<root>/restore-backups/<ts>/adopted/<slug>` -> remove the `<ts>` run.
        let bp = Path::new(bp);
        if let Some(run) = bp.parent().and_then(Path::parent) {
            if run.parent() == Some(root_dir.join("restore-backups").as_path()) {
                let _ = fs::remove_dir_all(run);
            }
        }
    }
    let _ = fs::remove_file(intent_file);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use scenery_core::restore_journal::{JournalEntry, JournalOp, JournalSource};
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        state: AppState,
        root: PathBuf,
        skills: PathBuf,
        target: PathBuf,
        journal: RestoreJournal,
    }

    /// A data dir plus `skills/demo` holding the user's own two files.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = AppState::bootstrap_in(root_dir).unwrap();
        let root = state.root_dir.as_std_path().to_path_buf();
        let skills = tmp.path().join("skills");
        let target = skills.join("demo");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "user original").unwrap();
        fs::write(target.join("notes.md"), "user notes").unwrap();
        let journal = RestoreJournal::for_state(&state);
        Fixture {
            _tmp: tmp,
            state,
            root,
            skills,
            target,
            journal,
        }
    }

    fn siblings(skills: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(skills)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    fn intents(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(intent_dir(root))
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    fn assert_original_intact_and_clean(f: &Fixture) {
        assert_eq!(
            fs::read(f.target.join("SKILL.md")).unwrap(),
            b"user original"
        );
        assert_eq!(fs::read(f.target.join("notes.md")).unwrap(), b"user notes");
        assert!(!f.target.join(".scenery-managed.json").exists());
        assert_eq!(
            siblings(&f.skills),
            vec!["demo"],
            "no staging/old leftovers"
        );
        assert!(intents(&f.root).is_empty(), "intent removed");
        assert!(
            f.journal.read_all().unwrap().is_empty(),
            "journal entry dropped"
        );
        assert!(
            !f.root.join("restore-backups").exists()
                || fs::read_dir(f.root.join("restore-backups"))
                    .unwrap()
                    .count()
                    == 0,
            "pointless backup dropped"
        );
    }

    #[test]
    fn begin_writes_intent_and_stages_beside_the_target_without_touching_it() {
        let f = fixture();
        let c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        assert_eq!(c.staging().parent(), f.target.parent());
        assert!(c.staging().is_dir());
        assert_eq!(intents(&f.root).len(), 1);
        assert_eq!(
            fs::read(f.target.join("SKILL.md")).unwrap(),
            b"user original"
        );
        // Hidden: discovery must never see it.
        assert!(c
            .staging()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with('.'));
    }

    #[test]
    fn crash_while_staging_is_rolled_back_at_startup() {
        let f = fixture();
        let c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "half written").unwrap();
        drop(c); // the process "dies" here

        let report = recover_interrupted(&f.state);
        assert_eq!(report.rolled_back, vec!["demo"]);
        assert!(report.failed.is_empty());
        assert_original_intact_and_clean(&f);
    }

    #[test]
    fn crash_after_the_swap_restores_the_original_at_startup() {
        let f = fixture();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        c.swap(&f.journal).unwrap();
        // Mid-conversion state: new tree is live, original aside, backup and
        // `adopted` journal entry exist, marker not yet written.
        assert_eq!(fs::read(f.target.join("SKILL.md")).unwrap(), b"published");
        assert_eq!(f.journal.read_all().unwrap().len(), 1);
        drop(c); // crash before the marker / commit

        let report = recover_interrupted(&f.state);
        assert_eq!(report.rolled_back, vec!["demo"]);
        assert_original_intact_and_clean(&f);
    }

    #[test]
    fn crash_between_the_two_renames_restores_the_original_at_startup() {
        let f = fixture();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        // First rename done, second not: the canonical path is missing.
        c.intent.phase = Phase::Swapping;
        write_intent(&c.intent_path, &c.intent).unwrap();
        fs::rename(&f.target, &c.intent.old).unwrap();
        drop(c);

        let report = recover_interrupted(&f.state);
        assert_eq!(report.rolled_back, vec!["demo"]);
        assert_original_intact_and_clean(&f);
    }

    #[test]
    fn committed_conversion_is_only_cleaned_up_not_undone() {
        let f = fixture();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        c.swap(&f.journal).unwrap();
        // Marker written, commit interrupted after recording the phase.
        c.intent.phase = Phase::Marked;
        write_intent(&c.intent_path, &c.intent).unwrap();
        drop(c);

        let report = recover_interrupted(&f.state);
        assert_eq!(report.finished, vec!["demo"]);
        assert_eq!(fs::read(f.target.join("SKILL.md")).unwrap(), b"published");
        assert_eq!(siblings(&f.skills), vec!["demo"], "old dir removed");
        assert_eq!(f.journal.read_all().unwrap().len(), 1, "adopted entry kept");
    }

    #[test]
    fn a_full_conversion_replaces_the_tree_and_keeps_the_backup() {
        let f = fixture();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        c.swap(&f.journal).unwrap();
        c.commit();

        assert_eq!(fs::read(f.target.join("SKILL.md")).unwrap(), b"published");
        assert!(!f.target.join("notes.md").exists(), "no stale user files");
        assert_eq!(siblings(&f.skills), vec!["demo"]);
        assert!(intents(&f.root).is_empty());
        let entries = f.journal.read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, JournalSource::Adopted);
        assert_eq!(entries[0].op, JournalOp::FileDelete);
        let backup = PathBuf::from(entries[0].backup_path.clone().unwrap());
        assert_eq!(fs::read(backup.join("SKILL.md")).unwrap(), b"user original");
        assert_eq!(fs::read(backup.join("notes.md")).unwrap(), b"user notes");
    }

    #[test]
    fn a_failed_backup_aborts_without_touching_the_original() {
        let f = fixture();
        // restore-backups is a FILE, so the backup copy cannot be created
        // (the same failure shape as a full disk).
        fs::write(f.root.join("restore-backups"), "not a dir").unwrap();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        assert!(c.swap(&f.journal).is_err());
        c.rollback();

        assert_eq!(
            fs::read(f.target.join("SKILL.md")).unwrap(),
            b"user original"
        );
        assert_eq!(fs::read(f.target.join("notes.md")).unwrap(), b"user notes");
        assert_eq!(siblings(&f.skills), vec!["demo"]);
        assert!(intents(&f.root).is_empty());
        assert!(f.journal.read_all().unwrap().is_empty());
    }

    #[test]
    fn a_tampered_intent_cannot_point_deletion_elsewhere() {
        let f = fixture();
        let victim = f.skills.join("precious");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("data"), "keep me").unwrap();
        let intent = Intent {
            slug: "demo".into(),
            target: f.target.to_string_lossy().to_string(),
            staging: victim.to_string_lossy().to_string(),
            old: f
                .skills
                .join(".demo.scenery-old-x")
                .to_string_lossy()
                .to_string(),
            backup_path: None,
            phase: Phase::Staging,
            started: "now".into(),
        };
        fs::create_dir_all(intent_dir(&f.root)).unwrap();
        write_intent(&intent_path(&f.root, "demo"), &intent).unwrap();

        let report = recover_interrupted(&f.state);
        assert_eq!(report.failed, vec!["demo"]);
        assert_eq!(fs::read(victim.join("data")).unwrap(), b"keep me");
        assert_eq!(
            fs::read(f.target.join("SKILL.md")).unwrap(),
            b"user original"
        );
    }

    #[test]
    fn rollback_keeps_journal_entries_for_other_paths() {
        let f = fixture();
        f.journal
            .append(JournalEntry::new(
                JournalOp::FileDelete,
                JournalSource::Adopted,
                "/elsewhere",
            ))
            .unwrap();
        let mut c = Conversion::begin(&f.root, "demo", &f.target).unwrap();
        fs::write(c.staging().join("SKILL.md"), "published").unwrap();
        c.swap(&f.journal).unwrap();
        drop(c);
        recover_interrupted(&f.state);
        let left = f.journal.read_all().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].target_path, "/elsewhere");
    }
}
