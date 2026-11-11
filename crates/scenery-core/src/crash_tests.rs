#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::registry::RegistryClient;
use camino::Utf8PathBuf;

fn root() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, p)
}

fn sample(kind: CrashKind) -> CrashRecord {
    new_record(kind, "2.2.1", now_rfc3339())
}

#[test]
fn truncate_respects_char_boundaries() {
    let s = "ab\u{1F600}cd";
    assert_eq!(truncate(s, 3), "ab");
    assert_eq!(truncate(s, 100), s);
}

#[test]
fn panic_record_is_capped_and_has_no_env() {
    let big = "x".repeat(MAX_MESSAGE + 500);
    let rec = build_panic_record("2.2.1", &big, Some("src/a.rs:1:2"), None);
    assert_eq!(rec.kind, CrashKind::Panic);
    assert_eq!(rec.message.as_deref().map(str::len), Some(MAX_MESSAGE));
    let json = serde_json::to_string(&rec).unwrap();
    assert!(!json.contains("PATH"));
}

#[test]
fn write_list_delete_roundtrip() {
    let (_d, root) = root();
    let rec = sample(CrashKind::Panic);
    write_record(&root, &rec).unwrap();
    assert_eq!(list_pending(&root), vec![rec.clone()]);
    delete_records(&root, std::slice::from_ref(&rec.report_id));
    assert!(list_pending(&root).is_empty());
}

#[test]
fn corrupt_record_is_discarded() {
    let (_d, root) = root();
    let dir = root.join(CRASH_DIR);
    std::fs::create_dir_all(dir.as_std_path()).unwrap();
    std::fs::write(dir.join("bad.json").as_std_path(), b"{not json").unwrap();
    assert!(list_pending(&root).is_empty());
    assert!(!dir.join("bad.json").exists());
}

#[test]
fn pending_records_are_capped() {
    let (_d, root) = root();
    for _ in 0..(MAX_PENDING + 5) {
        write_record(&root, &sample(CrashKind::Panic)).unwrap();
    }
    assert_eq!(list_pending(&root).len(), MAX_PENDING);
}

#[cfg(unix)]
#[test]
fn record_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = root();
    let rec = sample(CrashKind::Panic);
    write_record(&root, &rec).unwrap();
    let path = root.join(CRASH_DIR).join(format!("{}.json", rec.report_id));
    let mode = std::fs::metadata(path.as_std_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn first_start_writes_marker_without_record() {
    let (_d, root) = root();
    assert!(mark_run_started(&root, "2.2.1").unwrap().is_none());
    assert!(root.join(RUN_MARKER).exists());
    assert!(list_pending(&root).is_empty());
}

#[test]
fn leftover_marker_becomes_unclean_shutdown_with_old_version() {
    let (_d, root) = root();
    mark_run_started(&root, "2.1.0").unwrap();
    let rec = mark_run_started(&root, "2.2.1").unwrap().expect("record");
    assert_eq!(rec.kind, CrashKind::UncleanShutdown);
    assert_eq!(rec.runner_version, "2.1.0");
    assert!(rec.message.is_none());
    assert_eq!(list_pending(&root).len(), 1);
}

#[test]
fn clean_shutdown_leaves_no_record() {
    let (_d, root) = root();
    mark_run_started(&root, "2.2.1").unwrap();
    clear_run_marker(&root);
    assert!(mark_run_started(&root, "2.2.1").unwrap().is_none());
    assert!(list_pending(&root).is_empty());
}

fn authed(server_url: String) -> RegistryClient {
    let client = RegistryClient::new(server_url);
    client.set_auth("tok");
    client.set_device_id("00000000-0000-0000-0000-000000000001");
    client
}

#[test]
fn upload_deletes_on_success() {
    let mut server = mockito::Server::new();
    let mock = server
        .mock("POST", "/api/sync/crash-reports")
        .match_header("x-device-id", "00000000-0000-0000-0000-000000000001")
        .with_status(200)
        .with_body(r#"{"received":1,"accepted":1}"#)
        .create();
    let (_d, root) = root();
    write_record(&root, &sample(CrashKind::Panic)).unwrap();
    assert_eq!(upload_pending(&root, &authed(server.url())).unwrap(), 1);
    assert!(list_pending(&root).is_empty());
    mock.assert();
}

#[test]
fn upload_keeps_records_on_retryable_or_auth_failures() {
    for code in [401, 403, 404, 426, 429, 500, 502, 503] {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/api/sync/crash-reports")
            .with_status(code)
            .create();
        let (_d, root) = root();
        write_record(&root, &sample(CrashKind::Panic)).unwrap();
        assert!(
            upload_pending(&root, &authed(server.url())).is_err(),
            "HTTP {code}"
        );
        assert_eq!(
            list_pending(&root).len(),
            1,
            "HTTP {code} must keep the record"
        );
    }
}

#[test]
fn upload_drops_records_the_backend_rejects_as_invalid() {
    for code in [400, 413, 422] {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/api/sync/crash-reports")
            .with_status(code)
            .create();
        let (_d, root) = root();
        write_record(&root, &sample(CrashKind::Panic)).unwrap();
        assert_eq!(
            upload_pending(&root, &authed(server.url())).unwrap(),
            1,
            "HTTP {code}"
        );
        assert!(
            list_pending(&root).is_empty(),
            "HTTP {code} must drop the record"
        );
    }
}

#[test]
fn upload_without_credentials_errors_and_keeps_records() {
    let (_d, root) = root();
    write_record(&root, &sample(CrashKind::Panic)).unwrap();
    let client = RegistryClient::new("http://127.0.0.1:1".to_string());
    assert!(upload_pending(&root, &client).is_err());
    assert_eq!(list_pending(&root).len(), 1);
}

#[test]
fn panic_hook_writes_record_and_clears_marker() {
    // Runs the real hook inside a caught panic. Unwinding is on in test builds.
    let (_d, root) = root();
    mark_run_started(&root, "2.2.1").unwrap();
    install_panic_hook(root.clone(), "2.2.1");
    let result = std::panic::catch_unwind(|| {
        panic!("boom at /nonexistent/path");
    });
    let _ = std::panic::take_hook();
    assert!(result.is_err());
    // The hook is process-global: panics from other parallel tests may add records.
    let pending: Vec<_> = list_pending(&root)
        .into_iter()
        .filter(|r| r.message.as_deref().is_some_and(|m| m.contains("boom")))
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, CrashKind::Panic);
    assert!(pending[0].message.as_deref().unwrap().contains("boom"));
    assert!(pending[0].location.is_some());
    assert!(!root.join(RUN_MARKER).exists());
}
