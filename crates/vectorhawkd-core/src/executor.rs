use crate::{
    model::{ModelClient, ModelRequest, ModelSource},
    policy::PolicyClient,
    resolver::{resolve_skill, ResolveOutcome},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs;
use vectorhawkd_manifest::{PromptSource, SkillPackage, WorkflowStep};

// ── Public result types ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct StepResult {
    pub id: String,
    pub step_type: String,
    pub note: String,
    pub output: Option<serde_json::Value>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
    pub model_source: Option<ModelSource>,
}

#[derive(Debug)]
pub struct RunResult {
    pub skill_id: String,
    pub version: String,
    pub steps: Vec<StepResult>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_latency_ms: u64,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Resolve, load, validate input, and execute a skill's workflow.
///
/// When `model_client` is `Some`, `llm` steps are sent to the model. When
/// `None`, every step is stub-executed (no network calls, useful for tests
/// and dry-runs).
pub fn run_skill(
    state: &AppState,
    policy_client: &dyn PolicyClient,
    skill_id: &str,
    input: &serde_json::Value,
    model_client: Option<&dyn ModelClient>,
) -> Result<RunResult> {
    run_skill_with_scope(state, policy_client, skill_id, input, model_client, None)
}

/// Like `run_skill` but accepts an optional project root for scope-aware resolution.
pub fn run_skill_with_scope(
    state: &AppState,
    policy_client: &dyn PolicyClient,
    skill_id: &str,
    input: &serde_json::Value,
    model_client: Option<&dyn ModelClient>,
    project_root: Option<&camino::Utf8Path>,
) -> Result<RunResult> {
    let wall_start = std::time::Instant::now();

    // 1. Resolve → get version and install path.
    let outcome = resolve_skill(state, policy_client, skill_id, project_root)?;
    let (version, install_path) = match outcome {
        ResolveOutcome::Active {
            version,
            install_path,
            ..
        } => (version, install_path),
        ResolveOutcome::NotInstalled { skill_id } => {
            anyhow::bail!("skill '{}' is not installed", skill_id)
        }
        ResolveOutcome::Blocked { skill_id, reason } => {
            anyhow::bail!("skill '{}' is blocked: {}", skill_id, reason)
        }
    };

    // 2. Load the skill package from the active install path.
    let pkg_path = Utf8PathBuf::from(&install_path);
    let pkg = SkillPackage::load_from_dir(&pkg_path)
        .with_context(|| format!("failed to load skill package at {pkg_path}"))?;

    // 3. Validate input against inputs_schema.
    validate_input_against_schema(&pkg.manifest.inputs_schema_or_default(), input)?;

    // 4. Advisory model requirements check.
    if let (Some(client), Some(reqs)) = (model_client, &pkg.manifest.model_requirements) {
        check_model_requirements(reqs, client);
    }

    // 5. Execute each workflow step, threading outputs forward.
    let mut steps: Vec<StepResult> = Vec::new();
    let mut step_outputs: HashMap<String, serde_json::Value> = HashMap::new();
    for step in &pkg.workflow.steps {
        let result = execute_step(&pkg, step, input, &step_outputs, model_client)?;
        if let Some(out) = &result.output {
            step_outputs.insert(result.id.clone(), out.clone());
        }
        steps.push(result);
    }

    let total_latency_ms = wall_start.elapsed().as_millis() as u64;
    let total_prompt_tokens: u64 = steps.iter().filter_map(|s| s.prompt_tokens).sum();
    let total_completion_tokens: u64 = steps.iter().filter_map(|s| s.completion_tokens).sum();

    // 6. Record execution history.
    record_execution(
        state,
        skill_id,
        &version,
        total_prompt_tokens,
        total_completion_tokens,
        total_latency_ms,
    )?;

    Ok(RunResult {
        skill_id: skill_id.to_string(),
        version,
        steps,
        total_prompt_tokens,
        total_completion_tokens,
        total_latency_ms,
    })
}

// ── Step dispatch ─────────────────────────────────────────────────────────────

fn execute_step(
    pkg: &SkillPackage,
    step: &WorkflowStep,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
    model_client: Option<&dyn ModelClient>,
) -> Result<StepResult> {
    match step {
        WorkflowStep::Tool { id, tool, input } => execute_tool_step(id, tool, input, run_input),
        WorkflowStep::Llm {
            id,
            prompt,
            inputs,
            output_schema,
        } => match model_client {
            Some(client) => execute_llm_step(
                pkg,
                LlmStepParams {
                    id,
                    prompt_source: prompt,
                    step_inputs: inputs,
                    output_schema_rel: output_schema.as_deref(),
                    run_input,
                    step_outputs,
                    client,
                },
            ),
            None => Ok(stub_step(step)),
        },
        WorkflowStep::Transform { id, op, input } => {
            execute_transform_step(id, op, input, run_input, step_outputs)
        }
        WorkflowStep::Validate { id, schema, input } => {
            execute_validate_step(pkg, id, schema, input, run_input, step_outputs)
        }
    }
}

// ── Tool step ────────────────────────────────────────────────────────────────

fn execute_tool_step(
    id: &str,
    tool: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
) -> Result<StepResult> {
    match tool {
        "extract_text" => {
            let field = input.as_str().ok_or_else(|| {
                anyhow::anyhow!("tool step '{id}': extract_text input must be a field name string")
            })?;
            let text = match run_input.get(field) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => anyhow::bail!("tool step '{id}': input field '{field}' not found"),
            };
            Ok(StepResult {
                id: id.to_string(),
                step_type: "tool".to_string(),
                note: format!(
                    "extract_text: extracted field '{field}' ({} chars)",
                    text.len()
                ),
                output: Some(serde_json::Value::String(text)),
                prompt_tokens: None,
                completion_tokens: None,
                latency_ms: None,
                model_source: None,
            })
        }
        other => anyhow::bail!("tool step '{id}': unknown built-in tool '{other}'"),
    }
}

