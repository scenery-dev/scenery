//! VectorHawk runner — skill manifest and bundle types.
//!
//! Ports the data layer from `skillrunner-manifest`. Pure data; no I/O beyond
//! reading files passed in by the caller.

use camino::{Utf8Path, Utf8PathBuf};
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;

pub mod skill_md;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("missing required file: {0}")]
    MissingFile(String),
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

// ── Clipboard access enum ────────────────────────────────────────────────────

/// Clipboard capability scope. Canonical string-enum form only.
/// Valid values: `"none"`, `"read"`, `"write"`, `"full"`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardAccess {
    None,
    Read,
    Write,
    Full,
}

// ── Filesystem access enum ───────────────────────────────────────────────────

/// Filesystem capability scope. Canonical values only: `"none"`, `"read-only"`, `"full"`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilesystemAccess {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "full")]
    Full,
}

// ── Sandbox profile enum ─────────────────────────────────────────────────────

/// Sandbox isolation level. Matches the JSON Schema enum and the AUTH1b
/// consistency fix (sandbox_profile: String → sandbox: SandboxProfile).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxProfile {
    Strict,
    Relaxed,
    Unrestricted,
}

// ── Manifest and sub-types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: String,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub publisher: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub entrypoint: String,
    pub inputs_schema: Option<serde_json::Value>,
    pub outputs_schema: Option<serde_json::Value>,
    pub permissions: Permissions,
    pub execution: Execution,
    pub model_requirements: Option<ModelRequirements>,
    pub update: Option<UpdateConfig>,
    /// Trigger phrases that help AI clients decide when to invoke this skill.
    #[serde(default)]
    pub triggers: Vec<String>,
}

impl Manifest {
    /// Returns the inputs JSON Schema, or a pass-through schema if none is declared.
    pub fn inputs_schema_or_default(&self) -> serde_json::Value {
        self.inputs_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "additionalProperties": true}))
    }

    /// Returns the outputs JSON Schema, or a pass-through schema if none is declared.
    pub fn outputs_schema_or_default(&self) -> serde_json::Value {
        self.outputs_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "additionalProperties": true}))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    pub filesystem: FilesystemAccess,
    pub network: String,
    pub clipboard: ClipboardAccess,
}

/// Execution constraints for a skill. Canonical fields only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub sandbox: SandboxProfile,
    pub timeout_ms: u64,
    pub memory_mb: u64,
}

/// Fallback behavior when no recommended model is available locally.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelFallback {
    McpSampling,
    Error,
}

/// Model requirements and recommendations for this skill.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRequirements {
    /// Minimum model size in billions of parameters (e.g. 7.0 for a 7B model).
    pub min_params_b: Option<f64>,
    /// Ordered list of recommended model identifiers.
    #[serde(default)]
    pub recommended: Vec<String>,
    /// Behavior when no recommended model is available locally.
    pub fallback: Option<ModelFallback>,
    /// When `true`, the runtime tries a locally-running model first
    /// (Ollama) and falls back to MCP sampling if that fails or no
    /// local model is available. When `false` (default), the runtime
    /// uses MCP sampling directly — the AI client handles generation.
    pub prefer_local: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    pub channel: Option<String>,
    pub auto_update: Option<bool>,
}

/// Where the system prompt for an LLM step comes from.
///
/// Deserializes from either a plain string (legacy path — treated as a
/// relative file path) or an explicit tagged object:
///   - `{kind: "file", path: "prompts/foo.txt"}`
///   - `{kind: "inline", body: "You are a helpful assistant..."}`
///
/// Serializes to the tagged-object form so round-trips are unambiguous.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptSource {
    /// Relative path from the skill bundle root to a prompt text file.
    File(String),
    /// Prompt body embedded directly in the workflow (no disk read needed).
    Inline(String),
}

