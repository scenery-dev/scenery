//! Regression tests for `vectorhawk skill validate` — bug: constructing
//! `OllamaClient` (a blocking `reqwest::blocking::Client`) directly on the
//! async executor thread panics with "Cannot drop a runtime in a context
//! where blocking is not allowed" (reqwest's debug-only
//! `blocking::wait::enter()` guard detects the nested runtime). See
//! `check_ollama_for_validate` and friends.
//!
//! Kept separate from production code per project convention — tests need
//! `#[allow(clippy::unwrap_used)]` without polluting prod modules.
#![allow(clippy::unwrap_used)]

use super::{check_ollama_for_validate, cmd_skill_validate};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_bundle_dir(label: &str) -> camino::Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    camino::Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("vh-validate-cli-{label}-{nanos}")),
    )
    .unwrap()
}

/// A minimal SKILL.md bundle that passes every core `validate_bundle` check.
fn write_passing_bundle(root: &camino::Utf8PathBuf) {
    fs::create_dir_all(root.join("prompts")).unwrap();
    fs::write(
        root.join("SKILL.md"),
        concat!(
            "---\nname: Validate Test Skill\ndescription: A skill used to test validate.\n",
            "metadata:\n  vectorhawk:\n",
            "    version: 0.1.0\n    publisher: skillclub\n",
            "    permissions:\n      filesystem: none\n      network: none\n      clipboard: none\n",
            "    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n",
            "    triggers:\n      - validate this\n",
            "    workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
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

/// Points VECTORHAWK_OLLAMA_URL at a port nothing is listening on, so the
/// health check fails fast and deterministically — the test must not depend
/// on whether a real Ollama happens to be running on the machine.
///
/// SAFETY: env mutation is process-global. No other test in this binary
/// reads/writes VECTORHAWK_OLLAMA_URL (grepped repo-wide), and this test
/// does not spawn children that would race on it.
fn set_unreachable_ollama_url() {
    unsafe { std::env::set_var("VECTORHAWK_OLLAMA_URL", "http://127.0.0.1:1") };
}

/// Reproduces the panic report: `vectorhawk skill validate` on a manifest
/// that passes the schema check must not panic when it reaches the Ollama
/// probe, with no real Ollama required.
#[tokio::test]
async fn validate_passing_manifest_does_not_panic() {
    set_unreachable_ollama_url();
    let bundle = temp_bundle_dir("passing");
    write_passing_bundle(&bundle);

    let result = cmd_skill_validate(bundle.clone()).await;

    let _ = fs::remove_dir_all(&bundle);
    assert!(
        result.is_ok(),
        "validate on a passing manifest must succeed, got {result:?}"
    );
}

/// When Ollama is unreachable, the probe must degrade to a warning string
/// instead of propagating an error or panicking.
#[tokio::test]
async fn check_ollama_for_validate_degrades_to_warning_when_unreachable() {
    set_unreachable_ollama_url();
    let bundle = temp_bundle_dir("degrade");
    write_passing_bundle(&bundle);

    let status = check_ollama_for_validate(&bundle).await;

    let _ = fs::remove_dir_all(&bundle);
    let status = status.expect("bundle has a loadable manifest, so Some(..) is expected");
    assert!(
        status.starts_with("not"),
        "expected an unreachable-Ollama warning, got {status:?}"
    );
}

/// Same reproduction under a multi-thread runtime, since the underlying
/// reqwest debug-assertions guard trips on *any* tokio runtime context, not
/// just `current_thread` (which is what the shipped CLI uses).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_passing_manifest_does_not_panic_multi_thread() {
    set_unreachable_ollama_url();
    let bundle = temp_bundle_dir("passing-mt");
    write_passing_bundle(&bundle);

    let result = cmd_skill_validate(bundle.clone()).await;

    let _ = fs::remove_dir_all(&bundle);
    assert!(
        result.is_ok(),
        "validate on a passing manifest must succeed, got {result:?}"
    );
}
