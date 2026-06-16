//! SKILL.md loader — parses the `metadata.vectorhawk.*` frontmatter and builds
//! an in-memory `SkillPackage` without writing any intermediate files to disk.
//!
//! The schema for the frontmatter lives at
//! `vectorhawkd-manifest/schemas/skill_md_frontmatter.json`. The rules here
//! mirror that schema. All VectorHawk-specific fields live under the
//! `metadata.vectorhawk` namespace; top-level `vh_*` keys are rejected with
//! a migration hint.

use crate::{
    to_skill_id, ClipboardAccess, Execution, FilesystemAccess, Manifest, ManifestError,
    ModelFallback, ModelRequirements, Permissions, PromptSource, SandboxProfile, SkillPackage,
    UpdateConfig, Workflow, WorkflowStep,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::fs;

// ── Top-level frontmatter structs ────────────────────────────────────────────

/// The YAML frontmatter block at the top of a SKILL.md file.
///
/// Top-level keys are the minimal standard set: `name`, `description`,
/// `license`. All VectorHawk-specific fields live under
/// `metadata.vectorhawk.*`. Top-level `vh_*` keys are explicitly rejected
/// with a migration error (see `reject_unknown_vh_keys`).
#[derive(Debug, Deserialize)]
struct SkillMdFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    metadata: Option<SkillMdMetadata>,
}

#[derive(Debug, Deserialize)]
struct SkillMdMetadata {
    #[serde(default)]
    vectorhawk: Option<VhExtensions>,
}

/// All VectorHawk-specific frontmatter fields, nested under
/// `metadata.vectorhawk`.
// JUSTIFICATION: skill_type and declared_tools are parsed and validated but
// not yet consumed by the runner. They exist to prevent valid SKILL.md files
// from being rejected and for future registry-side enforcement.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct VhExtensions {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    permissions: Option<VhPermissions>,
    #[serde(default)]
    execution: Option<VhExecution>,
    #[serde(default)]
    model: Option<VhModel>,
    #[serde(default)]
    schemas: Option<VhSchemas>,
    #[serde(default)]
    workflow: Option<Vec<WorkflowStep>>,
    #[serde(default)]
    workflow_ref: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
    /// Skill type tag (e.g., "skill"). Renamed from the reserved word `type`.
    #[serde(default, rename = "type")]
    skill_type: Option<String>,
    #[serde(default)]
    declared_tools: Vec<String>,
}

/// `permissions` sub-object under `metadata.vectorhawk`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VhPermissions {
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    filesystem: Option<FilesystemAccess>,
    #[serde(default)]
    clipboard: Option<ClipboardAccess>,
}

/// `execution` sub-object under `metadata.vectorhawk`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VhExecution {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    sandbox: Option<SandboxProfile>,
}

/// `model` sub-object under `metadata.vectorhawk`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VhModel {
    #[serde(default)]
    min_params_b: Option<f64>,
    #[serde(default)]
    recommended: Vec<String>,
    #[serde(default)]
    fallback: Option<ModelFallback>,
    #[serde(default)]
    prefer_local: Option<bool>,
}

/// `schemas` sub-object under `metadata.vectorhawk`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VhSchemas {
    #[serde(default)]
    inputs: Option<serde_json::Value>,
    #[serde(default)]
    outputs: Option<serde_json::Value>,
}

// ── Public loader ─────────────────────────────────────────────────────────────

