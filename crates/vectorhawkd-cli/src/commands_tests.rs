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

#[test]
fn doctor_parses() {
    use super::Command;
    match parse(&["doctor"]).command {
        Command::Doctor => {}
        other => panic!("expected Doctor, got {other:?}"),
    }
}

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
fn daemon_run_foreground_parses() {
    use super::{Command, DaemonCommand};
    match parse(&["daemon", "run", "--foreground"]).command {
        Command::Daemon(DaemonCommand::Run { foreground }) => {
            assert!(foreground);
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
