//! Phase 9b runner tests: merge-tree writing, conflict detection, applying a
//! merge, the SSE merge event, strict mode, sharing a branch, and the
//! uninstall copy of unsynced branch edits. Mock registry, fake `$HOME`.
#![allow(clippy::unwrap_used, clippy::await_holding_lock)]

use super::*;
use crate::managed_paths::{pusher::ManagedPathsPusher, ENV_MUTEX};
use mockito::{Matcher, Server, ServerGuard};
use scenery_core::registry::MergeFileEntry;

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    server: ServerGuard,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Fx {
    async fn new() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
        std::env::set_var("SCENERY_LOCAL_EDIT_SETTLE_SECS", "0");
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
        let state = Arc::new(AppState::bootstrap_in(root).unwrap());
        let server = Server::new_async().await;
        scenery_core::auth::save_tokens(&state, &server.url(), "tok", "refresh").unwrap();
        state.set_sync_state("device_id", "dev-1").unwrap();
        Self {
            _tmp: tmp,
            home,
            state,
            server,
            prev_home,
            _lock: lock,
        }
    }

    /// A catalog skill `cat` with a recorded, UPLOADED branch (live tree ==
    /// uploaded tree), base version 1.0.0.
    fn branch(&self) -> PathBuf {
        let dir = self.home.join(".agents/skills/cat");
        ManagedPathsPusher::new(&self.state)
            .push_skill(
                "cat",
                Some("inst-1"),
                b"---\nname: cat\ndescription: d\n---\noriginal",
                &[("prompts/p.md".to_string(), b"my branch".to_vec())],
            )
            .unwrap();
        self.record_branch(&dir);
        dir
    }

    fn record_branch(&self, dir: &Path) {
        let tree = skill_tree::survey(dir).unwrap().tree_hash;
        self.conn()
            .execute(
                "INSERT INTO skill_local_state (skill_id, authorship, branch_state, base_version, local_version, local_version_id, uploaded_tree, updated_at) \
                 VALUES ('cat','catalog','branch','1.0.0','1.0.0+local.x.1','vid-old',?1,0) \
                 ON CONFLICT(skill_id) DO UPDATE SET branch_state='branch', uploaded_tree=excluded.uploaded_tree",
                [tree],
            )
            .unwrap();
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.state.db_path).unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        std::env::remove_var("SCENERY_LOCAL_EDIT_SETTLE_SECS");
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn entry(path: &str, content: &str, conflict: Option<&str>, sidecar: bool) -> MergeFileEntry {
    MergeFileEntry {
        path: path.into(),
        executable: false,
        encoding: "utf8".into(),
        content: content.into(),
        conflict: conflict.map(str::to_string),
        sidecar,
    }
}

fn preview(files: Vec<MergeFileEntry>) -> MergePreview {
    MergePreview {
        skill_id: "cat".into(),
        base_version: Some("1.0.0".into()),
        head_version: Some("1.1.0".into()),
        branch_version_id: Some("vid-old".into()),
        branch_version: Some("1.0.0+local.x.1".into()),
        clean: files.iter().all(|f| f.conflict.is_none()),
        up_to_date: false,
        files,
    }
}

fn tar_gz(files: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut b = tar::Builder::new(enc);
        for (n, c) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(c.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, n, c.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }
    out
}

const CLEAN_SKILL_MD: &str = "---\nname: cat\ndescription: d\n---\nmerged body\n";
const CONFLICT_MD: &str =
    "---\nname: cat\ndescription: d\n---\n<<<<<<< mine\nA\n=======\nB\n>>>>>>> theirs\n";

// ── pure pieces ──────────────────────────────────────────────────────────────

