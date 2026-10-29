use crate::{
    model::{model_source_str, ModelClient, ModelRequest, ModelSource},
    policy::PolicyClient,
    resolver::{resolve_skill, ResolveOutcome},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use scenery_manifest::{
    resolve_bundle_file, BundlePathError, PromptSource, SkillPackage, WorkflowStep,
};
use std::collections::HashMap;
use std::fs;

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
    /// Cost in USD charged by the inference backend for this step. `None` for
    /// non-LLM steps; `Some(0.0)` for local/free backends.
    pub cost_usd: Option<f64>,
}

#[derive(Debug)]
pub struct RunResult {
    pub skill_id: String,
    pub version: String,
    pub steps: Vec<StepResult>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_latency_ms: u64,
    /// Sum of `cost_usd` across all steps that reported a cost.
    pub total_cost_usd: f64,
    /// String representation of the first step's `model_source` that is `Some`.
    /// `None` when no LLM steps ran (stub mode or non-LLM workflow).
    pub model_source: Option<String>,
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

    // Bug #9: distinguish "never installed" from "registered but files missing".
    // resolve_skill already confirmed a state.db row exists (Active branch above),
    // so if the directory is absent the install is corrupted/incomplete — surface
    // an actionable message rather than a generic load error.
    if !pkg_path.exists() {
        anyhow::bail!(
            "Skill '{}' is registered in state.db but its install directory is missing or corrupt.\n\
             Run: scenery skill install {}  (to reinstall from the registry)\n\
              or: scenery skill install ./path/to/{}  (to reinstall from source)",
            skill_id,
            skill_id,
            skill_id,
        );
    }

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
    let total_cost_usd: f64 = steps.iter().filter_map(|s| s.cost_usd).sum();
    let run_model_source: Option<String> = steps
        .iter()
        .find_map(|s| s.model_source.as_ref())
        .map(model_source_str);

    // Per-run execution history was previously persisted to a local
    // `execution_history` SQLite table. It was only ever read by
    // `scenery skill info`, never uploaded anywhere — so we keep it in
    // the returned RunResult for the immediate caller and drop the
    // long-term local store. Aggregate counts still land in
    // `skill_execution_counts` via `ratings.rs` and get synced to the
    // registry.
    let _ = total_cost_usd;
    let _ = state;