impl<'de> Deserialize<'de> for PromptSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct PromptSourceVisitor;

        impl<'de> Visitor<'de> for PromptSourceVisitor {
            type Value = PromptSource;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a prompt path string or a {kind, ...} object")
            }

            // Legacy plain-string form: `prompt: prompts/system.txt`
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(PromptSource::File(value.to_string()))
            }

            // Tagged-object form: `prompt: {kind: inline, body: "..."}`
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut kind: Option<String> = None;
                let mut path: Option<String> = None;
                let mut body: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => {
                            kind = Some(map.next_value()?);
                        }
                        "path" => {
                            path = Some(map.next_value()?);
                        }
                        "body" => {
                            body = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["kind", "path", "body"]));
                        }
                    }
                }

                match kind.as_deref() {
                    Some("file") => {
                        let p = path.ok_or_else(|| de::Error::missing_field("path"))?;
                        Ok(PromptSource::File(p))
                    }
                    Some("inline") => {
                        let b = body.ok_or_else(|| de::Error::missing_field("body"))?;
                        Ok(PromptSource::Inline(b))
                    }
                    Some(other) => Err(de::Error::unknown_variant(other, &["file", "inline"])),
                    None => Err(de::Error::missing_field("kind")),
                }
            }
        }

        deserializer.deserialize_any(PromptSourceVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkflowStep {
    #[serde(rename = "tool")]
    Tool {
        id: String,
        tool: String,
        input: serde_yaml::Value,
    },
    #[serde(rename = "llm")]
    Llm {
        id: String,
        prompt: PromptSource,
        inputs: Option<serde_yaml::Value>,
        output_schema: Option<String>,
    },
    #[serde(rename = "transform")]
    Transform {
        id: String,
        op: String,
        input: serde_yaml::Value,
    },
    #[serde(rename = "validate")]
    Validate {
        id: String,
        schema: String,
        input: serde_yaml::Value,
    },
}

#[derive(Debug, Clone)]
pub struct SkillPackage {
    pub root: Utf8PathBuf,
    pub manifest: Manifest,
    pub workflow: Workflow,
}

impl SkillPackage {
    /// Load a skill from a directory containing a `SKILL.md` at its root.
    /// Returns an error if `SKILL.md` is absent or unparseable.
    pub fn load_from_dir(root: impl AsRef<Utf8Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref().to_path_buf();
        if !root.join("SKILL.md").exists() {
            return Err(ManifestError::MissingFile("SKILL.md".to_string()));
        }
        skill_md::load_from_skill_md_dir(root)
    }
}

// ── Bundle path confinement ──────────────────────────────────────────────────

/// Why [`resolve_bundle_file`] did not return a path.
#[derive(Debug, thiserror::Error)]
pub enum BundlePathError {
    /// The path resolves to a location outside the bundle root.
    #[error("path resolves outside the skill bundle")]
    OutsideBundle,
    /// The path stays inside the bundle but could not be resolved there
    /// (typically because the file does not exist).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Resolve `rel`, a file path taken from a skill manifest, against the bundle
/// `root` and return the canonical path it names. Refuses any path that
/// resolves outside the bundle; `.`/`..` segments and absolute paths are fine
/// as long as they land inside it.
///
/// Two checks, in order:
/// 1. Lexical, with no filesystem access: the joined path is folded (`.`
///    dropped, `..` pops the previous component) and must sit under the bundle,
///    named either by its canonical path or (for an absolute `rel`) by `root`
///    as given. A path that fails this is refused before anything outside the
///    bundle is touched, so the error says nothing about whether it exists.
/// 2. Canonical: the joined path is canonicalized, resolving symlinks, and
///    must still sit under the canonical root. This defeats symlinks inside
///    the bundle that point out of it.
pub fn resolve_bundle_file(root: &Utf8Path, rel: &str) -> Result<Utf8PathBuf, BundlePathError> {
    let canonical_root = root.canonicalize_utf8()?;
    let joined = canonical_root.join(rel);

    let folded = lexical_fold(&joined);
    let lexically_inside = folded.starts_with(&canonical_root)
        || (Utf8Path::new(rel).is_absolute() && folded.starts_with(lexical_fold(root)));
    if !lexically_inside {
        return Err(BundlePathError::OutsideBundle);
    }

    let resolved = joined.canonicalize_utf8()?;
    if !resolved.starts_with(&canonical_root) {
        return Err(BundlePathError::OutsideBundle);
    }
    Ok(resolved)
}

/// Fold `.` and `..` out of `path` without consulting the filesystem.
/// `..` at the root stays at the root; a leading `..` on a relative path is kept.
fn lexical_fold(path: &Utf8Path) -> Utf8PathBuf {
    use camino::Utf8Component;

    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => match out.components().next_back() {
                Some(Utf8Component::Normal(_)) => {
                    out.pop();
                }
                Some(Utf8Component::RootDir) | Some(Utf8Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_str()),
        }
    }
    out
}

// ── Plugin Manifest ─────────────────────────────────────────────────────────

/// A plugin is a composite, governed bundle that packages skills + MCP servers
/// + slash commands into a single installable unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub schema_version: String,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub publisher: String,
    pub description: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,

    /// Embedded or registry-referenced skills.
    #[serde(default)]
    pub skills: Vec<PluginSkillRef>,
    /// MCP server connections (go through governance approval on install).
    #[serde(default)]
    pub mcp_servers: Vec<PluginMcpServer>,
    /// Slash command markdown files.
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    /// User-prompted configuration values.
    #[serde(default)]
    pub user_config: std::collections::HashMap<String, PluginUserConfigEntry>,
    /// Update settings.
    pub update: Option<UpdateConfig>,
}

