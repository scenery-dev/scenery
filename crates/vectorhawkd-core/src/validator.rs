use camino::Utf8Path;
use vectorhawkd_manifest::SkillPackage;

/// The severity level of a single check result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    /// Check passed with no issues.
    Pass,
    /// Check found a potential issue but does not block publishing.
    Warn,
    /// Check found a definitive problem that must be fixed.
    Fail,
}

#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub level: CheckLevel,
    pub detail: Option<String>,
    /// Legacy compat: callers that only care about pass/fail.
    pub passed: bool,
}

#[derive(Debug)]
pub struct ValidationReport {
    pub checks: Vec<CheckResult>,
}

impl ValidationReport {
    /// Returns `true` only if every check is `Pass` or `Warn`.
    /// A single `Fail` makes this return `false`.
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.level != CheckLevel::Fail)
    }

    /// Returns the number of checks that failed.
    pub fn fail_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.level == CheckLevel::Fail)
            .count()
    }

    /// Returns the number of checks that warned.
    pub fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.level == CheckLevel::Warn)
            .count()
    }
}

/// Run all validation checks against an unpacked skill bundle directory.
///
/// Checks (in order):
/// 1. Manifest parses, required fields present, referenced files exist,
///    workflow parses, and workflow prompt refs resolve.
/// 2. Description must not be a TODO placeholder.
/// 3. Workflow step count and prompt binding summary.
/// 4. Input schema (if present) is valid JSON Schema.
/// 5. Output schema (if present) is valid JSON Schema.
/// 6. Triggers warning — absence doesn't block, but skills without triggers
///    won't surface in /skill-search.
///
/// All checks are always run; the caller receives the full report.
pub fn validate_bundle(path: &Utf8Path) -> ValidationReport {
    let mut checks = Vec::new();

    let pkg = match SkillPackage::load_from_dir(path) {
        Ok(pkg) => {
            // Build a concise manifest summary for the pass detail.
            let detail = format!(
                "{} v{} (publisher: {})",
                pkg.manifest.id, pkg.manifest.version, pkg.manifest.publisher
            );
            checks.push(pass("manifest", &detail));
            Some(pkg)
        }
        Err(e) => {
            checks.push(fail("manifest", &e.to_string()));
            None::<SkillPackage>
        }
    };

    let pkg = match pkg {
        Some(p) => p,
        None => return ValidationReport { checks },
    };

    // ── Description check ────────────────────────────────────────────────────

    let desc = pkg.manifest.description.as_deref().unwrap_or("");
    let desc_lower = desc.to_lowercase();
    if desc_lower.starts_with("todo:") || desc_lower == "todo: describe what this skill does" {
        checks.push(fail(
            "description",
            "description is still the TODO placeholder — update it before publishing",
        ));
    } else if desc.is_empty() {
        checks.push(fail("description", "description is empty"));
    } else {
        // Show a truncated preview in the detail.
        let preview = if desc.len() > 60 {
            format!("\"{}…\"", &desc[..59])
        } else {
            format!("\"{desc}\"")
        };
        checks.push(pass("description", &preview));
    }

    // ── Workflow check ───────────────────────────────────────────────────────

    let step_count = pkg.workflow.steps.len();
    let has_inline_prompt = pkg.workflow.steps.iter().any(|s| {
        use vectorhawkd_manifest::{PromptSource, WorkflowStep};
        match s {
            WorkflowStep::Llm { prompt, .. } => matches!(prompt, PromptSource::Inline(_)),
            _ => false,
        }
    });
    let prompt_note = if has_inline_prompt {
        "inline prompt"
    } else {
        "file prompt"
    };
    let workflow_detail = format!("{step_count} step(s), {prompt_note}, inputs bound");
    checks.push(pass("workflow", &workflow_detail));

    // ── Input schema check ───────────────────────────────────────────────────

    if let Some(schema) = pkg.manifest.inputs_schema.as_ref() {
        let required_fields = extract_required_fields(schema);
        let label = if required_fields.is_empty() {
            "input schema".to_string()
        } else {
            format!("input schema  required: [{required_fields}]")
        };
        match jsonschema::JSONSchema::compile(schema) {
            Ok(_) => checks.push(pass("input schema", &{
                if required_fields.is_empty() {
                    "valid JSON Schema".to_string()
                } else {
                    format!("required: [{required_fields}]")
                }
            })),
            Err(e) => checks.push(fail(&label, &format!("not valid JSON Schema: {e}"))),
        }
    } else {
        checks.push(pass("input schema", "none declared (accepts any input)"));
    }

    // ── Output schema check ──────────────────────────────────────────────────

    if let Some(schema) = pkg.manifest.outputs_schema.as_ref() {
        match jsonschema::JSONSchema::compile(schema) {
            Ok(_) => checks.push(pass("output schema", "valid JSON Schema")),
            Err(e) => checks.push(fail(
                "output schema",
                &format!("not valid JSON Schema: {e}"),
            )),
        }
    } else {
        checks.push(pass("output schema", "none declared"));
    }

    // ── Triggers check ───────────────────────────────────────────────────────

    if pkg.manifest.triggers.is_empty() {
        checks.push(warn(
            "triggers",
            "none — skill won't surface in /skill-search",
        ));
    } else {
        let phrases = pkg
            .manifest
            .triggers
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        checks.push(pass("triggers", &phrases));
    }

    ValidationReport { checks }
}

