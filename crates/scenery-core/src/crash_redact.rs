//! Redaction for crash text (messages, locations, backtraces).
//!
//! Crash text can echo anything a failing code path had in hand: URLs with
//! credentials, headers, JSON bodies, file paths with the user's name. The
//! rules are deliberately aggressive: a false positive costs a few characters
//! of debugging detail, a false negative leaks a secret to the backend.
//!
//! Pure functions, no I/O.

const DELIMS: &[char] = &[
    '=', ':', '"', '\'', ',', ';', '(', ')', '[', ']', '{', '}', '&', '?', '<', '>', '|', '`',
];
const PREFIXES: &[&str] = &[
    "scn_pat_",
    "scn_",
    "eyJ",
    "sk-",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xox",
    "AKIA",
    "ASIA",
];
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "authorization",
    "credential",
    "cookie",
    "bearer",
    "basic",
    "key",
];
pub const REDACTED: &str = "[REDACTED]";

fn is_delim(c: char) -> bool {
    c.is_whitespace() || DELIMS.contains(&c)
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=' | '.')
}

/// A long opaque run: base64, hex, JWT-like. Requires a digit and a letter so
/// long identifiers and plain paths survive.
fn looks_opaque(s: &str) -> bool {
    let long = |p: &str| {
        p.len() >= 24
            && p.chars().all(is_token_char)
            && p.chars().any(|c| c.is_ascii_digit())
            && p.chars().any(|c| c.is_ascii_alphabetic())
    };
    long(s) || s.split('/').any(long)
}

fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        }
        None => false,
    }
}

fn has_secret_prefix(s: &str) -> bool {
    PREFIXES.iter().any(|p| s.starts_with(p))
}

fn is_secret_segment(seg: &str) -> bool {
    has_secret_prefix(seg) || looks_opaque(seg) || is_email(seg)
}

fn is_secret_key(seg: &str) -> bool {
    let lower = seg.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|k| lower.contains(k))
}

/// Replace `/Users/<x>`, `/home/<x>`, `C:\Users\<x>` (any drive, either
/// slash) with `~`, keeping the rest of the path.
fn collapse_user_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let hit = ["/Users/", "/home/", "\\Users\\", "/users/"]
            .iter()
            .filter_map(|m| rest.find(m).map(|i| (i, m.len())))
            .min();
        let Some((idx, mlen)) = hit else {
            out.push_str(rest);
            break;
        };
        let (before, after) = rest.split_at(idx);
        // Drop a Windows drive prefix ("C:") that precedes the match.
        let before = match before.as_bytes() {
            [.., d, b':'] if d.is_ascii_alphabetic() => &before[..before.len() - 2],
            _ => before,
        };
        out.push_str(before);
        out.push('~');
        let tail = &after[mlen..];
        let end = tail
            .find(|c: char| matches!(c, '/' | '\\') || c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out
}

/// Strip URL userinfo and query strings inside one whitespace-free word.
fn scrub_url(word: &str) -> String {
    let Some(scheme_end) = word.find("://") else {
        return word.to_string();
    };
    let (scheme, rest) = word.split_at(scheme_end + 3);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rfind('@') {
        Some(i) => format!("{REDACTED}@{}", &authority[i + 1..]),
        None => authority.to_string(),
    };
    let tail = match tail.find('?') {
        Some(i) => format!("{}?{REDACTED}", &tail[..i]),
        None => tail.to_string(),
    };
    format!("{scheme}{authority}{tail}")
}

fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() || c == '"' || c == '\'' {
            out.push_str(&scrub_url(&word));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&scrub_url(&word));
    out
}

fn mask_segments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut seg = String::new();
    let mut mask_next = false;
    let flush = |seg: &mut String, out: &mut String, mask_next: &mut bool| {
        if seg.is_empty() {
            return;
        }
        let scheme = seg.eq_ignore_ascii_case("bearer") || seg.eq_ignore_ascii_case("basic");
        if seg.as_str() == REDACTED || (*mask_next && scheme) {
            // An auth scheme word keeps the pending mask for the credential.
            out.push_str(seg);
        } else if *mask_next || is_secret_segment(seg) {
            out.push_str(REDACTED);
            *mask_next = false;
        } else {
            out.push_str(seg);
            *mask_next = is_secret_key(seg);
        }
        seg.clear();
    };
    for c in text.chars() {
        if is_delim(c) {
            flush(&mut seg, &mut out, &mut mask_next);
            out.push(c);
        } else {
            seg.push(c);
        }
    }
    flush(&mut seg, &mut out, &mut mask_next);
    out
}

/// Rewrite user paths and mask secrets. `home` and `user` are the current
/// process's values (pass `None` in tests that do not care).
pub fn redact(text: &str, home: Option<&str>, user: Option<&str>) -> String {
    let mut out = text.to_string();
    if let Some(h) = home {
        if h.len() > 1 {
            out = out.replace(h, "~");
        }
    }
    out = collapse_user_paths(&out);
    if let Some(u) = user {
        if u.len() >= 4 && u != "root" {
            out = out.replace(u, "~");
        }
    }
    mask_segments(&scrub_urls(&out))
}

/// Keep only frame symbols from a `Backtrace` rendering: lines like
/// `  12: scenery_core::state::open`. The `at <path>:<line>` lines are dropped.
pub fn backtrace_symbols(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let t = line.trim();
        let Some((num, sym)) = t.split_once(':') else {
            continue;
        };
        if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            out.push_str(num);
            out.push(':');
            out.push_str(sym);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
#[path = "crash_redact_tests.rs"]
mod tests;
