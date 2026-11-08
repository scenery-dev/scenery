//! Device-local approval of backend-pushed stdio MCP definitions (design
//! 2026-11-08 §3). A signed definition is necessary but not sufficient to run
//! a command on this machine: the user must approve its exact
//! `(command, args, env keys)` with `scenery approve` on a TTY.
//!
//! This module is pure data plus SQLite; the launch gate lives in the daemon.

use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Env keys a backend-supplied definition may never set.
/// Matched case-insensitively.
pub fn env_key_denied(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.starts_with("LD_")
        || k.starts_with("DYLD_")
        || k.starts_with("PYTHON")
        || k.starts_with("PERL5")
        || matches!(
            k.as_str(),
            "PATH"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "BASH_ENV"
                | "ENV"
                | "RUBYOPT"
                | "RUBYLIB"
                | "JAVA_TOOL_OPTIONS"
        )
}

/// First denied key among `env_keys`, if any.
pub fn first_denied_env_key<'a, I: IntoIterator<Item = &'a String>>(keys: I) -> Option<String> {
    keys.into_iter().find(|k| env_key_denied(k)).cloned()
}

/// The exact thing a user approves: command, args and the *names* of env vars.
/// Env values are excluded so rotating a token never re-prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioDefinition {
    pub command: String,
    pub args: Vec<String>,
    /// Sorted, deduplicated.
    pub env_keys: Vec<String>,
}

impl StdioDefinition {
    pub fn new(command: String, args: Vec<String>, env_keys: Vec<String>) -> Self {
        let mut env_keys = env_keys;
        env_keys.sort();
        env_keys.dedup();
        Self {
            command,
            args,
            env_keys,
        }
    }

    /// `sha256(serde_json::to_vec(&json!({"v":1,"command":c,"args":[..],"env_keys":[sorted]})))`.
    /// `json!` objects serialize in key order (no `preserve_order`), and a
    /// golden-value test pins this.
    pub fn approval_hash(&self) -> String {
        let v = serde_json::json!({
            "v": 1,
            "command": self.command,
            "args": self.args,
            "env_keys": self.env_keys,
        });
        let bytes = serde_json::to_vec(&v).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    /// Human-readable command line for notifications and the portal.
    pub fn display(&self) -> String {
        let mut s = self.command.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

/// Short form of a hash for lists.
pub fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub approval_hash: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub first_server_name: String,
    pub approved_at: String,
}

/// A stdio server waiting for local approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingServer {
    pub mcp_server_id: String,
    pub installation_id: String,
    pub name: String,
    pub approval_hash: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    /// Signer key id from the stored envelope, when known.
    pub signer_kid: Option<String>,
}

fn open(state: &AppState) -> Result<Connection> {
    Connection::open(&state.db_path).context("failed to open state DB")
}

pub fn is_approved(state: &AppState, hash: &str) -> Result<bool> {
    let conn = open(state)?;
    let n: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM stdio_approvals WHERE approval_hash = ?1",
            [hash],
            |r| r.get(0),
        )
        .optional()
        .context("failed to query stdio_approvals")?;
    Ok(n.is_some())
}

/// Record an approval. Called only by `scenery approve` after a TTY confirm.
pub fn insert_approval(state: &AppState, def: &StdioDefinition, server_name: &str) -> Result<()> {
    let conn = open(state)?;
    conn.execute(
        "INSERT OR IGNORE INTO stdio_approvals
         (approval_hash, command, args_json, env_keys_json, first_server_name, approved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            def.approval_hash(),
            def.command,
            serde_json::to_string(&def.args).context("encode args")?,
            serde_json::to_string(&def.env_keys).context("encode env keys")?,
            server_name,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .context("failed to insert approval")?;
    Ok(())
}

pub fn delete_approval(state: &AppState, hash: &str) -> Result<bool> {
    let conn = open(state)?;
    let n = conn
        .execute(
            "DELETE FROM stdio_approvals WHERE approval_hash = ?1",
            [hash],
        )
        .context("failed to delete approval")?;
    Ok(n > 0)
}

pub fn list_approvals(state: &AppState) -> Result<Vec<Approval>> {
    let conn = open(state)?;
    let mut stmt = conn
        .prepare(
            "SELECT approval_hash, command, args_json, env_keys_json, first_server_name, approved_at
             FROM stdio_approvals ORDER BY approved_at",
        )
        .context("failed to prepare approvals query")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .context("failed to read approvals")?;
    let mut out = Vec::new();
    for row in rows {
        let (approval_hash, command, a, e, first_server_name, approved_at) =
            row.context("bad approvals row")?;
        out.push(Approval {
            approval_hash,
            command,
            args: serde_json::from_str(&a).unwrap_or_default(),
            env_keys: serde_json::from_str(&e).unwrap_or_default(),
            first_server_name,
            approved_at,
        });
    }
    Ok(out)
}

/// Servers whose `approval_state` is `pending`.
pub fn list_pending(state: &AppState) -> Result<Vec<PendingServer>> {
    let conn = open(state)?;
    let mut stmt = conn
        .prepare(
            "SELECT mcp_server_id, installation_id, mcp_server_name, approval_hash,
                    server_config, signed_envelope
             FROM mcp_installations WHERE approval_state = 'pending' ORDER BY mcp_server_name",
        )
        .context("failed to prepare pending query")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .context("failed to read pending servers")?;
    let mut out = Vec::new();
    for row in rows {
        let (mcp_server_id, installation_id, name, hash, cfg, env) =
            row.context("bad pending row")?;
        let Some(approval_hash) = hash else { continue };
        let def = cfg
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .and_then(|v| definition_from_config(&v));
        let Some(def) = def else { continue };
        let signer_kid = env
            .as_deref()
            .and_then(|e| serde_json::from_str::<crate::signed::Envelope>(e).ok())
            .map(|e| e.kid);
        out.push(PendingServer {
            mcp_server_id,
            installation_id,
            name,
            approval_hash,
            command: def.command,
            args: def.args,
            env_keys: def.env_keys,
            signer_kid,
        });
    }
    Ok(out)
}

/// Extract the stdio definition from a `server_config` JSON value. Returns
/// `None` when the config is not a stdio definition (no `command`).
pub fn definition_from_config(cfg: &serde_json::Value) -> Option<StdioDefinition> {
    let command = cfg.get("command")?.as_str()?.to_string();
    let args = cfg
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let env_keys = cfg
        .get("env")
        .and_then(|e| e.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    Some(StdioDefinition::new(command, args, env_keys))
}

#[cfg(test)]
#[path = "approvals_tests.rs"]
mod tests;
