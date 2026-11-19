//! Merging a catalog update into a branch, and sharing a branch (plan 4.5.3,
//! AUTO-MANAGE Phase 9b).
//!
//! A catalog skill the user edited is a BRANCH. When the catalog moves on, the
//! server computes the three-way merge (`merge-preview`). Here:
//!
//! - [`prepare_merge`] (`scenery skill merge <slug>`) writes the merged tree,
//!   conflict markers included, to `<data dir>/merges/<slug>/`. That folder is
//!   outside every skill root, so the running skill is untouched. Binary and
//!   delete/modify conflicts keep MINE at the path and put THEIRS beside it as
//!   `<path>.theirs`.
//! - [`continue_merge`] (`--continue`) refuses while a conflict marker or a
//!   `.theirs` sidecar remains, then uploads the tree as a new version based on
//!   the catalog head. With `apply` (`--apply`) it also swaps the merged tree
//!   into the live skill, after copying the live tree to
//!   `<data dir>/merges/<slug>.pre-merge-<ts>/`.
//! - [`handle_merge_event`] is the portal's clean "Merge update": the server
//!   already stored the merged version, the runner downloads it and applies it
//!   the same way. In a strict mode (`quarantine`, `approve_required`) a branch
//!   does not run, so nothing is applied: the tree is only saved under
//!   `merges/`.
//! - [`share_branch`] uploads the current tree (when it has unsynced edits) and
//!   submits the branch for IT review.
//!
//! Nothing here ever overwrites unsynced edits: a live tree that differs from
//! what was last uploaded is left alone and the merge result is only saved.

use super::local_edits;
use super::marker::{self, ManagedPathMarker};
use anyhow::{bail, Context, Result};
use base64::Engine;
use rusqlite::Connection;
use scenery_core::{
    pack,
    registry::{AuthoringReply, LocalVersionRequest, MergePreview, RegistryClient},
    safe_copy, skill_tree,
    state::AppState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tracing::{info, warn};

const MARKER_FILE: &str = ".scenery-managed.json";
const THEIRS_SUFFIX: &str = ".theirs";
/// More than this many bytes in a merge result is refused.
const MAX_MERGE_BYTES: usize = 20 * 1024 * 1024;

/// What `prepare_merge` recorded next to the merge tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeMeta {
    pub skill_id: String,
    pub slug: String,
    pub base_version: Option<String>,
    pub head_version: Option<String>,
    pub branch_version_id: Option<String>,
    /// Text files that were written with conflict markers.
    #[serde(default)]
    pub conflicted_files: Vec<String>,
    /// Set once `--continue` uploaded the tree.
    #[serde(default)]
    pub uploaded_version: Option<String>,
    #[serde(default)]
    pub uploaded_version_id: Option<String>,
    #[serde(default)]
    pub uploaded_tree: Option<String>,
    /// Paths with a `.theirs` sidecar when the merge was written.
    #[serde(default)]
    pub sidecar_files: Vec<String>,
    /// Tree hash of the merge folder as `prepare_merge` wrote it; a different
    /// hash later means the user worked in it.
    #[serde(default)]
    pub written_tree: Option<String>,
}

/// `<data dir>/merges`.
pub fn merges_root(state: &AppState) -> PathBuf {
    state.root_dir.join("merges").into_std_path_buf()
}

/// `<data dir>/merges/<slug>`, the merge tree.
pub fn merge_dir(state: &AppState, slug: &str) -> PathBuf {
    merges_root(state).join(slug)
}

fn meta_path(state: &AppState, slug: &str) -> PathBuf {
    merges_root(state).join(format!("{slug}.meta.json"))
}

pub fn load_meta(state: &AppState, slug: &str) -> Option<MergeMeta> {
    serde_json::from_slice(&fs::read(meta_path(state, slug)).ok()?).ok()
}

fn save_meta(state: &AppState, slug: &str, meta: &MergeMeta) -> Result<()> {
    let p = meta_path(state, slug);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, serde_json::to_vec_pretty(meta)?)
        .with_context(|| format!("could not write {}", p.display()))
}

fn check_slug(slug: &str) -> Result<()> {
    scenery_core::safe_path::validate_component("slug", slug).map_err(|e| anyhow::anyhow!("{e}"))
}

// ── Pure helpers (tested without a registry) ─────────────────────────────────

