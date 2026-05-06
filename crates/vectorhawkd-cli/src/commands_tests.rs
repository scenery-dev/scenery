//! Clap parsing smoke tests for the `vectorhawk` CLI.
//!
//! Kept separate from production code so `#[allow(clippy::unwrap_used)]` stays
//! out of the production modules.
#![allow(clippy::unwrap_used)]

use super::Cli;
use clap::Parser;

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(["vectorhawk"].iter().chain(args).copied()).unwrap()
}

fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(["vectorhawk"].iter().chain(args).copied())
}

// ── doctor ────────────────────────────────────────────────────────────────────

#[test]
fn doctor_parses() {
    use super::Command;
    // Clear any inherited VECTORHAWK_REGISTRY_URL so this test doesn't
    // depend on the user's shell — Doctor::registry_url is wired with
    // `#[arg(env = "VECTORHAWK_REGISTRY_URL")]`, and the test's intent
    // is "no CLI flag → field is None", not "no env var".
    //
    // SAFETY: env mutation is process-global; this test is fast and
    // doesn't share state with other parallel tests because it doesn't
    // re-set the var.
    unsafe { std::env::remove_var("VECTORHAWK_REGISTRY_URL") };
    match parse(&["doctor"]).command {
        Command::Doctor { registry_url } => {
            assert!(
                registry_url.is_none(),
                "expected None, got {registry_url:?}"
            );
        }
        other => panic!("expected Doctor, got {other:?}"),
    }
}

#[test]
fn doctor_with_registry_url_parses() {
    use super::Command;
    match parse(&["doctor", "--registry-url", "http://localhost:8000"]).command {
        Command::Doctor { registry_url } => {
            assert_eq!(registry_url.as_deref(), Some("http://localhost:8000"));
        }
        other => panic!("expected Doctor, got {other:?}"),
    }
}

// ── skill subcommands ─────────────────────────────────────────────────────────

#[test]
fn skill_list_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "list"]).command {
        Command::Skill(SkillCommand::List) => {}
        other => panic!("expected Skill(List), got {other:?}"),
    }
}

#[test]
fn skill_install_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "install", "--path", "./my-skill"]).command {
        Command::Skill(SkillCommand::Install { path, link }) => {
            assert_eq!(path.as_str(), "./my-skill");
            assert!(!link);
        }
        other => panic!("expected Skill(Install), got {other:?}"),
    }
}

#[test]
fn skill_install_link_flag_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "install", "--path", "./my-skill", "--link"]).command {
        Command::Skill(SkillCommand::Install { path: _, link }) => {
            assert!(link);
        }
        other => panic!("expected Skill(Install), got {other:?}"),
    }
}

#[test]
fn skill_info_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "info", "my-skill"]).command {
        Command::Skill(SkillCommand::Info { id }) => {
            assert_eq!(id, "my-skill");
        }
        other => panic!("expected Skill(Info), got {other:?}"),
    }
}

#[test]
fn skill_run_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "run", "my-skill", "--input", "input.json"]).command {
        Command::Skill(SkillCommand::Run { id, input, stub }) => {
            assert_eq!(id, "my-skill");
            assert_eq!(input.as_str(), "input.json");
            assert!(!stub);
        }
        other => panic!("expected Skill(Run), got {other:?}"),
    }
}

#[test]
fn skill_run_stub_parses() {
    use super::{Command, SkillCommand};
    match parse(&[
        "skill",
        "run",
        "my-skill",
        "--input",
        "input.json",
        "--stub",
    ])
    .command
    {
        Command::Skill(SkillCommand::Run { stub, .. }) => {
            assert!(stub);
        }
        other => panic!("expected Skill(Run), got {other:?}"),
    }
}

#[test]
fn skill_import_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "import", "SKILL.md"]).command {
        Command::Skill(SkillCommand::Import { path }) => {
            assert_eq!(path.as_str(), "SKILL.md");
        }
        other => panic!("expected Skill(Import), got {other:?}"),
    }
}

#[test]
fn skill_validate_parses() {
    use super::{Command, SkillCommand};
    match parse(&["skill", "validate", "./bundle"]).command {
        Command::Skill(SkillCommand::Validate { path }) => {
            assert_eq!(path.as_str(), "./bundle");
        }
        other => panic!("expected Skill(Validate), got {other:?}"),
    }
}

