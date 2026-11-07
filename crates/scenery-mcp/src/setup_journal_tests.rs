//! `write_entry_in` + restore-journal integration: backups, journal detail,
//! JSONC/TOML preservation, refusal of unparsable files.
#![allow(clippy::unwrap_used)]

use super::*;
use scenery_core::restore_journal::{sha256_hex, JournalOp, RestoreJournal};

fn client(name: &str, path: PathBuf, key: &str, format: ConfigFormat) -> ClientConfig {
    ClientConfig {
        name: name.to_string(),
        config_path: path,
        mcp_key: key.to_string(),
        already_configured: false,
        format,
    }
}

fn root_of(tmp: &std::path::Path) -> camino::Utf8PathBuf {
    camino::Utf8PathBuf::from_path_buf(tmp.join("scenery-root")).unwrap()
}

fn entry_value() -> serde_json::Value {
    serde_json::json!({"command": "scenery", "args": ["mcp", "serve"]})
}

#[test]
fn cursor_and_vscode_backups_do_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let cursor = tmp.path().join(".cursor").join("mcp.json");
    let vscode = tmp.path().join("Code").join("User").join("mcp.json");
    for (p, body) in [
        (&cursor, r#"{"mcpServers":{"c":{"command":"cursor-own"}}}"#),
        (&vscode, r#"{"servers":{"v":{"command":"vscode-own"}}}"#),
    ] {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    let root = root_of(tmp.path());
    let c = client("Cursor", cursor.clone(), "mcpServers", ConfigFormat::Json);
    let v = client("VS Code", vscode.clone(), "servers", ConfigFormat::Json);
    write_entry_in(&c, entry_value(), Some(root.clone())).unwrap();
    write_entry_in(&v, entry_value(), Some(root.clone())).unwrap();

    let entries = RestoreJournal::new(root).read_all().unwrap();
    assert_eq!(entries.len(), 2);
    let backup = |name: &str| {
        let e = entries
            .iter()
            .find(|e| e.client.as_deref() == Some(name))
            .unwrap();
        fs::read_to_string(e.backup_path.as_ref().unwrap()).unwrap()
    };
    assert!(backup("Cursor").contains("cursor-own"));
    assert!(backup("VS Code").contains("vscode-own"));
    assert_ne!(
        entries[0].backup_path, entries[1].backup_path,
        "same basename, different clients: different backup files"
    );
}

#[test]
fn jsonc_with_comments_and_trailing_commas_is_edited_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mcp.json");
    let original = "{\n  // my servers\n  \"servers\": {\n    \"mine\": { \"command\": \"x\" }, // keep\n  },\n}\n";
    fs::write(&path, original).unwrap();
    let cfg = client("VS Code", path.clone(), "servers", ConfigFormat::Json);
    write_entry_in(&cfg, entry_value(), Some(root_of(tmp.path()))).unwrap();

    let after = fs::read_to_string(&path).unwrap();
    assert!(after.contains("// my servers"), "{after}");
    assert!(after.contains("// keep"), "{after}");
    assert!(after.contains("\"mine\""), "{after}");
    assert!(after.contains("\"scenery\""), "{after}");
}

#[test]
fn unparsable_file_is_refused_and_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mcp.json");
    let original = "{\"servers\": {\"a\": ";
    fs::write(&path, original).unwrap();
    let cfg = client("VS Code", path.clone(), "servers", ConfigFormat::Json);
    let err = write_entry_in(&cfg, entry_value(), Some(root_of(tmp.path()))).unwrap_err();
    assert!(format!("{err:#}").contains("refusing"), "{err:#}");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn journal_records_hash_and_creation_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("new").join("mcp.json");
    let cfg = client("Cursor", path.clone(), "mcpServers", ConfigFormat::Json);
    let root = root_of(tmp.path());
    write_entry_in(&cfg, entry_value(), Some(root.clone())).unwrap();

    let entries = RestoreJournal::new(root).read_all().unwrap();
    let e = &entries[0];
    assert_eq!(e.op, JournalOp::ConfigEdit);
    assert!(e.backup_path.is_none());
    assert_eq!(e.detail["file_existed"], false);
    assert_eq!(e.detail["mcp_key_created"], true);
    let written = fs::read(&path).unwrap();
    assert_eq!(e.detail["post_setup_sha256"], sha256_hex(&written));
}

#[test]
fn resetup_keeps_original_backup_but_setup_after_removal_takes_a_fresh_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mcp.json");
    fs::write(&path, r#"{"mcpServers":{"a":{"command":"a"}}}"#).unwrap();
    let cfg = client("Cursor", path.clone(), "mcpServers", ConfigFormat::Json);
    let root = root_of(tmp.path());
    let journal = RestoreJournal::new(root.clone());

    write_entry_in(&cfg, entry_value(), Some(root.clone())).unwrap();
    let first = journal.read_all().unwrap()[0].backup_path.clone().unwrap();
    // A later, different entry value (e.g. command path repair) re-runs setup.
    let changed = serde_json::json!({"command": "/other/scenery", "args": ["mcp", "serve"]});
    write_entry_in(&cfg, changed, Some(root.clone())).unwrap();
    let entries = journal.read_all().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].backup_path.as_deref(), Some(first.as_str()));
    assert!(!fs::read_to_string(&first).unwrap().contains("scenery"));

    // The user removes our entry and adds their own, then setup runs again.
    fs::write(
        &path,
        r#"{"mcpServers":{"a":{"command":"a"},"b":{"command":"b"}}}"#,
    )
    .unwrap();
    write_entry_in(&cfg, entry_value(), Some(root)).unwrap();
    let third = journal
        .read_all()
        .unwrap()
        .pop()
        .unwrap()
        .backup_path
        .unwrap();
    assert_ne!(third, first);
    assert!(fs::read_to_string(&third).unwrap().contains("\"b\""));
}

#[test]
fn codex_toml_comments_survive_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let original =
        "# my codex\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.mine]\ncommand = \"x\"\n";
    fs::write(&path, original).unwrap();
    let cfg = client("Codex", path.clone(), "mcp_servers", ConfigFormat::Toml);
    write_entry_in(&cfg, build_mcp_entry(), Some(root_of(tmp.path()))).unwrap();
    let after = fs::read_to_string(&path).unwrap();
    assert!(after.starts_with(original), "{after}");
    assert!(after.contains("[mcp_servers.scenery]"), "{after}");
}

#[test]
fn identical_entry_does_not_rewrite_or_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mcp.json");
    fs::write(&path, "{}").unwrap();
    let cfg = client("Cursor", path.clone(), "mcpServers", ConfigFormat::Json);
    let root = root_of(tmp.path());
    write_entry_in(&cfg, entry_value(), Some(root.clone())).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    write_entry_in(&cfg, entry_value(), Some(root.clone())).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert_eq!(RestoreJournal::new(root).read_all().unwrap().len(), 1);
}
