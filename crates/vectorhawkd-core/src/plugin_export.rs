use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use vectorhawkd_manifest::PluginPackage;
use std::{fs, io::Write as IoWrite};
use zip::{write::SimpleFileOptions, ZipWriter};

/// Export a VectorHawk plugin to the Claude Code plugin format.
///
/// Creates `{output_dir}/{manifest.id}/` containing:
/// - `.claude-plugin/plugin.json`
/// - `.mcp.json` (if MCP servers are present)
/// - `skills/` directory with command .md files and embedded skill SKILL.md files
///
/// Returns the path to the created plugin directory.
pub fn export_claude_code(plugin_dir: &Utf8Path, output_dir: &Utf8Path) -> Result<Utf8PathBuf> {
    let pkg = PluginPackage::load_from_dir(plugin_dir)
        .with_context(|| format!("failed to load plugin from {plugin_dir}"))?;

    let dest = output_dir.join(&pkg.manifest.id);
    let claude_plugin_dir = dest.join(".claude-plugin");
    let skills_dir = dest.join("skills");

    fs::create_dir_all(&claude_plugin_dir)
        .with_context(|| format!("failed to create {claude_plugin_dir}"))?;

    let has_commands = !pkg.manifest.commands.is_empty();
    let has_embedded_skills = pkg.manifest.skills.iter().any(|s| s.path.is_some());
    let has_skills_output = has_commands || has_embedded_skills;
    let has_mcp = !pkg.manifest.mcp_servers.is_empty();

    if has_skills_output {
        fs::create_dir_all(&skills_dir)
            .with_context(|| format!("failed to create {skills_dir}"))?;
    }

    // Build plugin.json for Claude Code format
    let mut plugin_json = serde_json::json!({
        "name": pkg.manifest.id,
        "version": pkg.manifest.version.to_string(),
        "description": pkg.manifest.description,
        "author": { "name": pkg.manifest.publisher },
    });

    if has_skills_output {
        plugin_json["skills"] = serde_json::json!("./skills/");
    }

    if has_mcp {
        plugin_json["mcpServers"] = serde_json::json!("./.mcp.json");
    }

    if !pkg.manifest.user_config.is_empty() {
        let user_config: serde_json::Map<String, serde_json::Value> = pkg
            .manifest
            .user_config
            .iter()
            .map(|(key, entry)| {
                let val = serde_json::json!({
                    "description": entry.description,
                    "sensitive": entry.sensitive,
                });
                (key.clone(), val)
            })
            .collect();
        plugin_json["userConfig"] = serde_json::Value::Object(user_config);
    }

    let plugin_json_text = serde_json::to_string_pretty(&plugin_json)
        .context("failed to serialize .claude-plugin/plugin.json")?;
    fs::write(claude_plugin_dir.join("plugin.json"), &plugin_json_text)
        .context("failed to write .claude-plugin/plugin.json")?;

    // Write .mcp.json if MCP servers are present
    if has_mcp {
        let mcp_servers: serde_json::Map<String, serde_json::Value> = pkg
            .manifest
            .mcp_servers
            .iter()
            .map(|server| {
                let entry = build_mcp_server_entry(server.package_source.as_deref());
                (server.name.clone(), entry)
            })
            .collect();

        let mcp_json = serde_json::json!({ "mcpServers": mcp_servers });
        let mcp_json_text =
            serde_json::to_string_pretty(&mcp_json).context("failed to serialize .mcp.json")?;
        fs::write(dest.join(".mcp.json"), &mcp_json_text).context("failed to write .mcp.json")?;
    }

    // Copy command .md files into skills/
    for cmd in &pkg.manifest.commands {
        let src = plugin_dir.join(&cmd.path);
        let file_name = src
            .file_name()
            .with_context(|| format!("command path has no file name: {}", cmd.path))?;
        let dst = skills_dir.join(file_name);
        fs::copy(&src, &dst)
            .with_context(|| format!("failed to copy command file {src} to {dst}"))?;
    }

    // Export embedded skills as SKILL.md files under skills/{skill-id}/SKILL.md.
    // Load each skill through the canonical SKILL.md loader so we don't
    // hand-parse the frontmatter — AUTH1f: manifest.json is no longer the
    // source of truth.
    for skill_ref in &pkg.manifest.skills {
        let skill_path = match &skill_ref.path {
            Some(p) => p,
            None => continue, // registry-only refs have nothing to export locally
        };

        let skill_dir = plugin_dir.join(skill_path);
        let skill_pkg = vectorhawkd_manifest::SkillPackage::load_from_dir(&skill_dir)
            .with_context(|| format!("failed to load skill from {skill_dir}"))?;

        let skill_id = skill_pkg.manifest.id.as_str();
        let skill_name = skill_pkg.manifest.name.as_str();
        let skill_description = skill_pkg.manifest.description.as_deref().unwrap_or("");

        let prompt_path = skill_dir.join("prompts").join("system.txt");
        let system_prompt = if prompt_path.exists() {
            fs::read_to_string(&prompt_path)
                .with_context(|| format!("failed to read {prompt_path}"))?
        } else {
            String::new()
        };

        let skill_out_dir = skills_dir.join(skill_id);
        fs::create_dir_all(&skill_out_dir)
            .with_context(|| format!("failed to create {skill_out_dir}"))?;

        let frontmatter = format!(
            "---\nname: {skill_name}\ndescription: {skill_description}\n---\n\n{system_prompt}\n"
        );
        fs::write(skill_out_dir.join("SKILL.md"), &frontmatter)
            .with_context(|| format!("failed to write SKILL.md for skill {skill_id}"))?;
    }

    Ok(dest)
}

