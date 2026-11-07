//! End-to-end tests for the client-config half of uninstall: real
//! `write_mcp_entry` setup, then `strip_client_configs`, in a fake HOME.
#![allow(clippy::unwrap_used)]

use super::tests::{brokered_row, temp_dir, with_fake_home};
use super::*;
use scenery_core::restore_journal::{JournalOp, JournalSource};
use scenery_core::state::AppState;
use scenery_mcp::config_edit;
use scenery_mcp::setup::{detect_ai_clients, write_mcp_entry, ConfigFormat};
use std::fs;

fn vscode_user_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Code/User")
    } else {
        home.join(".config/Code/User")
    }
}

fn desktop_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude")
    } else {
        home.join(".config/Claude")
    }
}

/// Create the directories that make every supported client detectable.
fn make_all_client_dirs(home: &Path) {
    for d in [
        home.join(".claude"),
        home.join(".cursor"),
        home.join(".codeium/windsurf"),
        home.join(".gemini"),
        home.join(".codex"),
        desktop_dir(home),
        vscode_user_dir(home),
    ] {
        fs::create_dir_all(d).unwrap();
    }
}

fn client_path(home: &Path, name: &str) -> PathBuf {
    match name {
        "Claude Code" => home.join(".claude.json"),
        "Claude Desktop" => desktop_dir(home).join("claude_desktop_config.json"),
        "Cursor" => home.join(".cursor/mcp.json"),
        "Windsurf" => home.join(".codeium/windsurf/mcp_config.json"),
        "VS Code" => vscode_user_dir(home).join("mcp.json"),
        "Gemini CLI" => home.join(".gemini/settings.json"),
        "Codex" => home.join(".codex/config.toml"),
        other => panic!("unknown client {other}"),
    }
}

fn setup_all() {
    for c in detect_ai_clients() {
        write_mcp_entry(&c).unwrap();
    }
}

fn journal_entries() -> Vec<JournalEntry> {
    let root = AppState::resolve_root_dir().unwrap();
    RestoreJournal::new(root).read_all().unwrap()
}

/// What `run` does for client configs, minus daemons and sign-out.
fn uninstall_with(brokered: Vec<scenery_core::state::McpInstallRow>) -> RestoreReport {
    let entries = journal_entries();
    let plan = Plan {
        brokered,
        clients: detect_ai_clients(),
    };
    let mut report = RestoreReport::default();
    strip_client_configs(&plan, &entries, &mut report);
    if report.failed.is_empty() {
        let root = AppState::resolve_root_dir().unwrap();
        RestoreJournal::new(root).clear().unwrap();
    }
    report
}

fn uninstall() -> RestoreReport {
    uninstall_with(Vec::new())
}

fn add_server(path: &Path, format: ConfigFormat, key: &str, name: &str) {
    let text = fs::read_to_string(path).unwrap();
    let value = serde_json::json!({"command": name});
    let out = config_edit::set_entry(format, &text, key, name, &value).unwrap();
    fs::write(path, out.text).unwrap();
}

const CLAUDE_4SPACE: &str = "{\n    \"zeta\": 1,\n    \"mcpServers\": {\n        \"mine\": {\"command\": \"m\"}\n    },\n    \"alpha\": [1, 2]\n}\n";
const WINDSURF_MIN: &str = "{\"mcpServers\":{\"w\":{\"command\":\"w\"}}}";
const CODEX_TOML: &str =
    "# codex\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.mine]\ncommand = \"x\"\n";

// ── 1. backup collision ─────────────────────────────────────────────────────

#[test]
fn cursor_and_vscode_each_get_their_own_content_back() {
    let home = temp_dir("collide");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let cursor = "{\"mcpServers\": {\"c\": {\"command\": \"cursor-own\"}}}\n";
        let vscode = "{\n  // vscode\n  \"servers\": {\"v\": {\"command\": \"vscode-own\"}}\n}\n";
        fs::write(client_path(&home, "Cursor"), cursor).unwrap();
        fs::write(client_path(&home, "VS Code"), vscode).unwrap();
        setup_all();
        let report = uninstall();

        assert_eq!(
            fs::read_to_string(client_path(&home, "Cursor")).unwrap(),
            cursor
        );
        assert_eq!(
            fs::read_to_string(client_path(&home, "VS Code")).unwrap(),
            vscode
        );
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        // 6. every Restored entry names its client
        for want in ["Cursor", "VS Code"] {
            assert!(
                report.restored.iter().any(|r| r.client == want),
                "no restored entry for {want}: {:?}",
                report.restored
            );
        }
        assert!(render_markdown(&report).contains("**VS Code**"));
    });
}

