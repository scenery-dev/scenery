//! The in-process secrets memo must never hide a change of tokens.
#![allow(clippy::unwrap_used)]

use super::*;

const URL: &str = "https://registry.example";

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
    let st = AppState::bootstrap_in(root).unwrap();
    (tmp, st)
}

fn access(st: &AppState) -> String {
    load_tokens(st, URL).unwrap().unwrap().access_token
}

/// What another process (`scenery auth pair`) does: new secrets plus a row
/// whose `saved_at` moves on.
fn external_save(st: &AppState, access: &str, saved_at: &str) {
    let blob = serde_json::to_string(&KeychainBlob {
        access_token: access,
        refresh_token: "r",
    })
    .unwrap();
    secret_file::write(st.root_dir.as_std_path(), &token_secret_name(URL), &blob).unwrap();
    Connection::open(&st.db_path)
        .unwrap()
        .execute(
            "UPDATE auth_tokens SET saved_at = ?1 WHERE registry_url = ?2",
            params![saved_at, URL],
        )
        .unwrap();
}

#[test]
fn repeated_loads_are_served_from_the_memo() {
    let (_t, st) = state();
    save_tokens(&st, URL, "a1", "r1").unwrap();
    assert_eq!(access(&st), "a1");
    // Change the stored secret behind the memo's back WITHOUT touching the
    // row: the memo answers (this is the read it saves).
    let blob = serde_json::to_string(&KeychainBlob {
        access_token: "sneaky",
        refresh_token: "r",
    })
    .unwrap();
    secret_file::write(st.root_dir.as_std_path(), &token_secret_name(URL), &blob).unwrap();
    assert_eq!(access(&st), "a1");
}

#[test]
fn a_save_from_this_process_is_seen_at_once() {
    let (_t, st) = state();
    save_tokens(&st, URL, "a1", "r1").unwrap();
    assert_eq!(access(&st), "a1");
    // Same second: saved_at may not move, the save drops the memo itself.
    save_tokens(&st, URL, "a2", "r2").unwrap();
    assert_eq!(access(&st), "a2");
}

#[test]
fn a_save_from_another_process_is_seen_through_saved_at() {
    let (_t, st) = state();
    save_tokens(&st, URL, "a1", "r1").unwrap();
    assert_eq!(access(&st), "a1");
    external_save(&st, "from-cli", "2099-01-01 00:00:00");
    assert_eq!(access(&st), "from-cli");
}

#[test]
fn a_logout_is_seen_at_once() {
    let (_t, st) = state();
    save_tokens(&st, URL, "a1", "r1").unwrap();
    assert_eq!(access(&st), "a1");
    clear_tokens(&st, URL).unwrap();
    assert!(load_tokens(&st, URL).unwrap().is_none());
}

#[test]
fn two_state_dirs_never_share_an_entry() {
    let (_t1, a) = state();
    let (_t2, b) = state();
    save_tokens(&a, URL, "from-a", "r").unwrap();
    save_tokens(&b, URL, "from-b", "r").unwrap();
    assert_eq!(access(&a), "from-a");
    assert_eq!(access(&b), "from-b");
}
