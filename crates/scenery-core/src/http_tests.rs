#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The 426 state is process-global; serialize the tests that touch it.
static STATE_LOCK: Mutex<()> = Mutex::new(());

const BODY_426: &str = r#"{"code":"runner_upgrade_required","min":"9.9.9"}"#;

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Guard rail: every backend-bound client must come from this module so it
/// carries `X-Scenery-Runner-Version`. Ollama (local, not the backend) is the
/// only exemption.
#[test]
fn no_client_built_outside_this_module() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let forbidden = [
        "reqwest::Client::builder(",
        "reqwest::blocking::Client::builder(",
        "reqwest::Client::new(",
        "reqwest::blocking::Client::new(",
        "reqwest::get(",
        "reqwest::blocking::get(",
    ];
    let mut files = Vec::new();
    for krate in std::fs::read_dir(&crates_dir).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    let mut offenders = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with("_tests.rs") || name == "http.rs" || name == "ollama.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for pat in forbidden {
            if text.contains(pat) {
                offenders.push(format!("{} uses `{pat}`", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "build clients with scenery_core::http::{{async,blocking}}_client_builder():\n{}",
        offenders.join("\n")
    );
}

#[test]
fn blocking_client_sends_version_header() {
    let mut server = mockito::Server::new();
    let m = server
        .mock("GET", "/x")
        .match_header(RUNNER_VERSION_HEADER, RUNNER_VERSION)
        .with_status(200)
        .create();
    let client = blocking_client().unwrap();
    let resp = client.get(format!("{}/x", server.url())).send().unwrap();
    assert!(resp.status().is_success());
    m.assert();
}

#[test]
fn async_client_sends_version_header() {
    let mut server = mockito::Server::new();
    let m = server
        .mock("GET", "/x")
        .match_header(RUNNER_VERSION_HEADER, RUNNER_VERSION)
        .with_status(200)
        .create();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let resp = rt
        .block_on(async {
            async_client()
                .unwrap()
                .get(format!("{}/x", server.url()))
                .send_tracked()
                .await
        })
        .unwrap();
    assert!(resp.status().is_success());
    m.assert();
}

#[test]
fn version_header_is_the_crate_version() {
    let headers = default_headers();
    assert_eq!(
        headers
            .get(RUNNER_VERSION_HEADER)
            .unwrap()
            .to_str()
            .unwrap(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn parses_upgrade_body_plain_and_wrapped() {
    assert_eq!(
        parse_upgrade_body(BODY_426.as_bytes()).as_deref(),
        Some("9.9.9")
    );
    let wrapped = format!(r#"{{"detail":{BODY_426}}}"#);
    assert_eq!(
        parse_upgrade_body(wrapped.as_bytes()).as_deref(),
        Some("9.9.9")
    );
    assert_eq!(parse_upgrade_body(b"not json"), None);
    assert_eq!(
        parse_upgrade_body(br#"{"code":"other","min":"1.0.0"}"#),
        None
    );
}

#[test]
fn upgrade_message_is_actionable() {
    let e = UpgradeRequired {
        min: Some("2.1.0".into()),
    };
    assert_eq!(
        e.to_string(),
        "Scenery 2.1.0 or newer required; run `brew upgrade scenery`"
    );
    assert!(UpgradeRequired { min: None }
        .to_string()
        .contains("brew upgrade scenery"));
}

#[test]
fn blocking_send_tracked_records_426_and_preserves_response() {
    let _g = STATE_LOCK.lock().unwrap();
    clear_upgrade_required();
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/gated")
        .with_status(426)
        .with_header("content-type", "application/json")
        .with_body(BODY_426)
        .create();
    let resp = blocking_client()
        .unwrap()
        .get(format!("{}/gated", server.url()))
        .send_tracked()
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(resp.text().unwrap(), BODY_426);
    assert_eq!(
        current_upgrade_required(),
        Some(UpgradeRequired {
            min: Some("9.9.9".into())
        })
    );
    clear_upgrade_required();
}

#[test]
fn async_send_tracked_records_426_and_preserves_response() {
    let _g = STATE_LOCK.lock().unwrap();
    clear_upgrade_required();
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/gated")
        .with_status(426)
        .with_body(BODY_426)
        .create();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let body = rt.block_on(async {
        let resp = async_client()
            .unwrap()
            .get(format!("{}/gated", server.url()))
            .send_tracked()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
        resp.text().await.unwrap()
    });
    assert_eq!(body, BODY_426);
    assert_eq!(
        current_upgrade_required().unwrap().min.as_deref(),
        Some("9.9.9")
    );
    clear_upgrade_required();
}

#[test]
fn non_426_does_not_record() {
    let _g = STATE_LOCK.lock().unwrap();
    clear_upgrade_required();
    let mut server = mockito::Server::new();
    let _m = server.mock("GET", "/ok").with_status(200).create();
    blocking_client()
        .unwrap()
        .get(format!("{}/ok", server.url()))
        .send_tracked()
        .unwrap();
    assert_eq!(current_upgrade_required(), None);
}
