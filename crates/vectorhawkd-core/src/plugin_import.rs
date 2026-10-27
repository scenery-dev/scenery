use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use vectorhawkd_manifest::{PluginCommand, PluginMcpServer, PluginUserConfigEntry};

/// Recognized external plugin formats that can be imported.
#[derive(Debug, PartialEq)]
pub enum ExternalPluginFormat {
    ClaudeCode,
    Mcpb,
}

/// Detect the format of an external plugin at the given path.
///
/// Returns `None` if the path does not match a recognized format.
pub fn detect_plugin_format(path: &Utf8Path) -> Option<ExternalPluginFormat> {
    if path.join(".claude-plugin/plugin.json").exists() {
        return Some(ExternalPluginFormat::ClaudeCode);
    }

    if path.is_file() {
        if let Some(ext) = path.extension() {
            if ext == "mcpb" {
                return Some(ExternalPluginFormat::Mcpb);
            }
        }
    }

    None
}

/// Derive a URL-safe plugin ID from a display name.
///
/// Lowercases the name, replaces non-alphanumeric characters with hyphens,
/// and collapses runs of hyphens into one.
fn derive_plugin_id(name: &str) -> String {
    let lowered = name.to_lowercase();
    let replaced: String = lowered
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and strip leading/trailing hyphens
    let mut result = String::with_capacity(replaced.len());
    let mut prev_was_hyphen = false;
    for ch in replaced.chars() {
        if ch == '-' {
            if !prev_was_hyphen && !result.is_empty() {
                result.push('-');
            }
            prev_was_hyphen = true;
        } else {
            result.push(ch);
            prev_was_hyphen = false;
        }
    }
    // Strip trailing hyphen
    if result.ends_with('-') {
        result.pop();
    }

    if result.is_empty() {
        "imported-plugin".to_string()
    } else {
        result
    }
}

/// Read MCP servers from an `.mcp.json`-style file.
///
/// Expected format:
/// ```json
/// { "mcpServers": { "server-name": { "command": "...", "args": [...] } } }
/// ```
fn read_mcp_servers_from_file(mcp_file: &Utf8Path) -> Result<Vec<PluginMcpServer>> {
    let text = fs::read_to_string(mcp_file)
        .with_context(|| format!("failed to read MCP config at {mcp_file}"))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in MCP config at {mcp_file}"))?;

    let servers_map = match value.get("mcpServers").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };

    let mut servers = Vec::with_capacity(servers_map.len());
    for (name, entry) in servers_map {
        let command = entry
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let args: Vec<String> = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let package_source = if args.is_empty() {
            if command.is_empty() {
                None
            } else {
                Some(command.clone())
            }
        } else {
            Some(format!("{} {}", command, args.join(" ")))
        };

        servers.push(PluginMcpServer {
            name: name.clone(),
            package_source,
            description: None,
            downstream_scopes: Vec::new(),
            credential_note: None,
        });
    }

    Ok(servers)
}

