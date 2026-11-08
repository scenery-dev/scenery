//! `scenery trust …`, `scenery pending`, `scenery approve`, `scenery deny`,
//! `scenery approvals …` (design 2026-11-08 §2, §3).
//!
//! Anything that grants trust (`trust pin`, `trust reset`, `approve`,
//! `approvals revoke`) is human-only: it needs a controlling terminal
//! (`/dev/tty`) and there is no `--yes`. The daemon and the MCP server have no
//! way to call these, so prompt injection into an AI client cannot approve a
//! command.

use anyhow::{Context, Result};
use clap::Subcommand;
use scenery_core::approvals::{self, PendingServer};
use scenery_core::state::AppState;
use scenery_core::trust;
use std::io::{BufRead, Write};

#[derive(Debug, Subcommand)]
pub enum TrustCommand {
    /// Pin a registry's signing key (needed for origins other than Scenery's
    /// own). Compares the key the registry serves with the fingerprint you
    /// obtained out of band.
    Pin {
        /// Registry origin, for example https://registry.example.com
        #[arg(value_parser = crate::parse_registry_url)]
        origin: String,
        /// Hex SHA-256 of the registry's raw 32-byte Ed25519 public key.
        #[arg(long)]
        fingerprint: String,
    },
    /// Remove pinned keys for an origin (asks for confirmation on the terminal).
    Reset {
        #[arg(value_parser = crate::parse_registry_url)]
        origin: String,
    },
    /// List the keys this runner trusts, with fingerprints.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ApprovalsCommand {
    /// List approved stdio commands.
    List,
    /// Withdraw an approval (asks for confirmation on the terminal). The
    /// server is torn down and goes back to waiting for approval.
    Revoke {
        /// Approval hash or a unique prefix of it.
        hash: String,
    },
}

// ── terminal confirmation ─────────────────────────────────────────────────────

/// The user's controlling terminal.
pub(crate) struct Tty {
    reader: std::io::BufReader<std::fs::File>,
    writer: std::fs::File,
}

/// Open `/dev/tty`. Fails when the process has no controlling terminal, which
/// is exactly the case for an AI-client subprocess or a daemon.
pub(crate) fn open_tty() -> Result<Tty> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context(
            "this command needs a controlling terminal (it cannot be run from a script, \
             a daemon or an AI client). Run it in a terminal window.",
        )?;
    let writer = file.try_clone().context("failed to open the terminal")?;
    Ok(Tty {
        reader: std::io::BufReader::new(file),
        writer,
    })
}

/// Print `detail`, ask `prompt [y/N]`, read one line. Anything but y/yes is no.
pub(crate) fn confirm_with<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    detail: &str,
    prompt: &str,
) -> Result<bool> {
    write!(writer, "{detail}\n{prompt} [y/N] ").context("failed to write to the terminal")?;
    writer.flush().context("failed to flush the terminal")?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("failed to read from the terminal")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn confirm_on_tty(detail: &str, prompt: &str) -> Result<bool> {
    let mut tty = open_tty()?;
    confirm_with(&mut tty.reader, &mut tty.writer, detail, prompt)
}

// ── pending selection ─────────────────────────────────────────────────────────

/// Pick one pending server by exact name (case-insensitive) or hash prefix.
pub(crate) fn select_pending<'a>(
    pending: &'a [PendingServer],
    needle: &str,
) -> Result<&'a PendingServer> {
    let n = needle.trim();
    anyhow::ensure!(!n.is_empty(), "name or hash prefix required");
    let by_name: Vec<&PendingServer> = pending
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(n))
        .collect();
    let candidates = if by_name.is_empty() {
        let lower = n.to_ascii_lowercase();
        let by_hash: Vec<&PendingServer> = pending
            .iter()
            .filter(|p| lower.len() >= 4 && p.approval_hash.starts_with(&lower))
            .collect();
        by_hash
    } else {
        by_name
    };
    match candidates.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!("no pending server matches '{n}'. Run `scenery pending`."),
        many => anyhow::bail!(
            "'{n}' matches {} pending servers; use a longer hash prefix",
            many.len()
        ),
    }
}

