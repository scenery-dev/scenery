//! SKILL.md loader — parses the extended `vh_*` frontmatter and builds an
//! in-memory `SkillPackage` without writing any intermediate files to disk.
//!
//! The schema for the frontmatter lives at
//! `vectorhawkd-manifest/schemas/skill_md_frontmatter.json`. The rules here
//! mirror that schema (AUTH1b decisions document, decisions 1–10).

use crate::{
    to_skill_id, ClipboardAccess, Execution, FilesystemAccess, Manifest, ManifestError,
    ModelFallback, ModelRequirements, Permissions, PromptSource, SandboxProfile, SkillPackage,
    UpdateConfig, Workflow, WorkflowStep,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::fs;

// ── Top-level frontmatter struct ─────────────────────────────────────────────

/// The YAML frontmatter block at the top of a SKILL.md file.
///
/// Unknown `vh_*` fields are rejected at deserialize time by the pattern
/// property guard in the JSON Schema. In Rust we enforce this by *not* using
/// `#[serde(deny_unknown_fields)]` on the top-level struct (to allow
/// forward-compatible non-`vh_` Anthropic fields), but explicitly checking
/// for unknown `vh_*` keys after deserialization.
#[derive(Debug, Deserialize)]
struct SkillMdFrontmatter {
    // ── Standard Anthropic Agent Skills fields (required) ────────────────────
    name: String,
    description: String,
    license: String,

    // ── VectorHawk core metadata ─────────────────────────────────────────────
    #[serde(default)]
    vh_version: Option<String>,
    #[serde(default)]
    vh_publisher: Option<String>,

    // ── VectorHawk optional blocks ───────────────────────────────────────────
    #[serde(default)]
    vh_permissions: Option<VhPermissions>,
    #[serde(default)]
    vh_execution: Option<VhExecution>,
    #[serde(default)]
    vh_model: Option<VhModel>,
    #[serde(default)]
    vh_schemas: Option<VhSchemas>,
    #[serde(default)]
    vh_workflow: Option<Vec<WorkflowStep>>,
    #[serde(default)]
    vh_workflow_ref: Option<String>,
    #[serde(default)]
    vh_triggers: Vec<String>,
}

/// `vh_permissions` sub-object.
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

/// `vh_execution` sub-object.
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

/// `vh_model` sub-object.
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

/// `vh_schemas` sub-object.
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
/// root. The workflow is sourced either from inline `vh_workflow` steps in the
/// frontmatter or from the file referenced by `vh_workflow_ref`.
///
/// Returns a `ManifestError` if:
/// - `SKILL.md` is missing or unparseable.
/// - Required frontmatter fields are absent.
/// - A referenced workflow file is missing.
/// - An unknown `vh_*` field is present (mirrors JSON Schema reject rule).
/// - Both `vh_workflow` and `vh_workflow_ref` are set simultaneously.
pub(crate) fn load_from_skill_md_dir(root: Utf8PathBuf) -> Result<SkillPackage, ManifestError> {
    let skill_md_path = root.join("SKILL.md");
    let content = fs::read_to_string(&skill_md_path)?;

    let (raw_yaml, body) = split_frontmatter(&content).ok_or_else(|| {
        ManifestError::Invalid(
            "SKILL.md must begin with a --- frontmatter block followed by a closing ---"
                .to_string(),
        )
    })?;

    // Reject unknown vh_* keys before structural deserialization.
    reject_unknown_vh_keys(raw_yaml)?;

    let frontmatter: SkillMdFrontmatter =
        serde_yaml::from_str(raw_yaml).map_err(|e| ManifestError::Invalid(e.to_string()))?;

    validate_frontmatter_basics(&frontmatter)?;

    // Derive id from name (AUTH1b decision 10).
    let id = to_skill_id(&frontmatter.name);

    // Version defaults to "0.1.0" if omitted.
    let version_str = frontmatter.vh_version.as_deref().unwrap_or("0.1.0");
    let version = semver::Version::parse(version_str)
        .map_err(|e| ManifestError::Invalid(format!("vh_version is not valid semver: {e}")))?;

    let publisher = frontmatter
        .vh_publisher
        .clone()
        .unwrap_or_else(|| "local".to_string());

    let permissions = build_permissions(frontmatter.vh_permissions.as_ref());
    let execution = build_execution(frontmatter.vh_execution.as_ref());

    let model_requirements = frontmatter.vh_model.as_ref().map(|m| ModelRequirements {
        min_params_b: m.min_params_b,
        recommended: m.recommended.clone(),
        fallback: m.fallback,
        prefer_local: m.prefer_local,
    });

    let (inputs_schema, outputs_schema) = match &frontmatter.vh_schemas {
        Some(s) => (s.inputs.clone(), s.outputs.clone()),
        None => (None, None),
    };

    // Build a synthetic system prompt entrypoint name.
    let entrypoint = "workflow.yaml".to_string();

    let manifest = Manifest {
        schema_version: "1.0".to_string(),
        id,
        name: frontmatter.name.clone(),
        version,
        publisher,
        description: Some(frontmatter.description.clone()),
        license: Some(frontmatter.license.clone()),
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
        triggers: validate_and_normalize_triggers(frontmatter.vh_triggers.clone())?,
    };

    let workflow = build_workflow(&root, &frontmatter, &manifest.name, body)?;

    Ok(SkillPackage {
        root,
        manifest,
        workflow,
    })
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

/// Scan the raw YAML string for keys that start with `vh_` but are not in the
/// explicit allowlist. This mirrors the JSON Schema's `patternProperties` guard.
fn reject_unknown_vh_keys(yaml_str: &str) -> Result<(), ManifestError> {
    const ALLOWED_VH_KEYS: &[&str] = &[
        "vh_version",
        "vh_publisher",
        "vh_permissions",
        "vh_execution",
        "vh_model",
        "vh_schemas",
        "vh_workflow",
        "vh_workflow_ref",
        "vh_triggers",
    ];

    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml_str).map_err(|e| ManifestError::Invalid(e.to_string()))?;

    if let serde_yaml::Value::Mapping(map) = &value {
        for key in map.keys() {
            if let serde_yaml::Value::String(k) = key {
                if k.starts_with("vh_") && !ALLOWED_VH_KEYS.contains(&k.as_str()) {
                    return Err(ManifestError::Invalid(format!(
                        "unknown vh_* field in SKILL.md frontmatter: '{k}' — \
                         only {ALLOWED_VH_KEYS:?} are permitted"
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
    if fm.license.trim().is_empty() {
        return Err(ManifestError::Invalid(
            "SKILL.md 'license' field must be non-empty".to_string(),
        ));
    }
    if fm.vh_workflow.is_some() && fm.vh_workflow_ref.is_some() {
        return Err(ManifestError::Invalid(
            "vh_workflow and vh_workflow_ref are mutually exclusive; use one or neither"
                .to_string(),
        ));
    }
    Ok(())
}

/// Validate and normalize a raw `vh_triggers` list from the frontmatter.
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
            "vh_triggers may contain at most 10 items, got {}",
            normalized.len()
        )));
    }

    // Step 5: per-item length constraints.
    for trigger in &normalized {
        if trigger.len() < 3 {
            return Err(ManifestError::Invalid(format!(
                "vh_triggers item '{}' is too short (minimum 3 characters)",
                trigger
            )));
        }
        if trigger.len() > 200 {
            return Err(ManifestError::Invalid(format!(
                "vh_triggers item is too long ({} characters, maximum 200)",
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
    if let Some(ref_path) = &fm.vh_workflow_ref {
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

    if let Some(inline_steps) = &fm.vh_workflow {
        validate_workflow_prompt_refs(
            root,
            &Workflow {
                name: to_skill_id(skill_name),
                steps: inline_steps.clone(),
            },
        )?;
        return Ok(Workflow {
            name: to_skill_id(skill_name),
            steps: inline_steps.clone(),
        });
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
            "---\nname: test-skill\ndescription: A test skill.\nlicense: MIT\n{extra_frontmatter}---\n\nDo the thing.\n"
        )
    }

    // ── validate_and_normalize_triggers unit tests ────────────────────────────

    #[test]
    fn test_vh_triggers_parsed_into_manifest() {
        let content = minimal_skill_md(
            "vh_triggers:\n  - compare contracts\n  - diff legal documents\n  - review contract changes\n",
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
        let triggers = (1..=11)
            .map(|i| format!("  - trigger phrase {i}\n"))
            .collect::<String>();
        let content = minimal_skill_md(&format!("vh_triggers:\n{triggers}"));
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
        let content = minimal_skill_md("vh_triggers:\n  - ok\n  - ab\n");
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
        let content = minimal_skill_md(&format!("vh_triggers:\n  - {long}\n"));
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
        let content = minimal_skill_md("vh_triggers:\n  - compare\n  - COMPARE\n  - compare\n");
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
        let content = minimal_skill_md("vh_triggers:\n  - \"\"\n  - valid trigger\n  - \"\"\n");
        let pkg = load_skill_md(&content).unwrap();
        assert_eq!(
            pkg.manifest.triggers.len(),
            1,
            "expected 1 item after stripping empties, got: {:?}",
            pkg.manifest.triggers
        );
        assert_eq!(pkg.manifest.triggers[0], "valid trigger");
    }
}