/// Load a `SkillPackage` from a directory that contains a `SKILL.md` at its
/// root. The workflow is sourced either from inline `metadata.vectorhawk.workflow`
/// steps in the frontmatter or from the file referenced by
/// `metadata.vectorhawk.workflow_ref`.
///
/// Returns a `ManifestError` if:
/// - `SKILL.md` is missing or unparseable.
/// - Required frontmatter fields are absent.
/// - A referenced workflow file is missing.
/// - Any top-level `vh_*` key is present (migration error with hint).
/// - Any unknown key appears under `metadata.vectorhawk`.
/// - Both `workflow` and `workflow_ref` are set simultaneously.
pub(crate) fn load_from_skill_md_dir(root: Utf8PathBuf) -> Result<SkillPackage, ManifestError> {
    let skill_md_path = root.join("SKILL.md");
    let content = fs::read_to_string(&skill_md_path)?;

    let (raw_yaml, body) = split_frontmatter(&content).ok_or_else(|| {
        ManifestError::Invalid(
            "SKILL.md must begin with a --- frontmatter block followed by a closing ---"
                .to_string(),
        )
    })?;

    // Reject top-level vh_* keys and unknown keys under metadata.vectorhawk.
    reject_unknown_vh_keys(raw_yaml)?;

    let frontmatter: SkillMdFrontmatter =
        serde_yaml::from_str(raw_yaml).map_err(|e| ManifestError::Invalid(e.to_string()))?;

    validate_frontmatter_basics(&frontmatter)?;

    // Derive id from name (AUTH1b decision 10).
    let id = to_skill_id(&frontmatter.name);

    // Version defaults to "0.1.0" if omitted.
    let version_str = vh(&frontmatter)
        .and_then(|v| v.version.as_deref())
        .unwrap_or("0.1.0");
    let version = semver::Version::parse(version_str).map_err(|e| {
        ManifestError::Invalid(format!(
            "metadata.vectorhawk.version is not valid semver: {e}"
        ))
    })?;

    let publisher = vh(&frontmatter)
        .and_then(|v| v.publisher.clone())
        .unwrap_or_else(|| "local".to_string());

    let permissions = build_permissions(vh(&frontmatter).and_then(|v| v.permissions.as_ref()));
    let execution = build_execution(vh(&frontmatter).and_then(|v| v.execution.as_ref()));

    let model_requirements =
        vh(&frontmatter)
            .and_then(|v| v.model.as_ref())
            .map(|m| ModelRequirements {
                min_params_b: m.min_params_b,
                recommended: m.recommended.clone(),
                fallback: m.fallback,
                prefer_local: m.prefer_local,
            });

    let (inputs_schema, outputs_schema) = match vh(&frontmatter).and_then(|v| v.schemas.as_ref()) {
        Some(s) => (s.inputs.clone(), s.outputs.clone()),
        None => (None, None),
    };

    // Build a synthetic system prompt entrypoint name.
    let entrypoint = "workflow.yaml".to_string();

    let raw_triggers = vh(&frontmatter)
        .map(|v| v.triggers.clone())
        .unwrap_or_default();

    let manifest = Manifest {
        schema_version: "1.0".to_string(),
        id,
        name: frontmatter.name.clone(),
        version,
        publisher,
        description: Some(frontmatter.description.clone()),
        license: frontmatter.license.clone(),
        entrypoint,
        inputs_schema,
        outputs_schema,
        permissions,
        execution,
        model_requirements,
        update: Some(UpdateConfig {
            channel: Some("stable".to_string()),
            auto_update: Some(true),
        }),
        triggers: validate_and_normalize_triggers(raw_triggers)?,
    };

    let workflow = build_workflow(&root, &frontmatter, &manifest.name, body)?;

    Ok(SkillPackage {
        root,
        manifest,
        workflow,
    })
}

/// Extract the `metadata.vectorhawk` block from a frontmatter struct, if present.
fn vh(fm: &SkillMdFrontmatter) -> Option<&VhExtensions> {
    fm.metadata.as_ref()?.vectorhawk.as_ref()
}

// ── Frontmatter parsing helpers ───────────────────────────────────────────────

/// Split a SKILL.md `content` string into the YAML block (between `---` fences)
/// and the Markdown body after the closing fence. Returns `None` if the
/// fences are absent or malformed.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let after_open = content.strip_prefix("---\n")?;
    let close = after_open.find("\n---\n")?;
    let yaml_str = &after_open[..close];
    let body = &after_open[close + 5..]; // skip "\n---\n"
    Some((yaml_str, body))
}