// ── auth subcommands ──────────────────────────────────────────────────────────

#[test]
fn auth_login_parses() {
    use super::{AuthCommand, Command};
    match parse(&["auth", "login"]).command {
        Command::Auth(AuthCommand::Login { registry_url }) => {
            assert_eq!(registry_url, "https://app.vectorhawk.ai");
        }
        other => panic!("expected Auth(Login), got {other:?}"),
    }
}

#[test]
fn auth_login_with_registry_url_parses() {
    use super::{AuthCommand, Command};
    match parse(&["auth", "login", "--registry-url", "http://localhost:8000"]).command {
        Command::Auth(AuthCommand::Login { registry_url }) => {
            assert_eq!(registry_url, "http://localhost:8000");
        }
        other => panic!("expected Auth(Login), got {other:?}"),
    }
}

#[test]
fn auth_logout_parses() {
    use super::{AuthCommand, Command};
    match parse(&["auth", "logout"]).command {
        Command::Auth(AuthCommand::Logout { .. }) => {}
        other => panic!("expected Auth(Logout), got {other:?}"),
    }
}

#[test]
fn auth_status_parses() {
    use super::{AuthCommand, Command};
    match parse(&["auth", "status"]).command {
        Command::Auth(AuthCommand::Status { .. }) => {}
        other => panic!("expected Auth(Status), got {other:?}"),
    }
}

// ── mcp subcommands ───────────────────────────────────────────────────────────

#[test]
fn mcp_serve_parses() {
    use super::{Command, McpCommand};
    match parse(&["mcp", "serve"]).command {
        Command::Mcp(McpCommand::Serve) => {}
        other => panic!("expected Mcp(Serve), got {other:?}"),
    }
}

#[test]
fn mcp_setup_claude_code_parses() {
    use super::{Command, McpCommand};
    match parse(&["mcp", "setup", "--client", "claude-code"]).command {
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            assert_eq!(client.as_deref(), Some("claude-code"));
            assert!(!dry_run);
        }
        other => panic!("expected Mcp(Setup), got {other:?}"),
    }
}

#[test]
fn mcp_setup_dry_run_parses() {
    use super::{Command, McpCommand};
    match parse(&["mcp", "setup", "--client", "claude-code", "--dry-run"]).command {
        Command::Mcp(McpCommand::Setup { client, dry_run }) => {
            assert_eq!(client.as_deref(), Some("claude-code"));
            assert!(dry_run);
        }
        other => panic!("expected Mcp(Setup), got {other:?}"),
    }
}

#[test]
fn mcp_sync_parses() {
    use super::{Command, McpCommand};
    match parse(&["mcp", "sync"]).command {
        Command::Mcp(McpCommand::Sync) => {}
        other => panic!("expected Mcp(Sync), got {other:?}"),
    }
}

#[test]
fn mcp_backends_parses() {
    use super::{Command, McpCommand};
    match parse(&["mcp", "backends"]).command {
        Command::Mcp(McpCommand::Backends) => {}
        other => panic!("expected Mcp(Backends), got {other:?}"),
    }
}

// ── daemon subcommands ────────────────────────────────────────────────────────

#[test]
fn daemon_run_foreground_parses() {
    use super::{Command, DaemonCommand};
    match parse(&["daemon", "run", "--foreground"]).command {
        Command::Daemon(DaemonCommand::Run { foreground, .. }) => {
            assert!(foreground);
        }
        other => panic!("expected Daemon(Run), got {other:?}"),
    }
}

#[test]
fn daemon_run_with_registry_url_parses() {
    use super::{Command, DaemonCommand};
    match parse(&[
        "daemon",
        "run",
        "--foreground",
        "--registry-url",
        "http://localhost:8000",
    ])
    .command
    {
        Command::Daemon(DaemonCommand::Run {
            foreground,
            registry_url,
        }) => {
            assert!(foreground);
            assert_eq!(registry_url.as_deref(), Some("http://localhost:8000"));
        }
        other => panic!("expected Daemon(Run), got {other:?}"),
    }
}

