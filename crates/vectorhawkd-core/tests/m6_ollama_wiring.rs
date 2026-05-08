//! M6.1 integration tests: OllamaClient wired into executor, execution_history
//! extended with `model_source` and `cost_usd` columns.

use camino::Utf8PathBuf;
use rusqlite::Connection;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use vectorhawkd_core::{
    executor::run_skill,
    installer::{install_unpacked_skill, InstallMode},
    model::{model_source_str, ModelSource},
    ollama::OllamaClient,
    policy::MockPolicyClient,
    state::AppState,
};
use vectorhawkd_manifest::SkillPackage;

fn temp_root(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!("vh-m6-tests-{label}-{nanos}")))
        .expect("temp path should be UTF-8")
}

fn write_skill_bundle(root: &Utf8PathBuf) {
    fs::create_dir_all(root.join("prompts")).expect("create prompts dir");
    let skill_md = concat!(
        "---\n",
        "name: Test Skill\n",
        "description: A test skill for M6.\n",
        "license: MIT\n",
        "vh_version: 0.1.0\n",
        "vh_publisher: skillclub\n",
        "vh_permissions:\n",
        "  filesystem: none\n",
        "  network: none\n",
        "  clipboard: none\n",
        "vh_execution:\n",
        "  sandbox: strict\n",
        "  timeout_ms: 30000\n",
        "  memory_mb: 256\n",
        "vh_schemas:\n",
        "  inputs: {}\n",
        "  outputs: {\"type\": \"object\"}\n",
        "vh_workflow_ref: ./workflow.yaml\n",
        "---\n\n",
        "Do the thing.\n"
    );
    fs::write(root.join("SKILL.md"), skill_md).expect("write SKILL.md");
    fs::write(
        root.join("workflow.yaml"),
        "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
    )
    .expect("write workflow.yaml");
    fs::write(root.join("prompts/system.txt"), "Do the thing.").expect("write system.txt");
}

/// Test 1: OllamaClient wired → skill runs → execution_history row has
/// `model_source = "local:test-model"` and `cost_usd = 0.0`.
#[test]
fn ollama_wired_skill_run_records_model_source_and_cost() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/api/generate")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"response":"mock answer","prompt_eval_count":8,"eval_count":4}"#)
        .create();

    let state_root = temp_root("ollama-ok");
    let skill_root = temp_root("ollama-ok-skill");
    let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

    write_skill_bundle(&skill_root);
    let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
    install_unpacked_skill(&state, &pkg, InstallMode::Copy).expect("install skill");

    let client = OllamaClient::with_timeouts(
        server.url(),
        "test-model",
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(5),
    );
    let policy = MockPolicyClient::new();

    let result = run_skill(
        &state,
        &policy,
        "test-skill",
        &serde_json::json!({}),
        Some(&client),
    )
    .expect("run skill should succeed");

    assert_eq!(result.steps.len(), 1);
    assert_eq!(result.steps[0].step_type, "llm");

    // Verify the model_source string on RunResult
    let source_str = result.model_source.as_deref().unwrap_or("");
    assert_eq!(source_str, "local:test-model", "model_source: {source_str}");
    assert!(
        (result.total_cost_usd - 0.0_f64).abs() < f64::EPSILON,
        "cost_usd should be 0.0, got: {}",
        result.total_cost_usd
    );

    // Verify the execution_history row in SQLite
    let conn = Connection::open(&state.db_path).expect("open db");
    let (db_model_source, db_cost_usd): (Option<String>, Option<f64>) = conn
        .query_row(
            "SELECT model_source, cost_usd FROM execution_history WHERE skill_id = 'test-skill' ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query execution_history row");

    assert_eq!(
        db_model_source.as_deref(),
        Some("local:test-model"),
        "DB model_source: {db_model_source:?}"
    );
    assert!(
        (db_cost_usd.unwrap_or(f64::NAN) - 0.0_f64).abs() < f64::EPSILON,
        "DB cost_usd should be 0.0, got: {db_cost_usd:?}"
    );

    let _ = fs::remove_dir_all(&state_root);
    let _ = fs::remove_dir_all(&skill_root);
}

/// Test 2: Ollama returns 500 → run_skill returns Err (is_error path).
#[test]
fn ollama_500_causes_skill_run_error() {
    let mut server = mockito::Server::new();

    let _m = server
        .mock("POST", "/api/generate")
        .with_status(500)
        .with_body("internal server error")
        .create();

    let state_root = temp_root("ollama-500");
    let skill_root = temp_root("ollama-500-skill");
    let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

    write_skill_bundle(&skill_root);
    let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
    install_unpacked_skill(&state, &pkg, InstallMode::Copy).expect("install skill");

    let client = OllamaClient::with_timeouts(
        server.url(),
        "test-model",
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(5),
    );
    let policy = MockPolicyClient::new();

    let err = run_skill(
        &state,
        &policy,
        "test-skill",
        &serde_json::json!({}),
        Some(&client),
    )
    .expect_err("should fail when Ollama returns 500");

    assert!(
        err.to_string().contains("500") || err.to_string().contains("LLM call failed"),
        "expected error mentioning HTTP 500 or LLM call failed, got: {err}"
    );

    let _ = fs::remove_dir_all(&state_root);
    let _ = fs::remove_dir_all(&skill_root);
}

/// Test 3: model_source_str free function maps all variants correctly.
#[test]
fn model_source_str_maps_all_variants() {
    assert_eq!(
        model_source_str(&ModelSource::Local("llama3".to_string())),
        "local:llama3"
    );
    assert_eq!(
        model_source_str(&ModelSource::Internal("phi".to_string())),
        "internal:phi"
    );
    assert_eq!(
        model_source_str(&ModelSource::Provider("anthropic".to_string())),
        "provider:anthropic"
    );
    assert_eq!(model_source_str(&ModelSource::McpSampling), "mcp_sampling");
}

/// Test 4: bootstrap_in creates the new columns (idempotent ALTER TABLE).
#[test]
fn bootstrap_creates_model_source_and_cost_usd_columns() {
    let state_root = temp_root("bootstrap-cols");
    let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

    let conn = Connection::open(&state.db_path).expect("open db");

    // Insert a row using the new columns to confirm they exist.
    conn.execute(
        "INSERT INTO execution_history (skill_id, version, status, model_source, cost_usd) \
         VALUES ('s', '0.1.0', 'completed', 'local:x', 0.001)",
        [],
    )
    .expect("insert with new columns should succeed");

    let (src, cost): (Option<String>, Option<f64>) = conn
        .query_row(
            "SELECT model_source, cost_usd FROM execution_history WHERE skill_id = 's'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query new columns");

    assert_eq!(src.as_deref(), Some("local:x"));
    assert!((cost.unwrap_or(0.0) - 0.001).abs() < 1e-9);

    // Second bootstrap must not fail (idempotent).
    AppState::bootstrap_in(state_root.clone()).expect("second bootstrap should not fail");

    let _ = fs::remove_dir_all(&state_root);
}
