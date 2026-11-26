//! `scenery scan --once` as a real process: the org scan token never shows up
//! in argv, stdout, stderr (even at `RUST_LOG=trace`) or the saved report.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

const TOKEN: &str = "scn_scan_CLISECRET0123456789";

fn all_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Run the binary against a server whose upload fails, so the report is saved.
fn run_scan(use_file: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let tmpdir = tmp.path().join("tmp");
    fs::create_dir_all(home.join(".agents/skills/notes")).unwrap();
    fs::write(
        home.join(".agents/skills/notes/SKILL.md"),
        "---\nname: notes\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    fs::create_dir_all(&tmpdir).unwrap();
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
        .with_status(401)
        .with_body(r#"{"detail":{"code":"scan_token_revoked"}}"#)
        .expect(1)
        .create();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scenery"));
    cmd.args(["scan", "--once", "--registry-url", &server.url()])
        .env("HOME", &home)
        .env("TMPDIR", &tmpdir)
        .env("RUST_LOG", "trace")
        .env("SCENERY_DISABLE_KEYCHAIN", "1")
        .env_remove("XDG_DATA_HOME")
        .env_remove("SCENERY_SCAN_TOKEN");
    if use_file {
        let token_file = tmp.path().join("token");
        fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
        cmd.arg("--token-file").arg(&token_file);
    } else {
        cmd.env("SCENERY_SCAN_TOKEN", TOKEN);
    }
    let argv: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(argv.iter().all(|a| !a.contains(TOKEN)), "token in argv");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(!stdout.contains(TOKEN), "token in stdout");
    assert!(!stderr.contains(TOKEN), "token in stderr");
    post.assert();
    let mut saved = Vec::new();
    all_files(&tmpdir, &mut saved);
    let reports: Vec<_> = saved
        .iter()
        .filter(|p| p.to_string_lossy().contains("scenery-scan-"))
        .collect();
    assert_eq!(reports.len(), 1, "{saved:?}");
    for p in &saved {
        let body = fs::read(p).unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains(TOKEN),
            "token in {}",
            p.display()
        );
    }
}

#[test]
fn token_never_in_argv_or_output() {
    run_scan(false);
    run_scan(true);
}
