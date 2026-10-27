//! Structural guard for skill-supplied JSON Schemas (F21).
//!
//! Skill bundles carry JSON Schemas (`metadata.vectorhawk.schemas.inputs` /
//! `outputs`, plus the schema files named by workflow `llm.output_schema` and
//! `validate.schema` steps). They come from publishers, local paths and
//! imported plugins, so they are untrusted.
//!
//! jsonschema 0.17 resolves `$ref` lazily and recompiles the target on every
//! `is_valid` call with no depth guard: `{"$ref": "#"}` recurses until the
//! stack overflows and aborts the daemon, and an acyclic fan-out (each level an
//! `allOf` of two `$ref`s to the next) costs 2^depth steps. Skill schemas do
//! not need references, so this module refuses every referencing keyword
//! outright and caps total size and nesting depth. It runs **before**
//! jsonschema ever sees the schema; every compile site in this crate must go
//! through [`compile_skill_schema`].
//!
//! Without references, jsonschema's validation work is bounded by
//! (schema nodes x instance nodes), and compile recursion by the depth cap.

use anyhow::{Context, Result};
use camino::Utf8Path;
use serde_json::Value;
use vectorhawkd_manifest::{SkillPackage, WorkflowStep};

/// Maximum number of JSON values (objects, arrays, scalars, counted
/// individually) in one skill schema.
pub const MAX_SCHEMA_NODES: usize = 10_000;

/// Maximum nesting depth of one skill schema (the root value is depth 1).
pub const MAX_SCHEMA_DEPTH: usize = 64;

/// Keywords that make jsonschema resolve another (sub)schema. Anchors are
/// included because they only exist to be referenced. `$id` is allowed: with
/// no reference keyword present nothing can resolve against it.
const REFERENCE_KEYWORDS: &[&str] = &[
    "$ref",
    "$dynamicRef",
    "$recursiveRef",
    "$anchor",
    "$dynamicAnchor",
    "$recursiveAnchor",
];

