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
    match parse(&["doctor"]).command {
        Command::Doctor { registry_url } => {
            assert!(registry_url.is_none());
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
