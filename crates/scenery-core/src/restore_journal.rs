//! Universal restore journal — an append-only ledger of every mutation
//! VectorHawk makes to files *outside* its own data directory (AI-client
//! config edits, F2 pushes into `~/.claude/...`, F1 native takeovers).
//!
//! `vectorhawk uninstall` replays this journal in reverse so the invariant
//! holds: **uninstall is a RESTORE, not a DELETE.** Everything the user
//! brought to the machine comes back; everything that only ever worked
//! because VectorHawk brokered it goes away cleanly.
//!
//! # Layout
//!
//! - Journal: `<root_dir>/restore-journal.json` — a single JSON array of
//!   [`JournalEntry`], oldest first.
//! - Backups: `<root_dir>/restore-backups/<ts>/...` — snapshots of files/dirs
//!   taken immediately before the first VectorHawk-caused mutation to them.
//!
//! # Schema stability
//!
//! The on-disk shape of [`JournalEntry`] is a **fixed contract** — other
//! tooling (including a shell script that parses it with `jq`) depends on
//! these exact field names. Do not rename `op`, `ts`, `source`, `slug`,
//! `client`, `target_path`, `backup_path`, or `detail`.
//!
//! # Corruption tolerance
//!
//! [`RestoreJournal::read_all`] never fails because of malformed *content* —
//! only genuine I/O errors (e.g. permission denied) propagate. A corrupt or
//! truncated journal file is salvaged on a best-effort basis: individual
//! entries that fail to parse are skipped and logged, never the whole file.
//! A broken journal must never block an uninstall.

use crate::state::AppState;
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::fs;

// ── Types ────────────────────────────────────────────────────────────────────

/// Kind of mutation recorded in the restore journal.
///
/// Part of the fixed on-disk contract — serializes as the exact snake_case
/// strings `config_edit`, `file_replace`, `file_delete`, `artifact_push`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOp {
    /// A keyed entry was added/changed inside a JSON config file (AI client
    /// config, `~/.claude.json`) rather than the whole file being replaced.
    ConfigEdit,
    /// A whole file or directory was replaced in place with a VectorHawk-
    /// managed copy (e.g. F1 taking ownership of a native skill dir).
    FileReplace,
    /// A file or directory was deleted outright.
    FileDelete,
    /// VectorHawk pushed a new managed artifact into a native directory that
    /// did not contain it before (F2 skill/plugin/MCP push).
    ArtifactPush,
}

/// Provenance classification — drives the three-way uninstall decision.
///
/// - `Native` / `Adopted` → the target existed (or was derived from something
///   that existed) before VectorHawk touched it. Uninstall **restores**
///   `target_path` from `backup_path`.
/// - `Brokered` / `Managed` → the target only ever worked *through*
///   VectorHawk (credential-brokered MCP server, or a governed copy pushed
///   into a native directory). Uninstall **removes it completely** — there
///   is nothing of the user's to restore.
///
/// Part of the fixed on-disk contract — serializes as the exact snake_case
/// strings `brokered`, `managed`, `native`, `adopted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalSource {
    Brokered,
    Managed,
    Native,
    Adopted,
}

impl JournalSource {
    /// `true` for sources whose `target_path` should be restored from
    /// `backup_path` on uninstall (`native`, `adopted`); `false` for sources
    /// that should be removed outright (`brokered`, `managed`).
    pub fn should_restore(self) -> bool {
        matches!(self, JournalSource::Native | JournalSource::Adopted)
    }
}

/// One entry in the restore journal. See module docs for the schema
/// stability contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op: JournalOp,
    /// RFC 3339 timestamp (e.g. `2026-07-19T12:00:00Z`) of when the mutation
    /// happened.
    pub ts: String,
    pub source: JournalSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Absolute path of the thing that was mutated.
    pub target_path: String,
    /// Absolute path of the pre-mutation backup, if one was taken. `None`
    /// means either (a) `source` is `brokered`/`managed` and no backup was
    /// ever needed, or (b) `target_path` did not exist before this mutation
    /// — in which case uninstall should *delete* `target_path` rather than
    /// restore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
    /// Free-form structured detail, e.g. `{"server_key": "slack-mcp",
    /// "mcp_key": "mcpServers"}` so a precise removal can be performed
    /// without re-deriving it from `slug` alone.
    #[serde(default = "default_detail")]
    pub detail: serde_json::Value,
}