#[test]
fn daemon_install_parses() {
    use super::{Command, DaemonCommand};
    match parse(&["daemon", "install"]).command {
        Command::Daemon(DaemonCommand::Install) => {}
        other => panic!("expected Daemon(Install), got {other:?}"),
    }
}

#[test]
fn daemon_uninstall_parses() {
    use super::{Command, DaemonCommand};
    match parse(&["daemon", "uninstall"]).command {
        Command::Daemon(DaemonCommand::Uninstall) => {}
        other => panic!("expected Daemon(Uninstall), got {other:?}"),
    }
}

#[test]
fn unknown_command_fails() {
    assert!(try_parse(&["notacommand"]).is_err());
}

// ── M3: doctor OAuth listener line ───────────────────────────────────────────

/// AC5 (M3): `vectorhawk doctor` must emit an `OAuth listener:` line.
///
/// When the daemon is not running the line reads "not running".  The test
// ── plugin subcommands ────────────────────────────────────────────────────────

#[test]
fn plugin_export_default_format_parses() {
    use super::{Command, PluginCommand};
    match parse(&["plugin", "export", "/some/plugin"]).command {
        Command::Plugin(PluginCommand::Export {
            path,
            format,
            output_dir,
        }) => {
            assert_eq!(path.as_str(), "/some/plugin");
            assert_eq!(format, "mcpb");
            assert!(output_dir.is_none());
        }
        other => panic!("expected Plugin::Export, got {other:?}"),
    }
}

#[test]
fn plugin_export_with_format_and_output_dir_parses() {
    use super::{Command, PluginCommand};
    match parse(&[
        "plugin",
        "export",
        "/some/plugin",
        "--format",
        "claude-code",
        "--output-dir",
        "./dist",
    ])
    .command
    {
        Command::Plugin(PluginCommand::Export {
            path,
            format,
            output_dir,
        }) => {
            assert_eq!(path.as_str(), "/some/plugin");
            assert_eq!(format, "claude-code");
            assert_eq!(output_dir.as_deref().map(|p| p.as_str()), Some("./dist"));
        }
        other => panic!("expected Plugin::Export, got {other:?}"),
    }
}

#[test]
fn plugin_import_parses() {
    use super::{Command, PluginCommand};
    match parse(&["plugin", "import", "/some/extension.mcpb"]).command {
        Command::Plugin(PluginCommand::Import { path, output_dir }) => {
            assert_eq!(path.as_str(), "/some/extension.mcpb");
            assert!(output_dir.is_none());
        }
        other => panic!("expected Plugin::Import, got {other:?}"),
    }
}

#[test]
fn plugin_import_with_output_dir_parses() {
    use super::{Command, PluginCommand};
    match parse(&[
        "plugin",
        "import",
        "/some/extension.mcpb",
        "--output-dir",
        "./out",
    ])
    .command
    {
        Command::Plugin(PluginCommand::Import { path, output_dir }) => {
            assert_eq!(path.as_str(), "/some/extension.mcpb");
            assert_eq!(output_dir.as_deref().map(|p| p.as_str()), Some("./out"));
        }
        other => panic!("expected Plugin::Import, got {other:?}"),
    }
}

// ── doctor output check ───────────────────────────────────────────────────────

/// exercises the output label — not the daemon state — so it does not require
/// a live daemon.
///
/// Marked `#[ignore]` because it requires the release binary.
#[test]
#[ignore = "requires pre-built release binary — run cargo build --workspace --release first"]
fn doctor_output_contains_oauth_listener_line() {
    use std::{path::PathBuf, process::Command};

    // Locate the release binary relative to CARGO_MANIFEST_DIR.
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo");
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .expect("cli crate should have a parent")
        .parent()
        .expect("crates/ should have a parent (workspace root)")
        .to_path_buf();
    let cli_bin = workspace_root
        .join("target")
        .join("release")
        .join("vectorhawk");

    assert!(
        cli_bin.exists(),
        "vectorhawk release binary not found at {cli_bin:?} — run cargo build --workspace --release"
    );

    let output = Command::new(&cli_bin)
        .args(["doctor"])
        .output()
        .expect("failed to spawn vectorhawk doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("OAuth listener:"),
        "doctor output must contain 'OAuth listener:' line; got:\n{stdout}"
    );
}
