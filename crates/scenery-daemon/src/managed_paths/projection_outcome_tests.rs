#![allow(clippy::unwrap_used)]

use super::*;
use chrono::Duration;

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = AppState::bootstrap_in(root).unwrap();
    (tmp, state)
}

#[test]
fn round_trips_every_outcome() {
    let (_t, state) = state();
    let past = Utc::now() - Duration::seconds(5);
    for o in [
        ProjectionOutcome::Ok,
        ProjectionOutcome::LinkRefused {
            path: "/h/.claude/skills/x".into(),
        },
        ProjectionOutcome::PushRefused {
            path: "/h/.agents/skills/x".into(),
        },
        ProjectionOutcome::PushFailed {
            detail: "disk full".into(),
        },
    ] {
        record(&state, "x", &o).unwrap();
        assert_eq!(read_since(&state, "x", past).unwrap(), Some(o));
    }
}

#[test]
fn ignores_a_record_older_than_the_install() {
    let (_t, state) = state();
    record(&state, "x", &ProjectionOutcome::Ok).unwrap();
    let later = Utc::now() + Duration::seconds(5);
    assert_eq!(read_since(&state, "x", later).unwrap(), None);
    assert_eq!(
        read_since(&state, "other", Utc::now() - Duration::seconds(5)).unwrap(),
        None
    );
}
