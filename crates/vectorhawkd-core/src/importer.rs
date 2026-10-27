use crate::scan::{ScanClient, ScanVerdict};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::fs;
use tracing::warn;
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
}

/// Error returned when the scan verdict requires `--confirm-risky` and the
/// caller did not confirm. Nothing has been written when this is returned.
#[derive(Debug)]
pub struct ImportBlocked {
    pub verdict: ScanVerdict,
}

impl std::fmt::Display for ImportBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Import blocked: verdict is {:?}. \
             Review findings above and re-run with --confirm-risky to override.",
            self.verdict.verdict
        )
    }
}

impl std::error::Error for ImportBlocked {}

// ── ImportResult ──────────────────────────────────────────────────────────────

/// Outcome of `import_local_skill_md_with_scan`, combining the scaffolded bundle
/// with an optional security verdict.
#[derive(Debug)]
pub struct ImportResult {
    pub bundle: ScaffoldedBundle,
    /// `None` when no scanner was provided (offline / unauthenticated mode).
    pub scan_verdict: Option<ScanVerdict>,
}

// ── Public import functions ───────────────────────────────────────────────────

/// A SKILL.md that has been read, parsed, validated and (optionally) scanned,
/// but not yet written anywhere. Call [`PreparedImport::scaffold`] to write the
/// bundle once the caller has accepted the scan verdict.
#[derive(Debug)]
pub struct PreparedImport {
    skill_md_path: Utf8PathBuf,
    id: String,
    display_name: String,
    frontmatter: SkillMdFrontmatter,
    body: String,
    scan_verdict: Option<ScanVerdict>,
}

impl PreparedImport {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// `None` when no scanner was provided or the scanner errored (fail-open).
    pub fn scan_verdict(&self) -> Option<&ScanVerdict> {
        self.scan_verdict.as_ref()
    }

    /// Write the bundle into `<SKILL.md parent>/<id>/`.
    ///
    /// The parent directory is canonicalised; the bundle directory, its
    /// `prompts/` and `schemas/` subdirectories and every file written must not
    /// be symlinks, so a symlink planted in a downloaded skill folder cannot
    /// redirect writes outside it.
    pub fn scaffold(self) -> Result<ScaffoldedBundle> {
        let parent = match self.skill_md_path.parent() {
            Some(p) if !p.as_str().is_empty() => p.to_path_buf(),
            _ => Utf8PathBuf::from("."),
        };
        let canon_parent = canonical_utf8(&parent)?;
        let output_dir = canon_parent.join(&self.id);

        ensure_real_dir(&output_dir)?;
        let canon_out = canonical_utf8(&output_dir)?;
        if canon_out != output_dir || canon_out.parent() != Some(canon_parent.as_path()) {
            anyhow::bail!(
                "refusing to scaffold: bundle directory {output_dir} resolves to {canon_out}, \
                 outside {canon_parent}"
            );
        }

        let files = scaffold_bundle(
            &output_dir,
            &self.display_name,
            &self.frontmatter,
            self.body.trim(),
        )?;

        Ok(ScaffoldedBundle {
            id: self.id,
            output_dir,
            files,
        })
    }
}

/// Read, parse and validate a SKILL.md, then scan it when `scanner` is `Some`.
/// Writes nothing.
///
/// The scan is **fail-open**: a scanner error is logged and the verdict is
/// `None`. Enforcing the verdict is the caller's job (see
/// [`import_local_skill_md_with_scan`]).
pub fn prepare_local_skill_md_import(
    skill_md_path: &Utf8Path,
    scanner: Option<&dyn ScanClient>,
) -> Result<PreparedImport> {
    let raw_bytes =
        fs::read(skill_md_path).with_context(|| format!("failed to read {skill_md_path}"))?;
    let content = std::str::from_utf8(&raw_bytes)
        .with_context(|| format!("{skill_md_path} is not valid UTF-8"))?;

    let (frontmatter, body) = parse_frontmatter(content)
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
    validate_skill_id(&id)?;
    let body = body.to_string();

    let scan_verdict = match scanner {
        None => None,
        Some(sc) => match sc.scan(&raw_bytes, "skill_md") {
            Ok(verdict) => Some(verdict),
            Err(e) => {
                warn!(error = %e, path = %skill_md_path, "scan returned error — proceeding without verdict");
                None
            }
        },
    };

    Ok(PreparedImport {
        skill_md_path: skill_md_path.to_path_buf(),
        id,
        display_name,
        frontmatter,
        body,
        scan_verdict,
    })
}