#[test]
fn write_merge_tree_writes_markers_sidecars_and_exec_bits_and_rejects_unsafe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("merges/cat");
    let mut run = entry("scripts/run.sh", "#!/bin/sh\n", None, false);
    run.executable = true;
    let mut bin = entry("img.bin", "AAEC", Some("binary"), false);
    bin.encoding = "base64".into();
    let mut theirs = entry("img.bin.theirs", "AwQF", Some("binary"), true);
    theirs.encoding = "base64".into();
    let w = write_merge_tree(
        &dest,
        &preview(vec![
            entry("SKILL.md", CONFLICT_MD, Some("text"), false),
            run,
            bin,
            theirs,
        ]),
    )
    .unwrap();
    assert_eq!(w.files, 4);
    assert_eq!(w.conflicted_text, vec!["SKILL.md"]);
    assert_eq!(w.sidecar_conflicts, vec!["img.bin"]);
    assert_eq!(fs::read(dest.join("img.bin")).unwrap(), vec![0, 1, 2]);
    assert_eq!(
        fs::read(dest.join("img.bin.theirs")).unwrap(),
        vec![3, 4, 5]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = fs::metadata(dest.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o111, 0o111);
    }
    for bad in ["../x", "/etc/passwd", "a/../../x"] {
        assert!(
            write_merge_tree(&dest, &preview(vec![entry(bad, "x", None, false)])).is_err(),
            "{bad}"
        );
    }
    // A second write replaces the first completely.
    write_merge_tree(
        &dest,
        &preview(vec![entry("SKILL.md", CLEAN_SKILL_MD, None, false)]),
    )
    .unwrap();
    assert!(!dest.join("img.bin").exists());
}

#[test]
fn remaining_conflicts_only_looks_at_files_recorded_as_conflicted() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let text = vec!["SKILL.md".to_string()];
    let side = vec!["x.bin".to_string()];
    fs::write(d.join("SKILL.md"), CONFLICT_MD).unwrap();
    fs::write(d.join("x.bin.theirs"), [1u8, 2]).unwrap();
    // not recorded as conflicted: neither a marker-like doc nor a stray .theirs counts
    fs::write(d.join("notes.md"), "<<<<<<< not ours\n>>>>>>>\n").unwrap();
    fs::write(d.join("heading.md"), "Title\n=======\ntext\n").unwrap();
    fs::write(d.join("other.bin.theirs"), [3u8]).unwrap();
    let left = remaining_conflicts(d, &text, &side);
    assert_eq!(left, vec!["SKILL.md", "x.bin.theirs"]);
    // resolved: markers gone and sidecar deleted
    fs::write(d.join("SKILL.md"), CLEAN_SKILL_MD).unwrap();
    fs::remove_file(d.join("x.bin.theirs")).unwrap();
    assert!(remaining_conflicts(d, &text, &side).is_empty());
    // a leftover separator in a file that WAS conflicted still counts
    fs::write(d.join("SKILL.md"), "a\n=======\nb\n").unwrap();
    assert_eq!(remaining_conflicts(d, &text, &side), vec!["SKILL.md"]);
}

#[test]
fn swap_tree_backs_up_the_live_tree_keeps_the_marker_and_replaces_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("skills/cat");
    fs::create_dir_all(live.join("prompts")).unwrap();
    fs::write(live.join("SKILL.md"), "old").unwrap();
    fs::write(live.join("prompts/gone.md"), "gone").unwrap();
    fs::write(live.join(MARKER_FILE), "{\"marker\":1}").unwrap();
    let src = tmp.path().join("merge");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("SKILL.md"), "new").unwrap();
    let backup = tmp.path().join("merges/cat.pre-merge-1");
    swap_tree(&live, &src, &backup).unwrap();
    assert_eq!(fs::read_to_string(live.join("SKILL.md")).unwrap(), "new");
    assert!(!live.join("prompts/gone.md").exists());
    assert_eq!(
        fs::read_to_string(live.join(MARKER_FILE)).unwrap(),
        "{\"marker\":1}"
    );
    assert_eq!(
        fs::read_to_string(backup.join("prompts/gone.md")).unwrap(),
        "gone"
    );
    assert_eq!(fs::read_to_string(backup.join("SKILL.md")).unwrap(), "old");
    // no staging leftovers beside the live dir
    let names: Vec<_> = fs::read_dir(live.parent().unwrap())
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn unpack_source_strips_a_single_wrapper_dir_and_refuses_a_tree_without_skill_md() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("m/cat");
    unpack_source(
        &tar_gz(&[("cat/SKILL.md", "x"), ("cat/a/b.md", "y")]),
        &dest,
    )
    .unwrap();
    assert!(dest.join("SKILL.md").is_file() && dest.join("a/b.md").is_file());
    unpack_source(&tar_gz(&[("SKILL.md", "z")]), &dest).unwrap();
    assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "z");
    assert!(unpack_source(&tar_gz(&[("README.md", "z")]), &dest).is_err());
}