/// Export a VectorHawk plugin to the .mcpb (MCP Bundle / Desktop Extension) archive format.
///
/// The plugin must have exactly one MCP server declared. The resulting ZIP archive
/// contains `manifest.json` describing the server — this is the wire format consumed
/// by Claude Desktop's enterprise allowlist. Returns the path to the created
/// `{id}-{version}.mcpb` archive.
pub fn export_mcpb(plugin_dir: &Utf8Path, output_dir: &Utf8Path) -> Result<Utf8PathBuf> {
    let pkg = PluginPackage::load_from_dir(plugin_dir)
        .with_context(|| format!("failed to load plugin from {plugin_dir}"))?;

    match pkg.manifest.mcp_servers.len() {
        0 => bail!("mcpb format requires exactly one MCP server, but this plugin has none"),
        1 => {}
        n => bail!("mcpb format supports exactly one MCP server, but this plugin has {n}"),
    }

    let server = &pkg.manifest.mcp_servers[0];
    let server_entry = build_mcp_server_entry(server.package_source.as_deref());

    let manifest_obj = serde_json::json!({
        "name": pkg.manifest.name,
        "version": pkg.manifest.version.to_string(),
        "description": pkg.manifest.description,
        "server": {
            "name": server.name,
            "command": server_entry,
        }
    });

    let manifest_text = serde_json::to_string_pretty(&manifest_obj)
        .context("failed to serialize mcpb manifest.json")?;

    let archive_name = format!("{}-{}.mcpb", pkg.manifest.id, pkg.manifest.version);
    let archive_path = output_dir.join(&archive_name);

    let file = fs::File::create(&archive_path)
        .with_context(|| format!("failed to create archive file {archive_path}"))?;
    let mut zip = ZipWriter::new(file);

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    zip.start_file("manifest.json", options)
        .context("failed to start manifest.json entry in ZIP")?;
    zip.write_all(manifest_text.as_bytes())
        .context("failed to write manifest.json into ZIP")?;

    zip.finish().context("failed to finalize ZIP archive")?;

    Ok(archive_path)
}