// ── 2. JSONC ────────────────────────────────────────────────────────────────

#[test]
fn jsonc_with_comments_survives_setup_edit_and_uninstall() {
    let home = temp_dir("jsonc");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let path = client_path(&home, "VS Code");
        fs::write(
            &path,
            "{\n  // my servers\n  \"servers\": {\n    \"mine\": { \"command\": \"x\" }, // keep\n  },\n}\n",
        )
        .unwrap();
        setup_all();
        let during = fs::read_to_string(&path).unwrap();
        assert!(
            during.contains("// my servers") && during.contains("\"mine\""),
            "{during}"
        );

        // The user edits the file while Scenery is installed.
        let edited = during.replace("// my servers", "// my servers (edited)");
        fs::write(&path, &edited).unwrap();
        let report = uninstall();

        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("// my servers (edited)"), "{after}");
        assert!(
            after.contains("// keep") && after.contains("\"mine\""),
            "{after}"
        );
        assert!(!after.contains("scenery"), "{after}");
        // 6. JSONC servers are reported as kept
        assert!(report
            .kept
            .iter()
            .any(|k| k.name == "mine" && k.location == "VS Code"));
    });
}

// ── 3. stale restore ────────────────────────────────────────────────────────

#[test]
fn edits_after_setup_survive_in_claude_json_and_cursor() {
    let home = temp_dir("stale");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let claude = client_path(&home, "Claude Code");
        let cursor = client_path(&home, "Cursor");
        fs::write(&claude, CLAUDE_4SPACE).unwrap();
        fs::write(&cursor, "{\"mcpServers\": {}}\n").unwrap();
        setup_all();
        add_server(&claude, ConfigFormat::Json, "mcpServers", "added-later");
        // Claude Code rewrites unrelated keys all the time.
        let t = fs::read_to_string(&claude)
            .unwrap()
            .replace("\"zeta\": 1", "\"zeta\": 2");
        fs::write(&claude, t).unwrap();
        add_server(&cursor, ConfigFormat::Json, "mcpServers", "cursor-later");

        let report = uninstall();
        let c = fs::read_to_string(&claude).unwrap();
        assert!(
            c.contains("added-later") && c.contains("\"zeta\": 2") && c.contains("\"mine\""),
            "{c}"
        );
        assert!(!c.contains("scenery"), "{c}");
        let u = fs::read_to_string(&cursor).unwrap();
        assert!(u.contains("cursor-later") && !u.contains("scenery"), "{u}");
        assert!(report.failed.is_empty());
    });
}

#[test]
fn file_absent_at_setup_is_deleted_only_when_nothing_else_is_in_it() {
    let home = temp_dir("absent");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let cursor = client_path(&home, "Cursor");
        let windsurf = client_path(&home, "Windsurf");
        assert!(!cursor.exists() && !windsurf.exists());
        setup_all();
        assert!(cursor.exists() && windsurf.exists());
        // The user adds a server to one of the files Scenery created.
        add_server(&cursor, ConfigFormat::Json, "mcpServers", "my-own");

        let report = uninstall();
        assert!(
            !windsurf.exists(),
            "untouched Scenery-created file is removed"
        );
        let kept = fs::read_to_string(&cursor).unwrap();
        assert!(
            kept.contains("my-own") && !kept.contains("scenery"),
            "{kept}"
        );
        assert!(report.kept.iter().any(|k| k.name == "my-own"));
    });
}

// ── 4. journal never cleared ────────────────────────────────────────────────