#[test]
fn the_skill_merge_event_parses_and_unsafe_ids_are_dropped() {
    use crate::sync::sse_client::{parse_sync_event, SyncEvent};
    let ok = r#"{"skill_id":"cat","device_id":"dev-1","version_id":"v1","version":"1.1.0+local.a.b","base_version":"1.1.0","head_version":"1.1.0"}"#;
    match parse_sync_event("skill_merge", ok).unwrap() {
        SyncEvent::SkillMerge { event } => {
            assert_eq!(event.skill_id, "cat");
            assert_eq!(event.head_version.as_deref(), Some("1.1.0"));
        }
        _ => panic!("wrong event"),
    }
    let bad = r#"{"skill_id":"../x","version_id":"v1"}"#;
    assert!(parse_sync_event("skill_merge", bad).is_err());
}

// ── merge flow against a mock registry ───────────────────────────────────────

#[tokio::test]
async fn merge_writes_conflicts_aside_continue_refuses_then_uploads_and_apply_swaps_with_a_backup()
{
    let mut f = Fx::new().await;
    let live = f.branch();
    let live_before = fs::read(live.join("SKILL.md")).unwrap();
    let url = f.server.url();
    let _mp = f
        .server
        .mock("GET", Matcher::Regex(r"^/runner/skills/cat/merge-preview\?device_id=dev-1$".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::to_string(&serde_json::json!({
                "skill_id":"cat","base_version":"1.0.0","head_version":"1.1.0",
                "branch_version_id":"vid-old","branch_version":"1.0.0+local.x.1",
                "clean":false,"up_to_date":false,
                "files":[
                  {"path":"SKILL.md","executable":false,"encoding":"utf8","content":CONFLICT_MD,"conflict":"text","sidecar":false},
                  {"path":"prompts/p.md","executable":false,"encoding":"utf8","content":"my branch","conflict":null,"sidecar":false}
                ]
            }))
            .unwrap(),
        )
        .create_async()
        .await;
    let out = prepare_merge(&f.state, &url, "cat").await.unwrap();
    let MergeOutcome::Written {
        dir,
        conflicts,
        head_version,
        ..
    } = out
    else {
        panic!("expected a written merge")
    };
    assert_eq!(conflicts, vec!["SKILL.md"]);
    assert_eq!(head_version.as_deref(), Some("1.1.0"));
    assert!(dir.starts_with(f.state.root_dir.as_std_path().join("merges")));
    // the running skill is untouched
    assert_eq!(fs::read(live.join("SKILL.md")).unwrap(), live_before);

    // --continue refuses while the markers remain
    let err = continue_merge(&f.state, &url, "cat", false)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("SKILL.md"), "{err}");

    // resolve, then upload with base_version = the catalog head
    fs::write(dir.join("SKILL.md"), CLEAN_SKILL_MD).unwrap();
    let up = f
        .server
        .mock("POST", "/runner/skills/cat/versions/local")
        .match_body(Matcher::Regex(r#"(?s)name="base_version"\r\n\r\n1\.1\.0"#.into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"outcome":"stored","skill_id":"cat","authorship":"catalog","branch_state":"branch","version":"1.1.0+local.x.2","version_id":"vid-new","head_version":"1.1.0","scan":{"verdict":"clean"}}"#)
        .expect(1)
        .create_async()
        .await;
    let _patch = f
        .server
        .mock("PATCH", "/api/installations/inst-1")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    let cont = continue_merge(&f.state, &url, "cat", false).await.unwrap();
    assert_eq!(cont.version.as_deref(), Some("1.1.0+local.x.2"));
    assert!(!cont.applied);
    assert_eq!(
        fs::read(live.join("SKILL.md")).unwrap(),
        live_before,
        "not applied without --apply"
    );

    // --apply: swap, backup, marker + local state re-baselined; no second upload
    let ap = continue_merge(&f.state, &url, "cat", true).await.unwrap();
    up.assert_async().await;
    assert!(ap.applied);
    let backup = ap.applied_backup.unwrap();
    assert_eq!(fs::read(backup.join("SKILL.md")).unwrap(), live_before);
    assert_eq!(
        fs::read_to_string(live.join("SKILL.md")).unwrap(),
        CLEAN_SKILL_MD
    );
    let ls = local_edits::load(&f.conn(), "cat").unwrap();
    assert_eq!(ls.base_version.as_deref(), Some("1.1.0"));
    assert_eq!(ls.local_version_id.as_deref(), Some("vid-new"));
    assert_eq!(
        ls.uploaded_tree.as_deref(),
        Some(skill_tree::survey(&live).unwrap().tree_hash.as_str())
    );
    assert!(!has_unsynced_branch_edits(&f.state, "cat"));
}

#[tokio::test]
async fn merge_refuses_a_skill_with_no_branch_and_reports_up_to_date() {
    let mut f = Fx::new().await;
    let url = f.server.url();
    assert!(prepare_merge(&f.state, &url, "nope").await.is_err());
    f.branch();
    let _mp = f
        .server
        .mock("GET", Matcher::Regex(r"^/runner/skills/cat/merge-preview".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"skill_id":"cat","head_version":"1.0.0","up_to_date":true,"clean":true,"files":[]}"#)
        .create_async()
        .await;
    assert_eq!(
        prepare_merge(&f.state, &url, "cat").await.unwrap(),
        MergeOutcome::UpToDate {
            head_version: Some("1.0.0".into())
        }
    );
    f.server.reset();
    let _nb = f
        .server
        .mock(
            "GET",
            Matcher::Regex(r"^/runner/skills/cat/merge-preview".into()),
        )
        .with_status(409)
        .with_body(r#"{"detail":{"code":"not_a_branch"}}"#)
        .create_async()
        .await;
    let err = prepare_merge(&f.state, &url, "cat").await.unwrap_err();
    assert!(format!("{err}").contains("not_a_branch"), "{err}");
}

fn event(f: &Fx) -> MergeEvent {
    let _ = f;
    MergeEvent {
        skill_id: "cat".into(),
        device_id: Some("dev-1".into()),
        version_id: "vid-m".into(),
        version: Some("1.1.0+local.x.9".into()),
        base_version: Some("1.1.0".into()),
        head_version: Some("1.1.0".into()),
        branch_version_id: Some("vid-old".into()),
    }
}

fn set_local_version_id(f: &Fx, id: &str) {
    f.conn()
        .execute(
            "UPDATE skill_local_state SET local_version_id = ?1 WHERE skill_id = 'cat'",
            [id],
        )
        .unwrap();
}

async fn mock_clean_preview(f: &mut Fx) {
    f.server
        .mock("GET", Matcher::Regex(r"^/runner/skills/cat/merge-preview\?device_id=dev-1$".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::to_string(&serde_json::json!({
                "skill_id":"cat","base_version":"1.0.0","head_version":"1.1.0",
                "branch_version_id":"vid-old","branch_version":"1.0.0+local.x.1",
                "clean":true,"up_to_date":false,
                "files":[
                  {"path":"SKILL.md","executable":false,"encoding":"utf8","content":CLEAN_SKILL_MD,"conflict":null,"sidecar":false},
                  {"path":"prompts/p.md","executable":false,"encoding":"utf8","content":"my branch","conflict":null,"sidecar":false}
                ]
            }))
            .unwrap(),
        )
        .create_async()
        .await;
}

#[tokio::test]
async fn continue_apply_refuses_when_the_branch_moved_since_the_merge_was_computed() {
    let mut f = Fx::new().await;
    let live = f.branch();
    let before = fs::read(live.join("SKILL.md")).unwrap();
    mock_clean_preview(&mut f).await;
    f.server
        .mock("POST", "/runner/skills/cat/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"outcome":"stored","skill_id":"cat","authorship":"catalog","branch_state":"branch","version":"1.1.0+local.x.2","version_id":"vid-new","head_version":"1.1.0","scan":{"verdict":"clean"}}"#)
        .create_async()
        .await;
    let url = f.server.url();
    prepare_merge(&f.state, &url, "cat").await.unwrap();
    // a newer, synced branch upload lands between prepare and apply
    set_local_version_id(&f, "vid-newer");
    let out = continue_merge(&f.state, &url, "cat", true).await.unwrap();
    assert!(!out.applied);
    let why = out.not_applied_reason.unwrap();
    assert!(why.contains("scenery skill merge"), "{why}");
    assert_eq!(fs::read(live.join("SKILL.md")).unwrap(), before);
    let ls = local_edits::load(&f.conn(), "cat").unwrap();
    assert_eq!(ls.local_version_id.as_deref(), Some("vid-newer"));
}

#[tokio::test]
async fn a_merge_event_is_saved_only_when_the_branch_moved_or_the_event_has_no_base() {
    for base in [None, Some("vid-old")] {
        let mut f = Fx::new().await;
        let live = f.branch();
        mock_source(&mut f).await;
        let before = fs::read(live.join("SKILL.md")).unwrap();
        set_local_version_id(&f, "vid-newer");
        let mut ev = event(&f);
        ev.branch_version_id = base.map(str::to_string);
        let url = f.server.url();
        let out = handle_merge_event(&f.state, &url, ev).await.unwrap();
        let EventOutcome::SavedOnly { reason, .. } = out else {
            panic!("expected saved only, got {out:?}")
        };
        assert!(reason.contains("scenery skill merge"), "{reason}");
        assert_eq!(fs::read(live.join("SKILL.md")).unwrap(), before);
    }
}

#[tokio::test]
async fn a_rerun_keeps_an_edited_unfinished_merge_aside_and_replaces_an_untouched_one() {
    let mut f = Fx::new().await;
    f.branch();
    mock_clean_preview(&mut f).await;
    let url = f.server.url();
    let first = prepare_merge(&f.state, &url, "cat").await.unwrap();
    let MergeOutcome::Written { dir, abandoned, .. } = first else {
        panic!("written")
    };
    assert!(abandoned.is_none());
    // untouched: replaced without a copy
    let again = prepare_merge(&f.state, &url, "cat").await.unwrap();
    let MergeOutcome::Written { abandoned, .. } = again else {
        panic!("written")
    };
    assert!(abandoned.is_none());
    // user work: kept aside
    fs::write(dir.join("prompts/p.md"), "my resolution").unwrap();
    let third = prepare_merge(&f.state, &url, "cat").await.unwrap();
    let MergeOutcome::Written { abandoned, .. } = third else {
        panic!("written")
    };
    let kept = abandoned.unwrap();
    assert!(kept.to_string_lossy().contains("cat.abandoned-"));
    assert_eq!(
        fs::read_to_string(kept.join("prompts/p.md")).unwrap(),
        "my resolution"
    );
    assert_eq!(
        fs::read_to_string(dir.join("prompts/p.md")).unwrap(),
        "my branch"
    );
}

#[tokio::test]
async fn a_merge_event_keeps_an_edited_unfinished_merge_aside() {
    let mut f = Fx::new().await;
    f.branch();
    mock_clean_preview(&mut f).await;
    mock_source(&mut f).await;
    let url = f.server.url();
    prepare_merge(&f.state, &url, "cat").await.unwrap();
    let dir = merge_dir(&f.state, "cat");
    fs::write(dir.join("prompts/p.md"), "half resolved").unwrap();
    handle_merge_event(&f.state, &url, event(&f)).await.unwrap();
    let kept: Vec<_> = fs::read_dir(merges_root(&f.state))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("cat.abandoned-")
        })
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read_to_string(kept[0].path().join("prompts/p.md")).unwrap(),
        "half resolved"
    );
}

async fn mock_source(f: &mut Fx) {
    f.server
        .mock("GET", "/runner/skills/cat/local-versions/vid-m/source")
        .with_status(200)
        .with_body(tar_gz(&[
            ("SKILL.md", CLEAN_SKILL_MD),
            ("prompts/p.md", "my branch"),
        ]))
        .create_async()
        .await;
    f.server
        .mock("PATCH", "/api/installations/inst-1")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
}

#[tokio::test]
async fn a_merge_event_applies_the_merged_version_after_backing_up_the_live_tree() {
    let mut f = Fx::new().await;
    let live = f.branch();
    mock_source(&mut f).await;
    let before = fs::read(live.join("SKILL.md")).unwrap();
    let url = f.server.url();
    let out = handle_merge_event(&f.state, &url, event(&f)).await.unwrap();
    let EventOutcome::Applied { backup } = out else {
        panic!("expected applied, got {out:?}")
    };
    assert_eq!(fs::read(backup.join("SKILL.md")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(live.join("SKILL.md")).unwrap(),
        CLEAN_SKILL_MD
    );
    let ls = local_edits::load(&f.conn(), "cat").unwrap();
    assert_eq!(ls.local_version_id.as_deref(), Some("vid-m"));
    assert_eq!(ls.base_version.as_deref(), Some("1.1.0"));
}

#[tokio::test]
async fn a_merge_event_never_overwrites_edits_that_were_not_uploaded_yet() {
    let mut f = Fx::new().await;
    let live = f.branch();
    mock_source(&mut f).await;
    fs::write(live.join("prompts/p.md"), "typed after the last upload").unwrap();
    let url = f.server.url();
    let out = handle_merge_event(&f.state, &url, event(&f)).await.unwrap();
    assert!(matches!(out, EventOutcome::SavedOnly { .. }), "{out:?}");
    assert_eq!(
        fs::read_to_string(live.join("prompts/p.md")).unwrap(),
        "typed after the last upload"
    );
    assert!(merge_dir(&f.state, "cat").join("SKILL.md").is_file());
    assert!(has_unsynced_branch_edits(&f.state, "cat"));
}

#[tokio::test]
async fn in_a_strict_mode_a_merge_event_is_saved_but_never_applied() {
    for mode in ["quarantine", "approve_required"] {
        let mut f = Fx::new().await;
        let live = f.branch();
        f.state.set_sync_state("managed_paths_mode", mode).unwrap();
        mock_source(&mut f).await;
        let before = fs::read(live.join("SKILL.md")).unwrap();
        let url = f.server.url();
        let out = handle_merge_event(&f.state, &url, event(&f)).await.unwrap();
        let EventOutcome::SavedOnly { dir, reason } = out else {
            panic!("{mode}: expected saved only")
        };
        assert!(reason.contains("policy"), "{mode}: {reason}");
        assert!(dir.join("SKILL.md").is_file());
        assert_eq!(fs::read(live.join("SKILL.md")).unwrap(), before, "{mode}");
    }
}

#[tokio::test]
async fn a_merge_event_for_another_device_or_a_skill_without_a_branch_is_ignored() {
    let f = Fx::new().await;
    f.branch();
    let url = f.server.url();
    let mut ev = event(&f);
    ev.device_id = Some("dev-other".into());
    assert!(matches!(
        handle_merge_event(&f.state, &url, ev).await.unwrap(),
        EventOutcome::Ignored { .. }
    ));
    f.conn()
        .execute(
            "UPDATE skill_local_state SET branch_state='none' WHERE skill_id='cat'",
            [],
        )
        .unwrap();
    assert!(matches!(
        handle_merge_event(&f.state, &url, event(&f)).await.unwrap(),
        EventOutcome::Ignored { .. }
    ));
}

// ── share ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn share_uploads_unsynced_edits_then_submits_and_maps_needs_rebase() {
    let mut f = Fx::new().await;
    let live = f.branch();
    let url = f.server.url();
    fs::write(live.join("prompts/p.md"), "new edit").unwrap();
    let up = f
        .server
        .mock("POST", "/runner/skills/cat/versions/local")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"outcome":"stored","skill_id":"cat","authorship":"catalog","branch_state":"branch","version":"1.0.0+local.x.3","version_id":"vid-3","head_version":"1.0.0","scan":{"verdict":"clean"}}"#)
        .expect(1)
        .create_async()
        .await;
    let _patch = f
        .server
        .mock("PATCH", "/api/installations/inst-1")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    let cr = f
        .server
        .mock("POST", "/runner/skills/cat/change-requests")
        .match_body(Matcher::PartialJson(serde_json::json!({"device_id":"dev-1","notes":"please"})))
        .with_status(201)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"cr-1","status":"pending","version":"1.0.0+local.x.3","base_version":"1.0.0"}"#)
        .expect(1)
        .create_async()
        .await;
    let out = share_branch(&f.state, &url, "cat", Some("please"))
        .await
        .unwrap();
    assert_eq!(
        (out.request_id.as_str(), out.status.as_str()),
        ("cr-1", "pending")
    );
    up.assert_async().await;
    cr.assert_async().await;

    // synced now: no second upload; a 409 needs_rebase becomes the merge hint
    f.server.reset();
    let _nr = f
        .server
        .mock("POST", "/runner/skills/cat/change-requests")
        .with_status(409)
        .with_body(
            r#"{"detail":{"code":"needs_rebase","head_version":"1.1.0","base_version":"1.0.0"}}"#,
        )
        .create_async()
        .await;
    let err = share_branch(&f.state, &url, "cat", None).await.unwrap_err();
    assert!(
        format!("{err}").contains("scenery skill merge cat"),
        "{err}"
    );
}

#[tokio::test]
async fn share_refuses_an_own_skill() {
    let f = Fx::new().await;
    f.branch();
    f.conn()
        .execute(
            "UPDATE skill_local_state SET authorship='own' WHERE skill_id='cat'",
            [],
        )
        .unwrap();
    let url = f.server.url();
    assert!(share_branch(&f.state, &url, "cat", None).await.is_err());
}

// ── uninstall: unsynced branch edits ─────────────────────────────────────────

#[tokio::test]
async fn uninstall_copies_unsynced_branch_edits_and_skips_synced_ones() {
    let f = Fx::new().await;
    let live = f.branch();
    let report = f.home.join("Scenery-uninstall-x");
    // synced: nothing to keep
    assert!(save_unsynced_branch_edits(&f.state, &report, None).is_empty());
    // unsynced edit: copied to branches/<slug>/, without the marker file
    fs::write(
        live.join("prompts/p.md"),
        "work that never reached the registry",
    )
    .unwrap();
    let saved = save_unsynced_branch_edits(&f.state, &report, None);
    assert_eq!(saved.len(), 1);
    let at = saved[0].copy_at.clone().unwrap();
    assert_eq!(at, report.join("branches/cat"));
    assert_eq!(
        fs::read_to_string(at.join("prompts/p.md")).unwrap(),
        "work that never reached the registry"
    );
    assert!(!at.join(MARKER_FILE).exists());
    // `only` filters; an own skill is never handled here
    assert!(save_unsynced_branch_edits(&f.state, &report, Some("other")).is_empty());
    f.conn()
        .execute(
            "UPDATE skill_local_state SET authorship='own' WHERE skill_id='cat'",
            [],
        )
        .unwrap();
    let report2 = f.home.join("Scenery-uninstall-y");
    assert!(save_unsynced_branch_edits(&f.state, &report2, None).is_empty());
}

#[tokio::test]
async fn uninstall_keeps_a_first_edit_the_scan_has_not_seen_yet() {
    let f = Fx::new().await;
    // Installed, never scanned: no skill_local_state row, branch_state unset.
    let live = f.branch();
    f.conn()
        .execute("DELETE FROM skill_local_state WHERE skill_id='cat'", [])
        .unwrap();
    let report = f.home.join("Scenery-uninstall-first");
    assert!(
        save_unsynced_branch_edits(&f.state, &report, None).is_empty(),
        "an untouched install needs no copy"
    );
    fs::write(live.join("prompts/p.md"), "first edit, never scanned").unwrap();
    let saved = save_unsynced_branch_edits(&f.state, &report, None);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let at = saved[0].copy_at.clone().unwrap();
    assert_eq!(at, report.join("branches/cat"));
    assert_eq!(
        fs::read_to_string(at.join("prompts/p.md")).unwrap(),
        "first edit, never scanned"
    );
}

// ── convergence: a cleared branch never installs over unsynced edits ─────────

#[tokio::test]
async fn a_cleared_branch_installs_the_catalog_version_only_when_no_edits_would_be_lost() {
    use crate::sync::sse_client::InstallationRecord;
    let f = Fx::new().await;
    let live = f.branch();
    let rec = || InstallationRecord {
        installation_id: uuid::Uuid::new_v4(),
        skill_id: "cat".into(),
        version: "1.1.0".into(),
        state: "desired".into(),
        source: None,
        local_slug: None,
        authorship: Some("catalog".into()),
        // the backend approved a change request and cleared the branch
        branch_state: Some("none".into()),
        base_version: None,
    };
    // synced branch: nothing would be lost, the new catalog version installs
    let st = Arc::clone(&f.state);
    let evs = tokio::task::spawn_blocking(move || {
        crate::sync::reconciler::build_derived_events_blocking(vec![rec()], &st)
    })
    .await
    .unwrap();
    assert_eq!(evs.len(), 1);
    // unsynced edit: the install waits until the edit is uploaded
    fs::write(live.join("prompts/p.md"), "unsaved work").unwrap();
    let st = Arc::clone(&f.state);
    let evs = tokio::task::spawn_blocking(move || {
        crate::sync::reconciler::build_derived_events_blocking(vec![rec()], &st)
    })
    .await
    .unwrap();
    assert!(evs.is_empty());
}
