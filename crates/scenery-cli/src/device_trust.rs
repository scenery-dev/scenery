//! `scenery trust …` (design 2026-11-08 §2).
//!
//! Anything that grants trust (`trust pin`, `trust reset`) is human-only:
//! `trust reset` needs a controlling terminal (`/dev/tty`) and there is no
//! `--yes`. The daemon and the MCP server have no way to call these, so prompt
//! injection into an AI client cannot change which keys are trusted.
//! (`scenery approve` and friends were removed in 2.2.1: a verified stdio
//! definition runs without a device prompt.)

use anyhow::{Context, Result};
use clap::Subcommand;
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