/// A relative path from the wire as safe components, or `None`.
fn safe_rel(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Result of writing a merge tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrittenMerge {
    pub files: usize,
    /// Text files holding conflict markers.
    pub conflicted_text: Vec<String>,
    /// Paths with a `.theirs` sidecar (binary, delete/modify, add/add).
    pub sidecar_conflicts: Vec<String>,
}

impl WrittenMerge {
    pub fn conflicts(&self) -> Vec<String> {
        let mut v = self.conflicted_text.clone();
        v.extend(self.sidecar_conflicts.iter().cloned());
        v
    }
}

/// Write `preview`'s files into `dest` (replacing what was there). Paths are
/// validated; nothing leaves `dest`.
pub fn write_merge_tree(dest: &Path, preview: &MergePreview) -> Result<WrittenMerge> {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut staged: Vec<(PathBuf, Vec<u8>, bool)> = Vec::new();
    let mut out = WrittenMerge::default();
    let mut total = 0usize;
    for f in &preview.files {
        let rel = safe_rel(&f.path)
            .with_context(|| format!("the registry sent an unsafe path: {:?}", f.path))?;
        if rel.file_name().is_some_and(|n| n == MARKER_FILE) {
            continue;
        }
        let bytes = if f.encoding == "base64" {
            engine
                .decode(f.content.as_bytes())
                .with_context(|| format!("bad base64 for {}", f.path))?
        } else {
            f.content.clone().into_bytes()
        };
        total += bytes.len();
        if total > MAX_MERGE_BYTES {
            bail!("the merge result is larger than {MAX_MERGE_BYTES} bytes");
        }
        let shown = rel.to_string_lossy().replace('\\', "/");
        match f.conflict.as_deref() {
            Some("text") => out.conflicted_text.push(shown.clone()),
            Some(_) if f.sidecar || shown.ends_with(THEIRS_SUFFIX) => {}
            Some(_) => out.sidecar_conflicts.push(shown.clone()),
            None => {}
        }
        staged.push((rel, bytes, f.executable));
    }
    // A sidecar entry marks its main path as conflicted even if the server did
    // not flag the main entry.
    for (rel, _, _) in &staged {
        let s = rel.to_string_lossy().replace('\\', "/");
        if let Some(main) = s.strip_suffix(THEIRS_SUFFIX) {
            if !out.sidecar_conflicts.iter().any(|c| c == main) {
                out.sidecar_conflicts.push(main.to_string());
            }
        }
    }
    out.conflicted_text.sort();
    out.sidecar_conflicts.sort();

    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("could not clear the old merge at {}", dest.display()))?;
    }
    fs::create_dir_all(dest)?;
    for (rel, bytes, exec) in staged {
        let target = dest.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &bytes)?;
        set_exec(&target, exec);
        out.files += 1;
    }
    Ok(out)
}