/// A skill referenced by a plugin — either embedded (path) or from registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSkillRef {
    /// Path to embedded skill bundle directory (relative to plugin root).
    pub path: Option<String>,
    /// Registry skill ID (resolved at install time).
    pub registry_id: Option<String>,
    /// Minimum version for registry-referenced skills.
    pub min_version: Option<String>,
}

/// An MCP server connection declared by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpServer {
    pub name: String,
    /// How to run the server (e.g. "npx -y @anthropic/mcp-server-jira").
    pub package_source: Option<String>,
    pub description: Option<String>,
    /// OAuth scopes needed from the backend system.
    #[serde(default)]
    pub downstream_scopes: Vec<String>,
    /// Human-readable note about credentials.
    pub credential_note: Option<String>,
}

/// A slash command markdown file declared by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    /// Path to the command markdown file (relative to plugin root).
    pub path: String,
}

/// A user-config entry prompted at install time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUserConfigEntry {
    pub description: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// A loaded plugin bundle from disk.
#[derive(Debug, Clone)]
pub struct PluginPackage {
    pub root: Utf8PathBuf,
    pub manifest: PluginManifest,
}

impl PluginPackage {
    /// Load and validate a plugin bundle from a directory containing `plugin.json`.
    pub fn load_from_dir(root: impl AsRef<Utf8Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref().to_path_buf();
        let manifest_path = root.join("plugin.json");
        let manifest_text = fs::read_to_string(&manifest_path)?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_text)?;

        validate_plugin_manifest(&root, &manifest)?;

        Ok(Self { root, manifest })
    }
}

fn validate_plugin_manifest(
    root: &Utf8Path,
    manifest: &PluginManifest,
) -> Result<(), ManifestError> {
    if manifest.id.trim().is_empty()
        || manifest.name.trim().is_empty()
        || manifest.publisher.trim().is_empty()
    {
        return Err(ManifestError::Invalid(
            "id, name, and publisher must be non-empty".to_string(),
        ));
    }

    if manifest.schema_version != "1.0" {
        return Err(ManifestError::Invalid(format!(
            "unsupported plugin schema_version {}",
            manifest.schema_version
        )));
    }

    // Must have at least one component
    if manifest.skills.is_empty() && manifest.mcp_servers.is_empty() && manifest.commands.is_empty()
    {
        return Err(ManifestError::Invalid(
            "plugin must contain at least one skill, MCP server, or command".to_string(),
        ));
    }

    // Validate embedded skill refs have paths that exist
    for skill_ref in &manifest.skills {
        if let Some(path) = &skill_ref.path {
            let skill_dir = root.join(path);
            if !skill_dir.join("SKILL.md").exists() {
                return Err(ManifestError::MissingFile(format!("{path}/SKILL.md")));
            }
        }
        // registry_id refs are validated at install time, not load time
        if skill_ref.path.is_none() && skill_ref.registry_id.is_none() {
            return Err(ManifestError::Invalid(
                "skill ref must have either 'path' or 'registry_id'".to_string(),
            ));
        }
    }

    // Validate command paths exist
    for cmd in &manifest.commands {
        let cmd_path = root.join(&cmd.path);
        if !cmd_path.exists() {
            return Err(ManifestError::MissingFile(cmd.path.clone()));
        }
    }

    // Validate MCP servers have names
    for server in &manifest.mcp_servers {
        if server.name.trim().is_empty() {
            return Err(ManifestError::Invalid(
                "MCP server name must be non-empty".to_string(),
            ));
        }
    }

    Ok(())
}

// ── ID derivation ─────────────────────────────────────────────────────────────