/// Read a SKILL.md, parse its frontmatter and body, and scaffold a complete
/// bundle directory next to the source file. No scan is performed.
///
/// The SKILL.md body becomes `prompts/system.txt`. A single `llm` workflow
/// step is generated that passes user requirements through the system prompt.
pub fn import_local_skill_md(skill_md_path: &Utf8Path) -> Result<ScaffoldedBundle> {
    prepare_local_skill_md_import(skill_md_path, None)?.scaffold()
}

/// Read a SKILL.md, scan it when `scanner` is `Some`, enforce the verdict, and
/// only then scaffold the bundle.
///
/// A verdict that requires confirmation returns an [`ImportBlocked`] error
/// (before anything is written) unless `confirm_risky` is true. A scanner
/// error is fail-open: the verdict is `None` and the import proceeds.
pub fn import_local_skill_md_with_scan(
    skill_md_path: &Utf8Path,
    scanner: Option<&dyn ScanClient>,
    confirm_risky: bool,
) -> Result<ImportResult> {
    let prepared = prepare_local_skill_md_import(skill_md_path, scanner)?;
    if let Some(verdict) = prepared.scan_verdict() {
        if verdict.requires_confirmation() && !confirm_risky {
            return Err(ImportBlocked {
                verdict: verdict.clone(),
            }
            .into());
        }
    }
    let scan_verdict = prepared.scan_verdict.clone();
    let bundle = prepared.scaffold()?;
    Ok(ImportResult {
        bundle,
        scan_verdict,
    })
}

/// A skill id becomes a directory name, so it must be non-empty and use only
/// `[a-z0-9-]`. Anything else is rejected rather than normalised.
pub fn validate_skill_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        anyhow::bail!(
            "invalid skill id {id:?}: the id must be non-empty and use only lowercase \
             letters, digits and '-' (set `id:` in the SKILL.md frontmatter)"
        );
    }
    Ok(())
}

fn canonical_utf8(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let canon = fs::canonicalize(path).with_context(|| format!("failed to resolve {path}"))?;
    Utf8PathBuf::from_path_buf(canon)
        .map_err(|p| anyhow::anyhow!("path is not UTF-8: {}", p.display()))
}

/// Create `dir` if it is missing; if it exists it must be a real directory,
/// not a symlink (to a directory or anything else).
fn ensure_real_dir(dir: &Utf8Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("refusing to write through symlink {dir}")
        }
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => anyhow::bail!("{dir} exists and is not a directory"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(dir).with_context(|| format!("failed to create directory {dir}"))
        }
        Err(e) => Err(e).with_context(|| format!("failed to inspect {dir}")),
    }
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

