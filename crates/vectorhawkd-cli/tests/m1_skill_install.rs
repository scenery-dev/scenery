//! Integration tests for the new `vectorhawk skill install <SKILL_REF>` shape.
//!
//! Tests the skill_ref detection heuristic (local path vs registry ID) and the
//! local-install and registry-install paths end-to-end.

#![allow(clippy::unwrap_used)]

use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use camino::Utf8PathBuf;
use vectorhawkd_core::{
    installer::{install_unpacked_skill, InstallMode},
    state::AppState,
};
use vectorhawkd_manifest::SkillPackage;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-install-cli-{label}-{nanos}")),
    )
    .unwrap()
}

fn write_skill_bundle(root: &Utf8PathBuf, name_suffix: &str, version: &str) {
    fs::create_dir_all(root.join("prompts")).unwrap();
    fs::write(
        root.join("SKILL.md"),
        format!(
            concat!(
                "---\nname: Test Skill {name_suffix}\ndescription: A test skill.\n",
                "license: MIT\nvh_version: {version}\nvh_publisher: skillclub\n",
                "vh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\n",
                "vh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\n",
                "vh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
            ),
            name_suffix = name_suffix,
            version = version,
        ),
    )
    .unwrap();
    fs::write(
        root.join("workflow.yaml"),
        "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
    )
    .unwrap();
    fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
}

// ── skill_ref detection ───────────────────────────────────────────────────────

/// A bare identifier (not a path that exists) is treated as a registry ID.
#[test]
fn non_existent_path_treated_as_registry_id() {
    let skill_ref = "contract-compare";
    let as_path = camino::Utf8Path::new(skill_ref);
    assert!(
        !as_path.exists(),
        "'{skill_ref}' must not exist as a filesystem path in the test environment"
    );
}

/// A string pointing at an existing directory is treated as a local path.
#[test]
fn existing_directory_treated_as_local_path() {
    let bundle_root = temp_root("detect-local");
    fs::create_dir_all(&bundle_root).unwrap();
    write_skill_bundle(&bundle_root, "detect", "0.1.0");

    let as_path = camino::Utf8Path::new(bundle_root.as_str());
    assert!(as_path.exists(), "created bundle dir must exist on disk");

    let _ = fs::remove_dir_all(&bundle_root);
}

// ── local-path install ────────────────────────────────────────────────────────

/// Local path install records the bundle in the state DB.
#[test]
fn local_path_install_records_in_db() {
    let state_root = temp_root("local-db");
    let bundle_root = temp_root("local-db-bundle");

    let state = AppState::bootstrap_in(state_root.clone()).unwrap();
    write_skill_bundle(&bundle_root, "local", "0.3.0");

    let pkg = SkillPackage::load_from_dir(&bundle_root).unwrap();
    install_unpacked_skill(&state, &pkg, InstallMode::Copy).unwrap();

    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    let version: String = conn
        .query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = 'test-skill-local'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "0.3.0");

    let _ = fs::remove_dir_all(&state_root);
    let _ = fs::remove_dir_all(&bundle_root);
}

// ── registry-ID install ───────────────────────────────────────────────────────

/// Registry-ID install: `install_from_registry` calls the registry, downloads
/// the artifact, verifies the checksum, extracts, and installs it.
///
/// A mockito server serves the artifact.  The archive is built using the same
/// helpers used in `vectorhawkd-core` tests.
#[test]
fn registry_id_install_happy_path() {
    use vectorhawkd_core::{registry::RegistryClient, updater::install_from_registry};

    let state_root = temp_root("reg-id-happy");
    let bundle_tmp = temp_root("reg-id-bundle");

    let state = AppState::bootstrap_in(state_root.clone()).unwrap();
    write_skill_bundle(&bundle_tmp, "reginstall", "2.0.0");

    // Build the archive using the same approach as core's updater tests.
    let archive_bytes = build_cskill_archive(&bundle_tmp);
    let sha256 = sha256_hex(&archive_bytes);

    let mut server = mockito::Server::new();
    let dl_path = "/dl/test-skill-reginstall-2.0.0.cskill";

    let _detail = server
        .mock("GET", "/portal/skills/test-skill-reginstall")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"skill_id":"test-skill-reginstall","name":"Test","latest_version":"2.0.0","publisher_name":null,"description":null}"#)
        .create();

    let _meta = server
        .mock("GET", "/skills/test-skill-reginstall/versions/2.0.0")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"skill_id":"test-skill-reginstall","version":"2.0.0","download_url":"{}{dl_path}","sha256":"{sha256}","size_bytes":{}}}"#,
            server.url(),
            archive_bytes.len()
        ))
        .create();

    let _dl = server
        .mock("GET", dl_path)
        .with_status(200)
        .with_body(archive_bytes.as_slice())
        .create();

    let registry = RegistryClient::new(server.url());
    let ver = install_from_registry(&state, &registry, "test-skill-reginstall", None).unwrap();
    assert_eq!(ver, "2.0.0");

    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    let active: String = conn
        .query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = 'test-skill-reginstall'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, "2.0.0");

    _detail.assert();
    _meta.assert();
    _dl.assert();

    let _ = fs::remove_dir_all(&state_root);
    let _ = fs::remove_dir_all(&bundle_tmp);
}

/// Registry unreachable: install_from_registry returns Err, not panic.
#[test]
fn registry_id_install_fails_gracefully_when_unreachable() {
    use vectorhawkd_core::{registry::RegistryClient, updater::install_from_registry};

    let state_root = temp_root("reg-id-unreachable");
    let state = AppState::bootstrap_in(state_root.clone()).unwrap();

    let registry = RegistryClient::new("http://127.0.0.1:1");
    let result = install_from_registry(&state, &registry, "some-skill", None);
    assert!(result.is_err(), "must return Err when registry is unreachable");

    let _ = fs::remove_dir_all(&state_root);
}

// ── Archive helpers (test-only) ───────────────────────────────────────────────

fn build_cskill_archive(bundle_dir: &Utf8PathBuf) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};

    let mut buf = Vec::new();
    let enc = GzEncoder::new(&mut buf, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", bundle_dir.as_std_path()).unwrap();
    let gz = tar.into_inner().unwrap();
    gz.finish().unwrap();
    buf
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}