/// Derive a kebab-case skill ID from a human-readable name.
///
/// Rules:
/// - Lowercase all characters.
/// - Replace any character that is not alphanumeric or `-` with `-`.
/// - Collapse consecutive `-` into one; strip leading/trailing `-`.
///
/// Examples: `"Contract Compare"` → `"contract-compare"`,
///           `"my_skill"` → `"my-skill"`.
///
/// This function is the canonical source of truth for ID derivation. It is
/// also re-exported from `scenery-core::import` for callers that already
/// depend on that crate.
pub fn to_skill_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test_name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("forge-tests-{test_name}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn write_example_skill(root: &Utf8Path) {
        fs::create_dir_all(root.join("schemas")).expect("schemas dir should be created");
        fs::create_dir_all(root.join("prompts")).expect("prompts dir should be created");

        fs::write(
            root.join("SKILL.md"),
            "---\nname: Contract Compare\ndescription: Compare two contracts and summarize changes.\nlicense: MIT\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: forge\n    permissions:\n      filesystem: read-only\n      network: none\n      clipboard: none\n    execution:\n      sandbox: strict\n      timeout_ms: 90000\n      memory_mb: 1024\n    workflow_ref: ./workflow.yaml\n    schemas:\n      inputs:\n        type: object\n      outputs:\n        type: object\n---\n\nCompare the contracts.\n",
        )
        .expect("SKILL.md should be written");
        fs::write(
            root.join("workflow.yaml"),
            "name: contract_compare\nsteps:\n  - id: compare\n    type: llm\n    prompt: prompts/compare.txt\n    inputs:\n      text_a: input.doc_a\n      text_b: input.doc_b\n    output_schema: schemas/output.schema.json\n",
        )
        .expect("workflow.yaml should be written");
        fs::write(root.join("schemas/input.schema.json"), "{}")
            .expect("input schema should be written");
        fs::write(root.join("schemas/output.schema.json"), "{}")
            .expect("output schema should be written");
        fs::write(root.join("prompts/compare.txt"), "Compare the contracts.")
            .expect("prompt should be written");
        fs::write(
            root.join("workflow.yaml"),
            r#"name: contract_compare
steps:
  - id: compare
    type: llm
    prompt: prompts/compare.txt
    inputs:
      text_a: input.doc_a
      text_b: input.doc_b
    output_schema: schemas/output.schema.json
"#,
        )
        .expect("workflow should be written");
        fs::write(root.join("schemas/input.schema.json"), "{}")
            .expect("input schema should be written");
        fs::write(root.join("schemas/output.schema.json"), "{}")
            .expect("output schema should be written");
        fs::write(root.join("prompts/compare.txt"), "Compare the contracts.")
            .expect("prompt should be written");
    }

    #[test]
    fn load_from_dir_reads_valid_skill_package() {
        let root = temp_root("manifest-valid");
        write_example_skill(&root);

        let package = SkillPackage::load_from_dir(&root).expect("valid skill package should load");

        assert_eq!(package.manifest.id, "contract-compare");
        assert_eq!(
            package.manifest.version,
            Version::parse("0.1.0").expect("semver should parse")
        );
        assert_eq!(package.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_missing_workflow_ref_file() {
        // SKILL.md references workflow.yaml via vh_workflow_ref — if that file
        // is absent, load_from_dir must return MissingFile.
        let root = temp_root("manifest-missing-workflow");
        write_example_skill(&root);
        fs::remove_file(root.join("workflow.yaml"))
            .expect("workflow file should be removable for the test");

        let error =
            SkillPackage::load_from_dir(&root).expect_err("missing workflow_ref should fail");

        match error {
            ManifestError::MissingFile(path) => assert!(
                path.contains("workflow.yaml"),
                "expected workflow.yaml in error, got: {path}"
            ),
            other => panic!("expected missing file error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_empty_name() {
        // In the SKILL.md world the id is derived from name — an empty name
        // must be rejected as an invalid frontmatter field.
        let root = temp_root("manifest-empty-name");
        fs::create_dir_all(&root).expect("root should be created");
        fs::write(
            root.join("SKILL.md"),
            "---\nname:   \ndescription: Test.\nlicense: MIT\n---\n\nPrompt.\n",
        )
        .expect("SKILL.md should be written");

        let error = SkillPackage::load_from_dir(&root).expect_err("blank name should fail");

        match error {
            ManifestError::Invalid(message) => {
                assert!(message.contains("non-empty"), "got: {message}")
            }
            other => panic!("expected invalid manifest error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_missing_workflow_prompt_ref() {
        let root = temp_root("manifest-missing-prompt");
        write_example_skill(&root);
        // Remove the prompt file that the workflow references.
        fs::remove_file(root.join("prompts/compare.txt"))
            .expect("prompt file should be removable for the test");

        let error = SkillPackage::load_from_dir(&root).expect_err("missing prompt ref should fail");

        match error {
            ManifestError::MissingFile(path) => {
                assert_eq!(path, "prompts/compare.txt")
            }
            other => panic!("expected missing file error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    // ── Bundle path confinement ─────────────────────────────────────────────

    /// `<tmp>/bundle` holding `prompts/system.txt`, next to `<tmp>/outside/secret.txt`.
    fn bundle_fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf-8 tempdir");
        let root = base.join("bundle");
        fs::create_dir_all(root.join("prompts")).expect("prompts dir");
        fs::write(root.join("prompts/system.txt"), "in bundle").expect("prompt");
        fs::create_dir_all(base.join("outside")).expect("outside dir");
        fs::write(base.join("outside/secret.txt"), "secret").expect("secret");
        (tmp, root)
    }

    fn write_workflow_prompt(root: &Utf8Path, prompt: &str) {
        fs::write(
            root.join("workflow.yaml"),
            format!("name: contract_compare\nsteps:\n  - id: compare\n    type: llm\n    prompt: '{prompt}'\n"),
        )
        .expect("workflow should be written");
    }

    #[test]
    fn resolve_bundle_file_accepts_paths_that_land_inside_the_bundle() {
        let (_tmp, root) = bundle_fixture();
        let expected = root
            .join("prompts/system.txt")
            .canonicalize_utf8()
            .expect("canonical prompt");
        let absolute = root.join("prompts/system.txt");
        for rel in [
            "prompts/system.txt",
            "./prompts/system.txt",
            "prompts/../prompts/system.txt",
            "../bundle/prompts/system.txt",
            absolute.as_str(),
        ] {
            let got = resolve_bundle_file(&root, rel).unwrap_or_else(|e| panic!("{rel}: {e}"));
            assert_eq!(got, expected, "{rel}");
        }
    }

    #[test]
    fn resolve_bundle_file_refuses_escapes_whether_or_not_the_target_exists() {
        let (_tmp, root) = bundle_fixture();
        let base = root.parent().expect("bundle has a parent").to_path_buf();
        let secret_abs = base.join("outside/secret.txt");
        let missing_abs = base.join("outside/missing.txt");
        for rel in [
            "../outside/secret.txt",
            "../outside/missing.txt",
            "prompts/../../outside/secret.txt",
            "../bundle-sibling/system.txt",
            "../../../../../../../../../../etc/passwd",
            "/etc/passwd",
            secret_abs.as_str(),
            missing_abs.as_str(),
        ] {
            match resolve_bundle_file(&root, rel) {
                Err(BundlePathError::OutsideBundle) => {}
                other => panic!("{rel}: expected OutsideBundle, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_bundle_file_reports_a_missing_in_bundle_file_as_io() {
        let (_tmp, root) = bundle_fixture();
        assert!(matches!(
            resolve_bundle_file(&root, "prompts/missing.txt"),
            Err(BundlePathError::Io(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bundle_file_refuses_a_symlink_that_points_out_of_the_bundle() {
        let (_tmp, root) = bundle_fixture();
        let secret = root.parent().expect("parent").join("outside/secret.txt");
        std::os::unix::fs::symlink(&secret, root.join("prompts/out.txt")).expect("symlink out");
        std::os::unix::fs::symlink(root.join("prompts/system.txt"), root.join("prompts/in.txt"))
            .expect("symlink in");

        assert!(matches!(
            resolve_bundle_file(&root, "prompts/out.txt"),
            Err(BundlePathError::OutsideBundle)
        ));
        assert!(resolve_bundle_file(&root, "prompts/in.txt").is_ok());
    }

    #[test]
    fn load_from_dir_rejects_prompt_ref_outside_the_bundle() {
        let (tmp, root) = bundle_fixture();
        write_example_skill(&root);
        let secret =
            Utf8PathBuf::from_path_buf(tmp.path().join("outside/secret.txt")).expect("utf-8 path");
        for prompt in [
            "../outside/secret.txt",
            secret.as_str(),
            "/nonexistent/secret.txt",
        ] {
            write_workflow_prompt(&root, prompt);
            match SkillPackage::load_from_dir(&root) {
                Err(ManifestError::Invalid(message)) => {
                    assert!(
                        message.contains("outside the skill bundle"),
                        "{prompt}: {message}"
                    )
                }
                other => panic!("{prompt}: expected invalid manifest error, got {other:?}"),
            }
        }
    }

    #[test]
    fn load_from_dir_accepts_in_bundle_prompt_ref_with_dot_dot() {
        let (_tmp, root) = bundle_fixture();
        write_example_skill(&root);
        write_workflow_prompt(&root, "prompts/../prompts/compare.txt");

        let pkg = SkillPackage::load_from_dir(&root).expect("in-bundle .. path should load");
        match &pkg.workflow.steps[0] {
            WorkflowStep::Llm {
                prompt: PromptSource::File(path),
                ..
            } => assert_eq!(path, "prompts/../prompts/compare.txt"),
            other => panic!("expected llm step with file prompt, got {other:?}"),
        }
    }

    // ── Plugin manifest tests ───────────────────────────────────────────────

    fn write_example_plugin(root: &Utf8Path) {
        // Create embedded skill (SKILL.md format)
        let skill_dir = root.join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: My Skill\ndescription: A test skill.\nlicense: MIT\n---\n\nDo the thing.\n",
        )
        .unwrap();

        // Create command
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("my-command.md"),
            "---\nname: my-command\ndescription: test\n---\nDo the thing.",
        )
        .unwrap();

        // Create plugin.json
        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0",
            "id": "test-plugin",
            "name": "Test Plugin",
            "version": "0.1.0",
            "publisher": "test-publisher",
            "description": "A test plugin",
            "category": "testing",
            "tags": ["test"],
            "skills": [
                { "path": "./skills/my-skill" }
            ],
            "mcp_servers": [
                {
                    "name": "Test MCP",
                    "package_source": "npx -y @test/mcp-server",
                    "description": "Test server"
                }
            ],
            "commands": [
                { "path": "./commands/my-command.md" }
            ],
            "user_config": {
                "api_key": { "description": "Your API key", "sensitive": true }
            }
        }"#,
        )
        .unwrap();
    }

    #[test]
    fn plugin_package_loads_valid_bundle() {
        let root = temp_root("plugin-valid");
        fs::create_dir_all(&root).unwrap();
        write_example_plugin(&root);

        let pkg = PluginPackage::load_from_dir(&root).expect("valid plugin should load");
        assert_eq!(pkg.manifest.id, "test-plugin");
        assert_eq!(pkg.manifest.name, "Test Plugin");
        assert_eq!(pkg.manifest.version.to_string(), "0.1.0");
        assert_eq!(pkg.manifest.publisher, "test-publisher");
        assert_eq!(pkg.manifest.skills.len(), 1);
        assert_eq!(pkg.manifest.mcp_servers.len(), 1);
        assert_eq!(pkg.manifest.mcp_servers[0].name, "Test MCP");
        assert_eq!(pkg.manifest.commands.len(), 1);
        assert_eq!(pkg.manifest.user_config.len(), 1);
        assert!(pkg.manifest.user_config.contains_key("api_key"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_empty_components() {
        let root = temp_root("plugin-empty");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0", "id": "empty", "name": "Empty",
            "version": "0.1.0", "publisher": "test"
        }"#,
        )
        .unwrap();

        let err = PluginPackage::load_from_dir(&root).expect_err("empty plugin should fail");
        assert!(matches!(err, ManifestError::Invalid(msg) if msg.contains("at least one")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_missing_skill_manifest() {
        let root = temp_root("plugin-missing-skill");
        fs::create_dir_all(root.join("skills/bad-skill")).unwrap();
        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0", "id": "bad", "name": "Bad",
            "version": "0.1.0", "publisher": "test",
            "skills": [{ "path": "./skills/bad-skill" }]
        }"#,
        )
        .unwrap();

        let err =
            PluginPackage::load_from_dir(&root).expect_err("missing skill manifest should fail");
        assert!(matches!(err, ManifestError::MissingFile(f) if f.contains("SKILL.md")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_skill_ref_without_path_or_registry_id() {
        let root = temp_root("plugin-bad-ref");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0", "id": "bad", "name": "Bad",
            "version": "0.1.0", "publisher": "test",
            "skills": [{}]
        }"#,
        )
        .unwrap();

        let err = PluginPackage::load_from_dir(&root).expect_err("bad ref should fail");
        assert!(matches!(err, ManifestError::Invalid(msg) if msg.contains("path")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_accepts_registry_id_skill_ref() {
        let root = temp_root("plugin-registry-ref");
        fs::create_dir_all(&root).unwrap();
        // Command needed so plugin has at least one component besides the registry skill
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("cmd.md"),
            "---\nname: cmd\ndescription: test\n---\nDo it.",
        )
        .unwrap();

        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0", "id": "reg", "name": "Registry Ref",
            "version": "0.1.0", "publisher": "test",
            "skills": [{ "registry_id": "sprint-planning", "min_version": "1.0.0" }],
            "commands": [{ "path": "./commands/cmd.md" }]
        }"#,
        )
        .unwrap();

        let pkg = PluginPackage::load_from_dir(&root).expect("registry ref should load");
        assert_eq!(
            pkg.manifest.skills[0].registry_id.as_deref(),
            Some("sprint-planning")
        );

        let _ = fs::remove_dir_all(root);
    }

    // ── to_skill_id tests ───────────────────────────────────────────────────

    #[test]
    fn to_skill_id_handles_spaces_and_mixed_case() {
        assert_eq!(to_skill_id("Frontend Design"), "frontend-design");
        assert_eq!(to_skill_id("Contract Compare"), "contract-compare");
    }

    #[test]
    fn to_skill_id_replaces_underscores_with_hyphens() {
        assert_eq!(to_skill_id("my_skill"), "my-skill");
        assert_eq!(to_skill_id("already-kebab"), "already-kebab");
    }

    #[test]
    fn to_skill_id_strips_leading_trailing_and_collapsed_hyphens() {
        assert_eq!(to_skill_id("  hello world  "), "hello-world");
        assert_eq!(to_skill_id("foo--bar"), "foo-bar");
        assert_eq!(to_skill_id("---"), "");
    }

    #[test]
    fn to_skill_id_handles_uppercase_and_special_chars() {
        // ASCII special chars (!, @) → hyphens; uppercase → lowercase.
        assert_eq!(to_skill_id("GPT-4 Summary!"), "gpt-4-summary");
        assert_eq!(to_skill_id("skill@v2"), "skill-v2");
        // Non-ASCII letters are alphanumeric in Unicode, so they pass through
        // lowercased (is_alphanumeric() is Unicode-aware).
        assert_eq!(to_skill_id("Résumé Checker"), "résumé-checker");
    }

    // ── SKILL.md loading tests ───────────────────────────────────────────────

    fn write_minimal_skill_md(root: &Utf8Path) {
        fs::create_dir_all(root).expect("root dir should be created");
        fs::write(
            root.join("SKILL.md"),
            "---\nname: hello-world\ndescription: A greeter skill.\nmetadata:\n  vectorhawk:\n    version: 0.1.0\n    publisher: test\n---\n\nYou are a greeter.\n",
        )
        .expect("SKILL.md should be written");
    }

    fn write_full_skill_md(root: &Utf8Path) {
        fs::create_dir_all(root.join("prompts")).expect("prompts dir should be created");
        fs::write(root.join("prompts/analyze.md"), "Analyze the passage.").unwrap();
        fs::write(
            root.join("SKILL.md"),
            r#"---
name: passage-analyzer
description: Analyzes a passage and summarizes it.
metadata:
  vectorhawk:
    version: 1.0.0
    publisher: vectorhawk
    permissions:
      network: none
      filesystem: read-only
      clipboard: none
    execution:
      timeout_ms: 60000
      memory_mb: 1024
      sandbox: strict
    model:
      min_params_b: 7
      recommended:
        - llama3.1:8b
        - claude-3-haiku
      fallback: mcp_sampling
    schemas:
      inputs:
        type: object
        required: [passage]
        properties:
          passage: {type: string}
      outputs:
        type: object
        properties:
          summary: {type: string}
    workflow:
      - id: analyze
        type: llm
        prompt: prompts/analyze.md
---

You are an expert text analyst.
"#,
        )
        .expect("SKILL.md should be written");
    }

    #[test]
    fn skill_md_minimal_loads_and_derives_id() {
        let root = temp_root("skill-md-minimal");
        write_minimal_skill_md(&root);

        let pkg = SkillPackage::load_from_dir(&root).expect("minimal SKILL.md should load");

        assert_eq!(pkg.manifest.id, "hello-world");
        assert_eq!(pkg.manifest.name, "hello-world");
        assert_eq!(pkg.manifest.publisher, "test");
        assert_eq!(
            pkg.manifest.version,
            semver::Version::parse("0.1.0").unwrap()
        );
        assert_eq!(
            pkg.manifest.description.as_deref(),
            Some("A greeter skill.")
        );
        assert!(pkg.manifest.license.is_none());
        // No schemas declared — both should be None.
        assert!(pkg.manifest.inputs_schema.is_none());
        assert!(pkg.manifest.outputs_schema.is_none());
        // Synthesized single-step workflow.
        assert_eq!(pkg.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_md_full_vh_frontmatter_loads_correctly() {
        let root = temp_root("skill-md-full");
        write_full_skill_md(&root);

        let pkg = SkillPackage::load_from_dir(&root).expect("full SKILL.md should load");

        assert_eq!(pkg.manifest.id, "passage-analyzer");
        assert_eq!(pkg.manifest.version.to_string(), "1.0.0");
        assert_eq!(
            pkg.manifest.permissions.filesystem,
            FilesystemAccess::ReadOnly
        );
        assert_eq!(pkg.manifest.permissions.clipboard, ClipboardAccess::None);
        assert_eq!(pkg.manifest.execution.timeout_ms, 60000);
        assert_eq!(pkg.manifest.execution.memory_mb, 1024);
        assert_eq!(pkg.manifest.execution.sandbox, SandboxProfile::Strict);

        let model = pkg.manifest.model_requirements.as_ref().unwrap();
        assert_eq!(model.min_params_b, Some(7.0));
        assert_eq!(model.recommended, vec!["llama3.1:8b", "claude-3-haiku"]);
        assert_eq!(model.fallback, Some(ModelFallback::McpSampling));

        assert!(pkg.manifest.inputs_schema.is_some());
        assert!(pkg.manifest.outputs_schema.is_some());

        assert_eq!(pkg.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_md_rejects_unknown_vh_field() {
        let root = temp_root("skill-md-unknown-vh");
        fs::create_dir_all(&root).expect("root dir should be created");
        // A top-level vh_* key should be rejected with a migration hint.
        fs::write(
            root.join("SKILL.md"),
            "---\nname: bad-skill\ndescription: Test.\nlicense: MIT\nvh_unknown_extension: oops\n---\n\nPrompt.\n",
        )
        .expect("SKILL.md should be written");

        let err = SkillPackage::load_from_dir(&root).expect_err("unknown vh_ field should fail");

        match err {
            ManifestError::Invalid(msg) => {
                assert!(
                    msg.contains("metadata.vectorhawk") || msg.contains("vh_unknown_extension"),
                    "error should reference the migration path or field name, got: {msg}"
                );
            }
            other => panic!("expected ManifestError::Invalid, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_md_rejects_mutually_exclusive_vh_workflow_and_ref() {
        let root = temp_root("skill-md-workflow-conflict");
        fs::create_dir_all(&root).expect("root dir should be created");
        // The mutual-exclusion check fires before the prompt-file check, so we
        // don't need prompts/p.md to actually exist for this test.
        fs::write(
            root.join("SKILL.md"),
            "---\nname: bad\ndescription: Test.\nlicense: MIT\nmetadata:\n  vectorhawk:\n    workflow:\n      - {id: step1, type: llm, prompt: prompts/p.md}\n    workflow_ref: ./workflow.yaml\n---\n\nPrompt.\n",
        )
        .expect("SKILL.md should be written");

        let err = SkillPackage::load_from_dir(&root)
            .expect_err("mutually exclusive workflow fields should fail");

        match err {
            ManifestError::Invalid(msg) => {
                assert!(msg.contains("mutually exclusive"), "got: {msg}");
            }
            other => panic!("expected ManifestError::Invalid, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_md_real_minimal_example_loads() {
        // Load the AUTH1a real example skill from the examples directory.
        let root = Utf8PathBuf::from(
            "/Users/shadowduck/code/skillclub/skillrunner/examples/skills/skill-md-minimal",
        );
        if !root.exists() {
            return; // Skip if running in a context without the examples directory.
        }
        let pkg = SkillPackage::load_from_dir(&root).expect("real minimal SKILL.md should load");
        assert_eq!(pkg.manifest.id, "skill-md-minimal");
        assert_eq!(pkg.workflow.steps.len(), 1);
    }

    #[test]
    fn skill_md_real_complex_example_loads() {
        // Load the AUTH1a complex example (has vh_workflow_ref and vh_schemas).
        let root = Utf8PathBuf::from(
            "/Users/shadowduck/code/skillclub/skillrunner/examples/skills/skill-md-complex",
        );
        if !root.exists() {
            return;
        }
        let pkg = SkillPackage::load_from_dir(&root).expect("real complex SKILL.md should load");
        assert_eq!(pkg.manifest.id, "skill-md-complex");
        assert!(
            pkg.manifest.inputs_schema.is_some(),
            "complex skill should have inputs schema"
        );
        assert!(
            pkg.manifest.outputs_schema.is_some(),
            "complex skill should have outputs schema"
        );
        let model = pkg.manifest.model_requirements.as_ref().unwrap();
        assert_eq!(model.fallback, Some(ModelFallback::McpSampling));
        assert!(
            pkg.workflow.steps.len() > 1,
            "complex skill has multiple steps"
        );
    }
}
