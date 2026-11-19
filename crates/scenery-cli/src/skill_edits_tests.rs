#![allow(clippy::unwrap_used)]

use super::*;

fn outcome(applied: bool, why: Option<&str>) -> skill_merge::ContinueOutcome {
    skill_merge::ContinueOutcome {
        version: Some("1.0.0+local.x".into()),
        applied_backup: None,
        applied,
        not_applied_reason: why.map(str::to_string),
    }
}

#[test]
fn apply_that_was_not_applied_is_an_error_but_a_plain_continue_is_not() {
    assert!(apply_result(true, &outcome(true, None)).is_ok());
    assert!(apply_result(false, &outcome(false, None)).is_ok());
    assert!(apply_result(false, &outcome(false, Some("strict mode"))).is_ok());
    let err = apply_result(true, &outcome(false, Some("edits not uploaded"))).unwrap_err();
    assert!(format!("{err}").contains("edits not uploaded"), "{err}");
    assert!(apply_result(true, &outcome(false, None)).is_err());
}

#[test]
fn list_shows_the_held_branch_instead_of_the_catalog_version() {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    // Plain skill: the installed version, no note.
    let v = listed_version(&state, "plain", "1.0.0");
    assert_eq!((v.version.as_str(), v.note.as_str()), ("1.0.0", ""));
    // Held branch: catalog moved to 1.1.0 but the live copy is the branch.
    conn.execute(
        "INSERT INTO skill_local_state (skill_id, authorship, branch_state, base_version, local_version, updated_at) VALUES ('cat','catalog','branch','1.0.0','1.0.0+local.x.1',0)",
        [],
    )
    .unwrap();
    let v = listed_version(&state, "cat", "1.1.0");
    assert_eq!(v.version, "1.0.0+local.x.1");
    assert!(v.note.contains("your branch of 1.0.0"), "{}", v.note);
    assert!(v.note.contains("catalog 1.1.0 not applied"), "{}", v.note);
}