#[cfg(unix)]
fn set_exec(path: &Path, exec: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_exec(_path: &Path, _exec: bool) {}

/// Files of `dir` (relative, `/` separated) that still hold a conflict. Only
/// the files the merge recorded as conflicted are looked at: a text file in
/// `text_conflicted` with a `<<<<<<<` / `=======` / `>>>>>>>` line, and a path in
/// `sidecar_conflicted` whose `.theirs` copy still exists. Other files are
/// never inspected (a doc may legitimately contain marker-like lines).
pub fn remaining_conflicts(
    dir: &Path,
    text_conflicted: &[String],
    sidecar_conflicted: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    for shown in text_conflicted {
        let Some(rel) = safe_rel(shown) else { continue };
        let Ok(bytes) = fs::read(dir.join(rel)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let hit = text.lines().any(|l| {
            l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l.trim_end() == "======="
        });
        if hit {
            out.push(shown.clone());
        }
    }
    for main in sidecar_conflicted {
        let theirs = format!("{main}{THEIRS_SUFFIX}");
        let Some(rel) = safe_rel(&theirs) else {
            continue;
        };
        if fs::symlink_metadata(dir.join(rel)).is_ok() {
            out.push(theirs);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Before a merge folder is replaced: when the user worked in it (its tree is
/// not what was written or uploaded), move it to
/// `merges/<slug>.abandoned-<ts>` instead of deleting it. `Some(path)` when it
/// was moved.
fn preserve_unfinished_merge(state: &AppState, slug: &str) -> Result<Option<PathBuf>> {
    let dir = merge_dir(state, slug);
    if !dir.is_dir() {
        return Ok(None);
    }
    let baseline = load_meta(state, slug).and_then(|m| m.uploaded_tree.or(m.written_tree));
    let now = skill_tree::survey(&dir).ok().map(|s| s.tree_hash);
    if baseline.is_some() && baseline == now {
        return Ok(None);
    }
    let dest = merges_root(state).join(format!("{slug}.abandoned-{}", stamp()));
    fs::rename(&dir, &dest).with_context(|| {
        format!(
            "could not keep your unfinished merge at {} before replacing it",
            dir.display()
        )
    })?;
    warn!(slug, saved = %dest.display(), "skill merge: unfinished merge saved aside before it was replaced");
    Ok(Some(dest))
}

/// Replace the contents of `live` with `src`, keeping `live`'s Scenery marker
/// file. The old tree is copied to `backup` first (never lost), the swap is
/// two renames, and the live path never holds a half-written tree.
pub fn swap_tree(live: &Path, src: &Path, backup: &Path) -> Result<()> {
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent)?;
    }
    safe_copy::copy_tree(live, backup)
        .with_context(|| format!("could not back up {}", live.display()))?;
    let parent = live.parent().context("skill dir has no parent")?;
    let name = live
        .file_name()
        .context("skill dir has no name")?
        .to_string_lossy()
        .to_string();
    let token = safe_copy::unique_token();
    let stage = parent.join(format!(".{name}.merge-stage-{token}"));
    let old = parent.join(format!(".{name}.merge-old-{token}"));
    safe_copy::copy_tree(src, &stage).context("could not stage the merged tree")?;
    let marker = live.join(MARKER_FILE);
    if fs::symlink_metadata(&marker).is_ok() {
        fs::copy(&marker, stage.join(MARKER_FILE)).context("could not carry the marker over")?;
    }
    fs::rename(live, &old).context("could not move the live skill aside")?;
    if let Err(e) = fs::rename(&stage, live) {
        let _ = fs::rename(&old, live);
        let _ = fs::remove_dir_all(&stage);
        return Err(anyhow::anyhow!(e)).context("could not put the merged tree in place");
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

fn stamp() -> String {
    scenery_core::timefmt::now_rfc3339()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn skill_md_sha(dir: &Path) -> String {
    fs::read(dir.join("SKILL.md"))
        .map(|b| hex::encode(Sha256::digest(&b)))
        .unwrap_or_default()
}

// ── Locating the skill ───────────────────────────────────────────────────────

/// A catalog skill on this device: its marker row (absent when a strict mode
/// quarantined it), registry id and local branch state.
#[derive(Debug, Clone)]
pub struct LocalSkill {
    pub slug: String,
    pub skill_id: String,
    pub marker: Option<ManagedPathMarker>,
    pub state: local_edits::LocalState,
    pub authorship: local_edits::Authorship,
}

/// Find a skill by folder name or registry id: a marker row first, else a
/// recorded local state (a quarantined strict-mode branch has no marker).
pub fn find_local_skill(state: &AppState, name: &str) -> Option<LocalSkill> {
    let conn = Connection::open(&state.db_path).ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT path, kind, slug, installation_id, source_sha256, migrated_at \
             FROM managed_path_markers WHERE kind = 'skill'",
        )
        .ok()?;
    let rows: Vec<ManagedPathMarker> = stmt
        .query_map([], |row| {
            Ok(ManagedPathMarker {
                path: row.get(0)?,
                kind: row.get(1)?,
                slug: row.get(2)?,
                installation_id: row.get(3)?,
                source_sha256: row.get(4)?,
                migrated_at: row.get(5)?,
            })
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    for m in rows {
        let prov = marker::marker_provenance(&conn, &m.path);
        let rid = local_edits::registry_id_for(state, &m, &prov);
        if m.slug == name || rid == name {
            let st = local_edits::load(&conn, &rid).unwrap_or_default();
            let authorship = local_edits::authorship(&conn, &rid, &prov);
            return Some(LocalSkill {
                slug: m.slug.clone(),
                skill_id: rid,
                marker: Some(m),
                state: st,
                authorship,
            });
        }
    }
    // No marker: a recorded state keyed by the registry id (or its local name).
    let rid = state
        .registry_name_for_local(name)
        .unwrap_or_else(|| name.to_string());
    let st = local_edits::load(&conn, &rid)?;
    let authorship = match st.authorship.as_deref() {
        Some(local_edits::AUTHORSHIP_OWN) => local_edits::Authorship::Own,
        _ => local_edits::Authorship::Catalog,
    };
    Some(LocalSkill {
        slug: name.to_string(),
        skill_id: rid,
        marker: None,
        state: st,
        authorship,
    })
}

/// Does this skill hold a local branch whose live tree differs from what was
/// last uploaded (so installing a registry version over it would lose edits)?
pub fn has_unsynced_branch_edits(state: &AppState, skill_id: &str) -> bool {
    let Some(local) = find_local_skill(state, skill_id) else {
        return false;
    };
    if local.state.branch_state != local_edits::BRANCH_BRANCH {
        return false;
    }
    let (Some(marker), Some(uploaded)) = (local.marker, local.state.uploaded_tree) else {
        return false;
    };
    match skill_tree::survey(Path::new(&marker.path)) {
        Ok(s) => s.tree_hash != uploaded,
        Err(_) => false,
    }
}

fn is_strict(state: &AppState) -> bool {
    matches!(
        state
            .get_sync_state("managed_paths_mode")
            .ok()
            .flatten()
            .as_deref(),
        Some("quarantine") | Some("approve_required")
    )
}

/// A registry client builder plus this device's id. The blocking HTTP client
/// must be created and dropped inside `spawn_blocking`, never in async code.
struct Conn {
    url: String,
    token: String,
}

impl Conn {
    fn client(&self) -> RegistryClient {
        RegistryClient::new(self.url.clone()).with_auth(self.token.clone())
    }
}

fn client_and_device(state: &AppState, registry_url: &str) -> Result<(Conn, String)> {
    let token = scenery_core::auth::load_tokens(state, registry_url)
        .ok()
        .flatten()
        .map(|t| t.access_token)
        .filter(|t| !t.is_empty())
        .context("not signed in; run `scenery auth login`")?;
    let device = state
        .get_sync_state("device_id")
        .ok()
        .flatten()
        .context("this device is not paired yet")?;
    Ok((
        Conn {
            url: registry_url.to_string(),
            token,
        },
        device,
    ))
}

fn refusal_error(what: &str, r: &scenery_core::registry::AuthoringRefusal) -> anyhow::Error {
    let hint = match (r.status, r.code.as_str()) {
        (426, _) => " (this runner is too old for the registry; run `brew upgrade scenery`)",
        (401, _) => " (sign in again with `scenery auth login`)",
        (409, "not_a_branch") => " (this skill has no branch of yours: edit it first)",
        _ => "",
    };
    let msg = if r.message.is_empty() {
        &r.code
    } else {
        &r.message
    };
    anyhow::anyhow!("{what} refused ({}): {msg}{hint}", r.status)
}

// ── scenery skill merge ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The branch is already based on the catalog head.
    UpToDate { head_version: Option<String> },
    /// Written to `dir`; `conflicts` is empty for a clean merge.
    Written {
        dir: PathBuf,
        head_version: Option<String>,
        conflicts: Vec<String>,
        /// Where an earlier unfinished merge of yours was kept, if it was replaced.
        abandoned: Option<PathBuf>,
    },
}

/// `scenery skill merge <slug>`: fetch the server's merge and write it to
/// `merges/<slug>/`.
pub async fn prepare_merge(
    state: &Arc<AppState>,
    registry_url: &str,
    slug: &str,
) -> Result<MergeOutcome> {
    check_slug(slug)?;
    let local = find_local_skill(state, slug)
        .with_context(|| format!("'{slug}' is not a skill Scenery manages on this device"))?;
    if local.state.branch_state != local_edits::BRANCH_BRANCH {
        bail!(
            "'{slug}' has no branch of yours to merge (it is not edited, or it is your own skill)"
        );
    }
    let (client, device) = client_and_device(state, registry_url)?;
    let sid = local.skill_id.clone();
    let preview = tokio::task::spawn_blocking(move || client.client().merge_preview(&sid, &device))
        .await
        .context("merge task failed")??;
    let preview = match preview {
        AuthoringReply::Done(p) => p,
        AuthoringReply::Refused(r) => return Err(refusal_error("merge", &r)),
    };
    if preview.up_to_date {
        return Ok(MergeOutcome::UpToDate {
            head_version: preview.head_version,
        });
    }
    let dir = merge_dir(state, slug);
    let abandoned = preserve_unfinished_merge(state, slug)?;
    let written = write_merge_tree(&dir, &preview)?;
    let conflicts = written.conflicts();
    let meta = MergeMeta {
        skill_id: local.skill_id.clone(),
        slug: slug.to_string(),
        base_version: preview.base_version.clone(),
        head_version: preview.head_version.clone(),
        branch_version_id: preview.branch_version_id.clone(),
        conflicted_files: written.conflicted_text.clone(),
        sidecar_files: written.sidecar_conflicts.clone(),
        written_tree: skill_tree::survey(&dir).ok().map(|s| s.tree_hash),
        ..Default::default()
    };
    save_meta(state, slug, &meta)?;
    info!(
        slug,
        conflicts = conflicts.len(),
        "skill merge: merge tree written"
    );
    Ok(MergeOutcome::Written {
        dir,
        head_version: preview.head_version,
        conflicts,
        abandoned,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueOutcome {
    pub version: Option<String>,
    /// Where the pre-merge tree went, when the merge was applied.
    pub applied_backup: Option<PathBuf>,
    /// Applied to the live skill?
    pub applied: bool,
    /// Why it was not applied although asked (strict mode, unsynced edits...).
    pub not_applied_reason: Option<String>,
}

/// `scenery skill merge <slug> --continue [--apply]`.
pub async fn continue_merge(
    state: &Arc<AppState>,
    registry_url: &str,
    slug: &str,
    apply: bool,
) -> Result<ContinueOutcome> {
    check_slug(slug)?;
    let mut meta = load_meta(state, slug).with_context(|| {
        format!("no merge in progress for '{slug}'; run `scenery skill merge {slug}`")
    })?;
    let dir = merge_dir(state, slug);
    if !dir.is_dir() {
        bail!(
            "the merge folder {} is gone; run `scenery skill merge {slug}` again",
            dir.display()
        );
    }
    let left = remaining_conflicts(&dir, &meta.conflicted_files, &meta.sidecar_files);
    if !left.is_empty() {
        bail!(
            "conflicts remain in {}: {}. Resolve them (remove the <<<<<<< / ======= / >>>>>>> lines, \
             or choose a file and delete its .theirs copy), then run this again.",
            dir.display(),
            left.join(", ")
        );
    }
    let mut version = meta.uploaded_version.clone();
    let tree_now = skill_tree::survey(&dir)?.tree_hash;
    if meta.uploaded_tree.as_deref() != Some(tree_now.as_str()) || version.is_none() {
        match skill_tree::scan_secrets(&dir) {
            Ok(hits) if !hits.is_empty() => bail!(
                "the merged tree holds files that look like secrets ({}); remove them first",
                hits.join(", ")
            ),
            Ok(_) => {}
            Err(e) => bail!("secret scan failed: {e:#}"),
        }
        let tar_gz = pack::pack_dir_to_tar_gz_no_links(&dir).context("could not pack the merge")?;
        let (client, device) = client_and_device(state, registry_url)?;
        let (sid, th, head) = (
            meta.skill_id.clone(),
            tree_now.clone(),
            meta.head_version.clone(),
        );
        let reply = tokio::task::spawn_blocking(move || {
            client.client().upload_local_version(LocalVersionRequest {
                tar_gz,
                skill_id: &sid,
                tree_hash: &th,
                device_id: &device,
                base_version: head.as_deref(),
            })
        })
        .await
        .context("upload task failed")??;
        let resp = match reply {
            AuthoringReply::Done(r) => r,
            AuthoringReply::Refused(r) => return Err(refusal_error("upload", &r)),
        };
        if resp.outcome != "stored" {
            bail!(
                "the registry did not accept the merged tree: {}",
                resp.reason.unwrap_or(resp.outcome)
            );
        }
        version = resp.version.clone();
        meta.uploaded_version = resp.version;
        meta.uploaded_version_id = resp.version_id;
        meta.uploaded_tree = Some(tree_now);
        save_meta(state, slug, &meta)?;
    }
    if !apply {
        return Ok(ContinueOutcome {
            version,
            applied_backup: None,
            applied: false,
            not_applied_reason: None,
        });
    }
    let applied = apply_merge_tree(state, registry_url, slug, &dir, &meta).await?;
    Ok(ContinueOutcome {
        version,
        applied: applied.0.is_some(),
        applied_backup: applied.0,
        not_applied_reason: applied.1,
    })
}

/// Swap `tree` into the live skill. `(Some(backup), None)` on success,
/// `(None, Some(reason))` when it must not (or cannot) be applied.
async fn apply_merge_tree(
    state: &Arc<AppState>,
    registry_url: &str,
    slug: &str,
    tree: &Path,
    meta: &MergeMeta,
) -> Result<(Option<PathBuf>, Option<String>)> {
    if is_strict(state) {
        return Ok((
            None,
            Some(
                "this organization's policy does not let a branch run; the merge is saved only"
                    .into(),
            ),
        ));
    }
    let Some(local) = find_local_skill(state, slug) else {
        return Ok((None, Some("the skill is not installed here".into())));
    };
    let Some(marker) = local.marker.clone() else {
        return Ok((
            None,
            Some("the skill has no managed folder on this device".into()),
        ));
    };
    // The merge was computed from one branch version. If the branch moved since
    // (a newer synced upload), applying would drop that newer work.
    if meta.branch_version_id.is_none() || meta.branch_version_id != local.state.local_version_id {
        return Ok((
            None,
            Some(
                "your branch changed since this merge was computed; the merge is saved only. \
                 Run `scenery skill merge` again"
                    .into(),
            ),
        ));
    }
    let live = PathBuf::from(&marker.path);
    // Unsynced edits (the live tree is not what was last uploaded) are never
    // overwritten.
    let live_hash = skill_tree::survey(&live)
        .map(|s| s.tree_hash)
        .unwrap_or_default();
    let synced = local
        .state
        .uploaded_tree
        .as_deref()
        .is_some_and(|u| u == live_hash)
        || {
            let conn = Connection::open(&state.db_path).ok();
            conn.map(|c| {
                marker::marker_provenance(&c, &marker.path)
                    .tree_hash
                    .as_deref()
                    == Some(live_hash.as_str())
            })
            .unwrap_or(false)
        };
    if !synced {
        return Ok((
            None,
            Some(
                "the live skill has edits that were not uploaded yet; they were left alone".into(),
            ),
        ));
    }
    let backup = merges_root(state).join(format!("{slug}.pre-merge-{}", stamp()));
    let (l, t, b) = (live.clone(), tree.to_path_buf(), backup.clone());
    tokio::task::spawn_blocking(move || swap_tree(&l, &t, &b))
        .await
        .context("apply task failed")??;
    // Re-baseline the marker and the local state on what is now live.
    let new_tree = skill_tree::survey(&live)?.tree_hash;
    let new_md = skill_md_sha(&live);
    let (st, sid, mp) = (
        Arc::clone(state),
        local.skill_id.clone(),
        marker.path.clone(),
    );
    let m = meta.clone();
    let installation_id = tokio::task::spawn_blocking(move || -> Option<String> {
        let conn = Connection::open(&st.db_path).ok()?;
        if let Err(e) = marker::set_tree_baseline(&conn, &mp, &new_tree, &new_md) {
            warn!(error = %e, "skill merge: could not re-baseline the marker");
        }
        let _ = conn.execute(
            "INSERT INTO skill_local_state \
               (skill_id, authorship, branch_state, base_version, local_version, local_version_id, uploaded_tree, held_tree, updated_at) \
             VALUES (?1, 'catalog', 'branch', ?2, ?3, ?4, ?5, NULL, strftime('%s','now')) \
             ON CONFLICT(skill_id) DO UPDATE SET branch_state = 'branch', \
               base_version = excluded.base_version, local_version = excluded.local_version, \
               local_version_id = excluded.local_version_id, uploaded_tree = excluded.uploaded_tree, \
               held_tree = NULL, updated_at = excluded.updated_at",
            rusqlite::params![
                sid,
                m.head_version,
                m.uploaded_version,
                m.uploaded_version_id,
                new_tree
            ],
        );
        conn.query_row(
            "SELECT installation_id FROM managed_path_markers WHERE path = ?1",
            [&mp],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten();
    local_edits::report_branch(state, registry_url, installation_id, &local.skill_id).await;
    info!(slug, backup = %backup.display(), "skill merge: merged tree applied to the live skill");
    Ok((Some(backup), None))
}

// ── SSE: the portal's clean "Merge update" ───────────────────────────────────

/// `skill_merge` event payload.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct MergeEvent {
    pub skill_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
    pub version_id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub base_version: Option<String>,
    #[serde(default)]
    pub head_version: Option<String>,
    /// The branch version this merge was computed from.
    #[serde(default)]
    pub branch_version_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventOutcome {
    /// Merged tree applied to the live skill.
    Applied { backup: PathBuf },
    /// Downloaded to `merges/<slug>/` only.
    SavedOnly { dir: PathBuf, reason: String },
    /// Not for this device or not a branch here.
    Ignored { reason: String },
}

/// Handle a `skill_merge` event: download the merged version, save it under
/// `merges/<slug>/`, and apply it unless that would clobber edits or a strict
/// mode forbids running a branch.
pub async fn handle_merge_event(
    state: &Arc<AppState>,
    registry_url: &str,
    ev: MergeEvent,
) -> Result<EventOutcome> {
    let own_device = state.get_sync_state("device_id").ok().flatten();
    if let (Some(target), Some(me)) = (ev.device_id.as_deref(), own_device.as_deref()) {
        if target != me {
            return Ok(EventOutcome::Ignored {
                reason: "for another device".into(),
            });
        }
    }
    let Some(local) = find_local_skill(state, &ev.skill_id) else {
        return Ok(EventOutcome::Ignored {
            reason: "skill not on this device".into(),
        });
    };
    if local.state.branch_state != local_edits::BRANCH_BRANCH {
        return Ok(EventOutcome::Ignored {
            reason: "no branch here".into(),
        });
    }
    let slug = local.slug.clone();
    check_slug(&slug)?;
    let (client, _) = client_and_device(state, registry_url)?;
    let (sid, vid) = (local.skill_id.clone(), ev.version_id.clone());
    let reply = tokio::task::spawn_blocking(move || {
        client.client().download_local_version_source(&sid, &vid)
    })
    .await
    .context("download task failed")??;
    let bytes = match reply {
        AuthoringReply::Done(b) => b,
        AuthoringReply::Refused(r) => return Err(refusal_error("download", &r)),
    };
    let dir = merge_dir(state, &slug);
    preserve_unfinished_merge(state, &slug)?;
    let (d, b) = (dir.clone(), bytes);
    tokio::task::spawn_blocking(move || unpack_source(&b, &d))
        .await
        .context("unpack task failed")??;
    let tree_hash = skill_tree::survey(&dir)?.tree_hash;
    let meta = MergeMeta {
        skill_id: local.skill_id.clone(),
        slug: slug.clone(),
        base_version: ev.base_version.clone(),
        head_version: ev.head_version.clone(),
        branch_version_id: ev.branch_version_id.clone(),
        conflicted_files: vec![],
        sidecar_files: vec![],
        written_tree: None,
        uploaded_version: ev.version.clone(),
        uploaded_version_id: Some(ev.version_id.clone()),
        uploaded_tree: Some(tree_hash),
    };
    save_meta(state, &slug, &meta)?;
    match apply_merge_tree(state, registry_url, &slug, &dir, &meta).await? {
        (Some(backup), _) => Ok(EventOutcome::Applied { backup }),
        (None, reason) => Ok(EventOutcome::SavedOnly {
            dir,
            reason: reason.unwrap_or_default(),
        }),
    }
}

/// Unpack a version's source tar.gz into `dest` (replacing it). A single
/// wrapper directory is stripped when `SKILL.md` is not at the top.
pub fn unpack_source(tar_gz: &[u8], dest: &Path) -> Result<()> {
    let parent = dest.parent().context("no parent")?;
    fs::create_dir_all(parent)?;
    let token = safe_copy::unique_token();
    let archive = parent.join(format!(
        ".{}.src-{token}.tar.gz",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    let stage = parent.join(format!(
        ".{}.unpack-{token}",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&archive, tar_gz)?;
    let utf = |p: &Path| {
        camino::Utf8PathBuf::from_path_buf(p.to_path_buf())
            .map_err(|p| anyhow::anyhow!("non-UTF-8 path {}", p.display()))
    };
    let res = (|| -> Result<()> {
        scenery_core::safe_unpack::unpack_cskill_archive(&utf(&archive)?, &utf(&stage)?)?;
        let mut root = stage.clone();
        if !root.join("SKILL.md").is_file() {
            let kids: Vec<_> = fs::read_dir(&stage)?.flatten().collect();
            if kids.len() == 1
                && kids[0].path().is_dir()
                && kids[0].path().join("SKILL.md").is_file()
            {
                root = kids[0].path();
            }
        }
        if !root.join("SKILL.md").is_file() {
            bail!("the downloaded version has no SKILL.md");
        }
        if fs::symlink_metadata(dest).is_ok() {
            fs::remove_dir_all(dest)?;
        }
        fs::rename(&root, dest)?;
        Ok(())
    })();
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&stage);
    res
}

// ── Uninstall: keep unsynced branch edits ────────────────────────────────────

/// What [`save_unsynced_branch_edits`] did for one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedBranch {
    pub slug: String,
    /// Where the copy is (`<dest_root>/branches/<slug>`), or `None` when the
    /// copy failed (see `error`).
    pub copy_at: Option<PathBuf>,
    pub error: Option<String>,
}

/// Before a catalog skill is removed: when its branch has edits the registry
/// does not have (live tree differs from the last uploaded tree), copy the live
/// tree to `<dest_root>/branches/<slug>/`. A synced branch needs no backup; own
/// skills and skills without a branch are skipped (own skills have their own
/// rule in uninstall). `only` limits it to one folder name.
pub fn save_unsynced_branch_edits(
    state: &AppState,
    dest_root: &Path,
    only: Option<&str>,
) -> Vec<SavedBranch> {
    let mut out = Vec::new();
    let Ok(conn) = Connection::open(&state.db_path) else {
        return out;
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT path, slug FROM managed_path_markers WHERE kind = 'skill'")
    else {
        return out;
    };
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    for (path, slug) in rows {
        if only.is_some_and(|o| o != slug) {
            continue;
        }
        let prov = marker::marker_provenance(&conn, &path);
        if prov.is_auto_managed() || prov.is_builtin() {
            continue;
        }
        let m = ManagedPathMarker {
            path: path.clone(),
            kind: "skill".into(),
            slug: slug.clone(),
            installation_id: None,
            source_sha256: String::new(),
            migrated_at: String::new(),
        };
        let rid = local_edits::registry_id_for(state, &m, &prov);
        let Some(ls) = local_edits::load(&conn, &rid) else {
            continue;
        };
        if ls.branch_state != local_edits::BRANCH_BRANCH
            || ls.authorship.as_deref() == Some(local_edits::AUTHORSHIP_OWN)
        {
            continue;
        }
        let live = Path::new(&path);
        let Ok(tree) = skill_tree::survey(live).map(|s| s.tree_hash) else {
            continue;
        };
        if ls.uploaded_tree.as_deref() == Some(tree.as_str()) {
            continue; // synced: the registry holds exactly this tree
        }
        let mut dest = dest_root.join("branches").join(&slug);
        if fs::symlink_metadata(&dest).is_ok() {
            dest = dest_root
                .join("branches")
                .join(format!("{slug}-{}", safe_copy::unique_token()));
        }
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match safe_copy::copy_tree(live, &dest) {
            Ok(_) => {
                let _ = fs::remove_file(dest.join(MARKER_FILE));
                out.push(SavedBranch {
                    slug,
                    copy_at: Some(dest),
                    error: None,
                });
            }
            Err(e) => out.push(SavedBranch {
                slug,
                copy_at: None,
                error: Some(format!("{e:#}")),
            }),
        }
    }
    out
}

// ── scenery skill share (catalog branch) ─────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    pub request_id: String,
    pub status: String,
    pub version: String,
}

/// Submit this device's branch of a catalog skill to IT. Uploads the current
/// tree first when it has edits that were never uploaded.
pub async fn share_branch(
    state: &Arc<AppState>,
    registry_url: &str,
    slug: &str,
    notes: Option<&str>,
) -> Result<ShareOutcome> {
    check_slug(slug)?;
    let local = find_local_skill(state, slug)
        .with_context(|| format!("'{slug}' is not a skill Scenery manages on this device"))?;
    if local.authorship == local_edits::Authorship::Own {
        bail!("'{slug}' is your own skill, not a catalog branch");
    }
    if let Some(marker) = &local.marker {
        let tree = skill_tree::survey(Path::new(&marker.path))?.tree_hash;
        let synced = local.state.uploaded_tree.as_deref() == Some(tree.as_str())
            && local.state.branch_state == local_edits::BRANCH_BRANCH;
        if !synced {
            match local_edits::upload_edit(state, registry_url, marker, true).await {
                local_edits::EditOutcome::Stored { branch_state, .. }
                    if branch_state == local_edits::BRANCH_BRANCH => {}
                // The same tree is already stored as the branch: just submit it.
                local_edits::EditOutcome::AlreadyUploaded => {}
                local_edits::EditOutcome::Stored { .. } => {
                    bail!("'{slug}' is not a branch of a catalog skill (the registry treats it as your own)")
                }
                local_edits::EditOutcome::Held { reason } => {
                    bail!("the edits were not uploaded: {reason}")
                }
                local_edits::EditOutcome::Settling => {
                    bail!("the skill was just edited; try again in a few seconds")
                }
                local_edits::EditOutcome::Retry { reason }
                | local_edits::EditOutcome::Failed { reason } => {
                    bail!("could not upload the edits: {reason}")
                }
            }
        }
    } else if local.state.branch_state != local_edits::BRANCH_BRANCH {
        bail!("'{slug}' has no branch of yours to share");
    }
    let (client, device) = client_and_device(state, registry_url)?;
    let (sid, n) = (local.skill_id.clone(), notes.map(str::to_string));
    let reply = tokio::task::spawn_blocking(move || {
        client
            .client()
            .submit_change_request(&sid, &device, n.as_deref())
    })
    .await
    .context("submit task failed")??;
    match reply {
        AuthoringReply::Done(cr) => Ok(ShareOutcome {
            request_id: cr.id,
            status: cr.status,
            version: cr.version,
        }),
        AuthoringReply::Refused(r) if r.code == "needs_rebase" => bail!(
            "the catalog moved on since you branched. Run `scenery skill merge {slug}` first, \
             then share again"
        ),
        AuthoringReply::Refused(r) if r.code == "already_pending" => {
            bail!("this version of '{slug}' is already waiting for IT review")
        }
        AuthoringReply::Refused(r) => Err(refusal_error("submit", &r)),
    }
}

#[cfg(test)]
#[path = "skill_merge_tests.rs"]
mod tests;
