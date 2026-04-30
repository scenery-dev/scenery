use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::fs;
use vectorhawkd_manifest::to_skill_id;

/// Files written when scaffolding a bundle from a SKILL.md.
#[derive(Debug)]
pub struct ScaffoldedBundle {
    pub id: String,
    pub output_dir: Utf8PathBuf,
    pub files: Vec<String>,
}

/// Outcome from a registry-routed import operation.
#[derive(Debug)]
pub enum ImportOutcome {
    /// A SKILL.md was scaffolded locally into a bundle directory.
    SkillScaffolded { bundle: Utf8PathBuf },
    /// The input was classified as an MCP server reference; registration
    /// was submitted and is pending IT review.
    McpServerRequested { server_name: String, status: String },
    /// The input was submitted to the registry as a skill import; the
    /// registry has accepted it for review / processing.
    SkillSubmitted {
        submission_id: String,
        status: String,
    },
}

/// YAML frontmatter block parsed from the top of a SKILL.md file.
#[derive(Debug, Deserialize)]
struct SkillMdFrontmatter {
    name: Option<String>,
    id: Option<String>,
    description: Option<String>,
    publisher: Option<String>,
    license: Option<String>,
}

/// Read a SKILL.md, parse its frontmatter and body, and scaffold a complete
/// bundle directory next to the source file.
///
/// The SKILL.md body becomes `prompts/system.txt`. A single `llm` workflow
/// step is generated that passes user requirements through the system prompt.
pub fn import_local_skill_md(skill_md_path: &Utf8Path) -> Result<ScaffoldedBundle> {
    let content = fs::read_to_string(skill_md_path)
        .with_context(|| format!("failed to read {skill_md_path}"))?;

    let (frontmatter, body) = parse_frontmatter(&content)
        .with_context(|| format!("failed to parse frontmatter in {skill_md_path}"))?;

    let (id, display_name) = match (&frontmatter.id, &frontmatter.name) {
        (Some(id), Some(name)) => (id.clone(), name.clone()),
        (Some(id), None) => {
            let name = id.replace('-', " ");
            (id.clone(), name)
        }
        (None, Some(name)) => {
            let id = to_skill_id(name);
            (id, name.clone())
        }
        (None, None) => {
            anyhow::bail!("SKILL.md frontmatter must include at least `name` or `id`");
        }
    };

    let parent = skill_md_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("SKILL.md has no parent directory"))?;
    let output_dir = parent.join(&id);
    let files = scaffold_bundle(&output_dir, &id, &display_name, &frontmatter, body.trim())?;

    Ok(ScaffoldedBundle {
        id,
        output_dir,
        files,
    })
}

fn parse_frontmatter(content: &str) -> Result<(SkillMdFrontmatter, &str)> {
    let after_open = content
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md must begin with a --- frontmatter block"))?;

    let close = after_open
        .find("\n---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter closing --- not found"))?;

    let yaml_str = &after_open[..close];
    let body = &after_open[close + 5..];

    let frontmatter: SkillMdFrontmatter =
        serde_yaml::from_str(yaml_str).context("SKILL.md frontmatter is not valid YAML")?;

    Ok((frontmatter, body))
}

fn scaffold_bundle(
    dir: &Utf8Path,
    _id: &str,
    display_name: &str,
    frontmatter: &SkillMdFrontmatter,
    system_prompt: &str,
) -> Result<Vec<String>> {
    fs::create_dir_all(dir.join("prompts"))?;
    fs::create_dir_all(dir.join("schemas"))?;

    let mut written: Vec<String> = Vec::new();

    write_file(dir, "prompts/system.txt", system_prompt, &mut written)?;
    write_file(dir, "schemas/input.schema.json", INPUT_SCHEMA, &mut written)?;
    write_file(
        dir,
        "schemas/output.schema.json",
        OUTPUT_SCHEMA,
        &mut written,
    )?;

    let workflow = "name: imported_skill\nsteps:\n\
         \x20 - id: generate\n\
         \x20   type: llm\n\
         \x20   prompt: prompts/system.txt\n\
         \x20   inputs:\n\
         \x20     input: input.input\n\
         \x20   output_schema: schemas/output.schema.json\n";
    write_file(dir, "workflow.yaml", workflow, &mut written)?;

    let skill_md = build_skill_md(display_name, frontmatter, system_prompt);
    write_file(dir, "SKILL.md", &skill_md, &mut written)?;

    Ok(written)
}

