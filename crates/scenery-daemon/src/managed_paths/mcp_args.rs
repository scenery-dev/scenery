//! Argument redaction for MCP server discovery (plan 4.2.1).
//!
//! `args` leave the machine only after the backend has said
//! `policy.auto_submit_mcp = true`, and only after passing through
//! [`redact_args`]. The rules are SHAPE-based and OVER-redact: a denylist can
//! only be as complete as the patterns someone wrote down, so nothing is kept
//! on the strength of "it did not look secret".
//!
//! Kept verbatim:
//! - flags with no value, and `--flag=word` / `--flag word` for the small
//!   benign set ([`BENIGN_FLAGS`]) when the value is itself safe;
//! - package or image identifiers that pass the per-launcher grammar
//!   (`mcp_discovery::validate_identifier`), version pins included;
//! - a handful of structural words (`run`, `dlx`, `exec`, `serve`...), bare
//!   port numbers, and paths (with `$HOME` masked to `~`).
//!
//! Redacted:
//! - the value after any other flag (`--token opaque`, `-p pw`);
//! - `-pSECRET` style attached values, `--flag=value` for any other flag;
//! - any `KEY=VALUE` (`KEY=<redacted>`);
//! - any URL (reduced to its origin, never userinfo, path or query);
//! - anything credential-shaped ([`scenery_core::skill_tree::looks_like_secret_token`])
//!   or high-entropy;
//! - any single argument containing whitespace (`Authorization: Bearer x`,
//!   `Authorization: Bearer x`), and EVERYTHING after `-c`, `-e` or `--eval`;
//! - every other bare positional, including a SECOND identifier-shaped one
//!   (`npx -y pkg hunter2` keeps `pkg` only);
//! - a long flag whose name is not a plain word (`--token:abc`, `--ghp_...`).
//!
//! A command string's leading `KEY=VALUE` words are assignments
//! (`TOKEN=x node srv` -> `TOKEN=<redacted>`, executable `node`).
//!
//! `-p` is deliberately NOT benign: `mysql -pSECRET`, `sshpass -p pw`.
//!
//! The backend applies the same rules again (shared golden vectors in
//! `mcp_args_golden.json`).

use scenery_core::skill_tree::{looks_like_secret_token, text_has_secret};

pub const REDACTED: &str = "<redacted>";

/// Flags that carry no secret on their own and whose (safe) value may stay.
/// `-p` is NOT here.
const BENIGN_FLAGS: &[&str] = &["-y", "--yes", "--stdio", "--port", "--transport", "--from"];

/// Valueless docker/launcher switches that are safe to show. Not value-taking.
const BENIGN_BOOLEANS: &[&str] = &["-i", "-t", "-it", "-ti", "--rm", "--quiet", "-q"];

/// After one of these, everything is a script or a secret-bearing string.
const EVAL_FLAGS: &[&str] = &["-c", "-e", "--eval"];

const SAFE_WORDS: &[&str] = &[
    "run", "dlx", "exec", "x", "serve", "start", "mcp", "stdio", "server", "--",
];

fn is_flag(a: &str) -> bool {
    a.len() > 1 && a.starts_with('-') && !is_number(a)
}

fn is_number(a: &str) -> bool {
    a.strip_prefix('-')
        .unwrap_or(a)
        .bytes()
        .all(|b| b.is_ascii_digit())
}

fn is_port(a: &str) -> bool {
    !a.is_empty() && a.len() <= 5 && a.bytes().all(|b| b.is_ascii_digit())
}

/// Reduce a URL (optionally `git+https://...`) to `scheme://host[:port]`.
fn url_origin(a: &str) -> Option<String> {
    let (prefix, rest) = match a.split_once('+') {
        Some((p, r)) if r.contains("://") && p.bytes().all(|b| b.is_ascii_alphabetic()) => {
            (format!("{p}+"), r)
        }
        _ => (String::new(), a),
    };
    let origin = super::mcp_discovery::safe_url_origin(rest)?;
    Some(format!("{prefix}{origin}"))
}

