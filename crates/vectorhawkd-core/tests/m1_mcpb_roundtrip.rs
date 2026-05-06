//! Round-trip integration test: build a synthetic VectorHawk plugin, export it
//! as a `.mcpb` archive, then import it back and assert the MCP server
//! configuration is faithfully preserved.

use camino::Utf8PathBuf;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use vectorhawkd_core::{plugin_export, plugin_import};

fn temp_dir(label: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("mcpb-roundtrip-{label}-{nanos}"));
    Utf8PathBuf::from_path_buf(path).expect("temp dir path should be UTF-8")
}

/// Write the minimal plugin structure required for export_mcpb to succeed.
fn write_minimal_plugin(root: &Utf8PathBuf) {
    fs::create_dir_all(root).expect("create plugin root");

    let cmd_dir = root.join("commands");
    fs::create_dir_all(&cmd_dir).expect("create commands dir");
    fs::write(
        cmd_dir.join("hello.md"),
        "---\nname: hello\ndescription: Say hello\n---\nSay hello.",
    )
    .expect("write hello.md");

    fs::write(
        root.join("plugin.json"),
        r#"{
            "schema_version": "1.0",
            "id": "acme-tools",
            "name": "Acme Tools",
            "version": "3.1.4",
            "publisher": "acme-corp",
            "description": "Tools from Acme Corp",
            "skills": [],
            "mcp_servers": [{
                "name": "acme-server",
                "package_source": "npx -y @acme/mcp-server",
                "description": "Acme MCP server"
            }],
            "commands": [{ "path": "./commands/hello.md" }],
            "user_config": {}
        }"#,
    )
    .expect("write plugin.json");
}

#[test]
fn mcpb_export_then_import_preserves_server_config() {
    let plugin_dir = temp_dir("plugin");
    let export_dir = temp_dir("export");
    let import_dir = temp_dir("import");

    write_minimal_plugin(&plugin_dir);
    fs::create_dir_all(&export_dir).expect("create export dir");
    fs::create_dir_all(&import_dir).expect("create import dir");

    // Export to .mcpb
    let archive_path = plugin_export::export_mcpb(&plugin_dir, &export_dir)
        .expect("export_mcpb should succeed");

    assert!(archive_path.exists(), ".mcpb archive should exist");
    assert_eq!(
        archive_path.file_name(),
        Some("acme-tools-3.1.4.mcpb"),
        "archive name should be {{id}}-{{version}}.mcpb"
    );

    // Import back from .mcpb
    let out_dir = plugin_import::import_mcpb(&archive_path, &import_dir)
        .expect("import_mcpb should succeed");

    assert!(out_dir.exists(), "output plugin directory should exist");

    // Verify the round-tripped plugin.json
    let plugin_json_text =
        fs::read_to_string(out_dir.join("plugin.json")).expect("read plugin.json");
    let plugin_json: serde_json::Value =
        serde_json::from_str(&plugin_json_text).expect("parse plugin.json");

    assert_eq!(plugin_json["schema_version"], "1.0");
    assert_eq!(plugin_json["name"], "Acme Tools");
    assert_eq!(plugin_json["version"], "3.1.4");

    let servers = plugin_json["mcp_servers"]
        .as_array()
        .expect("mcp_servers should be an array");
    assert_eq!(servers.len(), 1, "should have exactly one MCP server");

    // The server name is preserved from the manifest's "server.name" field
    assert_eq!(servers[0]["name"], "acme-server");

    // package_source reconstructed from command+args
    let source = servers[0]["package_source"]
        .as_str()
        .expect("package_source should be a string");
    assert!(
        source.contains("npx"),
        "package_source should reference npx, got: {source}"
    );
    assert!(
        source.contains("@acme/mcp-server"),
        "package_source should reference the package, got: {source}"
    );

    let _ = fs::remove_dir_all(&plugin_dir);
    let _ = fs::remove_dir_all(&export_dir);
    let _ = fs::remove_dir_all(&import_dir);
}

#[test]
fn detect_format_recognizes_mcpb_extension() {
    let root = temp_dir("detect");
    fs::create_dir_all(&root).expect("create root");
    let mcpb_file = root.join("my-extension.mcpb");
    // Minimal valid ZIP end-of-central-directory record
    let eocd: &[u8] = &[
        0x50, 0x4B, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    fs::write(&mcpb_file, eocd).expect("write fake .mcpb");

    assert_eq!(
        plugin_import::detect_plugin_format(&mcpb_file),
        Some(plugin_import::ExternalPluginFormat::Mcpb)
    );

    let _ = fs::remove_dir_all(&root);
}
