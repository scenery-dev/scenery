#![allow(clippy::unwrap_used)]

// Smoke test: reconciler builds without panicking.
use super::*;
use vectorhawkd_core::state::AppState;

#[test]
fn managed_paths_reconciler_new_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let state = Arc::new(AppState::bootstrap_in(root).unwrap());
    let reconciler = ManagedPathsReconciler::new(state, "https://example.com".to_string());
    assert!(reconciler.is_ok());
}
