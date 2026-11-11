#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use camino::Utf8PathBuf;

fn root() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, p)
}

#[test]
fn graceful_shutdown_leaves_no_crash_record_next_start() {
    let (_d, root) = root();
    assert!(crash::mark_run_started(&root, "2.2.1").unwrap().is_none());
    // SIGTERM: loop breaks, marker cleared before the slow drain/flush.
    mark_graceful_shutdown(&root);
    // Even if the process is SIGKILLed during the drain, the next start is clean.
    assert!(crash::mark_run_started(&root, "2.2.1").unwrap().is_none());
    assert!(crash::list_pending(&root).is_empty());
}

#[test]
fn simulated_crash_is_reported_at_next_start() {
    let (_d, root) = root();
    assert!(crash::mark_run_started(&root, "2.2.1").unwrap().is_none());
    // Process dies: no shutdown hook runs.
    let rec = crash::mark_run_started(&root, "2.2.2").unwrap().unwrap();
    assert_eq!(rec.kind, crash::CrashKind::UncleanShutdown);
    assert_eq!(rec.runner_version, "2.2.1");
    assert_eq!(crash::list_pending(&root).len(), 1);
}

#[test]
fn guard_drop_on_error_return_is_not_a_crash() {
    let (_d, root) = root();
    crash::mark_run_started(&root, "2.2.1").unwrap();
    drop(crash::RunMarkerGuard::new(&root));
    assert!(crash::mark_run_started(&root, "2.2.1").unwrap().is_none());
}
