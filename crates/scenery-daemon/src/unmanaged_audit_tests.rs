#![allow(clippy::unwrap_used)]

use super::*;

fn server(client: &str, name: &str) -> UnmanagedServer {
    UnmanagedServer {
        server_name: name.to_string(),
        config_path: format!("/home/u/.{client}.json"),
        client_name: client.to_string(),
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
    let st = AppState::bootstrap_in(root).unwrap();
    (tmp, st)
}

fn names(v: &[&UnmanagedServer]) -> Vec<String> {
    v.iter().map(|s| s.server_name.clone()).collect()
}

#[test]
fn an_unchanged_set_is_reported_once() {
    let (_t, st) = state();
    let cur = vec![server("claude", "github"), server("cursor", "linear")];
    assert_eq!(names(&newly_seen(&st, &cur)), vec!["github", "linear"]);
    assert!(newly_seen(&st, &cur).is_empty());
    assert!(newly_seen(&st, &cur).is_empty());
}

#[test]
fn only_the_new_server_is_reported_when_one_is_added() {
    let (_t, st) = state();
    let mut cur = vec![server("claude", "github")];
    newly_seen(&st, &cur);
    cur.push(server("claude", "notes"));
    assert_eq!(names(&newly_seen(&st, &cur)), vec!["notes"]);
}

#[test]
fn a_server_that_comes_back_is_reported_again() {
    let (_t, st) = state();
    let cur = vec![server("claude", "github")];
    newly_seen(&st, &cur);
    assert!(newly_seen(&st, &[]).is_empty());
    assert_eq!(names(&newly_seen(&st, &cur)), vec!["github"]);
}

#[test]
fn the_same_name_in_another_client_is_a_different_server() {
    let (_t, st) = state();
    newly_seen(&st, &[server("claude", "github")]);
    let cur = vec![server("claude", "github"), server("cursor", "github")];
    let fresh = newly_seen(&st, &cur);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].client_name, "cursor");
}

#[test]
fn the_reported_set_survives_a_restart() {
    let (_t, st) = state();
    let cur = vec![server("claude", "github")];
    newly_seen(&st, &cur);
    // A fresh handle on the same state dir (a daemon restart).
    let again = AppState::bootstrap_in(st.root_dir.clone()).unwrap();
    assert!(newly_seen(&again, &cur).is_empty());
}