/// Extract the `required` field names from a JSON Schema object and format them
/// as a comma-separated string.  Returns an empty string when the schema has no
/// `required` array or it is empty.
fn extract_required_fields(schema: &serde_json::Value) -> String {
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn pass(name: &str, detail: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        level: CheckLevel::Pass,
        passed: true,
        detail: Some(detail.to_string()),
    }
}

fn warn(name: &str, detail: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        level: CheckLevel::Warn,
        passed: true,
        detail: Some(detail.to_string()),
    }
}

fn fail(name: &str, detail: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        level: CheckLevel::Fail,
        passed: false,
        detail: Some(detail.to_string()),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-validator-tests-{label}-{nanos}")),
        )
        .expect("temp path should be utf-8")
    }

    fn write_valid_bundle(root: &Utf8PathBuf) {
        fs::create_dir_all(root.join("prompts")).expect("create prompts dir");
        fs::write(
            root.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: A test skill.\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: skillclub\n    permissions:\n      filesystem: none\n      network: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n    workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n",
        )
        .expect("write SKILL.md");
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .expect("write workflow.yaml");
        fs::write(root.join("prompts/system.txt"), "Do the thing.").expect("write system.txt");
    }

    #[test]
    fn validate_passes_for_well_formed_bundle() {
        let dir = temp_dir("ok");
        write_valid_bundle(&dir);

        let report = validate_bundle(&dir);

        assert!(
            report.all_passed(),
            "expected all checks to pass: {report:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_fails_manifest_check_when_skill_md_is_missing() {
        let dir = temp_dir("no-skill-md");
        fs::create_dir_all(&dir).expect("create dir");

        let report = validate_bundle(&dir);

        let manifest_check = report
            .checks
            .iter()
            .find(|c| c.name == "manifest")
            .expect("manifest check must exist");
        assert!(
            manifest_check.level == CheckLevel::Fail,
            "expected manifest check to fail"
        );
        assert!(!report.all_passed());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_all_checks_even_when_some_fail() {
        let dir = temp_dir("partial-fail");
        fs::create_dir_all(dir.join("prompts")).expect("create prompts dir");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: Test.\nmetadata:\n  vectorhawk:\n    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n    schemas:\n      outputs:\n        type: 42\n    workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n",
        )
        .expect("write SKILL.md");
        fs::write(
            dir.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .expect("write workflow.yaml");
        fs::write(dir.join("prompts/system.txt"), "Do the thing.").expect("write system.txt");

        let report = validate_bundle(&dir);

        let manifest_check = report
            .checks
            .iter()
            .find(|c| c.name == "manifest")
            .expect("manifest check must exist");
        assert!(
            manifest_check.level == CheckLevel::Pass,
            "manifest check should pass"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_warns_when_triggers_empty() {
        let dir = temp_dir("no-triggers");
        write_valid_bundle(&dir);

        let report = validate_bundle(&dir);

        let triggers_check = report
            .checks
            .iter()
            .find(|c| c.name == "triggers")
            .expect("triggers check must exist");
        assert_eq!(
            triggers_check.level,
            CheckLevel::Warn,
            "missing triggers should produce a warning, not a failure"
        );
        assert!(
            report.all_passed(),
            "warning-only report should be considered passing"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_fails_with_todo_description() {
        let dir = temp_dir("todo-desc");
        fs::create_dir_all(dir.join("prompts")).expect("create prompts dir");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: \"TODO: describe what this skill does\"\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: skillclub\n    permissions:\n      filesystem: none\n      network: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 30000\n      memory_mb: 256\n    workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n",
        )
        .expect("write SKILL.md");
        fs::write(
            dir.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .expect("write workflow.yaml");
        fs::write(dir.join("prompts/system.txt"), "Do the thing.").expect("write system.txt");

        let report = validate_bundle(&dir);

        let desc_check = report
            .checks
            .iter()
            .find(|c| c.name == "description")
            .expect("description check must exist");
        assert_eq!(
            desc_check.level,
            CheckLevel::Fail,
            "TODO description should fail"
        );
        assert!(
            !report.all_passed(),
            "report with a failed check should not be considered passing"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