fn describe(p: &PendingServer) -> String {
    let args = if p.args.is_empty() {
        "(none)".to_string()
    } else {
        p.args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let env = if p.env_keys.is_empty() {
        "(none)".to_string()
    } else {
        p.env_keys.join(", ")
    };
    format!(
        "Server   : {}\nCommand  : {}\nArguments: {}\nEnv keys : {}\nSigned by: {}\nHash     : {}",
        p.name,
        p.command,
        args,
        env,
        p.signer_kid.as_deref().unwrap_or("(unknown)"),
        p.approval_hash,
    )
}

// ── commands ──────────────────────────────────────────────────────────────────

pub(crate) fn cmd_pending() -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    let pending = approvals::list_pending(&state)?;
    if pending.is_empty() {
        println!("No servers are waiting for approval.");
        return Ok(());
    }
    println!("{} server(s) waiting for approval:\n", pending.len());
    for p in &pending {
        println!(
            "  {name}\n    command : {cmd} {args}\n    env keys: {env}\n    signer  : {kid}\n    hash    : {hash}\n",
            name = p.name,
            cmd = p.command,
            args = p.args.join(" "),
            env = if p.env_keys.is_empty() { "(none)".to_string() } else { p.env_keys.join(", ") },
            kid = p.signer_kid.as_deref().unwrap_or("(unknown)"),
            hash = approvals::short_hash(&p.approval_hash),
        );
    }
    println!("Review one and approve it with: scenery approve <name>");
    Ok(())
}

pub(crate) async fn cmd_approve(target: &str) -> Result<()> {
    // Terminal first: a caller without one never gets as far as reading state.
    let mut tty = open_tty()?;
    let state = AppState::bootstrap().context("failed to open state")?;
    let pending = approvals::list_pending(&state)?;
    let p = select_pending(&pending, target)?.clone();
    let yes = confirm_with(
        &mut tty.reader,
        &mut tty.writer,
        &describe(&p),
        "Run this command on this machine?",
    )?;
    if !yes {
        println!("Not approved. Nothing changed.");
        return Ok(());
    }
    let def =
        approvals::StdioDefinition::new(p.command.clone(), p.args.clone(), p.env_keys.clone());
    anyhow::ensure!(
        def.approval_hash() == p.approval_hash,
        "stored definition does not match its approval hash; refusing"
    );
    approvals::insert_approval(&state, &def, &p.name)?;
    println!(
        "Approved {} ({}).",
        p.name,
        approvals::short_hash(&p.approval_hash)
    );
    match crate::notify_daemon_approvals_changed(state.socket_path().as_str()).await {
        Some(n) => println!("Daemon updated ({n} server(s) changed)."),
        None => println!("The daemon is not running; the server starts with it."),
    }
    Ok(())
}

pub(crate) async fn cmd_deny(target: &str, registry_url: &str) -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    let pending = approvals::list_pending(&state)?;
    let p = select_pending(&pending, target)?.clone();
    state
        .delete_mcp_install(&p.mcp_server_id)
        .context("failed to drop the pending entry")?;
    println!("Denied {}. It was removed from this device.", p.name);
    if let Err(e) = crate::patch_mcp_installation(
        &state,
        registry_url,
        &p.installation_id,
        "error",
        Some("denied by user"),
    )
    .await
    {
        eprintln!("warning: could not tell the registry: {e:#}");
    }
    let _ = crate::notify_daemon_approvals_changed(state.socket_path().as_str()).await;
    Ok(())
}

pub(crate) fn cmd_approvals_list() -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    let list = approvals::list_approvals(&state)?;
    if list.is_empty() {
        println!("No approved commands.");
        return Ok(());
    }
    for a in &list {
        println!(
            "{hash}  {cmd} {args}  [env keys: {env}]  first for '{name}' on {when}",
            hash = approvals::short_hash(&a.approval_hash),
            cmd = a.command,
            args = a.args.join(" "),
            env = if a.env_keys.is_empty() {
                "-".to_string()
            } else {
                a.env_keys.join(",")
            },
            name = a.first_server_name,
            when = a.approved_at,
        );
    }
    Ok(())
}