#[test]
fn setup_uninstall_setup_edit_uninstall_keeps_the_edit() {
    let home = temp_dir("cycle");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let cursor = client_path(&home, "Cursor");
        let original = "{\n    \"mcpServers\": {\n        \"a\": {\"command\": \"a\"}\n    }\n}\n";
        fs::write(&cursor, original).unwrap();

        setup_all();
        uninstall();
        assert_eq!(fs::read_to_string(&cursor).unwrap(), original);
        assert!(
            journal_entries().is_empty(),
            "journal is cleared after uninstall"
        );

        setup_all();
        add_server(&cursor, ConfigFormat::Json, "mcpServers", "edit-two");
        uninstall();
        let after = fs::read_to_string(&cursor).unwrap();
        assert!(
            after.contains("edit-two") && after.contains("\"a\""),
            "{after}"
        );
        assert!(!after.contains("scenery"), "{after}");
    });
}

// ── 5. byte-identical round trips ───────────────────────────────────────────

#[test]
fn untouched_configs_round_trip_byte_identical_for_every_client() {
    let home = temp_dir("roundtrip");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let files = [
            ("Claude Code", CLAUDE_4SPACE),
            ("Windsurf", WINDSURF_MIN),
            ("Codex", CODEX_TOML),
            ("Gemini CLI", "{\n\t\"theme\": \"x\"\n}"),
            ("Claude Desktop", "{\"mcpServers\":{}}\n"),
            ("Cursor", "{\r\n  \"mcpServers\": {}\r\n}\r\n"),
            ("VS Code", "{\n  // c\n  \"servers\": {},\n}\n"),
        ];
        for (name, body) in files {
            fs::write(client_path(&home, name), body).unwrap();
        }
        setup_all();
        let report = uninstall();
        for (name, body) in files {
            assert_eq!(
                fs::read_to_string(client_path(&home, name)).unwrap(),
                body,
                "{name} must come back byte for byte"
            );
        }
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    });
}

#[test]
fn surgical_removal_keeps_formatting_and_number_spellings() {
    let home = temp_dir("lossless");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let claude = client_path(&home, "Claude Code");
        fs::write(
            &claude,
            "{\n    \"zeta\": 12345678901234567890123,\n    \"esc\": \"\\u00e9\\/\",\n    \"mcpServers\": {\n        \"mine\": {\"command\": \"m\"}\n    },\n    \"alpha\": 1\n}\n",
        )
        .unwrap();
        setup_all();
        // Any edit forces the surgical path.
        let t = fs::read_to_string(&claude)
            .unwrap()
            .replace("\"alpha\": 1", "\"alpha\": 2");
        fs::write(&claude, t).unwrap();
        uninstall();
        assert_eq!(
            fs::read_to_string(&claude).unwrap(),
            "{\n    \"zeta\": 12345678901234567890123,\n    \"esc\": \"\\u00e9\\/\",\n    \"mcpServers\": {\n        \"mine\": {\"command\": \"m\"}\n    },\n    \"alpha\": 2\n}\n"
        );
    });
}

#[test]
fn codex_toml_edit_after_setup_keeps_comments_and_the_edit() {
    let home = temp_dir("codex-edit");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let path = client_path(&home, "Codex");
        fs::write(&path, CODEX_TOML).unwrap();
        setup_all();
        let t = fs::read_to_string(&path).unwrap() + "\n[mcp_servers.later]\ncommand = \"l\"\n";
        fs::write(&path, t).unwrap();
        let report = uninstall();
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# codex") && after.contains("# pinned"),
            "{after}"
        );
        assert!(
            after.contains("[mcp_servers.later]") && !after.contains("scenery"),
            "{after}"
        );
        // 6. TOML servers are reported as kept
        assert!(report
            .kept
            .iter()
            .any(|k| k.name == "later" && k.location == "Codex"));
    });
}

// ── 6. report ───────────────────────────────────────────────────────────────

