//! Card 1 — CLI direct-execution Block Third-Party Inference enforcement.
//!
//! `vectorhawk skill run` (and any other direct-execution command that
//! performs real inference) must fail closed under the org policy exactly
//! like the daemon does. `AppState::bootstrap()` always starts the
//! `block_third_party_inference` atomic at `false`; only an explicit
//! `seed_block_third_party_from_sync_state()` call (which the daemon makes
//! at startup) picks up the persisted policy. The CLI never made that call
//! and never routed inference through the enforcement chokepoint
//! (`HybridModelClient::with_third_party_blocking`) at all — so a
//! policy-enabled endpoint's `skill run` silently ignored governance.
//!
//! These tests exercise the fix end to end: seeding from persisted
//! `sync_state`, and the `cli_inference_client` wiring that gates direct
//! CLI inference through the same enforcement chokepoint the daemon uses.
//! No real Ollama/network call is made anywhere below — the fail-closed
//! assertions rely on the compatibility check short-circuiting before any
//! HTTP call would be issued (verified in `vectorhawkd-mcp::sampling`,
//! re-verified here at the CLI wiring layer).

use super::{cli_inference_client, NoCloudChannel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vectorhawkd_core::model::{ModelClient, ModelRequest};
use vectorhawkd_core::ollama::OllamaClient;
use vectorhawkd_core::state::AppState;

fn temp_state() -> AppState {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .expect("utf8 path")
        .join(format!("vh-btpi-cli-test-{}", uuid::Uuid::new_v4()));
    // Leak the tempdir so it outlives the AppState for the duration of the
    // test process; these are small, short-lived test fixtures.
    std::mem::forget(dir);
    AppState::bootstrap_in(root).expect("bootstrap_in")
}

#[test]
fn bootstrap_defaults_to_unblocked_until_seeded() {
    let state = temp_state();
    assert!(
        !state.block_third_party_handle().load(Ordering::Relaxed),
        "AppState::bootstrap_in must default the flag to false before seeding"
    );
}

#[test]
fn seed_from_persisted_sync_state_flips_the_flag_cli_reads() {
    let state = temp_state();
    state
        .set_sync_state("block_third_party_inference", "true")
        .expect("set_sync_state");

    // Before seeding, the handle a CLI command would read is still false —
    // this is exactly the pre-fix bug: nothing ever called the seed fn.
    assert!(!state.block_third_party_handle().load(Ordering::Relaxed));

    state.seed_block_third_party_from_sync_state();

    assert!(
        state.block_third_party_handle().load(Ordering::Relaxed),
        "seeding from persisted sync_state must flip the live atomic"
    );
}

#[test]
fn cli_inference_client_flag_off_returns_bare_ollama_unchanged() {
    // Flag off must be byte-identical to pre-fix behavior: the bare Ollama
    // client, not wrapped in any enforcement layer. We assert this via
    // `local_model_name()` (cheap, no network): `HybridModelClient` does not
    // override it (defaults to `None`), while the bare `OllamaClient` always
    // reports its configured model name. No `.generate()` call is made.
    let ollama = OllamaClient::new("http://127.0.0.1:1", "mistral");
    let flag = Arc::new(AtomicBool::new(false));

    let client = cli_inference_client(ollama, flag);

    assert_eq!(
        client.local_model_name(),
        Some("mistral"),
        "flag-off path must not wrap the Ollama client in Hybrid enforcement"
    );
}

#[test]
fn cli_inference_client_flag_on_wraps_in_enforcement_chokepoint() {
    let ollama = OllamaClient::new("http://127.0.0.1:1", "mistral");
    let flag = Arc::new(AtomicBool::new(true));

    let client = cli_inference_client(ollama, flag);

    // HybridModelClient does not override local_model_name (default: None),
    // proving the flag-on path routes through the enforcement wrapper
    // rather than exposing the raw Ollama client directly.
    assert_eq!(client.local_model_name(), None);
}

#[test]
fn cli_skill_run_fails_closed_with_governance_message_on_model_mismatch() {
    // This is the Card 1 acceptance test: with the org policy on and the
    // configured local model not satisfying the skill's recommended list,
    // CLI inference must produce the exact governance error — never a raw
    // network error, and never success via a non-local tier. The
    // compatibility check in `HybridModelClient::generate` runs before any
    // HTTP call, so this never touches the network (the configured base
    // URL, 127.0.0.1:1, is deliberately unroutable to prove it).
    let ollama = OllamaClient::new("http://127.0.0.1:1", "mistral");
    let flag = Arc::new(AtomicBool::new(true));
    let client = cli_inference_client(ollama, flag);

    let err = client
        .generate(ModelRequest {
            recommended_models: vec!["llama3.1:8b".to_string()],
            ..Default::default()
        })
        .expect_err("must fail closed when the local model doesn't satisfy the skill");

    assert!(
        err.to_string().contains("Block Third-Party Inference"),
        "expected the governance error, got: {err}"
    );
}

#[test]
fn cli_skill_run_succeeds_on_local_when_policy_on_and_model_compatible() {
    // Reversibility / happy-path check: when the policy is on but the
    // configured local model DOES satisfy the skill, execution still
    // succeeds locally (fail-closed only blocks non-compliant outcomes).
    // `MockModelClient` stands in for a reachable, compatible local model —
    // no real Ollama/network call.
    use vectorhawkd_core::model::{MockModelClient, ModelResponse, ModelSource};

    struct CompatibleLocal(MockModelClient);
    impl ModelClient for CompatibleLocal {
        fn local_model_name(&self) -> Option<&str> {
            Some("llama3.1:8b")
        }
        fn generate(&self, request: ModelRequest) -> anyhow::Result<ModelResponse> {
            self.0.generate(request)
        }
    }

    let flag = Arc::new(AtomicBool::new(true));
    let local = Box::new(CompatibleLocal(MockModelClient::new("local ok")));
    let fallback: Box<dyn ModelClient> = Box::new(NoCloudChannel);

    let hybrid = vectorhawkd_mcp::sampling::HybridModelClient::with_third_party_blocking(
        Some(local as Box<dyn ModelClient>),
        fallback,
        flag,
    );

    let resp = hybrid
        .generate(ModelRequest {
            recommended_models: vec!["llama3.1:8b".to_string()],
            ..Default::default()
        })
        .expect("compatible local model must succeed under the policy");

    assert!(matches!(resp.source, ModelSource::Local(_)));
    assert_eq!(resp.text, "local ok");
}