/// `dir` must already be a verified real directory (see [`ensure_real_dir`]).
fn scaffold_bundle(
    dir: &Utf8Path,
    display_name: &str,
    frontmatter: &SkillMdFrontmatter,
    system_prompt: &str,
) -> Result<Vec<String>> {
    ensure_real_dir(&dir.join("prompts"))?;
    ensure_real_dir(&dir.join("schemas"))?;

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

/// Write `dir/rel` without following a symlink or writing into a hard-linked
/// inode: refuse an existing symlink, write a fresh temp file created with
/// `create_new` next to the target, then rename it over the target.
fn write_file(dir: &Utf8Path, rel: &str, content: &str, log: &mut Vec<String>) -> Result<()> {
    use std::io::Write;

    let path = dir.join(rel);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("refusing to write through symlink {path}")
        }
        Ok(meta) if meta.is_dir() => anyhow::bail!("{path} exists and is a directory"),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("failed to inspect {path}")),
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{path} has no file name"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_file_name(format!(
        ".{file_name}.vh-import-{}-{nanos}.tmp",
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("failed to create {tmp}"))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("failed to write {tmp}"))?;
        drop(f);
        fs::rename(&tmp, &path).with_context(|| format!("failed to write {path}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;

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
    let publisher = fm.publisher.as_deref().unwrap_or("local");

    format!(
        "---\nname: {display_name}\ndescription: {description}\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: {publisher}\n    permissions:\n      network: none\n      filesystem: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 120000\n      memory_mb: 512\n    workflow_ref: workflow.yaml\n---\n\n{body}\n"
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
    use crate::scan::{ScanVerdict, Severity};
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

    struct HighScanner;
    impl crate::scan::ScanClient for HighScanner {
        fn scan(&self, _content: &[u8], _content_type: &str) -> anyhow::Result<ScanVerdict> {
            Ok(ScanVerdict {
                verdict: Severity::High,
                max_severity: Some("high".to_string()),
                findings: vec![crate::scan::ScanFinding {
                    rule_id: Some("TEST001".to_string()),
                    severity: "high".to_string(),
                    title: Some("test finding".to_string()),
                    description: None,
                }],
                scanner_version: Some("0.0.0-test".to_string()),
                cached: false,
                content_hash: "sha256:test".to_string(),
            })
        }
    }

    const SAMPLE_SKILL_MD: &str = "\
---
name: my-skill
description: Does something cool.
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

    // ── import_local_skill_md_with_scan ───────────────────────────────────────

    #[test]
    fn with_scan_none_scanner_matches_original_output() {
        let dir = temp_dir("scan-none");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let plain = import_local_skill_md(&path).expect("plain import should succeed");
        let with_scan = import_local_skill_md_with_scan(&path, None, false)
            .expect("with_scan import should succeed");

        // Same bundle results.
        assert_eq!(plain.id, with_scan.bundle.id);
        assert_eq!(plain.output_dir, with_scan.bundle.output_dir);
        assert_eq!(plain.files.len(), with_scan.bundle.files.len());

        // No scan verdict when no scanner provided.
        assert!(with_scan.scan_verdict.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_scan_mock_high_scanner_includes_verdict() {
        let dir = temp_dir("scan-high");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_local_skill_md_with_scan(&path, Some(&HighScanner), true)
            .expect("import should succeed");

        let verdict = result.scan_verdict.expect("verdict should be present");
        assert_eq!(verdict.verdict, Severity::High);
        assert!(verdict.is_risky());
        assert!(verdict.requires_confirmation());
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].rule_id.as_deref(), Some("TEST001"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_scan_fail_open_when_scanner_errors() {
        struct ErrorScanner;
        impl crate::scan::ScanClient for ErrorScanner {
            fn scan(&self, _content: &[u8], _content_type: &str) -> anyhow::Result<ScanVerdict> {
                anyhow::bail!("simulated scan failure")
            }
        }

        let dir = temp_dir("scan-error");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        // Should NOT return Err — fail-open means verdict is None, import succeeds.
        let result = import_local_skill_md_with_scan(&path, Some(&ErrorScanner), false)
            .expect("import should succeed even when scanner errors");

        assert!(
            result.scan_verdict.is_none(),
            "verdict should be None when scanner errors"
        );
        assert_eq!(result.bundle.id, "my-skill");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── F3: id validation, symlink refusal, scan-before-write ─────────────────

    fn skill_md_with_id(id: &str) -> String {
        format!("---\nname: Evil\nid: \"{id}\"\ndescription: x\n---\n\nbody\n")
    }

    #[test]
    fn import_rejects_traversal_id() {
        let root = temp_dir("trav");
        let src = root.join("src");
        let path = write_skill_md(&src, &skill_md_with_id("../escaped"));

        let err = import_local_skill_md(&path).expect_err("traversal id must be rejected");
        assert!(err.to_string().contains("id"), "got: {err}");
        assert!(
            !root.join("escaped").exists(),
            "nothing may be written outside"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_absolute_id() {
        let root = temp_dir("abs");
        let target = root.join("abs-target");
        let path = write_skill_md(&root.join("src"), &skill_md_with_id(target.as_str()));

        import_local_skill_md(&path).expect_err("absolute id must be rejected");
        assert!(!target.exists(), "absolute id must not create {target}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_id_outside_charset_without_normalising() {
        let root = temp_dir("charset");
        let src = root.join("src");
        let path = write_skill_md(&src, &skill_md_with_id("My_Skill"));

        import_local_skill_md(&path).expect_err("id outside [a-z0-9-] must be rejected");
        assert!(!src.join("my-skill").exists(), "id must not be normalised");
        assert!(!src.join("My_Skill").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_name_that_derives_empty_id() {
        let root = temp_dir("empty-id");
        let src = root.join("src");
        let path = write_skill_md(&src, "---\nname: \"!!!\"\n---\n\nbody\n");

        import_local_skill_md(&path).expect_err("empty derived id must be rejected");
        assert!(
            !src.join("prompts").exists(),
            "must not scaffold into the parent"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn import_refuses_symlinked_output_dir() {
        let root = temp_dir("sym-out");
        let src = root.join("src");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let path = write_skill_md(&src, SAMPLE_SKILL_MD);
        std::os::unix::fs::symlink(&outside, src.join("my-skill")).unwrap();

        import_local_skill_md(&path).expect_err("symlinked bundle dir must be refused");
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "outside dir untouched"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn import_refuses_symlinked_subdir() {
        let root = temp_dir("sym-sub");
        let src = root.join("src");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let path = write_skill_md(&src, SAMPLE_SKILL_MD);
        fs::create_dir_all(src.join("my-skill")).unwrap();
        std::os::unix::fs::symlink(&outside, src.join("my-skill/prompts")).unwrap();

        import_local_skill_md(&path).expect_err("symlinked prompts/ must be refused");
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "outside dir untouched"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn import_refuses_symlinked_file() {
        let root = temp_dir("sym-file");
        let src = root.join("src");
        let victim = root.join("victim.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&victim, "original").unwrap();
        let path = write_skill_md(&src, SAMPLE_SKILL_MD);
        fs::create_dir_all(src.join("my-skill")).unwrap();
        std::os::unix::fs::symlink(&victim, src.join("my-skill/workflow.yaml")).unwrap();

        import_local_skill_md(&path).expect_err("symlinked target file must be refused");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn import_does_not_write_through_hardlinked_file() {
        let root = temp_dir("hardlink");
        let src = root.join("src");
        let victim = root.join("victim.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&victim, "original").unwrap();
        let path = write_skill_md(&src, SAMPLE_SKILL_MD);
        fs::create_dir_all(src.join("my-skill")).unwrap();
        fs::hard_link(&victim, src.join("my-skill/workflow.yaml")).unwrap();

        import_local_skill_md(&path).expect("import should succeed");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn with_scan_blocks_risky_verdict_before_writing_files() {
        let dir = temp_dir("scan-block");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let err = import_local_skill_md_with_scan(&path, Some(&HighScanner), false)
            .expect_err("risky verdict must block the import");
        assert!(err.downcast_ref::<ImportBlocked>().is_some(), "got: {err}");
        assert!(
            !dir.join("my-skill").exists(),
            "no files may be written when the scan blocks the import"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