fn default_detail() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

impl JournalEntry {
    /// Start building a new entry. `ts` defaults to "now" in RFC 3339
    /// (`Z`-suffixed, no fractional seconds) — override with
    /// [`JournalEntry::with_ts`] only in tests.
    pub fn new(op: JournalOp, source: JournalSource, target_path: impl Into<String>) -> Self {
        Self {
            op,
            ts: now_iso(),
            source,
            slug: None,
            client: None,
            target_path: target_path.into(),
            backup_path: None,
            detail: default_detail(),
        }
    }

    pub fn with_slug(mut self, slug: impl Into<String>) -> Self {
        self.slug = Some(slug.into());
        self
    }

    pub fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(client.into());
        self
    }

    pub fn with_backup_path(mut self, backup_path: impl Into<String>) -> Self {
        self.backup_path = Some(backup_path.into());
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = ts.into();
        self
    }
}

/// RFC 3339 timestamp, `Z`-suffixed, second precision — matches the schema
/// example in the restore-journal contract.
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Filesystem-safe timestamp (no colons) for `restore-backups/<ts>/`
/// directory names — mirrors the convention already used by the F1 migrator's
/// `.vectorhawk-backup/<ts>/` run directories.
pub fn new_backup_ts() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H%M%SZ").to_string()
}

// ── RestoreJournal ───────────────────────────────────────────────────────────

/// Handle onto the restore journal + backup area rooted at `<root_dir>`.
///
/// Cheap to construct — holds only a path. Safe to create fresh at every call
/// site rather than threading a shared instance around.
#[derive(Debug, Clone)]
pub struct RestoreJournal {
    root_dir: Utf8PathBuf,
}

impl RestoreJournal {
    pub fn new(root_dir: impl Into<Utf8PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    /// Construct from a daemon/CLI `AppState`.
    pub fn for_state(state: &AppState) -> Self {
        Self {
            root_dir: state.root_dir.clone(),
        }
    }

    /// `<root_dir>/restore-journal.json`
    pub fn journal_path(&self) -> Utf8PathBuf {
        self.root_dir.join("restore-journal.json")
    }

    fn lock_path(&self) -> Utf8PathBuf {
        self.root_dir.join("restore-journal.json.lock")
    }

    /// `<root_dir>/restore-backups/<ts>/`
    pub fn backup_dir_for(&self, ts: &str) -> Utf8PathBuf {
        self.root_dir.join("restore-backups").join(ts)
    }

    /// Back up a file or directory at `source` into `backup_dir_for(ts)`,
    /// preserving its file/dir name, and return the resulting backup path.
    ///
    /// Idempotent-ish: if the backup destination already exists it is left
    /// alone and its path is returned unchanged (callers that want a fresh
    /// snapshot should pass a fresh `ts`).
    pub fn backup_path_for(&self, ts: &str, source: &Utf8Path) -> Result<Utf8PathBuf> {
        let dest_dir = self.backup_dir_for(ts);
        fs::create_dir_all(dest_dir.as_std_path())
            .with_context(|| format!("restore_journal: failed to create backup dir {dest_dir}"))?;

        let name = source
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("restore_journal: source has no file name: {source}"))?;
        let dest = dest_dir.join(name);

        if dest.exists() {
            return Ok(dest);
        }

        if source.is_dir() {
            copy_dir_recursive(source.as_std_path(), dest.as_std_path()).with_context(|| {
                format!("restore_journal: failed to back up directory {source} -> {dest}")
            })?;
        } else {
            fs::copy(source.as_std_path(), dest.as_std_path()).with_context(|| {
                format!("restore_journal: failed to back up file {source} -> {dest}")
            })?;
        }

        Ok(dest)
    }