/// Check that `schema` is safe to hand to jsonschema: no reference keywords
/// anywhere (as any object key, even inside `properties`, `enum` or `const`,
/// so there is no keyword-vs-data ambiguity to get wrong), at most
/// [`MAX_SCHEMA_NODES`] values and at most [`MAX_SCHEMA_DEPTH`] levels.
///
/// The walk is iterative, so the check itself cannot overflow the stack.
pub fn check_schema_safe(schema: &Value) -> Result<()> {
    let mut stack: Vec<(&Value, usize)> = vec![(schema, 1)];
    let mut nodes: usize = 0;

    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_SCHEMA_NODES {
            anyhow::bail!("schema is too large (more than {MAX_SCHEMA_NODES} nodes)");
        }
        if depth > MAX_SCHEMA_DEPTH {
            anyhow::bail!("schema is nested too deeply (more than {MAX_SCHEMA_DEPTH} levels)");
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if REFERENCE_KEYWORDS.contains(&key.as_str()) {
                        anyhow::bail!(
                            "schema uses \"{key}\"; skill schemas may not contain references \
                             ($ref, $dynamicRef, $recursiveRef or anchors)"
                        );
                    }
                    stack.push((child, depth + 1));
                }
            }
            Value::Array(items) => {
                for child in items {
                    stack.push((child, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The only sanctioned way to compile a skill-supplied schema: structural
/// check first, then jsonschema.
pub fn compile_skill_schema(schema: &Value) -> Result<jsonschema::JSONSchema> {
    check_schema_safe(schema)?;
    jsonschema::JSONSchema::compile(schema).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Read, parse and compile a schema file shipped in a skill bundle.
pub fn compile_schema_file(
    pkg_root: &Utf8Path,
    schema_rel: &str,
) -> Result<jsonschema::JSONSchema> {
    let schema_path = pkg_root.join(schema_rel);
    let schema_text = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("failed to read schema {schema_path}"))?;
    let schema_json: Value = serde_json::from_str(&schema_text)
        .with_context(|| format!("{schema_rel} is not valid JSON"))?;
    compile_skill_schema(&schema_json)
        .with_context(|| format!("{schema_rel} is not an acceptable JSON Schema"))
}

/// Relative paths of the schema files a package's workflow steps reference.
pub fn workflow_schema_files(pkg: &SkillPackage) -> Vec<&str> {
    pkg.workflow
        .steps
        .iter()
        .filter_map(|step| match step {
            WorkflowStep::Llm {
                output_schema: Some(rel),
                ..
            } => Some(rel.as_str()),
            WorkflowStep::Validate { schema, .. } => Some(schema.as_str()),
            _ => None,
        })
        .collect()
}

/// Install-time check of every schema a package carries: the inline input and
/// output schemas, and each workflow schema file that exists in the bundle
/// (a missing file already fails at run time with a clear error).
pub fn check_package_schemas(pkg: &SkillPackage) -> Result<()> {
    if let Some(schema) = pkg.manifest.inputs_schema.as_ref() {
        compile_skill_schema(schema).context("inputs schema is not an acceptable JSON Schema")?;
    }
    if let Some(schema) = pkg.manifest.outputs_schema.as_ref() {
        compile_skill_schema(schema).context("outputs schema is not an acceptable JSON Schema")?;
    }
    for rel in workflow_schema_files(pkg) {
        if pkg.root.join(rel).exists() {
            compile_schema_file(&pkg.root, rel)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// The F21 reviewer's schema: `levels` definitions, each an `allOf` of two
    /// `$ref`s to the next, so naive resolution costs 2^levels steps.
    pub(crate) fn fanout_schema(levels: usize) -> Value {
        let mut defs = serde_json::Map::new();
        for i in 0..levels {
            let next = format!("#/definitions/d{}", i + 1);
            defs.insert(
                format!("d{i}"),
                json!({"allOf": [{"$ref": next}, {"$ref": next}]}),
            );
        }
        defs.insert(format!("d{levels}"), json!({"type": "object"}));
        json!({"definitions": defs, "$ref": "#/definitions/d0"})
    }

    pub(crate) fn deep_schema(levels: usize) -> Value {
        let mut schema = json!({"type": "string"});
        for _ in 0..levels {
            schema = json!({"type": "object", "properties": {"a": schema}});
        }
        schema
    }

    pub(crate) fn wide_schema(props: usize) -> Value {
        let mut properties = serde_json::Map::new();
        for i in 0..props {
            properties.insert(format!("p{i}"), json!({"type": "string"}));
        }
        json!({"type": "object", "properties": properties})
    }

    /// Run one `#[ignore]`d child test in a fresh process so a stack overflow
    /// (which aborts the whole process) or a hang shows up as a failure of the
    /// parent test instead of taking the test binary down.
    pub(crate) fn run_isolated(child_test: &str, timeout: Duration) {
        let exe = std::env::current_exe().expect("current test exe");
        let mut child = std::process::Command::new(exe)
            .args([child_test, "--exact", "--ignored", "--test-threads=1"])
            .env("VH_SCHEMA_GUARD_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child test");
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("wait child") {
                let mut stderr = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut stderr);
                }
                assert!(
                    status.success(),
                    "child test {child_test} failed or crashed ({status}): {stderr}"
                );
                return;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child test {child_test} did not finish within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Child tests are `#[ignore]`d so a normal run skips them; they only do
    /// their work when launched by [`run_isolated`].
    pub(crate) fn is_child() -> bool {
        std::env::var_os("VH_SCHEMA_GUARD_CHILD").is_some()
    }

    #[test]
    fn rejects_self_ref() {
        let err = compile_skill_schema(&json!({"$ref": "#"})).expect_err("must reject");
        assert!(err.to_string().contains("$ref"), "got: {err}");
    }

    #[test]
    fn rejects_fanout_quickly() {
        let start = Instant::now();
        let err = compile_skill_schema(&fanout_schema(40)).expect_err("must reject");
        assert!(err.to_string().contains("$ref"), "got: {err}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn rejects_nested_and_alternate_reference_keywords() {
        for key in REFERENCE_KEYWORDS {
            let schema = json!({
                "type": "object",
                "properties": {"x": {"anyOf": [{"type": "string"}, {*key: "#"}]}}
            });
            let err = compile_skill_schema(&schema).expect_err("must reject");
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn rejects_over_deep_schema() {
        let err = compile_skill_schema(&deep_schema(MAX_SCHEMA_DEPTH)).expect_err("too deep");
        assert!(err.to_string().contains("nested too deeply"), "got: {err}");
    }

    #[test]
    fn rejects_over_large_schema() {
        let err = compile_skill_schema(&wide_schema(MAX_SCHEMA_NODES)).expect_err("too large");
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    #[test]
    fn accepts_normal_schema_and_still_validates() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "https://example.com/in.json",
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string", "minLength": 1},
                "tags": {"type": "array", "items": {"enum": ["a", "b"]}}
            },
            "additionalProperties": false
        });
        let compiled = compile_skill_schema(&schema).expect("normal schema compiles");
        assert!(compiled.is_valid(&json!({"query": "hi", "tags": ["a"]})));
        assert!(!compiled.is_valid(&json!({"query": ""})));
        assert!(!compiled.is_valid(&json!({"other": 1})));
        // Within the caps: moderately nested and wide schemas still compile.
        compile_skill_schema(&deep_schema(MAX_SCHEMA_DEPTH / 2 - 1)).expect("moderate depth");
        compile_skill_schema(&wide_schema(1_000)).expect("moderate width");
    }
}
