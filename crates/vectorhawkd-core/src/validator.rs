use camino::Utf8Path;
use vectorhawkd_manifest::SkillPackage;

#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct ValidationReport {
    pub checks: Vec<CheckResult>,
}

impl ValidationReport {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// Run all validation checks against an unpacked skill bundle directory.
///
/// Checks (in order):
/// 1. Manifest parses, required fields present, referenced files exist,
///    workflow parses, and workflow prompt refs resolve.
/// 2. `inputs_schema` file is valid JSON and a valid JSON Schema document.
/// 3. `outputs_schema` file is valid JSON and a valid JSON Schema document.
///
/// All checks are always run; the caller receives the full report.
pub fn validate_bundle(path: &Utf8Path) -> ValidationReport {
    let mut checks = Vec::new();

    let pkg = match SkillPackage::load_from_dir(path) {
        Ok(pkg) => {
            checks.push(ok("manifest and workflow"));
            Some(pkg)
        }
        Err(e) => {
            checks.push(fail("manifest and workflow", &e.to_string()));
            None::<SkillPackage>
        }
    };

    if let Some(pkg) = pkg {
        checks.push(check_json_schema_value(
            "inputs_schema",
            pkg.manifest.inputs_schema.as_ref(),
        ));
        checks.push(check_json_schema_value(
            "outputs_schema",
            pkg.manifest.outputs_schema.as_ref(),
        ));
    }

    ValidationReport { checks }
}

fn check_json_schema_value(label: &str, schema: Option<&serde_json::Value>) -> CheckResult {
    let name = format!("{label} is valid JSON Schema");

    let json = match schema {
        Some(v) => v,
        None => return ok(&name),
    };

    match jsonschema::JSONSchema::compile(json) {
        Ok(_) => ok(&name),
        Err(e) => fail(&name, &format!("not valid JSON Schema: {e}")),
    }
}

fn ok(name: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        passed: true,
        detail: None,
    }
}

fn fail(name: &str, detail: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
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
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_version: 0.1.0\nvh_publisher: skillclub\nvh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n",
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
        assert_eq!(report.checks.len(), 3);

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
            .find(|c| c.name == "manifest and workflow")
            .expect("manifest check must exist");
        assert!(!manifest_check.passed, "expected manifest check to fail");
        assert!(!report.all_passed());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_all_checks_even_when_some_fail() {
        let dir = temp_dir("partial-fail");
        fs::create_dir_all(dir.join("prompts")).expect("create prompts dir");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: Test.\nlicense: MIT\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_schemas:\n  outputs:\n    type: 42\nvh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n",
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
            .find(|c| c.name == "manifest and workflow")
            .expect("manifest check must exist");
        assert!(manifest_check.passed, "manifest check should pass");

        let output_check = report
            .checks
            .iter()
            .find(|c| c.name.contains("output"))
            .expect("output schema check must exist");
        assert!(!output_check.passed, "output schema check should fail");

        let _ = fs::remove_dir_all(&dir);
    }
}
