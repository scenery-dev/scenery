//! Stdio MCP definitions pushed by the backend: the env-key denylist and the
//! definition hash. There is no device-local approval prompt (removed in
//! 2.2.1, design 2026-11-08 "Decision change"): a verified, signed definition
//! runs on install. The denylist still refuses dangerous env keys.

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

/// The identity of a stdio definition: command, args and the *names* of env
/// vars. Env values are excluded so rotating a token does not change it.
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

    /// Human-readable command line.
    pub fn display(&self) -> String {
        let mut s = self.command.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
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