// ── Transform step ───────────────────────────────────────────────────────────

fn execute_transform_step(
    id: &str,
    op: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> Result<StepResult> {
    let ref_str = input.as_str().ok_or_else(|| {
        anyhow::anyhow!("transform step '{id}': input must be a reference string")
    })?;
    let resolved = resolve_ref(ref_str, run_input, step_outputs);

    let output = match op {
        "json_parse" => serde_json::from_str(&resolved)
            .map_err(|e| anyhow::anyhow!("transform step '{id}': json_parse failed: {e}"))?,
        "to_string" => serde_json::Value::String(resolved.clone()),
        "to_uppercase" => serde_json::Value::String(resolved.to_uppercase()),
        "to_lowercase" => serde_json::Value::String(resolved.to_lowercase()),
        "trim" => serde_json::Value::String(resolved.trim().to_string()),
        other => anyhow::bail!("transform step '{id}': unknown op '{other}'"),
    };

    Ok(StepResult {
        id: id.to_string(),
        step_type: "transform".to_string(),
        note: format!("transform op '{op}' applied"),
        output: Some(output),
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
        model_source: None,
    })
}

// ── Validate step ─────────────────────────────────────────────────────────────

fn execute_validate_step(
    pkg: &SkillPackage,
    id: &str,
    schema_rel: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> Result<StepResult> {
    let ref_str = input
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("validate step '{id}': input must be a reference string"))?;
    let resolved_str = resolve_ref(ref_str, run_input, step_outputs);

    let value: serde_json::Value = serde_json::from_str(&resolved_str)
        .unwrap_or(serde_json::Value::String(resolved_str.clone()));

    validate_output(&pkg.root, schema_rel, &value)
        .with_context(|| format!("validate step '{id}' failed schema check"))?;

    Ok(StepResult {
        id: id.to_string(),
        step_type: "validate".to_string(),
        note: format!("validated against '{schema_rel}': ok"),
        output: Some(value),
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
        model_source: None,
    })
}

// ── LLM step ─────────────────────────────────────────────────────────────────

struct LlmStepParams<'a> {
    id: &'a str,
    prompt_source: &'a PromptSource,
    step_inputs: &'a Option<serde_yaml::Value>,
    output_schema_rel: Option<&'a str>,
    run_input: &'a serde_json::Value,
    step_outputs: &'a HashMap<String, serde_json::Value>,
    client: &'a dyn ModelClient,
}