    /// Append one entry to the journal.
    ///
    /// Acquires an exclusive lock on `<root_dir>/restore-journal.json.lock`
    /// for the whole read-modify-write cycle so the daemon and CLI can both
    /// append concurrently without corrupting the file. Writes via temp file
    /// + atomic rename, and sets `0600` permissions on the journal file.
    ///
    /// A pre-existing corrupt/partial journal does not prevent the append —
    /// unreadable entries are dropped (see [`RestoreJournal::read_all`]) and
    /// the file is rewritten clean with the new entry included.
    pub fn append(&self, entry: JournalEntry) -> Result<()> {
        fs::create_dir_all(self.root_dir.as_std_path()).with_context(|| {
            format!(
                "restore_journal: failed to create root dir {}",
                self.root_dir
            )
        })?;

        let lock_file = self.open_lock_file()?;
        // Fully-qualified: `fs2::FileExt::lock_shared`/`lock_exclusive` share a
        // name with `std::fs::File`'s own locking methods stabilised in Rust
        // 1.89 (shared only — std's exclusive method is named `lock`, not
        // `lock_exclusive`, so no collision there). Without qualifying, method
        // resolution silently prefers the inherent std method on newer
        // toolchains, which is above this workspace's 1.75 MSRV.
        fs2::FileExt::lock_exclusive(&lock_file).with_context(|| {
            format!(
                "restore_journal: failed to acquire exclusive lock: {}",
                self.lock_path()
            )
        })?;

        let mut entries = self.read_entries_unlocked();
        entries.push(entry);
        let result = self.write_entries_unlocked(&entries);

        // fs2 releases the lock when `lock_file` drops; make the release
        // point explicit regardless of write outcome.
        drop(lock_file);
        result
    }

    /// Read all entries, tolerating a corrupt/partial file.
    ///
    /// Only genuine I/O errors (e.g. permission denied opening the lock
    /// file) propagate as `Err`. A missing journal returns `Ok(vec![])`.
    /// Malformed JSON content is salvaged best-effort; entries that cannot
    /// be recovered are skipped and logged rather than failing the read.
    pub fn read_all(&self) -> Result<Vec<JournalEntry>> {
        if !self.lock_path().as_std_path().exists() && !self.journal_path().as_std_path().exists() {
            return Ok(Vec::new());
        }
        let lock_file = self.open_lock_file()?;
        // See the comment on the `lock_exclusive` call above — must be
        // fully-qualified to avoid resolving to std's (MSRV 1.89+) method.
        fs2::FileExt::lock_shared(&lock_file).with_context(|| {
            format!(
                "restore_journal: failed to acquire shared lock: {}",
                self.lock_path()
            )
        })?;
        let entries = self.read_entries_unlocked();
        drop(lock_file);
        Ok(entries)
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn open_lock_file(&self) -> Result<fs::File> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // The lock file is used only for flock — never truncate any
            // advisory data another process might (in future) store there.
            .truncate(false)
            .open(self.lock_path().as_std_path())
            .with_context(|| {
                format!(
                    "restore_journal: failed to open lock file: {}",
                    self.lock_path()
                )
            })
    }

    /// Read + lenient-parse the journal file. Must be called while holding
    /// at least a shared lock. Never errors — an unreadable or absent file
    /// is treated as an empty journal.
    fn read_entries_unlocked(&self) -> Vec<JournalEntry> {
        let path = self.journal_path();
        let text = match fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        parse_lenient(&text)
    }

    /// Serialize `entries` and atomically replace the journal file. Must be
    /// called while holding the exclusive lock.
    fn write_entries_unlocked(&self, entries: &[JournalEntry]) -> Result<()> {
        let json = serde_json::to_vec_pretty(entries)
            .context("restore_journal: failed to serialise journal entries")?;

        let tmp_path = self
            .root_dir
            .join(format!("restore-journal.json.{}.tmp", std::process::id()));
        fs::write(tmp_path.as_std_path(), &json)
            .with_context(|| format!("restore_journal: failed to write tmp journal {tmp_path}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(tmp_path.as_std_path(), fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restore_journal: failed to chmod {tmp_path}"))?;
        }

        fs::rename(tmp_path.as_std_path(), self.journal_path().as_std_path()).with_context(
            || {
                format!(
                    "restore_journal: failed to rename tmp journal into place: {}",
                    self.journal_path()
                )
            },
        )?;

        Ok(())
    }
}

// ── Lenient parsing ──────────────────────────────────────────────────────────