pub(crate) async fn cmd_approvals_revoke(prefix: &str) -> Result<()> {
    let mut tty = open_tty()?;
    let state = AppState::bootstrap().context("failed to open state")?;
    let list = approvals::list_approvals(&state)?;
    let lower = prefix.trim().to_ascii_lowercase();
    anyhow::ensure!(lower.len() >= 4, "give at least 4 characters of the hash");
    let hits: Vec<_> = list
        .iter()
        .filter(|a| a.approval_hash.starts_with(&lower))
        .collect();
    let a = match hits.as_slice() {
        [one] => *one,
        [] => anyhow::bail!("no approval matches '{prefix}'. Run `scenery approvals list`."),
        _ => anyhow::bail!("'{prefix}' matches several approvals; use a longer prefix"),
    };
    let detail = format!(
        "Command  : {} {}\nHash     : {}",
        a.command,
        a.args.join(" "),
        a.approval_hash
    );
    if !confirm_with(
        &mut tty.reader,
        &mut tty.writer,
        &detail,
        "Withdraw this approval? Servers using it stop and wait for approval again.",
    )? {
        println!("Kept. Nothing changed.");
        return Ok(());
    }
    approvals::delete_approval(&state, &a.approval_hash)?;
    println!("Approval withdrawn.");
    match crate::notify_daemon_approvals_changed(state.socket_path().as_str()).await {
        Some(n) => println!("Daemon updated ({n} server(s) changed)."),
        None => println!("The daemon is not running; servers hold at next start."),
    }
    Ok(())
}

// ── trust ─────────────────────────────────────────────────────────────────────

pub(crate) async fn cmd_trust(cmd: TrustCommand) -> Result<()> {
    match cmd {
        TrustCommand::Pin {
            origin,
            fingerprint,
        } => cmd_trust_pin(&origin, &fingerprint).await,
        TrustCommand::Reset { origin } => cmd_trust_reset(&origin),
        TrustCommand::List => cmd_trust_list(),
    }
}

async fn cmd_trust_pin(origin: &str, fingerprint: &str) -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    let url = format!("{origin}/api/runner/signing-keys");
    let resp = scenery_core::http::async_client()
        .context("failed to build HTTP client")?
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("failed to reach {url}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "{url} returned HTTP {}",
        resp.status()
    );
    let body: trust::SigningKeysResponse = resp
        .json()
        .await
        .context("unexpected signing-keys response")?;
    let (kid, key) = trust::select_key_by_fingerprint(&body, origin, fingerprint)?;
    trust::pin_key(&state, origin, &kid, &key)?;
    println!(
        "Pinned {kid} for {origin} (fingerprint {}).",
        trust::fingerprint(&key)
    );
    Ok(())
}

fn cmd_trust_reset(origin: &str) -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    let detail = format!("Pinned keys for {origin} will be removed.");
    if !confirm_on_tty(&detail, "Remove them?")? {
        println!("Kept. Nothing changed.");
        return Ok(());
    }
    let n = trust::reset_origin(&state, origin)?;
    println!("Removed {n} pinned key(s) for {origin}.");
    Ok(())
}

fn cmd_trust_list() -> Result<()> {
    let state = AppState::bootstrap().context("failed to open state")?;
    for (origin, keys) in trust::TRUST_ROOTS {
        for (kid, key) in *keys {
            let note = if *key == trust::PLACEHOLDER_KEY {
                "  (placeholder: not yet provisioned)".to_string()
            } else {
                format!("  {}", trust::fingerprint(key))
            };
            println!("built-in  {origin}  {kid}{note}");
        }
    }
    for k in all_pinned(&state)? {
        println!("pinned    {}  {}  {}", k.0, k.1, trust::fingerprint(&k.2));
    }
    Ok(())
}

fn all_pinned(state: &AppState) -> Result<Vec<(String, String, [u8; 32])>> {
    let conn = rusqlite::Connection::open(&state.db_path).context("failed to open state DB")?;
    let mut stmt = conn
        .prepare("SELECT origin, kid, pubkey FROM trusted_keys ORDER BY origin, kid")
        .context("failed to read trusted_keys")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .context("failed to read trusted_keys")?;
    let mut out = Vec::new();
    for row in rows {
        let (o, k, raw) = row.context("bad trusted_keys row")?;
        if let Ok(arr) = <[u8; 32]>::try_from(raw.as_slice()) {
            out.push((o, k, arr));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "device_trust_tests.rs"]
mod tests;