fn execute_llm_step(pkg: &SkillPackage, p: LlmStepParams<'_>) -> Result<StepResult> {
    let LlmStepParams {
        id,
        prompt_source,
        step_inputs,
        output_schema_rel,
        run_input,
        step_outputs,
        client,
    } = p;

    let system_prompt = match prompt_source {
        PromptSource::Inline(body) => body.clone(),
        PromptSource::File(rel_path) => {
            let prompt_path = pkg.root.join(rel_path.as_str());
            fs::read_to_string(&prompt_path)
                .with_context(|| format!("failed to read prompt file {prompt_path}"))?
        }
    };

    let user_message = resolve_inputs(step_inputs, run_input, step_outputs);

    let model_reqs = pkg.manifest.model_requirements.as_ref();
    let prefer_local = model_reqs.and_then(|m| m.prefer_local).unwrap_or(false);
    let recommended_models = model_reqs
        .map(|m| m.recommended.clone())
        .unwrap_or_default();
    let fallback = model_reqs.and_then(|m| m.fallback);

    let request = ModelRequest {
        system_prompt,
        user_message,
        json_output: output_schema_rel.is_some(),
        prefer_local,
        recommended_models,
        fallback,
    };

    let response = client
        .generate(request)
        .with_context(|| format!("LLM call failed for step '{id}'"))?;

    let output: Option<serde_json::Value> = if output_schema_rel.is_some() {
        serde_json::from_str(&response.text)
            .ok()
            .or_else(|| Some(serde_json::Value::String(response.text.clone())))
    } else {
        Some(serde_json::Value::String(response.text.clone()))
    };

    if let (Some(schema_rel), Some(output_val)) = (output_schema_rel, &output) {
        let shape = match output_val {
            serde_json::Value::String(_) => "string (not JSON)",
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
        };
        validate_output(&pkg.root, schema_rel, output_val).with_context(|| {
            format!(
                "step '{id}' output failed schema validation. Schema: {schema_rel}. \
                 Model returned a {shape}; expected output conforming to the schema. \
                 Run the skill locally with `vectorhawk skill run` to inspect the raw output."
            )
        })?;
    }

    Ok(StepResult {
        id: id.to_string(),
        step_type: "llm".to_string(),
        note: format!(
            "completed in {}ms ({} prompt + {} completion tokens)",
            response.latency_ms, response.prompt_tokens, response.completion_tokens
        ),
        output,
        prompt_tokens: Some(response.prompt_tokens),
        completion_tokens: Some(response.completion_tokens),
        latency_ms: Some(response.latency_ms),
        model_source: Some(response.source),
    })
}

// ── Input resolution ──────────────────────────────────────────────────────────