/// Import a Claude Code plugin directory into VectorHawk plugin format.
///
/// Reads `.claude-plugin/plugin.json`, converts MCP servers, copies skill
/// SKILL.md files as commands, and writes a VectorHawk `plugin.json` to
/// `{output_dir}/{plugin_id}/`.
pub fn import_claude_code_plugin(
    plugin_dir: &Utf8Path,
    output_dir: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let manifest_path = plugin_dir.join(".claude-plugin/plugin.json");
    let manifest_text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {manifest_path}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .with_context(|| format!("invalid JSON in {manifest_path}"))?;

    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("imported")
        .to_string();

    let version = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0")
        .to_string();

    let description = manifest
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let publisher = manifest
        .get("author")
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("imported")
        .to_string();

    let plugin_id = derive_plugin_id(&name);
    let out_dir = output_dir.join(&plugin_id);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory {out_dir}"))?;

    // Read MCP servers from the referenced file
    let mcp_servers = if let Some(mcp_ref) = manifest.get("mcpServers").and_then(|v| v.as_str()) {
        let mcp_path = plugin_dir.join(mcp_ref.trim_start_matches("./"));
        if mcp_path.exists() {
            read_mcp_servers_from_file(&mcp_path).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Copy skills from the referenced directory as slash commands
    let mut commands: Vec<PluginCommand> = Vec::new();
    if let Some(skills_ref) = manifest.get("skills").and_then(|v| v.as_str()) {
        let skills_path = plugin_dir.join(skills_ref.trim_start_matches("./"));
        if skills_path.exists() {
            let cmd_dir = out_dir.join("commands");
            fs::create_dir_all(&cmd_dir)
                .with_context(|| format!("failed to create commands directory {cmd_dir}"))?;

            let entries = fs::read_dir(&skills_path)
                .with_context(|| format!("failed to read skills directory {skills_path}"))?;

            for entry in entries {
                let entry = entry.with_context(|| "failed to read directory entry")?;
                let entry_path = entry.path();

                // Each skill is a subdirectory containing SKILL.md
                if !entry_path.is_dir() {
                    continue;
                }

                let skill_md = entry_path.join("SKILL.md");
                if !skill_md.exists() {
                    continue;
                }

                let skill_name = entry_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                let content = fs::read_to_string(&skill_md)
                    .with_context(|| format!("failed to read {}", skill_md.display()))?;

                let target = cmd_dir.join(format!("{skill_name}.md"));
                fs::write(&target, &content)
                    .with_context(|| format!("failed to write {target}"))?;

                commands.push(PluginCommand {
                    path: format!("commands/{skill_name}.md"),
                });
            }
        }
    }

    // Convert userConfig
    let user_config: HashMap<String, PluginUserConfigEntry> =
        if let Some(uc) = manifest.get("userConfig").and_then(|v| v.as_object()) {
            uc.iter()
                .map(|(key, val)| {
                    let desc = val
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("User configuration value")
                        .to_string();
                    let sensitive = val
                        .get("sensitive")
                        .and_then(|s| s.as_bool())
                        .unwrap_or(false);
                    (
                        key.clone(),
                        PluginUserConfigEntry {
                            description: desc,
                            sensitive,
                        },
                    )
                })
                .collect()
        } else {
            HashMap::new()
        };

    // Serialize to VectorHawk plugin.json
    let plugin_json = serde_json::json!({
        "schema_version": "1.0",
        "id": plugin_id,
        "name": name,
        "version": version,
        "publisher": publisher,
        "description": description,
        "skills": [],
        "mcp_servers": mcp_servers,
        "commands": commands,
        "user_config": user_config,
    });

    let plugin_json_text =
        serde_json::to_string_pretty(&plugin_json).context("failed to serialize plugin.json")?;

    let plugin_json_path = out_dir.join("plugin.json");
    fs::write(&plugin_json_path, plugin_json_text)
        .with_context(|| format!("failed to write {plugin_json_path}"))?;

    Ok(out_dir)
}

/// Largest `manifest.json` we will read from a `.mcpb` archive (decompressed).
pub(crate) const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Most entries a `.mcpb` central directory may list.
pub(crate) const MAX_MCPB_ENTRIES: usize = 20_000;
/// Raw archive bytes we will read to locate and parse the central directory
/// and read `manifest.json`. Bounds the work and allocation done while the
/// central directory is parsed, before the entry count can be checked.
const MAX_MCPB_METADATA_READ: u64 = 8 * 1024 * 1024;

/// `Read + Seek` wrapper that fails once more than `remaining` bytes have been
/// read through it. Seeking is free.
struct ReadBudget<R> {
    inner: R,
    remaining: u64,
}

impl<R: std::io::Read> std::io::Read for ReadBudget<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::other(
                ".mcpb archive metadata exceeds the read limit",
            ));
        }
        let max = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl<R: std::io::Seek> std::io::Seek for ReadBudget<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Import a `.mcpb` Desktop Extension into VectorHawk plugin format.