    Ok(RunResult {
        skill_id: skill_id.to_string(),
        version,
        steps,
        total_prompt_tokens,
        total_completion_tokens,
        total_latency_ms,
        total_cost_usd,
        model_source: run_model_source,
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
                cost_usd: None,
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
        cost_usd: None,
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
        cost_usd: None,
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
            let prompt_path = match resolve_bundle_file(&pkg.root, rel_path) {
                Ok(path) => path,
                Err(BundlePathError::OutsideBundle) => {
                    anyhow::bail!("prompt file {rel_path} resolves outside the skill bundle")
                }
                Err(BundlePathError::Io(e)) => {
                    let prompt_path = pkg.root.join(rel_path.as_str());
                    return Err(anyhow::Error::new(e)
                        .context(format!("failed to read prompt file {prompt_path}")));
                }
            };
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
                 Run the skill locally with `scenery skill run` to inspect the raw output."
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
        cost_usd: Some(response.cost_usd),
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
        cost_usd: None,
    }
}

// ── Schema validation helpers ─────────────────────────────────────────────────

fn validate_input_against_schema(
    schema_json: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<()> {
    // F21: refuse reference keywords and oversized schemas before jsonschema
    // compiles them (a `$ref` cycle overflows the stack in jsonschema 0.17).
    let validator = crate::schema_guard::compile_skill_schema(schema_json)
        .map_err(|e| anyhow::anyhow!("inputs_schema is not an acceptable JSON Schema: {e}"))?;

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
    // F21: structural guard runs before jsonschema compiles the file.
    let validator = crate::schema_guard::compile_schema_file(pkg_root, schema_rel)?;

    if !validator.is_valid(output) {
        anyhow::bail!("output failed validation against {schema_rel}");
    }

    Ok(())
}

// ── Model requirements check ──────────────────────────────────────────────────

fn check_model_requirements(reqs: &scenery_manifest::ModelRequirements, _client: &dyn ModelClient) {
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
            format!("      inputs: {input_schema_json}\n      outputs: {{\"type\": \"object\"}}");
        let skill_md = format!(
            "---\nname: Test Skill\ndescription: A test skill.\nmetadata:\n  scenery:\n    version: 0.1.0\n    publisher: scenery\n    permissions:\n      filesystem: none\n      network: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n    schemas:\n{input_schema_yaml}\n    workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
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

    // ── Bug #9 regression test ──────────────────────────────────────────────
    // When state.db has a record for a skill but the install directory on disk
    // is missing (e.g. corrupted install, manual rm), `skill run` previously
    // surfaced a generic "failed to load skill package" error that gave no hint
    // about what went wrong or how to fix it.  The fix must return a message
    // that mentions state.db and suggests a reinstall command.
    #[test]
    fn run_errors_with_actionable_message_when_db_record_exists_but_dir_missing() {
        use rusqlite::params;
        use rusqlite::Connection;

        let state_root = temp_root("run-corrupt-install");
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");

        // Seed state.db with an installed_skills row whose install_root points
        // at a directory that we deliberately do NOT create on disk.
        let phantom_install_root = state_root.join("skills").join("phantom-skill");
        let conn = Connection::open(&state.db_path).expect("open db");
        conn.execute(
            "INSERT INTO installed_skills \
             (skill_id, active_version, install_root, channel, current_status) \
             VALUES (?1, ?2, ?3, 'stable', 'active')",
            params!["phantom-skill", "1.0.0", phantom_install_root.as_str()],
        )
        .expect("seed row should insert");
        drop(conn);

        let client = MockPolicyClient::new();
        let err = run_skill(
            &state,
            &client,
            "phantom-skill",
            &serde_json::json!({}),
            None,
        )
        .expect_err("corrupt install should produce an error");

        let msg = err.to_string();
        // Must mention state.db so the user understands what is inconsistent.
        assert!(
            msg.contains("state.db") || msg.contains("registered"),
            "error should mention state.db or 'registered'; got: {msg}"
        );
        // Must suggest a remediation path.
        assert!(
            msg.contains("reinstall") || msg.contains("install"),
            "error should suggest reinstall; got: {msg}"
        );
        // Must NOT look like a plain "not installed" message (that's misleading
        // when the DB record exists but files are missing).
        assert!(
            !msg.contains("is not installed"),
            "should not use the generic 'not installed' message for a corrupt install; got: {msg}"
        );

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

    // ── Prompt file confinement ─────────────────────────────────────────────

    /// Records every system prompt it is sent.
    struct RecordingModelClient {
        system_prompts: std::sync::Mutex<Vec<String>>,
    }

    impl ModelClient for RecordingModelClient {
        fn generate(&self, request: ModelRequest) -> Result<crate::model::ModelResponse> {
            self.system_prompts
                .lock()
                .expect("lock")
                .push(request.system_prompt);
            Ok(crate::model::ModelResponse {
                text: "ok".to_string(),
                prompt_tokens: 1,
                completion_tokens: 1,
                latency_ms: 1,
                source: ModelSource::Local("recording".to_string()),
                cost_usd: 0.0,
            })
        }
    }

    fn run_llm_step_with_prompt_file(
        pkg: &SkillPackage,
        rel: &str,
        client: &RecordingModelClient,
    ) -> Result<StepResult> {
        execute_llm_step(
            pkg,
            LlmStepParams {
                id: "run",
                prompt_source: &PromptSource::File(rel.to_string()),
                step_inputs: &None,
                output_schema_rel: None,
                run_input: &serde_json::json!({}),
                step_outputs: &HashMap::new(),
                client,
            },
        )
    }

    #[test]
    fn llm_step_reads_in_bundle_prompt_reached_through_dot_dot_and_absolute_path() {
        let skill_root = temp_root("prompt-in-bundle");
        write_skill_bundle(&skill_root, "{}");
        let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
        let bundle_name = skill_root.file_name().expect("bundle dir name");
        let absolute = skill_root.join("prompts/system.txt");

        let client = RecordingModelClient {
            system_prompts: std::sync::Mutex::new(Vec::new()),
        };
        for rel in [
            "prompts/../prompts/system.txt".to_string(),
            format!("../{bundle_name}/prompts/system.txt"),
            absolute.to_string(),
        ] {
            run_llm_step_with_prompt_file(&pkg, &rel, &client)
                .unwrap_or_else(|e| panic!("{rel}: {e:#}"));
        }
        assert_eq!(
            *client.system_prompts.lock().expect("lock"),
            vec!["Do the thing.".to_string(); 3]
        );

        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn llm_step_refuses_prompt_file_outside_bundle_without_calling_the_model() {
        let skill_root = temp_root("prompt-escape");
        let outside = temp_root("prompt-escape-outside");
        write_skill_bundle(&skill_root, "{}");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("secret.txt"), "TOP SECRET").expect("secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.join("secret.txt"),
            skill_root.join("prompts/link.txt"),
        )
        .expect("symlink");
        let pkg = SkillPackage::load_from_dir(&skill_root).expect("load skill");
        let outside_name = outside.file_name().expect("outside dir name");

        let client = RecordingModelClient {
            system_prompts: std::sync::Mutex::new(Vec::new()),
        };
        let mut escapes = vec![
            outside.join("secret.txt").to_string(),
            outside.join("missing.txt").to_string(),
            format!("../{outside_name}/secret.txt"),
            format!("../{outside_name}/missing.txt"),
        ];
        if cfg!(unix) {
            escapes.push("prompts/link.txt".to_string());
        }
        for rel in &escapes {
            let err = run_llm_step_with_prompt_file(&pkg, rel, &client)
                .expect_err("escaping prompt path must be refused");
            assert!(
                err.to_string().contains("outside the skill bundle"),
                "{rel}: {err:#}"
            );
        }
        assert!(client.system_prompts.lock().expect("lock").is_empty());

        let _ = fs::remove_dir_all(&skill_root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn validate_output_refuses_schema_file_outside_bundle() {
        let skill_root = temp_root("schema-escape");
        let outside = temp_root("schema-escape-outside");
        write_skill_bundle(&skill_root, "{}");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("s.json"), r#"{"type":"object"}"#).expect("outside schema");
        let outside_name = outside.file_name().expect("outside dir name");

        for rel in [
            outside.join("s.json").to_string(),
            format!("../{outside_name}/s.json"),
        ] {
            let err = validate_output(&skill_root, &rel, &serde_json::json!({}))
                .expect_err("escaping schema path must be refused");
            assert!(
                err.to_string().contains("outside the skill bundle"),
                "{rel}: {err:#}"
            );
        }

        let _ = fs::remove_dir_all(&skill_root);
        let _ = fs::remove_dir_all(&outside);
    }

    // ── F21: crafted schemas must be refused, never crash or hang the daemon ─

    use crate::schema_guard::tests::{
        deep_schema, fanout_schema, is_child, run_isolated, wide_schema,
    };
    use std::time::Duration;

    /// Place a project-scope skill whose inline inputs schema is `schema` (no
    /// install step, so the run-time guard is what's under test, modelling a
    /// tampered or pre-fix install) and run it; the run must fail with a clear
    /// error mentioning `expect`.
    fn run_with_input_schema_expect_refusal(label: &str, schema: &serde_json::Value, expect: &str) {
        let state_root = temp_root(label);
        let project = temp_root(&format!("{label}-project"));
        let state = AppState::bootstrap_in(state_root.clone()).expect("bootstrap");
        write_skill_bundle(
            &project.join(".scenery/skills/test-skill"),
            &schema.to_string(),
        );
        let err = run_skill_with_scope(
            &state,
            &MockPolicyClient::new(),
            "test-skill",
            &serde_json::json!({}),
            None,
            Some(&project),
        )
        .expect_err("crafted inputs schema must be refused");
        assert!(format!("{err:#}").contains(expect), "got: {err:#}");
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn run_refuses_self_referential_input_schema() {
        run_isolated(
            "executor::tests::child_run_self_ref_input_schema",
            Duration::from_secs(30),
        );
    }

    #[test]
    #[ignore = "child of run_refuses_self_referential_input_schema"]
    fn child_run_self_ref_input_schema() {
        if !is_child() {
            return;
        }
        run_with_input_schema_expect_refusal(
            "f21-selfref",
            &serde_json::json!({"$ref": "#"}),
            "$ref",
        );
    }

    #[test]
    fn run_refuses_ref_fanout_input_schema_quickly() {
        run_isolated(
            "executor::tests::child_run_fanout_input_schema",
            Duration::from_secs(30),
        );
    }

    #[test]
    #[ignore = "child of run_refuses_ref_fanout_input_schema_quickly"]
    fn child_run_fanout_input_schema() {
        if !is_child() {
            return;
        }
        run_with_input_schema_expect_refusal("f21-fanout", &fanout_schema(40), "$ref");
    }

    #[test]
    fn run_refuses_over_deep_input_schema() {
        run_with_input_schema_expect_refusal("f21-deep", &deep_schema(40), "nested too deeply");
    }

    #[test]
    fn run_refuses_over_large_input_schema() {
        run_with_input_schema_expect_refusal("f21-large", &wide_schema(10_000), "too large");
    }

    #[test]
    fn validate_output_refuses_self_referential_schema_file() {
        run_isolated(
            "executor::tests::child_validate_output_self_ref",
            Duration::from_secs(30),
        );
    }

    #[test]
    #[ignore = "child of validate_output_refuses_self_referential_schema_file"]
    fn child_validate_output_self_ref() {
        if !is_child() {
            return;
        }
        for (label, schema) in [
            ("selfref", serde_json::json!({"$ref": "#"})),
            ("fanout", fanout_schema(40)),
        ] {
            let root = temp_root(&format!("f21-out-{label}"));
            fs::create_dir_all(root.join("schemas")).expect("mkdir");
            fs::write(root.join("schemas/out.json"), schema.to_string()).expect("write");
            let err = validate_output(&root, "schemas/out.json", &serde_json::json!({}))
                .expect_err("crafted output schema must be refused");
            assert!(format!("{err:#}").contains("$ref"), "{label}: {err:#}");
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn validate_output_still_checks_normal_schema() {
        let root = temp_root("f21-out-normal");
        fs::create_dir_all(root.join("schemas")).expect("mkdir");
        fs::write(
            root.join("schemas/out.json"),
            r#"{"type":"object","required":["a"]}"#,
        )
        .expect("write");
        validate_output(&root, "schemas/out.json", &serde_json::json!({"a": 1})).expect("valid");
        validate_output(&root, "schemas/out.json", &serde_json::json!({}))
            .expect_err("missing field must fail");
        let _ = fs::remove_dir_all(&root);
    }
}