/// Validate the raw YAML for namespace compliance:
///
/// (a) Any top-level key beginning with `vh_` is rejected — these must be
///     moved under `metadata.vectorhawk`.
/// (b) Any key directly under `metadata.vectorhawk` that is not in
///     `ALLOWED_VH_KEYS` is rejected as unknown.
fn reject_unknown_vh_keys(yaml_str: &str) -> Result<(), ManifestError> {
    const ALLOWED_VH_KEYS: &[&str] = &[
        "version",
        "publisher",
        "permissions",
        "execution",
        "model",
        "schemas",
        "workflow",
        "workflow_ref",
        "triggers",
        "type",
        "declared_tools",
    ];

    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml_str).map_err(|e| ManifestError::Invalid(e.to_string()))?;

    let mapping = match &value {
        serde_yaml::Value::Mapping(m) => m,
        _ => return Ok(()),
    };

    // (a) Reject any top-level vh_* key.
    for key in mapping.keys() {
        if let serde_yaml::Value::String(k) = key {
            if k.starts_with("vh_") {
                return Err(ManifestError::Invalid(format!(
                    "top-level 'vh_*' fields are no longer supported: '{k}' must be moved \
                     under metadata.vectorhawk (e.g., 'metadata.vectorhawk.permissions' \
                     not 'vh_permissions')"
                )));
            }
        }
    }

    // (b) Validate keys under metadata.vectorhawk if the block is present.
    let vh_map = mapping
        .get(serde_yaml::Value::String("metadata".to_string()))
        .and_then(|m| {
            if let serde_yaml::Value::Mapping(inner) = m {
                inner.get(serde_yaml::Value::String("vectorhawk".to_string()))
            } else {
                None
            }
        });

    if let Some(serde_yaml::Value::Mapping(vh_fields)) = vh_map {
        for key in vh_fields.keys() {
            if let serde_yaml::Value::String(k) = key {
                if !ALLOWED_VH_KEYS.contains(&k.as_str()) {
                    return Err(ManifestError::Invalid(format!(
                        "unknown field under metadata.vectorhawk: '{k}' — \
                         allowed: {ALLOWED_VH_KEYS:?}"
                    )));
                }
            }
        }
    }

    Ok(())
}

fn validate_frontmatter_basics(fm: &SkillMdFrontmatter) -> Result<(), ManifestError> {
    if fm.name.trim().is_empty() {
        return Err(ManifestError::Invalid(
            "SKILL.md 'name' field must be non-empty".to_string(),
        ));
    }
    if fm.description.trim().is_empty() {
        return Err(ManifestError::Invalid(
            "SKILL.md 'description' field must be non-empty".to_string(),
        ));
    }
    let workflow_conflict = vh(fm)
        .map(|v| v.workflow.is_some() && v.workflow_ref.is_some())
        .unwrap_or(false);
    if workflow_conflict {
        return Err(ManifestError::Invalid(
            "workflow and workflow_ref are mutually exclusive; use one or neither".to_string(),
        ));
    }
    Ok(())
}

/// Validate and normalize a raw `metadata.vectorhawk.triggers` list from the
/// frontmatter.
///
/// Rules applied in order:
/// 1. Strip empty strings.
/// 2. Lowercase-normalize each item.
/// 3. Deduplicate (post-normalize, so "Compare" and "compare" collapse).
/// 4. Reject if more than 10 items remain.
/// 5. Reject any item shorter than 3 characters or longer than 200 characters.
fn validate_and_normalize_triggers(raw: Vec<String>) -> Result<Vec<String>, ManifestError> {
    // Step 1 + 2: strip empties and lowercase.
    let mut normalized: Vec<String> = raw
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    // Step 3: dedup while preserving first-seen order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    normalized.retain(|s| seen.insert(s.clone()));

    // Step 4: max 10 triggers.
    if normalized.len() > 10 {
        return Err(ManifestError::Invalid(format!(
            "metadata.vectorhawk.triggers may contain at most 10 items, got {}",
            normalized.len()
        )));
    }

    // Step 5: per-item length constraints.
    for trigger in &normalized {
        if trigger.len() < 3 {
            return Err(ManifestError::Invalid(format!(
                "metadata.vectorhawk.triggers item '{}' is too short (minimum 3 characters)",
                trigger
            )));
        }
        if trigger.len() > 200 {
            return Err(ManifestError::Invalid(format!(
                "metadata.vectorhawk.triggers item is too long ({} characters, maximum 200)",
                trigger.len()
            )));
        }
    }

    Ok(normalized)
}