/// Parse a `package_source` string (e.g. "npx -y @foo/bar") into the JSON
/// object used in both `.mcp.json` and the mcpb manifest:
/// `{"command": "npx", "args": ["-y", "@foo/bar"]}`.
///
/// If `package_source` is None or empty, returns an object with an empty
/// command and no args.
fn build_mcp_server_entry(package_source: Option<&str>) -> serde_json::Value {
    let source = package_source.unwrap_or("").trim();
    if source.is_empty() {
        return serde_json::json!({ "command": "", "args": [] });
    }

    let mut parts = source.split_whitespace();
    let command = parts.next().unwrap_or("").to_string();
    let args: Vec<&str> = parts.collect();

    serde_json::json!({ "command": command, "args": args })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[allow(clippy::unwrap_used)]
    fn unique_temp_dir(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("plugin-export-{label}-{nanos}"));
        Utf8PathBuf::from_path_buf(dir).unwrap()
    }

    /// Build a minimal but valid plugin directory for tests.
    #[allow(clippy::unwrap_used)]
    fn write_test_plugin(root: &Utf8Path) {
        // Embedded skill — SKILL.md-rooted (AUTH1f: the legacy manifest.json
        // bundle format is no longer accepted by SkillPackage::load_from_dir).
        let skill_dir = root.join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: My Skill\n\
             description: Does the thing\n\
             license: Apache-2.0\n\
             vh_version: 0.1.0\n\
             vh_publisher: test\n\
             vh_permissions:\n\
               filesystem: none\n\
               network: none\n\
               clipboard: none\n\
             vh_execution:\n\
               sandbox: strict\n\
               timeout_ms: 30000\n\
               memory_mb: 512\n\
             vh_workflow_ref: workflow.yaml\n\
             ---\n\
             \n\
             # My Skill\n\
             \n\
             You are a helpful assistant.\n",
        )
        .unwrap();
        fs::write(skill_dir.join("workflow.yaml"), "name: test\nsteps: []").unwrap();
        // export_claude_code reads prompts/system.txt directly as the body of
        // the exported Claude-native SKILL.md — keep it around for the test.
        fs::create_dir_all(skill_dir.join("prompts")).unwrap();
        fs::write(
            skill_dir.join("prompts/system.txt"),
            "You are a helpful assistant.",
        )
        .unwrap();

        // Slash command
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("do-thing.md"),
            "---\nname: do-thing\ndescription: Does the thing\n---\nDo the thing.",
        )
        .unwrap();

        // plugin.json
        fs::write(
            root.join("plugin.json"),
            r#"{
                "schema_version": "1.0",
                "id": "dev-toolkit",
                "name": "Dev Toolkit",
                "version": "1.2.3",
                "publisher": "acme-corp",
                "description": "A toolkit for developers",
                "skills": [{ "path": "./skills/my-skill" }],
                "mcp_servers": [{
                    "name": "jira-mcp",
                    "package_source": "npx -y @anthropic/mcp-server-jira",
                    "description": "Jira integration"
                }],
                "commands": [{ "path": "./commands/do-thing.md" }],
                "user_config": {
                    "jira_url": { "description": "Your Jira instance URL", "sensitive": false }
                }
            }"#,
        )
        .unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn export_claude_code_creates_structure() {
        let plugin_dir = unique_temp_dir("src");
        let output_dir = unique_temp_dir("out");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&output_dir).unwrap();

        write_test_plugin(&plugin_dir);

        let result = export_claude_code(&plugin_dir, &output_dir);
        let dest = result.expect("export_claude_code should succeed");

        // .claude-plugin/plugin.json must exist and be valid JSON
        let plugin_json_path = dest.join(".claude-plugin").join("plugin.json");
        assert!(
            plugin_json_path.exists(),
            ".claude-plugin/plugin.json should exist"
        );
        let plugin_json_text = fs::read_to_string(&plugin_json_path).unwrap();
        let plugin_json: serde_json::Value = serde_json::from_str(&plugin_json_text).unwrap();
        assert_eq!(plugin_json["name"], "dev-toolkit");
        assert_eq!(plugin_json["version"], "1.2.3");
        assert_eq!(plugin_json["author"]["name"], "acme-corp");
        assert_eq!(plugin_json["skills"], "./skills/");
        assert_eq!(plugin_json["mcpServers"], "./.mcp.json");
        assert!(plugin_json["userConfig"]["jira_url"].is_object());

        // .mcp.json must exist and reference the MCP server
        let mcp_json_path = dest.join(".mcp.json");
        assert!(mcp_json_path.exists(), ".mcp.json should exist");
        let mcp_json_text = fs::read_to_string(&mcp_json_path).unwrap();
        let mcp_json: serde_json::Value = serde_json::from_str(&mcp_json_text).unwrap();
        let server = &mcp_json["mcpServers"]["jira-mcp"];
        assert_eq!(server["command"], "npx");
        assert_eq!(server["args"][0], "-y");
        assert_eq!(server["args"][1], "@anthropic/mcp-server-jira");

        // skills/ directory must exist
        let skills_dir = dest.join("skills");
        assert!(skills_dir.exists(), "skills/ should exist");

        // Command file should be copied in
        assert!(
            skills_dir.join("do-thing.md").exists(),
            "command do-thing.md should be copied"
        );

        // Embedded skill should be exported as SKILL.md
        let skill_md_path = skills_dir.join("my-skill").join("SKILL.md");
        assert!(skill_md_path.exists(), "my-skill/SKILL.md should exist");
        let skill_md_text = fs::read_to_string(&skill_md_path).unwrap();
        assert!(
            skill_md_text.contains("You are a helpful assistant."),
            "SKILL.md should contain the system prompt"
        );
        assert!(
            skill_md_text.contains("name: My Skill"),
            "SKILL.md should contain skill name in frontmatter"
        );

        let _ = fs::remove_dir_all(&plugin_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn export_mcpb_rejects_multi_server() {
        let plugin_dir = unique_temp_dir("mcpb-multi-src");
        let output_dir = unique_temp_dir("mcpb-multi-out");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&output_dir).unwrap();

        // Plugin with two MCP servers — mcpb only supports exactly one
        let cmd_dir = plugin_dir.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("cmd.md"),
            "---\nname: cmd\ndescription: test\n---\nDo it.",
        )
        .unwrap();
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
                "schema_version": "1.0",
                "id": "multi-server",
                "name": "Multi Server",
                "version": "0.1.0",
                "publisher": "test",
                "mcp_servers": [
                    { "name": "server-a", "package_source": "npx @foo/server-a" },
                    { "name": "server-b", "package_source": "npx @foo/server-b" }
                ],
                "commands": [{ "path": "./commands/cmd.md" }]
            }"#,
        )
        .unwrap();

        let err = export_mcpb(&plugin_dir, &output_dir)
            .expect_err("multi-server plugin should fail for mcpb format");
        let msg = err.to_string();
        assert!(
            msg.contains("exactly one MCP server"),
            "error should mention exactly one MCP server, got: {msg}"
        );

        let _ = fs::remove_dir_all(&plugin_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn export_mcpb_creates_archive_for_single_server() {
        let plugin_dir = unique_temp_dir("mcpb-single-src");
        let output_dir = unique_temp_dir("mcpb-single-out");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&output_dir).unwrap();

        // Plugin with exactly one MCP server
        let cmd_dir = plugin_dir.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("cmd.md"),
            "---\nname: cmd\ndescription: test\n---\nDo it.",
        )
        .unwrap();
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
                "schema_version": "1.0",
                "id": "my-plugin",
                "name": "My Plugin",
                "version": "2.0.0",
                "publisher": "test-corp",
                "mcp_servers": [
                    { "name": "my-server", "package_source": "npx -y @test/mcp-server" }
                ],
                "commands": [{ "path": "./commands/cmd.md" }]
            }"#,
        )
        .unwrap();

        let archive = export_mcpb(&plugin_dir, &output_dir)
            .expect("single-server plugin should export to mcpb");

        assert!(archive.exists(), "mcpb archive should exist at {archive}");
        assert_eq!(archive.file_name(), Some("my-plugin-2.0.0.mcpb"));

        // Verify the archive is a valid ZIP with manifest.json inside
        let file = fs::File::open(&archive).unwrap();
        let mut zip = zip::ZipArchive::new(file).expect("archive should be a valid ZIP");
        let mut manifest_entry = zip
            .by_name("manifest.json")
            .expect("manifest.json should be in the archive");
        let mut content = String::new();
        use std::io::Read as _;
        manifest_entry.read_to_string(&mut content).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(manifest["name"], "My Plugin");
        assert_eq!(manifest["version"], "2.0.0");
        assert_eq!(manifest["server"]["name"], "my-server");

        let _ = fs::remove_dir_all(&plugin_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn build_mcp_server_entry_parses_npx_command() {
        let entry = build_mcp_server_entry(Some("npx -y @anthropic/mcp-server-jira"));
        assert_eq!(entry["command"], "npx");
        assert_eq!(entry["args"][0], "-y");
        assert_eq!(entry["args"][1], "@anthropic/mcp-server-jira");
    }

    #[test]
    fn build_mcp_server_entry_handles_single_word() {
        let entry = build_mcp_server_entry(Some("node"));
        assert_eq!(entry["command"], "node");
        assert!(entry["args"].as_array().unwrap().is_empty());
    }

    #[test]
    fn build_mcp_server_entry_handles_none() {
        let entry = build_mcp_server_entry(None);
        assert_eq!(entry["command"], "");
    }
}
