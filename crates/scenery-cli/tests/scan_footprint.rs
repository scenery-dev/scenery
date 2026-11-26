//! `scenery scan --once` leaves no footprint: the fixture HOME is byte-for-byte
//! and mtime-for-mtime unchanged after a dry run and after a real upload, no
//! `Scenery` data dir appears, and no child process outlives the run.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const TOKEN: &str = "scn_scan_FOOTPRINT0123456789";

/// (size, mtime seconds, mtime nanos, mode) for every path under `root`.
type Snapshot = BTreeMap<PathBuf, (u64, i64, i64, u32)>;

fn snapshot(root: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let m = fs::symlink_metadata(&p).unwrap();
        let rel = p.strip_prefix(root).unwrap().to_path_buf();
        out.insert(rel, (m.len(), m.mtime(), m.mtime_nsec(), m.mode()));
        if m.is_dir() {
            walk(root, &p, out);
        }
    }
}

fn build_home(home: &Path) {
    let skill = home.join(".agents/skills/notes");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: notes\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","some-server"]}}}"#,
    )
    .unwrap();
}

/// Live members of process group `pgid` (the scan ran in its own group, so
/// anything it spawned and left behind shows up here after it exits).
fn group_members(pgid: u32) -> String {
    let out = Command::new("pgrep")
        .args(["-g", &pgid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Runs the scan in its own process group; returns the group id.
fn run_scan(home: &Path, tmpdir: &Path, registry: &str, dry_run: bool) -> u32 {
    let out_path = tmpdir.join("scan.stdout");
    let err_path = tmpdir.join("scan.stderr");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scenery"));
    cmd.args(["scan", "--once", "--registry-url", registry]);
    if dry_run {
        cmd.arg("--dry-run");
    }
    let mut child = cmd
        .process_group(0)
        // Files, not pipes: a leftover child holding a pipe would make the
        // wait block until it exits and hide it from the group check.
        .stdout(fs::File::create(&out_path).unwrap())
        .stderr(fs::File::create(&err_path).unwrap())
        .env("HOME", home)
        .env("TMPDIR", tmpdir)
        .env("SCENERY_DISABLE_KEYCHAIN", "1")
        .env("SCENERY_SCAN_TOKEN", TOKEN)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .spawn()
        .unwrap();
    let pgid = child.id(); // process_group(0): the child leads its own group
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        fs::read_to_string(&out_path).unwrap_or_default(),
        fs::read_to_string(&err_path).unwrap_or_default()
    );
    pgid
}

#[test]
fn scan_leaves_no_footprint() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let tmpdir = tmp.path().join("tmp");
    fs::create_dir_all(&tmpdir).unwrap();
    build_home(&home);
    let before = snapshot(&home);

    let mut server = mockito::Server::new();
    let auth = format!("Bearer {TOKEN}");
    let _session = server
        .mock("GET", "/api/scan/session")
        .match_header("authorization", auth.as_str())
        .with_status(200)
        .with_body(
            r#"{"org_name":"Acme","attribution":"token","user_email":null,
                "auto_submit_mcp":false,"max_items":5000}"#,
        )
        .create();
    let post = server
        .mock("POST", "/api/scan/reports")
        .match_header("authorization", auth.as_str())
        .with_status(200)
        .with_body(
            r#"{"device_id":"d1","accepted":2,"deduped":0,"rejected":[],
                "mcp_submitted":0,"inventory_url":"https://x.test/inv"}"#,
        )
        .expect(1)
        .create();

    let pgid = run_scan(&home, &tmpdir, &server.url(), true);
    assert_eq!(snapshot(&home), before, "HOME changed after --dry-run");
    assert!(
        group_members(pgid).is_empty(),
        "process left after --dry-run"
    );

    let pgid = run_scan(&home, &tmpdir, &server.url(), false);
    post.assert();
    assert_eq!(snapshot(&home), before, "HOME changed after upload");
    assert!(
        !snapshot(&home)
            .keys()
            .any(|p| p.components().any(|c| c.as_os_str() == "Scenery")),
        "a Scenery data dir was created"
    );
    assert!(group_members(pgid).is_empty(), "process left after upload");
}

/// The leftover check is not vacuous: a background child that outlives its
/// parent (same group) is seen by `group_members`.
#[test]
fn group_check_detects_a_leftover_child() {
    let child = Command::new("sh")
        .args(["-c", "sleep 30 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = child.id();
    let mut child = child;
    child.wait().unwrap();
    let seen = group_members(pgid);
    let _ = Command::new("pkill")
        .args(["-g", &pgid.to_string()])
        .status();
    assert!(!seen.is_empty(), "orphaned sleep was not detected");
}