// ── Sub-struct builders ───────────────────────────────────────────────────────

fn build_permissions(vh: Option<&VhPermissions>) -> Permissions {
    match vh {
        None => Permissions {
            filesystem: FilesystemAccess::None,
            network: "none".to_string(),
            clipboard: ClipboardAccess::None,
        },
        Some(p) => Permissions {
            filesystem: p.filesystem.unwrap_or(FilesystemAccess::None),
            network: p.network.clone().unwrap_or_else(|| "none".to_string()),
            clipboard: p.clipboard.unwrap_or(ClipboardAccess::None),
        },
    }
}

fn build_execution(vh: Option<&VhExecution>) -> Execution {
    match vh {
        None => Execution {
            sandbox: SandboxProfile::Strict,
            timeout_ms: 120_000,
            memory_mb: 512,
        },
        Some(e) => Execution {
            sandbox: e.sandbox.unwrap_or(SandboxProfile::Strict),
            timeout_ms: e.timeout_ms.unwrap_or(120_000),
            memory_mb: e.memory_mb.unwrap_or(512),
        },
    }
}

// ── Workflow builder ──────────────────────────────────────────────────────────

fn build_workflow(
    root: &Utf8Path,
    fm: &SkillMdFrontmatter,
    skill_name: &str,
    body: &str,
) -> Result<Workflow, ManifestError> {
    if let Some(ref_path) = vh(fm).and_then(|v| v.workflow_ref.as_ref()) {
        // Load workflow from a referenced YAML file.
        let workflow_path = root.join(ref_path.trim_start_matches("./"));
        if !workflow_path.exists() {
            return Err(ManifestError::MissingFile(ref_path.clone()));
        }
        let text = fs::read_to_string(&workflow_path)?;
        let workflow: Workflow =
            serde_yaml::from_str(&text).map_err(|e| ManifestError::Invalid(e.to_string()))?;
        validate_workflow_prompt_refs(root, &workflow)?;
        return Ok(workflow);
    }

    if let Some(inline_steps) = vh(fm).and_then(|v| v.workflow.as_ref()) {
        let wf = Workflow {
            name: to_skill_id(skill_name),
            steps: inline_steps.clone(),
        };
        validate_workflow_prompt_refs(root, &wf)?;
        return Ok(wf);
    }

    // No workflow defined — synthesize a single LLM step from the SKILL.md body.
    // The body is stored as an inline prompt so no disk write is needed.
    // AUTH1d: eliminated the prompts/system.txt workaround; executor now matches
    // on PromptSource::Inline directly.
    let workflow_name = to_skill_id(skill_name);
    Ok(Workflow {
        name: workflow_name.clone(),
        steps: vec![WorkflowStep::Llm {
            id: "generate".to_string(),
            prompt: PromptSource::Inline(body.trim().to_string()),
            inputs: None,
            output_schema: None,
        }],
    })
}