fn resolve_inputs(
    step_inputs: &Option<serde_yaml::Value>,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> String {
    let Some(inputs) = step_inputs else {
        return String::new();
    };
    let Some(mapping) = inputs.as_mapping() else {
        return String::new();
    };

    let mut parts = Vec::new();
    for (key, value) in mapping {
        let key_str = key.as_str().unwrap_or_default();
        let ref_str = value.as_str().unwrap_or_default();
        let resolved = resolve_ref(ref_str, run_input, step_outputs);
        parts.push(format!("{key_str}: {resolved}"));
    }
    parts.join("\n")
}

fn resolve_ref(
    ref_str: &str,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> String {
    if let Some(field) = ref_str.strip_prefix("input.") {
        if let Some(val) = run_input.get(field) {
            return match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    if let Some(step_id) = ref_str.strip_suffix(".output") {
        if let Some(val) = step_outputs.get(step_id) {
            return match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    ref_str.to_string()
}

// ── Stub execution ────────────────────────────────────────────────────────────

fn stub_step(step: &WorkflowStep) -> StepResult {
    let (id, step_type, note) = match step {
        WorkflowStep::Llm { id, prompt, .. } => {
            let prompt_desc = match prompt {
                PromptSource::File(path) => format!("file '{path}'"),
                PromptSource::Inline(_) => "inline body".to_string(),
            };
            (
                id.clone(),
                "llm",
                format!("LLM call with prompt {prompt_desc} — stub, no model invoked"),
            )
        }
        WorkflowStep::Tool { id, tool, .. } => {
            (id.clone(), "tool", format!("built-in tool '{tool}' — stub"))
        }
        WorkflowStep::Transform { id, op, .. } => (
            id.clone(),
            "transform",
            format!("transform op '{op}' — stub"),
        ),
        WorkflowStep::Validate { id, schema, .. } => (
            id.clone(),
            "validate",
            format!("schema validation '{schema}' — stub"),
        ),
    };
    StepResult {
        id,
        step_type: step_type.to_string(),
        note,
        output: None,
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
        model_source: None,
    }
}

// ── Schema validation helpers ─────────────────────────────────────────────────

fn validate_input_against_schema(
    schema_json: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<()> {
    let validator = jsonschema::JSONSchema::compile(schema_json)
        .map_err(|e| anyhow::anyhow!("inputs_schema is not a valid JSON Schema: {e}"))?;

    if !validator.is_valid(input) {
        anyhow::bail!("input failed validation against inputs_schema");
    }

    Ok(())
}

fn validate_output(
    pkg_root: &Utf8PathBuf,
    schema_rel: &str,
    output: &serde_json::Value,
) -> Result<()> {
    let schema_path = pkg_root.join(schema_rel);
    let schema_text = fs::read_to_string(&schema_path)
        .with_context(|| format!("failed to read output schema {schema_path}"))?;
    let schema_json: serde_json::Value = serde_json::from_str(&schema_text)
        .with_context(|| format!("{schema_rel} is not valid JSON"))?;

    let validator = jsonschema::JSONSchema::compile(&schema_json)
        .map_err(|e| anyhow::anyhow!("{schema_rel} is not a valid JSON Schema: {e}"))?;

    if !validator.is_valid(output) {
        anyhow::bail!("output failed validation against {schema_rel}");
    }

    Ok(())
}

// ── Execution history ─────────────────────────────────────────────────────────

fn record_execution(
    state: &AppState,
    skill_id: &str,
    version: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    latency_ms: u64,
) -> Result<()> {
    let conn =
        Connection::open(&state.db_path).context("failed to open state DB to record execution")?;
    conn.execute(
        "INSERT INTO execution_history (skill_id, version, status, prompt_tokens, completion_tokens, latency_ms)
         VALUES (?1, ?2, 'completed', ?3, ?4, ?5)",
        rusqlite::params![skill_id, version, prompt_tokens, completion_tokens, latency_ms],
    )
    .context("failed to insert execution_history row")?;
    Ok(())
}

// ── Model requirements check ──────────────────────────────────────────────────

fn check_model_requirements(
    reqs: &vectorhawkd_manifest::ModelRequirements,
    _client: &dyn ModelClient,
) {
    if let Some(min_params) = reqs.min_params_b {
        tracing::info!(
            "skill requires min_params_b={min_params}B — cannot verify locally, proceeding"
        );
    }
    if !reqs.recommended.is_empty() {
        tracing::info!("skill recommended models: {:?}", reqs.recommended);
    }
    if let Some(fallback) = reqs.fallback {
        tracing::info!("skill model fallback: {fallback:?}");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        installer::{install_unpacked_skill, InstallMode},
        model::MockModelClient,
        policy::MockPolicyClient,
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-executor-tests-{label}-{nanos}")),
        )
        .expect("temp path should be utf-8")
    }

    fn write_skill_bundle(root: &Utf8PathBuf, input_schema_json: &str) {
        fs::create_dir_all(root.join("prompts")).expect("create prompts");
        let input_schema_yaml =
            format!("  inputs: {input_schema_json}\n  outputs: {{\"type\": \"object\"}}");
        let skill_md = format!(
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_version: 0.1.0\nvh_publisher: skillclub\nvh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_schemas:\n{input_schema_yaml}\nvh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
        );
        fs::write(root.join("SKILL.md"), skill_md).expect("write SKILL.md");
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .expect("write workflow.yaml");
        fs::write(root.join("prompts/system.txt"), "Do the thing.").expect("write system.txt");
    }

    #[test]
    fn run_executes_steps_and_returns_results_for_installed_skill() {
        let state_root = temp_root("run-ok");
        let skill_root = temp_root("run-ok-skill");
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

        write_skill_bundle(&skill_root, "{}");
        let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).expect("install");

        let client = MockPolicyClient::new();
        let result = run_skill(&state, &client, "test-skill", &serde_json::json!({}), None)
            .expect("run skill");

        assert_eq!(result.skill_id, "test-skill");
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].id, "run");
        assert_eq!(result.steps[0].step_type, "llm");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn run_errors_when_skill_is_not_installed() {
        let state_root = temp_root("run-not-installed");
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

        let client = MockPolicyClient::new();
        let err = run_skill(&state, &client, "ghost-skill", &serde_json::json!({}), None)
            .expect_err("uninstalled skill should fail");

        assert!(err.to_string().contains("not installed"), "got: {err}");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn run_errors_when_input_fails_schema_validation() {
        let state_root = temp_root("run-schema-fail");
        let skill_root = temp_root("run-schema-fail-skill");
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

        write_skill_bundle(
            &skill_root,
            r#"{"type":"object","required":["query"],"properties":{"query":{"type":"string"}}}"#,
        );
        let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).expect("install");

        let client = MockPolicyClient::new();
        let err = run_skill(
            &state,
            &client,
            "test-skill",
            &serde_json::json!({"other": 1}),
            None,
        )
        .expect_err("invalid input should fail");

        assert!(err.to_string().contains("validation"), "got: {err}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn llm_step_with_mock_model_produces_output_and_token_counts() {
        let state_root = temp_root("mock-llm");
        let skill_root = temp_root("mock-llm-skill");
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

        write_skill_bundle(&skill_root, "{}");
        let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
        install_unpacked_skill(&state, &pkg, InstallMode::Copy).expect("install");

        let mock_model = MockModelClient::new("mock response text").with_tokens(20, 15);
        let policy = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &policy,
            "test-skill",
            &serde_json::json!({}),
            Some(&mock_model),
        )
        .expect("run skill");

        assert_eq!(result.steps.len(), 1);
        let step = &result.steps[0];
        assert_eq!(step.step_type, "llm");
        assert_eq!(
            step.output,
            Some(serde_json::Value::String("mock response text".to_string()))
        );
        assert_eq!(step.prompt_tokens, Some(20));
        assert_eq!(step.completion_tokens, Some(15));
        assert_eq!(result.total_prompt_tokens, 20);
        assert_eq!(result.total_completion_tokens, 15);

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