///
/// A `.mcpb` file is a ZIP archive containing a `manifest.json` that
/// describes the MCP server. Only `manifest.json` is read (capped at
/// [`MAX_MANIFEST_BYTES`]); no other entry is decompressed or extracted, so a
/// zip bomb elsewhere in the archive is never inflated. Archives listing more
/// than [`MAX_MCPB_ENTRIES`] entries are rejected. Writes a VectorHawk
/// `plugin.json` to `{output_dir}/{plugin_id}/`.
pub fn import_mcpb(mcpb_path: &Utf8Path, output_dir: &Utf8Path) -> Result<Utf8PathBuf> {
    let file = fs::File::open(mcpb_path).with_context(|| format!("failed to open {mcpb_path}"))?;
    let reader = ReadBudget {
        inner: file,
        remaining: MAX_MCPB_METADATA_READ,
    };

    let mut archive = zip::ZipArchive::new(reader)
        .with_context(|| format!("failed to read ZIP archive at {mcpb_path}"))?;

    if archive.len() > MAX_MCPB_ENTRIES {
        anyhow::bail!(
            ".mcpb archive at {mcpb_path} has {} entries (limit {MAX_MCPB_ENTRIES})",
            archive.len()
        );
    }

    let mut manifest_bytes = Vec::new();
    {
        let entry = archive
            .by_name("manifest.json")
            .context("failed to read manifest.json from .mcpb archive")?;
        entry
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut manifest_bytes)
            .context("failed to read manifest.json from .mcpb archive")?;
    }
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        anyhow::bail!("manifest.json in .mcpb archive exceeds {MAX_MANIFEST_BYTES} bytes");
    }
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .context("invalid JSON in manifest.json from .mcpb archive")?;

    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("imported")
        .to_string();

    let version = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0")
        .to_string();

    let description = manifest
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let plugin_id = derive_plugin_id(&name);
    let out_dir = output_dir.join(&plugin_id);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory {out_dir}"))?;

    // Build a single MCP server entry from the manifest server config.
    // The .mcpb manifest nests the command under "server": {"command": "...", "args": [...]}.
    let package_source = manifest
        .get("server")
        .and_then(|s| s.get("command"))
        .and_then(|cmd_obj| {
            // "command" is the nested object from build_mcp_server_entry —
            // extract the actual command string from it.
            let cmd = cmd_obj.get("command").and_then(|v| v.as_str())?;
            let args: Vec<String> = cmd_obj
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.as_str())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            if args.is_empty() {
                Some(cmd.to_string())
            } else {
                Some(format!("{} {}", cmd, args.join(" ")))
            }
        });

    let server_name = manifest
        .get("server")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string();

    let mcp_server = PluginMcpServer {
        name: server_name,
        package_source,
        description: description.clone(),
        downstream_scopes: Vec::new(),
        credential_note: None,
    };

    let plugin_json = serde_json::json!({
        "schema_version": "1.0",
        "id": plugin_id,
        "name": name,
        "version": version,
        "publisher": "imported",
        "description": description,
        "skills": [],
        "mcp_servers": [mcp_server],
        "commands": [],
        "user_config": {},
    });

    let plugin_json_text =
        serde_json::to_string_pretty(&plugin_json).context("failed to serialize plugin.json")?;

    let plugin_json_path = out_dir.join("plugin.json");
    fs::write(&plugin_json_path, plugin_json_text)
        .with_context(|| format!("failed to write {plugin_json_path}"))?;

    Ok(out_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("plugin-import-test-{label}-{nanos}")),
        )
        .expect("temp dir path should be UTF-8")
    }

    // ── derive_plugin_id ──────────────────────────────────────────────────────

    #[test]
    fn derive_plugin_id_converts_name_to_slug() {
        assert_eq!(derive_plugin_id("VectorHawk"), "vectorhawk");
        assert_eq!(derive_plugin_id("My Workflow Plugin"), "my-workflow-plugin");
        assert_eq!(derive_plugin_id("  --leading--  "), "leading");
        assert_eq!(derive_plugin_id("A.B.C"), "a-b-c");
        assert_eq!(derive_plugin_id(""), "imported-plugin");
    }

    // ── detect_plugin_format ──────────────────────────────────────────────────

    #[test]
    fn detect_claude_code_format() {
        let root = temp_root("detect-cc");
        let plugin_dir = root.join("my-plugin");
        let claude_dir = plugin_dir.join(".claude-plugin");
        fs::create_dir_all(&claude_dir).expect("create .claude-plugin dir");
        fs::write(claude_dir.join("plugin.json"), r#"{"name":"test"}"#).expect("write plugin.json");

        let result = detect_plugin_format(&plugin_dir);
        assert_eq!(result, Some(ExternalPluginFormat::ClaudeCode));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_mcpb_format() {
        let root = temp_root("detect-mcpb");
        fs::create_dir_all(&root).expect("create root");
        let mcpb_file = root.join("test-server.mcpb");
        // Write a minimal valid ZIP (empty ZIP end-of-central-directory record)
        let end_of_central_dir: &[u8] = &[
            0x50, 0x4B, 0x05, 0x06, // signature
            0x00, 0x00, // disk number
            0x00, 0x00, // disk with central dir
            0x00, 0x00, // number of entries on this disk
            0x00, 0x00, // total entries
            0x00, 0x00, 0x00, 0x00, // size of central dir
            0x00, 0x00, 0x00, 0x00, // offset to central dir
            0x00, 0x00, // comment length
        ];
        fs::write(&mcpb_file, end_of_central_dir).expect("write fake .mcpb");

        let result = detect_plugin_format(&mcpb_file);
        assert_eq!(result, Some(ExternalPluginFormat::Mcpb));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_returns_none_for_unknown_path() {
        let root = temp_root("detect-none");
        fs::create_dir_all(&root).expect("create root");

        let result = detect_plugin_format(&root);
        assert_eq!(result, None);

        let _ = fs::remove_dir_all(&root);
    }

    // ── import_claude_code_plugin ─────────────────────────────────────────────

    #[test]
    fn import_claude_code_creates_vectorhawk_plugin() {
        let src = temp_root("cc-src");
        let out = temp_root("cc-out");
        fs::create_dir_all(&out).expect("create output dir");

        // Build a minimal Claude Code plugin structure
        let claude_dir = src.join(".claude-plugin");
        fs::create_dir_all(&claude_dir).expect("create .claude-plugin");

        let plugin_json = serde_json::json!({
            "name": "My Test Plugin",
            "version": "1.2.3",
            "description": "A test plugin for import",
            "author": { "name": "Acme Corp" },
            "mcpServers": "./.mcp.json",
            "skills": "./skills/"
        });
        fs::write(
            claude_dir.join("plugin.json"),
            serde_json::to_string_pretty(&plugin_json).unwrap(),
        )
        .expect("write plugin.json");

        // MCP config
        let mcp_json = serde_json::json!({
            "mcpServers": {
                "acme-server": {
                    "command": "npx",
                    "args": ["-y", "@acme/mcp-server"]
                }
            }
        });
        fs::write(
            src.join(".mcp.json"),
            serde_json::to_string_pretty(&mcp_json).unwrap(),
        )
        .expect("write .mcp.json");

        // Skills directory with one skill
        let skills_dir = src.join("skills").join("plan-sprint");
        fs::create_dir_all(&skills_dir).expect("create skill dir");
        fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: plan-sprint\ndescription: Plan a sprint\n---\nPlan the sprint.",
        )
        .expect("write SKILL.md");

        let result = import_claude_code_plugin(&src, &out);
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());

        let out_path = result.unwrap();
        assert!(out_path.exists(), "output directory should exist");

        // Read and verify the generated plugin.json
        let generated_text =
            fs::read_to_string(out_path.join("plugin.json")).expect("read generated plugin.json");
        let generated: serde_json::Value =
            serde_json::from_str(&generated_text).expect("parse generated plugin.json");

        assert_eq!(generated["schema_version"], "1.0");
        assert_eq!(generated["id"], "my-test-plugin");
        assert_eq!(generated["name"], "My Test Plugin");
        assert_eq!(generated["version"], "1.2.3");
        assert_eq!(generated["publisher"], "Acme Corp");
        assert_eq!(generated["description"], "A test plugin for import");

        // MCP server should be converted
        let servers = generated["mcp_servers"]
            .as_array()
            .expect("mcp_servers array");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "acme-server");
        assert_eq!(servers[0]["package_source"], "npx -y @acme/mcp-server");

        // Command should be created from SKILL.md
        let commands = generated["commands"].as_array().expect("commands array");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["path"], "commands/plan-sprint.md");

        // The command file should exist
        assert!(
            out_path.join("commands/plan-sprint.md").exists(),
            "command file should be copied"
        );

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn import_claude_code_minimal_plugin_no_mcp_no_skills() {
        let src = temp_root("cc-minimal-src");
        let out = temp_root("cc-minimal-out");
        fs::create_dir_all(&out).expect("create output dir");

        let claude_dir = src.join(".claude-plugin");
        fs::create_dir_all(&claude_dir).expect("create .claude-plugin");
        fs::write(
            claude_dir.join("plugin.json"),
            r#"{"name": "simple", "version": "0.1.0"}"#,
        )
        .expect("write minimal plugin.json");

        let result = import_claude_code_plugin(&src, &out);
        assert!(
            result.is_ok(),
            "minimal import should succeed: {:?}",
            result.err()
        );

        let out_path = result.unwrap();
        let generated_text = fs::read_to_string(out_path.join("plugin.json")).unwrap();
        let generated: serde_json::Value = serde_json::from_str(&generated_text).unwrap();

        assert_eq!(generated["id"], "simple");
        assert_eq!(generated["publisher"], "imported");
        assert!(generated["mcp_servers"].as_array().unwrap().is_empty());
        assert!(generated["commands"].as_array().unwrap().is_empty());

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&out);
    }

    // ── import_mcpb ───────────────────────────────────────────────────────────

    fn write_mcpb(path: &Utf8Path, entries: &[(&str, &[u8], zip::CompressionMethod)]) {
        use std::io::Write;
        use zip::{write::SimpleFileOptions, ZipWriter};
        let mut zip = ZipWriter::new(fs::File::create(path).expect("create .mcpb"));
        for (name, data, method) in entries {
            let options = SimpleFileOptions::default()
                .compression_method(*method)
                .large_file(data.len() > u32::MAX as usize / 2);
            zip.start_file(*name, options).expect("start entry");
            zip.write_all(data).expect("write entry");
        }
        zip.finish().expect("finish .mcpb");
    }

    const MANIFEST: &[u8] = br#"{"name":"bomb","version":"1.0.0"}"#;

    #[test]
    fn import_mcpb_does_not_inflate_non_manifest_entries() {
        // A large deflated entry next to manifest.json whose compressed data
        // is corrupted. Inflating it (as a full extraction would) fails;
        // reading only manifest.json never touches it.
        let root = temp_root("mcpb-bomb");
        let out = root.join("out");
        fs::create_dir_all(&out).expect("create output dir");
        let mcpb_file = root.join("bomb.mcpb");
        let zeros = vec![0u8; 8 * 1024 * 1024];
        write_mcpb(
            &mcpb_file,
            &[
                ("manifest.json", MANIFEST, zip::CompressionMethod::Stored),
                ("payload.bin", &zeros, zip::CompressionMethod::Deflated),
            ],
        );

        // Corrupt the middle of payload.bin's compressed stream.
        let (start, len) = {
            let mut archive =
                zip::ZipArchive::new(fs::File::open(&mcpb_file).unwrap()).expect("open zip");
            let entry = archive.by_name("payload.bin").expect("payload entry");
            (entry.data_start(), entry.compressed_size())
        };
        let mut bytes = fs::read(&mcpb_file).unwrap();
        for b in &mut bytes[(start + len / 4) as usize..(start + len / 2) as usize] {
            *b = 0xff;
        }
        fs::write(&mcpb_file, &bytes).unwrap();

        let result = import_mcpb(&mcpb_file, &out);
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());
        assert!(out.join("bomb").join("plugin.json").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_mcpb_rejects_oversized_manifest() {
        let root = temp_root("mcpb-big-manifest");
        let out = root.join("out");
        fs::create_dir_all(&out).expect("create output dir");
        let mcpb_file = root.join("big.mcpb");
        // Valid JSON padded with whitespace past the manifest cap; deflates to a few KiB.
        let mut manifest = vec![b' '; (MAX_MANIFEST_BYTES as usize) + 1024];
        manifest.extend_from_slice(MANIFEST);
        write_mcpb(
            &mcpb_file,
            &[("manifest.json", &manifest, zip::CompressionMethod::Deflated)],
        );

        let err = import_mcpb(&mcpb_file, &out).expect_err("oversized manifest must be rejected");
        assert!(err.to_string().contains("manifest.json"), "got: {err}");
        assert!(!out.join("bomb").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_mcpb_rejects_too_many_entries() {
        let root = temp_root("mcpb-many");
        let out = root.join("out");
        fs::create_dir_all(&out).expect("create output dir");
        let mcpb_file = root.join("many.mcpb");
        let names: Vec<String> = (0..=MAX_MCPB_ENTRIES).map(|i| format!("f/{i}")).collect();
        let mut entries: Vec<(&str, &[u8], zip::CompressionMethod)> =
            vec![("manifest.json", MANIFEST, zip::CompressionMethod::Stored)];
        entries.extend(
            names
                .iter()
                .map(|n| (n.as_str(), &b""[..], zip::CompressionMethod::Stored)),
        );
        write_mcpb(&mcpb_file, &entries);

        let err = import_mcpb(&mcpb_file, &out).expect_err("too many entries must be rejected");
        assert!(err.to_string().contains("entries"), "got: {err}");
        assert!(!out.join("bomb").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn import_mcpb_does_not_write_absolute_path_entries() {
        use std::io::Write;
        use zip::{write::SimpleFileOptions, ZipWriter};

        let root = temp_root("mcpb-abs-entry");
        let out = root.join("out");
        fs::create_dir_all(&out).expect("create output dir");

        // An entry whose name is an absolute path outside the extraction dir.
        let escaped = root.join("escaped.txt");
        assert!(escaped.is_absolute());

        let mcpb_file = root.join("evil.mcpb");
        let mut zip = ZipWriter::new(fs::File::create(&mcpb_file).expect("create .mcpb"));
        let options = SimpleFileOptions::default();
        zip.start_file("manifest.json", options)
            .expect("start manifest.json");
        zip.write_all(br#"{"name":"evil","version":"1.0.0"}"#)
            .expect("write manifest.json");
        zip.start_file(escaped.as_str(), options)
            .expect("start absolute entry");
        zip.write_all(b"pwned").expect("write absolute entry");
        zip.finish().expect("finish .mcpb");

        let result = import_mcpb(&mcpb_file, &out);
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());
        assert!(
            !escaped.exists(),
            "absolute archive entry must not be written outside the extraction dir"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