fn validate_workflow_prompt_refs(
    root: &Utf8Path,
    workflow: &Workflow,
) -> Result<(), ManifestError> {
    for step in &workflow.steps {
        if let WorkflowStep::Llm { prompt, .. } = step {
            match prompt {
                PromptSource::Inline(body) => {
                    // Inline prompts have no file to validate; just check non-empty.
                    if body.trim().is_empty() {
                        return Err(ManifestError::Invalid(
                            "llm step inline prompt body must be non-empty".to_string(),
                        ));
                    }
                }
                PromptSource::File(path) => {
                    if path.trim().is_empty() {
                        return Err(ManifestError::Invalid(
                            "llm step prompt path must be non-empty".to_string(),
                        ));
                    }
                    let p = root.join(path.as_str());
                    if !p.exists() {
                        return Err(ManifestError::MissingFile(path.clone()));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::fs;

    /// Write a minimal SKILL.md to a tempdir and load it, returning the package.
    fn load_skill_md(skill_md_content: &str) -> Result<SkillPackage, ManifestError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        fs::write(path.join("SKILL.md"), skill_md_content).unwrap();
        let utf8_path = Utf8PathBuf::from_path_buf(path).unwrap();
        load_from_skill_md_dir(utf8_path)
    }

    /// Minimal valid SKILL.md body (no triggers).
    fn minimal_skill_md(extra_frontmatter: &str) -> String {
        format!(
            "---\nname: test-skill\ndescription: A test skill.\n{extra_frontmatter}---\n\nDo the thing.\n"
        )
    }

    // ── validate_and_normalize_triggers unit tests ────────────────────────────

    #[test]
    fn test_vh_triggers_parsed_into_manifest() {
        let content = minimal_skill_md(
            "metadata:\n  vectorhawk:\n    triggers:\n      - compare contracts\n      - diff legal documents\n      - review contract changes\n",
        );
        let pkg = load_skill_md(&content).unwrap();
        assert_eq!(pkg.manifest.triggers.len(), 3);
        assert!(pkg
            .manifest
            .triggers
            .contains(&"compare contracts".to_string()));
        assert!(pkg
            .manifest
            .triggers
            .contains(&"diff legal documents".to_string()));
        assert!(pkg
            .manifest
            .triggers
            .contains(&"review contract changes".to_string()));
    }

    #[test]
    fn test_vh_triggers_empty_is_ok() {
        let content = minimal_skill_md("");
        let pkg = load_skill_md(&content).unwrap();
        assert!(pkg.manifest.triggers.is_empty());
    }

    #[test]
    fn test_vh_triggers_max_10_rejects_11() {
        let trigger_items = (1..=11)
            .map(|i| format!("      - trigger phrase {i}\n"))
            .collect::<String>();
        let content = minimal_skill_md(&format!(
            "metadata:\n  vectorhawk:\n    triggers:\n{trigger_items}"
        ));
        let err = load_skill_md(&content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("at most 10"),
            "expected 'at most 10' in error, got: {msg}"
        );
    }

    #[test]
    fn test_vh_triggers_too_short_rejected() {
        // Two-character trigger is below the 3-char minimum.
        let content =
            minimal_skill_md("metadata:\n  vectorhawk:\n    triggers:\n      - ok\n      - ab\n");
        let err = load_skill_md(&content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too short"),
            "expected 'too short' in error, got: {msg}"
        );
    }

    #[test]
    fn test_vh_triggers_too_long_rejected() {
        // 201-character trigger exceeds the 200-char limit.
        let long = "a".repeat(201);
        let content = minimal_skill_md(&format!(
            "metadata:\n  vectorhawk:\n    triggers:\n      - {long}\n"
        ));
        let err = load_skill_md(&content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too long"),
            "expected 'too long' in error, got: {msg}"
        );
    }

    #[test]
    fn test_vh_triggers_deduplication() {
        // "compare", "COMPARE", and "compare" should collapse to a single entry.
        let content = minimal_skill_md(
            "metadata:\n  vectorhawk:\n    triggers:\n      - compare\n      - COMPARE\n      - compare\n",
        );
        let pkg = load_skill_md(&content).unwrap();
        assert_eq!(
            pkg.manifest.triggers.len(),
            1,
            "expected 1 item after dedup, got: {:?}",
            pkg.manifest.triggers
        );
        assert_eq!(pkg.manifest.triggers[0], "compare");
    }

    #[test]
    fn test_vh_triggers_empty_strings_stripped() {
        // Empty strings in the list must be silently dropped.
        let content = minimal_skill_md(
            "metadata:\n  vectorhawk:\n    triggers:\n      - \"\"\n      - valid trigger\n      - \"\"\n",
        );
        let pkg = load_skill_md(&content).unwrap();
        assert_eq!(
            pkg.manifest.triggers.len(),
            1,
            "expected 1 item after stripping empties, got: {:?}",
            pkg.manifest.triggers
        );
        assert_eq!(pkg.manifest.triggers[0], "valid trigger");
    }

    #[test]
    fn test_top_level_vh_key_is_rejected() {
        // A top-level vh_* key must now produce a clear migration error.
        let content = minimal_skill_md("vh_permissions:\n  network: none\n");
        let err = load_skill_md(&content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("metadata.vectorhawk"),
            "error should mention metadata.vectorhawk migration path, got: {msg}"
        );
    }
}
