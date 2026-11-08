#![allow(clippy::unwrap_used)]
use super::*;
use camino::Utf8PathBuf;

fn state() -> (tempfile::TempDir, AppState) {
    let d = tempfile::tempdir().unwrap();
    let st = AppState::bootstrap_in(Utf8PathBuf::from_path_buf(d.path().to_path_buf()).unwrap())
        .unwrap();
    (d, st)
}

#[test]
fn notification_names_are_sanitized() {
    assert_eq!(
        sanitize_name("Evil\"; do shell script \"x"),
        "Evil do shell script x"
    );
    assert_eq!(sanitize_name(&"a".repeat(100)).len(), 40);
}

#[test]
fn notification_is_sent_at_most_once_per_hash() {
    let (_d, st) = state();
    notify_pending_once(&st, "abc", "Srv");
    assert_eq!(
        st.get_sync_state("approval_notified:abc")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    st.delete_sync_state("approval_notified:abc").unwrap();
    notify_pending_once(&st, "abc", "Srv");
    notify_pending_once(&st, "abc", "Srv");
    assert!(st
        .get_sync_state("approval_notified:abc")
        .unwrap()
        .is_some());
}