/// Parse journal file content as leniently as possible: a single malformed
/// entry (or, in the worst case, a truncated/corrupt file) must never lose
/// the rest of the journal.
fn parse_lenient(text: &str) -> Vec<JournalEntry> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    // Fast path: the whole file is a valid JSON array. Still validate each
    // element independently so one entry with an unexpected shape doesn't
    // sink the rest.
    if let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(text) {
        return values
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<JournalEntry>(v) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!(error = %e, "restore_journal: skipping malformed entry");
                    None
                }
            })
            .collect();
    }

    // Slow path: the file itself is not valid JSON (truncated write,
    // external corruption, hand-editing gone wrong). Salvage whatever
    // top-level `{...}` objects can be found by brace-matching rather than
    // discarding the whole journal.
    tracing::warn!("restore_journal: journal file is not valid JSON — attempting salvage");
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = matching_brace(bytes, i) {
                let candidate = &text[i..=end];
                match serde_json::from_str::<JournalEntry>(candidate) {
                    Ok(e) => out.push(e),
                    Err(e) => {
                        tracing::warn!(error = %e, "restore_journal: skipping unrecoverable entry during salvage");
                    }
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Find the index of the `}` matching the `{` at `start`, honoring JSON
/// string escaping so braces inside string values don't confuse depth
/// counting.
fn matching_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (idx, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

// ── Recursive copy ───────────────────────────────────────────────────────────

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create backup dest: {}", dest.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read dir for backup: {}", src.display()))?
    {
        let entry = entry.context("failed to read dir entry during backup")?;
        let entry_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat: {}", entry_path.display()))?;

        if meta.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vh-restore-journal-{label}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temp path should be UTF-8")
    }

    fn cleanup(path: &Utf8Path) {
        let _ = fs::remove_dir_all(path.as_std_path());
    }

    // ── append / read_all round-trip ──────────────────────────────────────────

    #[test]
    fn append_and_read_all_round_trips() {
        let root = temp_root("roundtrip");
        let journal = RestoreJournal::new(root.clone());

        assert!(
            journal.read_all().unwrap().is_empty(),
            "no journal file yet -> empty, not an error"
        );

        let entry = JournalEntry::new(
            JournalOp::ConfigEdit,
            JournalSource::Native,
            "/x/.claude.json",
        )
        .with_slug("vectorhawk")
        .with_client("Claude Code")
        .with_backup_path("/x/backup/claude.json")
        .with_detail(serde_json::json!({"server_key": "vectorhawk", "mcp_key": "mcpServers"}));
        journal.append(entry.clone()).unwrap();

        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], entry);

        // Journal file must be 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(journal.journal_path().as_std_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        cleanup(&root);
    }

    #[test]
    fn append_accumulates_multiple_entries_in_order() {
        let root = temp_root("accumulate");
        let journal = RestoreJournal::new(root.clone());

        for i in 0..5 {
            journal
                .append(JournalEntry::new(
                    JournalOp::ArtifactPush,
                    JournalSource::Managed,
                    format!("/x/target-{i}"),
                ))
                .unwrap();
        }

        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 5);
        for (i, e) in all.iter().enumerate() {
            assert_eq!(e.target_path, format!("/x/target-{i}"));
        }

        cleanup(&root);
    }

    // ── corrupt-entry tolerance ────────────────────────────────────────────────

    #[test]
    fn read_all_skips_malformed_entries_in_otherwise_valid_array() {
        let root = temp_root("corrupt-entry");
        fs::create_dir_all(root.as_std_path()).unwrap();
        let journal = RestoreJournal::new(root.clone());

        let raw = r#"[
            {"op": "config_edit", "ts": "2026-07-19T12:00:00Z", "source": "native", "target_path": "/a"},
            {"this_is": "not a journal entry at all"},
            {"op": "artifact_push", "ts": "2026-07-19T12:01:00Z", "source": "managed", "target_path": "/b"}
        ]"#;
        fs::write(journal.journal_path().as_std_path(), raw).unwrap();

        let all = journal.read_all().unwrap();
        assert_eq!(
            all.len(),
            2,
            "malformed middle entry should be skipped, not fatal"
        );
        assert_eq!(all[0].target_path, "/a");
        assert_eq!(all[1].target_path, "/b");

        cleanup(&root);
    }

    #[test]
    fn read_all_salvages_entries_from_truncated_file() {
        let root = temp_root("truncated");
        fs::create_dir_all(root.as_std_path()).unwrap();
        let journal = RestoreJournal::new(root.clone());

        // Whole-array parse fails (missing closing bracket / truncated last
        // object), but the first two complete objects should be salvaged.
        let raw = r#"[
            {"op": "config_edit", "ts": "2026-07-19T12:00:00Z", "source": "native", "target_path": "/a"},
            {"op": "file_delete", "ts": "2026-07-19T12:01:00Z", "source": "brokered", "target_path": "/b"},
            {"op": "file_replace", "ts": "2026-07-19T12:02:00"#;
        fs::write(journal.journal_path().as_std_path(), raw).unwrap();

        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 2, "should salvage the two complete entries");
        assert_eq!(all[0].target_path, "/a");
        assert_eq!(all[1].target_path, "/b");

        cleanup(&root);
    }

    #[test]
    fn read_all_on_total_garbage_returns_empty_not_error() {
        let root = temp_root("garbage");
        fs::create_dir_all(root.as_std_path()).unwrap();
        let journal = RestoreJournal::new(root.clone());
        fs::write(
            journal.journal_path().as_std_path(),
            b"\x00\x01not json at all###",
        )
        .unwrap();

        let all = journal.read_all();
        assert!(all.is_ok(), "corrupt journal must never fail the read");
        assert!(all.unwrap().is_empty());

        cleanup(&root);
    }

    #[test]
    fn append_after_corruption_recovers_and_continues() {
        let root = temp_root("recover");
        fs::create_dir_all(root.as_std_path()).unwrap();
        let journal = RestoreJournal::new(root.clone());
        fs::write(journal.journal_path().as_std_path(), b"{{{garbage").unwrap();

        journal
            .append(JournalEntry::new(
                JournalOp::FileDelete,
                JournalSource::Brokered,
                "/x/y",
            ))
            .unwrap();

        let all = journal.read_all().unwrap();
        assert_eq!(
            all.len(),
            1,
            "append must succeed and not block on corruption"
        );
        assert_eq!(all[0].target_path, "/x/y");

        cleanup(&root);
    }

    // ── backup + restore of a config file ─────────────────────────────────────

    #[test]
    fn backup_path_for_copies_file_content() {
        let root = temp_root("backup-file");
        let journal = RestoreJournal::new(root.clone());

        let source_dir = temp_root("backup-file-source");
        fs::create_dir_all(source_dir.as_std_path()).unwrap();
        let source_file = source_dir.join("claude.json");
        fs::write(source_file.as_std_path(), r#"{"mcpServers":{}}"#).unwrap();

        let ts = "2026-07-19T120000Z";
        let backup_path = journal.backup_path_for(ts, &source_file).unwrap();

        assert_eq!(backup_path, journal.backup_dir_for(ts).join("claude.json"));
        assert_eq!(
            fs::read_to_string(backup_path.as_std_path()).unwrap(),
            r#"{"mcpServers":{}}"#
        );

        // "Restore" = the consumer copies backup_path back over target_path.
        // Simulate the target being mutated, then restored from the backup.
        fs::write(
            source_file.as_std_path(),
            r#"{"mcpServers":{"vectorhawk":{}}}"#,
        )
        .unwrap();
        fs::copy(backup_path.as_std_path(), source_file.as_std_path()).unwrap();
        assert_eq!(
            fs::read_to_string(source_file.as_std_path()).unwrap(),
            r#"{"mcpServers":{}}"#,
            "file should be back to its pre-mutation content"
        );

        cleanup(&root);
        cleanup(&source_dir);
    }

    #[test]
    fn backup_path_for_directory_copies_recursively() {
        let root = temp_root("backup-dir");
        let journal = RestoreJournal::new(root.clone());

        let source_dir = temp_root("backup-dir-source");
        fs::create_dir_all(source_dir.join("my-skill").join("prompts").as_std_path()).unwrap();
        fs::write(
            source_dir.join("my-skill").join("SKILL.md").as_std_path(),
            "---\nname: my-skill\n---\n",
        )
        .unwrap();
        fs::write(
            source_dir
                .join("my-skill")
                .join("prompts")
                .join("p.txt")
                .as_std_path(),
            "hello",
        )
        .unwrap();

        let ts = "2026-07-19T130000Z";
        let backup_path = journal
            .backup_path_for(ts, &source_dir.join("my-skill"))
            .unwrap();

        assert!(backup_path.join("SKILL.md").as_std_path().exists());
        assert_eq!(
            fs::read_to_string(backup_path.join("prompts").join("p.txt").as_std_path()).unwrap(),
            "hello"
        );

        cleanup(&root);
        cleanup(&source_dir);
    }

    #[test]
    fn backup_path_for_is_idempotent_for_same_ts() {
        let root = temp_root("backup-idempotent");
        let journal = RestoreJournal::new(root.clone());

        let source_dir = temp_root("backup-idempotent-source");
        fs::create_dir_all(source_dir.as_std_path()).unwrap();
        let source_file = source_dir.join("claude.json");
        fs::write(source_file.as_std_path(), "original").unwrap();

        let ts = "2026-07-19T140000Z";
        let first = journal.backup_path_for(ts, &source_file).unwrap();

        // Mutate the source after the first backup — a second call with the
        // same ts must NOT overwrite the already-captured original.
        fs::write(source_file.as_std_path(), "mutated").unwrap();
        let second = journal.backup_path_for(ts, &source_file).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(first.as_std_path()).unwrap(), "original");

        cleanup(&root);
        cleanup(&source_dir);
    }

    // ── concurrent append under lock ──────────────────────────────────────────

    #[test]
    fn concurrent_appends_do_not_lose_entries() {
        let root = temp_root("concurrent");
        fs::create_dir_all(root.as_std_path()).unwrap();

        let mut handles = Vec::new();
        for i in 0..12 {
            let root_clone = root.clone();
            handles.push(std::thread::spawn(move || {
                let journal = RestoreJournal::new(root_clone);
                journal
                    .append(JournalEntry::new(
                        JournalOp::ArtifactPush,
                        JournalSource::Managed,
                        format!("/concurrent/target-{i}"),
                    ))
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let journal = RestoreJournal::new(root.clone());
        let all = journal.read_all().unwrap();
        assert_eq!(
            all.len(),
            12,
            "every concurrent append must survive — got {} entries",
            all.len()
        );

        let mut targets: Vec<&str> = all.iter().map(|e| e.target_path.as_str()).collect();
        targets.sort_unstable();
        let mut expected: Vec<String> =
            (0..12).map(|i| format!("/concurrent/target-{i}")).collect();
        expected.sort();
        assert_eq!(targets, expected);

        cleanup(&root);
    }

    // ── JournalSource::should_restore ─────────────────────────────────────────

    #[test]
    fn should_restore_matches_native_and_adopted_only() {
        assert!(JournalSource::Native.should_restore());
        assert!(JournalSource::Adopted.should_restore());
        assert!(!JournalSource::Brokered.should_restore());
        assert!(!JournalSource::Managed.should_restore());
    }

    // ── serde shape (fixed contract) ──────────────────────────────────────────

    #[test]
    fn entry_serializes_with_contract_field_names() {
        let entry = JournalEntry::new(
            JournalOp::ArtifactPush,
            JournalSource::Brokered,
            "/x/.claude.json",
        )
        .with_slug("slack-mcp")
        .with_client("Claude Code")
        .with_detail(serde_json::json!({"server_key": "slack-mcp", "mcp_key": "mcpServers"}));

        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["op"], "artifact_push");
        assert_eq!(v["source"], "brokered");
        assert_eq!(v["slug"], "slack-mcp");
        assert_eq!(v["client"], "Claude Code");
        assert_eq!(v["target_path"], "/x/.claude.json");
        assert_eq!(v["detail"]["server_key"], "slack-mcp");
        assert_eq!(v["detail"]["mcp_key"], "mcpServers");
        assert!(
            v.get("backup_path").is_none(),
            "None backup_path should be omitted, not null"
        );
    }
}