#[test]
fn brokered_servers_are_removed_once_and_never_listed_as_kept() {
    let home = temp_dir("brokered");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let claude = client_path(&home, "Claude Code");
        fs::write(
            &claude,
            "{\"mcpServers\": {\"my-own\": {\"command\": \"m\"}}}\n",
        )
        .unwrap();
        setup_all();
        add_server(&claude, ConfigFormat::Json, "mcpServers", "slack-mcp");
        let mut entries = journal_entries();
        entries.push(
            JournalEntry::new(
                JournalOp::ArtifactPush,
                JournalSource::Brokered,
                claude.to_string_lossy().to_string(),
            )
            .with_slug("slack-mcp")
            .with_client("Claude Code")
            .with_detail(serde_json::json!({
                "server_key": "slack-mcp", "mcp_key": "mcpServers", "installation_id": "inst-1"
            })),
        );
        let plan = Plan {
            brokered: vec![brokered_row("uuid-slack", "inst-1", "Slack")],
            clients: detect_ai_clients(),
        };
        let mut report = RestoreReport::default();
        strip_client_configs(&plan, &entries, &mut report);

        let after = fs::read_to_string(&claude).unwrap();
        assert!(
            !after.contains("slack-mcp") && after.contains("my-own"),
            "{after}"
        );
        assert_eq!(report.removed.len(), 1);
        assert_eq!(
            report.removed[0].stripped_from,
            vec!["Claude Code".to_string()]
        );
        assert!(report.kept.iter().all(|k| k.name != "slack-mcp"));
        assert!(report.kept.iter().any(|k| k.name == "my-own"));
    });
}

#[test]
fn kept_only_lists_servers_that_are_still_in_the_file() {
    let home = temp_dir("kept-after");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let cursor = client_path(&home, "Cursor");
        fs::write(&cursor, "{\"mcpServers\": {\"x\": {}}}\n").unwrap();
        setup_all();
        let report = uninstall();
        let still: Vec<String> = config_edit::entry_names(
            ConfigFormat::Json,
            &fs::read_to_string(&cursor).unwrap(),
            "mcpServers",
        )
        .unwrap();
        let listed: Vec<&str> = report
            .kept
            .iter()
            .filter(|k| k.location == "Cursor")
            .map(|k| k.name.as_str())
            .collect();
        assert_eq!(listed, still.iter().map(String::as_str).collect::<Vec<_>>());
    });
}

#[test]
fn unparsable_config_is_refused_reported_and_fails_the_command() {
    let home = temp_dir("refuse");
    with_fake_home(&home, || {
        make_all_client_dirs(&home);
        let vscode = client_path(&home, "VS Code");
        fs::write(&vscode, "{\"servers\": {}}\n").unwrap();
        setup_all();
        let broken = "{\"servers\": {\"scenery\": ";
        fs::write(&vscode, broken).unwrap();

        let report = uninstall();
        assert_eq!(
            fs::read_to_string(&vscode).unwrap(),
            broken,
            "file untouched"
        );
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert!(report.failed[0].contains("VS Code"));
        assert!(report.warnings.iter().any(|w| w.contains("VS Code")));
        assert!(failure_result(&report).is_err());
        assert!(
            !journal_entries().is_empty(),
            "journal kept so a re-run can retry"
        );
        assert!(render_markdown(&report).contains("Not restored"));
    });
    assert!(failure_result(&RestoreReport::default()).is_ok());
}

#[test]
fn uninstall_creates_and_leaves_no_settings_lock() {
    let home = temp_dir("locks");
    with_fake_home(&home, || {
        let claude = home.join(".claude");
        fs::create_dir_all(claude.join("plugins")).unwrap();
        fs::write(claude.join("settings.json"), "{\"theme\": \"dark\"}\n").unwrap();
        let mut report = RestoreReport::default();
        remove_managed_artifacts(
            &Plan {
                brokered: vec![],
                clients: vec![],
            },
            &mut report,
        );
        assert!(!claude.join("settings.lock").exists());
        assert_eq!(
            fs::read_to_string(claude.join("settings.json")).unwrap(),
            "{\"theme\": \"dark\"}\n"
        );

        // A lock left by an earlier run is cleaned up too.
        fs::write(claude.join("settings.lock"), "").unwrap();
        fs::write(home.join(".claude.json.lock"), "").unwrap();
        remove_lock_files(&mut report);
        assert!(!claude.join("settings.lock").exists());
        assert!(!home.join(".claude.json.lock").exists());
    });
}