fn write_file(dir: &Utf8Path, rel: &str, content: &str, log: &mut Vec<String>) -> Result<()> {
    let path = dir.join(rel);
    fs::write(&path, content).with_context(|| format!("failed to write {path}"))?;
    log.push(rel.to_string());
    Ok(())
}

fn build_skill_md(display_name: &str, fm: &SkillMdFrontmatter, body: &str) -> String {
    let description = fm.description.as_deref().unwrap_or("");
    let description = if description.is_empty() {
        format!("A skill that helps with {}", display_name.to_lowercase())
    } else {
        description.to_string()
    };
    let license = fm.license.as_deref().unwrap_or("Apache-2.0");
    let publisher = fm.publisher.as_deref().unwrap_or("local");

    format!(
        "---\n\
         name: {display_name}\n\
         description: {description}\n\
         license: {license}\n\
         vh_version: 0.1.0\n\
         vh_publisher: {publisher}\n\
         vh_permissions:\n  \
           network: none\n  \
           filesystem: none\n  \
           clipboard: none\n\
         vh_execution:\n  \
           sandbox: strict\n  \
           timeout_ms: 120000\n  \
           memory_mb: 512\n\
         vh_workflow_ref: workflow.yaml\n\
         ---\n\
         \n\
         {body}\n"
    )
}

const INPUT_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["input"],
  "properties": {
    "input": {
      "type": "string",
      "description": "The input to pass to the skill."
    }
  },
  "additionalProperties": false
}"#;

const OUTPUT_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["result"],
  "properties": {
    "result": {
      "type": "string",
      "description": "The skill's output."
    },
    "notes": {
      "type": "string",
      "description": "Optional notes or additional context."
    }
  },
  "additionalProperties": false
}"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("vh-importer-tests-{label}-{nanos}")),
        )
        .expect("temp path should be utf-8")
    }

    fn write_skill_md(dir: &Utf8Path, content: &str) -> Utf8PathBuf {
        fs::create_dir_all(dir).expect("create dir");
        let path = dir.join("SKILL.md");
        fs::write(&path, content).expect("write SKILL.md");
        path
    }

    const SAMPLE_SKILL_MD: &str = "\
---
name: my-skill
description: Does something cool.
license: MIT
---

This is the system prompt body.
It can span multiple lines.
";

    #[test]
    fn import_creates_expected_bundle_files() {
        let dir = temp_dir("full");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_local_skill_md(&path).expect("import should succeed");

        assert_eq!(result.id, "my-skill");
        let out = &result.output_dir;
        assert!(out.join("SKILL.md").exists());
        assert!(out.join("workflow.yaml").exists());
        assert!(out.join("prompts/system.txt").exists());
        assert!(out.join("schemas/input.schema.json").exists());
        assert!(out.join("schemas/output.schema.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_writes_system_prompt_body_to_prompts_system_txt() {
        let dir = temp_dir("prompt");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_local_skill_md(&path).expect("import should succeed");

        let body = fs::read_to_string(result.output_dir.join("prompts/system.txt")).expect("read");
        assert!(body.contains("This is the system prompt body."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_skill_md_contains_correct_metadata() {
        let dir = temp_dir("metadata");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_local_skill_md(&path).expect("import should succeed");

        let skill_md_text = fs::read_to_string(result.output_dir.join("SKILL.md")).expect("read");
        assert!(skill_md_text.contains("name: my-skill"));
        assert!(skill_md_text.contains("description: Does something cool."));
        assert!(skill_md_text.contains("license: MIT"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_bundle_loads_cleanly_with_skill_package() {
        use vectorhawkd_manifest::SkillPackage;

        let dir = temp_dir("roundtrip");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_local_skill_md(&path).expect("import should succeed");

        let pkg = SkillPackage::load_from_dir(&result.output_dir)
            .expect("generated bundle should pass SkillPackage validation");
        assert_eq!(pkg.manifest.id, "my-skill");
        assert_eq!(pkg.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_missing_frontmatter() {
        let dir = temp_dir("bad-frontmatter");
        let path = write_skill_md(&dir, "No frontmatter here.");

        let err = import_local_skill_md(&path).expect_err("missing frontmatter should fail");
        assert!(err.to_string().contains("frontmatter"), "got: {err}");

        let _ = fs::remove_dir_all(&dir);
    }
}