/// Shannon entropy in bits per character.
fn entropy(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn high_entropy(a: &str) -> bool {
    a.len() >= 20 && entropy(a) >= 4.0
}

fn looks_like_path(a: &str) -> bool {
    a.starts_with('/')
        || a.starts_with("./")
        || a.starts_with("../")
        || a.starts_with("~/")
        || a == "~"
}

fn mask_home(a: &str) -> String {
    super::mcp_discovery::mask_home_dir(a)
}

fn secret_shaped(a: &str) -> bool {
    text_has_secret(a) || looks_like_secret_token(a)
}

/// A long flag is kept only when it is a plain `--[A-Za-z0-9][A-Za-z0-9_-]*`
/// word (<= 64 chars after `--`) that is neither secret- nor high-entropy
/// shaped. `--token:abc` and `--ghp_...` carry data in the name itself.
/// Short flags are always fine here (attached values are handled apart).
fn flag_name_ok(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("--") else {
        return true;
    };
    let mut bytes = rest.bytes();
    let plain = rest.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    plain && !secret_shaped(rest) && !high_entropy(rest)
}

/// A bare positional or a benign flag's value: keep it only if it is provably
/// harmless, else `<redacted>`.
///
/// Only the FIRST identifier-shaped positional is kept (`ident_kept`):
/// `npx -y pkg hunter2` keeps `pkg` and redacts `hunter2`. A benign flag's
/// value (`--from pkg`) never uses the slot up.
fn keep_or_redact_value(
    command_basename: &str,
    a: &str,
    ident_kept: &mut bool,
    consume_identifier: bool,
) -> String {
    if a.chars().any(char::is_whitespace) || secret_shaped(a) {
        return REDACTED.to_string();
    }
    if a.contains("://") {
        return url_origin(a).unwrap_or_else(|| REDACTED.to_string());
    }
    if SAFE_WORDS.contains(&a) || is_port(a) {
        return a.to_string();
    }
    if !*ident_kept && super::mcp_discovery::validate_identifier(a, command_basename).is_some() {
        if consume_identifier {
            *ident_kept = true;
        }
        return a.to_string();
    }
    if looks_like_path(a) && !high_entropy(a) {
        return mask_home(a);
    }
    REDACTED.to_string()
}

/// Redact one launcher's `args` (see the module docs). The result has one
/// element per input element, so the shape of the command line stays legible
/// to the reviewer.
pub fn redact_args(command_basename: &str, args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut ident_kept = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();

        // Whitespace-bearing arguments are shell strings or headers.
        if a.chars().any(char::is_whitespace) {
            out.push(REDACTED.to_string());
            i += 1;
            continue;
        }

        // A bare URL: origin only. Checked before `KEY=VALUE` because a query
        // string is full of `=`.
        if !is_flag(a) && a.contains("://") {
            out.push(url_origin(a).unwrap_or_else(|| REDACTED.to_string()));
            i += 1;
            continue;
        }

        if EVAL_FLAGS.contains(&a) {
            out.push(a.to_string());
            out.extend(args[i + 1..].iter().map(|_| REDACTED.to_string()));
            break;
        }

        if is_flag(a) {
            if BENIGN_BOOLEANS.contains(&a) || (BENIGN_FLAGS.contains(&a) && !takes_value(a)) {
                out.push(a.to_string());
                i += 1;
                continue;
            }
            if let Some((flag, value)) = a.split_once('=') {
                // An attached single-dash value (`-pMy=Pass`, `-pc2Vj..==`) is
                // checked BEFORE the split, which would otherwise leak the part
                // before `=`: the whole token is the secret. A bare `-p=x`
                // keeps only its name. A long flag whose name is not a plain
                // word (`--api-key:abc=xyz`) is redacted whole.
                if (!flag.starts_with("--") && flag.len() > 2) || !flag_name_ok(flag) {
                    out.push(REDACTED.to_string());
                } else if BENIGN_FLAGS.contains(&flag) {
                    out.push(format!(
                        "{flag}={}",
                        keep_or_redact_value(command_basename, value, &mut ident_kept, false)
                    ));
                } else {
                    out.push(format!("{flag}={REDACTED}"));
                }
                i += 1;
                continue;
            }
            if BENIGN_BOOLEANS.contains(&a) || (BENIGN_FLAGS.contains(&a) && !takes_value(a)) {
                out.push(a.to_string());
                i += 1;
                continue;
            }
            if BENIGN_FLAGS.contains(&a) {
                out.push(a.to_string());
                if let Some(v) = args.get(i + 1).filter(|v| !is_flag(v)) {
                    out.push(keep_or_redact_value(
                        command_basename,
                        v,
                        &mut ident_kept,
                        false,
                    ));
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            // Single-dash flag with an attached value (`-pSECRET`): all secret.
            if !a.starts_with("--") && a.len() > 2 {
                out.push(REDACTED.to_string());
                i += 1;
                continue;
            }
            // Any other flag: keep the name (if it is a plain word), redact a
            // following value.
            out.push(if flag_name_ok(a) {
                a.to_string()
            } else {
                REDACTED.to_string()
            });
            if args.get(i + 1).is_some_and(|v| !is_flag(v)) {
                out.push(REDACTED.to_string());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // `KEY=VALUE`
        if let Some((k, _)) = a.split_once('=') {
            if !k.is_empty() {
                out.push(format!("{k}={REDACTED}"));
                i += 1;
                continue;
            }
        }

        out.push(keep_or_redact_value(
            command_basename,
            a,
            &mut ident_kept,
            true,
        ));
        i += 1;
    }
    out
}

/// Flags in the benign set that take a value (the rest are switches).
fn takes_value(flag: &str) -> bool {
    matches!(flag, "--port" | "--transport" | "--from")
}

/// A leading `KEY=VALUE` word (an environment assignment, not an executable).
fn is_env_assignment(w: &str) -> bool {
    let Some((k, _)) = w.split_once('=') else {
        return false;
    };
    let mut b = k.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Split a `command` string that carries its own arguments (`npx -y pkg
/// --api-key sk-...`, or Codex's `TOKEN=x node srv`) into `(executable, extra
/// args)`. Leading `KEY=VALUE` words are assignments, not the executable: they
/// become the first extra args (so they come out `KEY=<redacted>`). A command
/// with no whitespace is returned unchanged with no extras. The executable is
/// empty when nothing usable is left (only assignments, or a name that looks
/// like a credential); the caller then has no launcher context.
pub fn split_command(command: &str) -> (String, Vec<String>) {
    let mut words = command.split_whitespace().peekable();
    let mut extras: Vec<String> = Vec::new();
    while let Some(w) = words.peek() {
        if !is_env_assignment(w) {
            break;
        }
        extras.push((*w).to_string());
        words.next();
    }
    let Some(first) = words.next() else {
        if extras.is_empty() {
            return (command.to_string(), Vec::new());
        }
        return (String::new(), extras);
    };
    let rest: Vec<String> = words.map(str::to_string).collect();
    if rest.is_empty() && extras.is_empty() {
        return (command.trim().to_string(), Vec::new());
    }
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
    extras.extend(rest);
    if secret_shaped(base) {
        return (String::new(), extras);
    }
    (first.to_string(), extras)
}

/// Public package a launcher invocation names, split for the registry fetch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PackageRef {
    /// `npm` | `pypi` | `docker`
    pub ecosystem: String,
    pub name: String,
    /// Pinned version, tag or digest when the identifier carries one.
    pub version: Option<String>,
}

/// Split a validated `package_identifier` for `command_basename`.
pub fn package_of(command_basename: &str, identifier: &str) -> Option<PackageRef> {
    match command_basename {
        "npx" | "bunx" | "pnpm" => {
            let at = identifier.rfind('@').filter(|&i| i > 0);
            let (name, version) = match at {
                Some(i) => (&identifier[..i], Some(identifier[i + 1..].to_string())),
                None => (identifier, None),
            };
            Some(PackageRef {
                ecosystem: "npm".into(),
                name: name.into(),
                version,
            })
        }
        "uvx" | "pipx" => {
            let (name, version) = match identifier.split_once("==") {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (identifier, None),
            };
            Some(PackageRef {
                ecosystem: "pypi".into(),
                name: name.into(),
                version,
            })
        }
        "docker" => {
            let (base, digest) = match identifier.split_once('@') {
                Some((b, d)) => (b, Some(d.to_string())),
                None => (identifier, None),
            };
            let last_slash = base.rfind('/').map_or(0, |i| i + 1);
            let (name, tag) = match base[last_slash..].rfind(':') {
                Some(i) => (
                    &base[..last_slash + i],
                    Some(base[last_slash + i + 1..].to_string()),
                ),
                None => (base, None),
            };
            Some(PackageRef {
                ecosystem: "docker".into(),
                name: name.into(),
                version: digest.or(tag),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "mcp_args_tests.rs"]
mod tests;
