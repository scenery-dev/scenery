//! Shadow-AI discovery, runner task R1: a **read-only** scanner that
//! enumerates MCP servers configured in AI clients outside VectorHawk
//! governance.
//!
//! This module never writes to any AI client config file. It builds
//! [`McpDiscoveryItem`]s from [`vectorhawkd_mcp::setup::detect_ai_clients`]
//! and [`vectorhawkd_mcp::setup::read_all_mcp_entries`] — the latter already
//! covers both on-disk formats (JSON clients, JSONC included, and Codex's
//! TOML `[mcp_servers]` table), so this scanner needs no parser of its own.
//!
//! Wiring these items into the existing discoveries batch POST
//! (`discoveries.rs`) is a separate task (R2) — this module only collects
//! and redacts.
//!
//! # Privacy (binding controller ruling) — ALLOWLIST, not denylist
//!
//! An earlier revision of this module screened `args`/`url` with a denylist
//! of secret-shaped patterns (known token prefixes, `--flag` name hints,
//! query-param names). Code review found that approach leaks by
//! construction: any secret shape not on the list — a space-separated
//! `["--token", "opaque-value"]` pair, a `"Authorization: Bearer <token>"`
//! string arg, a token embedded in a URL *path* segment rather than a query
//! param, or a query param the list didn't happen to name (`refresh_token`,
//! `client_secret`, ...) — passes straight through. **A denylist can only
//! ever be as complete as the patterns someone thought to write down.**
//!
//! [`McpDiscoveryDetail`] now emits an **allowlist** of fields instead:
//! nothing derived from `args` or `env` values, ever, full stop. Only:
//! - `client_name`, `server_key`, `transport` — identifying metadata, never
//!   secret.
//! - `command_basename` — the executable's file name only (never its full
//!   path — see [`command_basename_of`]).
//! - `package_identifier` — derived *only* from a closed set of well-known
//!   package-launcher invocations (`npx`, `bunx`, `pnpm dlx`, `uvx`, `pipx
//!   run`, `docker run <image>`) — see [`derive_package_identifier`]. This is
//!   the one place a raw arg value is surfaced, because a package/image
//!   reference is by definition public, not a secret — but getting there is
//!   a **two-layer** check, both required (round 2 of review): (1) the scan
//!   knows each launcher's value-taking flags and skips the flag *and* its
//!   value together, rather than treating any `-`-free token as positional —
//!   otherwise `npx --registry https://user:tok@host pkg` or `docker run -e
//!   TOKEN=secret image` hands back the flag's *value* instead of the
//!   package; (2) whatever candidate that scan lands on is validated against
//!   a strict per-ecosystem grammar (npm / PyPI requirement / docker image
//!   reference) before being accepted at all — a `uvx --from
//!   git+https://tok@host/...` value fails this and yields `None` rather
//!   than being trusted just because it sat in the right position. See
//!   [`scan_for_identifier`] and [`validate_identifier`].
//! - `url` — reduced to `scheme://host[:port]` only; userinfo, path, query,
//!   and fragment are always dropped, unconditionally — see
//!   [`safe_url_origin`]. An unparsable or schemeless `url` value emits
//!   `None`, never the raw string.
//! - `env_keys` / `header_keys` — key/header *names* only; values are never
//!   read out of the source JSON into this struct at all (not merely
//!   redacted after the fact).
//! - `arg_count` — a count, not content.
//!
//! `args` (the raw list) and the raw `command` string are **not** fields on
//! this struct at all — there is nothing to remember to redact because
//! there is nowhere for a raw value to leak into.
//!
//! [`McpDiscoveryItem::source_path`] gets the same `$HOME`-masking treatment
//! as `command_basename` (via [`mask_home_dir`], never the raw absolute
//! config path, which would otherwise embed the local username), and then
//! (review round 4) has `#<server key>` appended so it's unique per server,
//! not just per config file — see the field doc on
//! [`McpDiscoveryItem::source_path`] for why the plain masked path alone
//! caused a backend dedupe collision when a client config held more than one
//! shadow server.
//!
//! # What counts as "already governed" (and therefore excluded)
//!
//! 1. The `vectorhawk` aggregator entry itself — key `"vectorhawk"`, tested
//!    via [`vectorhawkd_mcp::ownership::is_vectorhawk_mcp_key`].
//! 2. Any entry whose `command` is the VectorHawk shim binary
//!    (`vectorhawkd_mcp::setup::MCP_COMMAND`, matched by file-name so both
//!    the aggregator's resolved absolute path and a bare `"vectorhawk"` are
//!    caught) with `args` starting `["mcp", "serve"]`
//!    (`vectorhawkd_mcp::setup::MCP_ARGS`). This is the shape
//!    `ManagedPathsPusher::push_mcp` (`pusher.rs`) writes for each
//!    individually-adopted MCP server: `{"command": "vectorhawk", "args":
//!    ["mcp", "serve", "--server", "<slug>"]}` under `mcpServers.<slug>` —
//!    same routing shim, different key than `"vectorhawk"` — so key-name
//!    matching alone (rule 1) would miss these. (This check reads the raw,
//!    unredacted `command`/`args` internally for classification only — it
//!    never ends up in the transmitted [`McpDiscoveryDetail`].)
//! 3. Any entry already present in `managed_path_markers` under the exact
//!    same virtual path key F1's own `scanner::scan_claude_json` builds
//!    (`"<config_path>:<key>"`) — reused via
//!    [`super::marker::is_already_marked`], which matches on `path` alone
//!    (there is no `kind` column filter in that query; the virtual path
//!    already encodes both the config file and the key, so no other kind of
//!    marker row can collide with it). In practice this only ever matches
//!    `~/.claude.json` entries, since that's the only file F1's
//!    `migrator.rs::migrate_item` auto-migrates today — F1 silently takes
//!    ownership of those entries on daemon start (backup + POST + marker
//!    write) even though it does not yet rewrite the client config to route
//!    through the gateway (`migrator.rs`: "fully destructive sole-source
//!    replacement for MCP servers ... is intentionally NOT done here ...
//!    daemon cannot yet serve adopted MCP backends via the aggregator").
//!    Without this check, every F1-adopted MCP server would be
//!    double-reported as shadow AI.
//! 4. Forward-looking: an entry whose `url` host matches this daemon's
//!    configured `registry_url` — a gateway-brokered server. No code path
//!    writes such an entry into a client config *today* (see the migrator.rs
//!    citation above — gateway-URL client rewriting is explicitly deferred),
//!    but once it ships, this scanner must not mis-report VectorHawk's own
//!    gateway-routed entries as shadow AI just because rule 2/3 don't cover
//!    a URL-shaped entry.

use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::warn;
use vectorhawkd_core::state::AppState;
use vectorhawkd_mcp::{
    ownership::is_vectorhawk_mcp_key,
    setup::{read_all_mcp_entries, ClientConfig, MCP_ARGS, MCP_COMMAND},
};

use super::marker::is_already_marked;

// ── Public types ──────────────────────────────────────────────────────────────

/// Metadata about one shadow (non-VectorHawk-governed) MCP server entry.
///
/// **Allowlist, not denylist** — see the module-level privacy doc. Every
/// field here is safe to transmit by construction: there is no field that
/// carries a raw `args`/`env` value or a full `url`/`command` path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpDiscoveryDetail {
    pub client_name: String,
    pub server_key: String,
    /// `"stdio"`, `"http"`, `"sse"`, or `"unknown"` — inferred from which
    /// keys are present on the entry (`command` vs `url`/`type`).
    pub transport: String,
    /// The executable's file name only — see [`command_basename_of`]. Never
    /// the full path (which could embed the local username via `$HOME`).
    pub command_basename: Option<String>,
    /// A public package/image identifier, derived only from a closed set of
    /// well-known launcher invocations — see [`derive_package_identifier`].
    /// `None` for anything else, including an unrecognized launcher; this is
    /// never a generic "first arg" dump.
    pub package_identifier: Option<String>,
    /// `scheme://host[:port]` only — see [`safe_url_origin`]. Never carries
    /// userinfo, path, query, or fragment.
    pub url: Option<String>,
    /// `env` key names only. Values are never read into this struct.
    pub env_keys: Vec<String>,
    /// `headers` key names only (e.g. `"Authorization"`). Values are never
    /// read into this struct.
    pub header_keys: Vec<String>,
    /// Count of `args`, not their content.
    pub arg_count: usize,
}

/// One discovered shadow MCP server, ready to be folded into the existing
/// discoveries batch POST (a later task — see module docs).
#[derive(Debug, Clone, Serialize)]
pub struct McpDiscoveryItem {
    pub kind: String,
    /// `"<client_name>:<key>"` — client-qualified so the same server name in
    /// two different clients (e.g. `"github-mcp"` in both Cursor and VS
    /// Code) doesn't collide.
    pub slug: String,
    /// `"<~-masked client config path>#<server key>"` (see [`mask_home_dir`]
    /// for the masking, applied before the `#`-suffix is appended) — never
    /// the raw absolute path alone.
    ///
    /// **Must be unique per server, not per config file** (review round 4):
    /// the backend dedupes uploaded discoveries on `(user_id, kind,
    /// source_path)` (`managed_path_discoveries` unique constraint /
    /// `upload_discoveries`'s existing-row lookup). A client config file
    /// (e.g. `~/.cursor/mcp.json`) commonly holds more than one shadow MCP
    /// server; using the bare masked config path as `source_path` for every
    /// server in that file would make the 2nd+ server collide with the 1st
    /// on that dedupe key and be silently dropped by the backend as a
    /// repeat sighting. Appending `#<server key>` (the same `key` that makes
    /// [`slug`](Self::slug) unique) keeps `source_path` unique and stable
    /// per server while still surfacing the config file path for IT/audit
    /// readability.
    pub source_path: String,
    /// SHA-256 of the (already-allowlisted) [`McpDiscoveryDetail`],
    /// hex-encoded.
    pub canonical_hash: String,
    pub detail: McpDiscoveryDetail,
}

// ── Scan ──────────────────────────────────────────────────────────────────────

/// Scan every detected AI client for MCP server entries that are neither
/// VectorHawk's own nor already governed, and return one [`McpDiscoveryItem`]
/// per shadow entry found.
///
/// Read-only: never writes to any client config file, never writes to
/// `managed_path_markers` (only reads it, to exclude entries F1 already
/// adopted).
///
/// `registry_url` is this daemon's configured registry — used only to
/// recognize a future gateway-brokered `url` entry as already-governed (see
/// module docs, exclusion rule 4).
pub fn collect_mcp_discoveries(
    clients: &[ClientConfig],
    state: &AppState,
    registry_url: &str,
) -> Vec<McpDiscoveryItem> {
    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "mcp_discovery: cannot open state DB — reporting nothing");
            return Vec::new();
        }
    };

    let mut items = Vec::new();

    for client in clients {
        if !client.config_path.exists() {
            continue;
        }

        for entry in read_all_mcp_entries(client) {
            if is_vectorhawk_mcp_key(&entry.key) {
                continue;
            }
            if is_governed_entry(&entry.value, registry_url) {
                continue;
            }

            let virtual_path = format!("{}:{}", client.config_path.display(), entry.key);
            if is_already_managed(&conn, &virtual_path) {
                continue;
            }

            let detail = build_detail(client, &entry.key, &entry.value);
            let canonical_hash = hex_sha256_of(&detail);

            let masked_config_path = mask_home_dir(&client.config_path.display().to_string());

            items.push(McpDiscoveryItem {
                kind: "mcp".to_string(),
                slug: format!("{}:{}", client.name, entry.key),
                // `#<server key>` suffix keeps this unique per server — see
                // the field doc on `McpDiscoveryItem::source_path` (review
                // round 4: without it, every server in the same client
                // config file collided on the backend's dedupe key and only
                // the first one ever got reported).
                source_path: format!("{masked_config_path}#{}", entry.key),
                canonical_hash,
                detail,
            });
        }
    }

    items
}

/// Wraps [`is_already_marked`], collapsing a DB error into "not marked" —
/// consistent with `discoveries.rs`'s fail-open-on-per-item-check stance
/// (the whole-scan DB-open failure above is the fail-closed guard).
fn is_already_managed(conn: &Connection, virtual_path: &str) -> bool {
    is_already_marked(conn, virtual_path).unwrap_or(false)
}

// ── Governed-entry detection ────────────────────────────────────────────────

/// True if `value` is an MCP entry VectorHawk itself is already responsible
/// for — see the module-level "What counts as already governed" doc.
///
/// Reads the raw `command`/`args`/`url` fields for classification only —
/// none of it is copied into a transmitted [`McpDiscoveryDetail`].
fn is_governed_entry(value: &serde_json::Value, registry_url: &str) -> bool {
    if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
        if command_is_vectorhawk_shim(command) && args_start_with_mcp_serve(value) {
            return true;
        }
    }

    if let Some(url) = value.get("url").and_then(|v| v.as_str()) {
        if url_host_matches(url, registry_url) {
            return true;
        }
    }

    false
}

/// True if `command`'s file name is exactly `MCP_COMMAND` ("vectorhawk") —
/// matches both the aggregator's resolved absolute path
/// (`/opt/homebrew/bin/vectorhawk`) and the bare command `pusher::push_mcp`
/// writes for per-server entries.
fn command_is_vectorhawk_shim(command: &str) -> bool {
    std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == MCP_COMMAND)
        .unwrap_or(false)
}

/// True if `value.args` starts with `MCP_ARGS` (`["mcp", "serve"]`) — the
/// shim invocation every VectorHawk-written entry uses, whether it's the
/// aggregator's own entry or a `push_mcp`-written per-server entry (which
/// appends `"--server", "<slug>"` after these two).
fn args_start_with_mcp_serve(value: &serde_json::Value) -> bool {
    let Some(arr) = value.get("args").and_then(|v| v.as_array()) else {
        return false;
    };
    let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    strs.get(0..MCP_ARGS.len()) == Some(MCP_ARGS)
}

/// True if `url` and `registry_url` share the same host. Used only for the
/// forward-looking gateway-URL governed check (see module docs, rule 4).
fn url_host_matches(url: &str, registry_url: &str) -> bool {
    let entry_host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let registry_host = reqwest::Url::parse(registry_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    matches!((entry_host, registry_host), (Some(a), Some(b)) if a == b)
}

// ── Detail construction (allowlist) ─────────────────────────────────────────

fn build_detail(client: &ClientConfig, key: &str, value: &serde_json::Value) -> McpDiscoveryDetail {
    let command = value.get("command").and_then(|v| v.as_str());
    let raw_args: Vec<String> = value
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let raw_url = value.get("url").and_then(|v| v.as_str());
    let type_field = value.get("type").and_then(|v| v.as_str());

    let command_basename = command.map(command_basename_of);
    let package_identifier = command_basename
        .as_deref()
        .and_then(|basename| derive_package_identifier(basename, &raw_args));

    McpDiscoveryDetail {
        client_name: client.name.clone(),
        server_key: key.to_string(),
        transport: infer_transport(command, raw_url, type_field).to_string(),
        command_basename,
        package_identifier,
        url: raw_url.and_then(safe_url_origin),
        env_keys: object_keys(value, "env"),
        header_keys: object_keys(value, "headers"),
        arg_count: raw_args.len(),
    }
}

/// Key names of an object-valued field (`env`/`headers`). Values are never
/// read — only `.keys()` is ever touched.
fn object_keys(value: &serde_json::Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(|v| v.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

fn infer_transport(
    command: Option<&str>,
    url: Option<&str>,
    type_field: Option<&str>,
) -> &'static str {
    if command.is_some() {
        return "stdio";
    }
    if url.is_some() {
        return match type_field.map(str::to_ascii_lowercase) {
            Some(t) if t == "sse" => "sse",
            _ => "http",
        };
    }
    "unknown"
}

/// Reduce `command` to its executable file name only — never the full path.
///
/// A full command path (e.g. `/Users/alice/.nvm/versions/node/v20/bin/npx`)
/// embeds the local username via `$HOME`; the basename (`npx`) is all IT
/// needs to see. Falls back to masking `$HOME` with `~` in the rare case a
/// path has no extractable file name at all (e.g. it ends in `/` or `..`),
/// as a last-resort safety net rather than ever emitting the raw string.
fn command_basename_of(command: &str) -> String {
    match std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
    {
        Some(name) => name.to_string(),
        None => mask_home_dir(command),
    }
}

/// Replace the current user's `$HOME` (if it occurs in `value`) with `~`.
/// Used both as [`command_basename_of`]'s last-resort fallback and directly
/// on [`McpDiscoveryItem::source_path`] — a raw absolute config path embeds
/// the local username (`/Users/<username>/...`), which the allowlist policy
/// (module docs) treats the same as any other locally-identifying detail:
/// mask it rather than transmit it.
fn mask_home_dir(value: &str) -> String {
    if let Some(home) = dirs::home_dir().and_then(|h| h.to_str().map(str::to_string)) {
        if !home.is_empty() && value.contains(&home) {
            return value.replacen(&home, "~", 1);
        }
    }
    value.to_string()
}

/// Derive a public package/image identifier from a closed set of well-known
/// package-launcher invocations. Returns `None` for anything else —
/// deliberately not a general "first positional arg" extractor, since that
/// would reintroduce a denylist-shaped guess about what's safe.
///
/// **Two layers, both required** (review round 2 — a first version that only
/// skipped `-`-prefixed tokens leaked a value-taking flag's *value* as if it
/// were the package: `npx --registry https://user:tok@host pkg` returned the
/// registry URL, `docker run -e TOKEN=secret image` returned `TOKEN=secret`):
///
/// 1. [`scan_for_identifier`] walks `args` knowing which flags for this
///    launcher consume a following value ([`value_taking_flags`]) — those
///    flags *and* their value are skipped as a pair, never treated as
///    positional. A small set of flags directly *name* the package
///    ([`package_naming_flags`]: `uvx --from`, `pipx run --spec`, `npx`/
///    `bunx -p`/`--package`) — for those the flag's value is the candidate,
///    not the next positional arg.
/// 2. Whatever candidate is found (positional or a naming flag's value) is
///    validated against a strict per-ecosystem grammar
///    ([`is_valid_npm_identifier`] / [`is_valid_pypi_identifier`] /
///    [`is_valid_docker_identifier`]) before being accepted — never returned
///    on the strength of "it didn't start with `-`" alone. A candidate that
///    fails validation (e.g. `uvx --from`'s value being a `git+https://
///    tok@host/...` URL) yields `None`, not a best-effort fallback.
///
/// A derived identifier (`@scope/pkg@1.2.3`, `myregistry/image:1.4`,
/// `pkg==1.0`) is surfaced verbatim, version/tag/specifier included — a
/// package or image reference is public by definition, not a secret; the
/// grammar check is what proves it's actually shaped like one before it's
/// trusted as such.
///
/// **Fail-closed on unknown flags** (review round 3 — round 2's scan still
/// treated any `-`-prefixed token it didn't specifically recognize as a
/// value-taking flag as a bare boolean switch, so an unlisted value-taking
/// flag's value fell straight through as the candidate:
/// `docker run --link secret-container-name:alias image` returned
/// `"secret-container-name:alias"`, a perfectly grammar-valid-looking docker
/// reference that was never actually the image). [`scan_for_identifier`] now
/// requires a flag to match an explicit per-launcher boolean-switch list
/// ([`boolean_switch_flags`], plus combined short docker forms like `-it`/
/// `-dit` — see [`is_combined_docker_boolean`]) or a known value-taking flag
/// ([`value_taking_flags`]) to be skipped; anything else `-`-prefixed is
/// **unrecognized** and the whole scan returns `None` rather than guessing
/// either way. `--flag=value` tokens are still always safe to skip
/// unconditionally (self-contained, no following token to misalign onto),
/// and a bare `--` ends option parsing (the very next arg is the candidate).
///
/// **Residual risk, accepted**: this only rejects *unrecognized syntax*
/// (flags this scan doesn't know), not *content*. A secret deliberately
/// shaped to pass one of the ecosystem grammars below — e.g. a token named
/// exactly like a valid npm package (`sk-attacker-controlled-name`) passed
/// as a bare positional arg with no intervening flag — is not detectable
/// from syntax alone and will still be reported as a `package_identifier`.
/// The grammars exist to reject *obviously* non-package shapes (URLs,
/// `user@host`, whitespace), not to prove a candidate is genuinely a public
/// package name.
fn derive_package_identifier(command_basename: &str, args: &[String]) -> Option<String> {
    match command_basename {
        "npx" | "bunx" | "uvx" => scan_for_identifier(command_basename, args, None),
        "pnpm" => scan_for_identifier(command_basename, args, Some("dlx")),
        "pipx" => scan_for_identifier(command_basename, args, Some("run")),
        "docker" => scan_for_identifier(command_basename, args, Some("run")),
        _ => None,
    }
}

/// Flags that consume a following value, for a given launcher — used only to
/// know how many tokens to skip while scanning for the positional package
/// arg. Never a source of the identifier itself (see [`package_naming_flags`]
/// for the small set of flags that are).
fn value_taking_flags(command_basename: &str) -> &'static [&'static str] {
    match command_basename {
        "npx" | "bunx" => &[
            "--registry",
            "--cache",
            "--userconfig",
            "--prefix",
            "--scope",
        ],
        "uvx" => &[
            "--with",
            "--index-url",
            "--index",
            "--python",
            "--python-preference",
            "--with-requirements",
        ],
        "pipx" => &["--pip-args", "--index-url", "--python"],
        "pnpm" => &["--package", "-C", "--dir", "--filter"],
        "docker" => &[
            "-e",
            "--env",
            "--env-file",
            "-v",
            "--volume",
            "-p",
            "--publish",
            "--name",
            "--network",
            "-w",
            "--workdir",
            "-u",
            "--user",
            "--mount",
            "--entrypoint",
            "--label",
            "-l",
            "--add-host",
            "--cpus",
            "--memory",
            "-m",
            "--restart",
            "--platform",
            "--pull",
            "--hostname",
            "-h",
        ],
        _ => &[],
    }
}

/// Flags whose *value* directly names the package, for a given launcher —
/// `uvx --from <pkg>`, `pipx run --spec <req>`, `npx`/`bunx -p`/`--package
/// <pkg>`. Checked before [`value_taking_flags`] so these are never merely
/// skipped.
fn package_naming_flags(command_basename: &str) -> &'static [&'static str] {
    match command_basename {
        "npx" | "bunx" => &["-p", "--package"],
        "uvx" => &["--from"],
        "pipx" => &["--spec"],
        _ => &[],
    }
}

/// Flags that take **no** value ("switches"), for a given launcher — the
/// only other kind of flag [`scan_for_identifier`] will skip over. Not
/// claimed to be exhaustive; an unrecognized flag fails closed (returns
/// `None`) rather than being guessed at either way — see the fail-closed
/// note on [`derive_package_identifier`].
fn boolean_switch_flags(command_basename: &str) -> &'static [&'static str] {
    match command_basename {
        "npx" | "bunx" => &["-y", "--yes", "--no", "-q", "--quiet", "--silent"],
        "uvx" => &[
            "--no-cache",
            "--refresh",
            "--isolated",
            "-q",
            "--quiet",
            "-v",
            "--verbose",
            "--prerelease",
            "--no-config",
        ],
        "pipx" => &["-q", "--quiet", "-v", "--verbose", "--global"],
        "pnpm" => &["-q", "--silent", "--stream", "--shell-mode"],
        "docker" => &[
            "-d",
            "--detach",
            "-i",
            "--interactive",
            "-t",
            "--tty",
            "-P",
            "--publish-all",
            "--rm",
            "--init",
            "--privileged",
            "--read-only",
            "--sig-proxy",
            "-q",
            "--quiet",
        ],
        _ => &[],
    }
}

/// Single-character docker switches that commonly appear combined in one
/// token (`-it`, `-dit`) — `d`/`i`/`t`/`P` only, matching the long forms
/// already listed in [`boolean_switch_flags`]. `-e` and friends are
/// deliberately excluded: they take a value and must never be treated as
/// combinable booleans.
const DOCKER_COMBINABLE_BOOLEAN_CHARS: &[char] = &['d', 'i', 't', 'P'];

/// True if `arg` is a docker short-flag token made up entirely of
/// [`DOCKER_COMBINABLE_BOOLEAN_CHARS`] (e.g. `-it`, `-dit`, `-P`) — docker
/// allows bundling single-character boolean flags into one token.
fn is_combined_docker_boolean(arg: &str) -> bool {
    match arg.strip_prefix('-') {
        Some(chars) if !chars.is_empty() && !chars.starts_with('-') => chars
            .chars()
            .all(|c| DOCKER_COMBINABLE_BOOLEAN_CHARS.contains(&c)),
        _ => false,
    }
}

/// Scan `args` for a package-identifier candidate, honoring `command_basename`'s
/// value-taking, package-naming, and boolean-switch flags, then
/// grammar-validate it before returning — see [`derive_package_identifier`]'s
/// doc for the full rationale (two validation layers, fail-closed on unknown
/// flags). `start_marker`, when given, skips everything up to and including
/// its first occurrence (`"run"` for `docker run`/`pipx run`, `"dlx"` for
/// `pnpm dlx`) before scanning begins; returns `None` immediately if the
/// marker never appears.
fn scan_for_identifier(
    command_basename: &str,
    args: &[String],
    start_marker: Option<&str>,
) -> Option<String> {
    let naming_flags = package_naming_flags(command_basename);
    let skip_flags = value_taking_flags(command_basename);
    let boolean_flags = boolean_switch_flags(command_basename);
    let is_docker = command_basename == "docker";

    let mut idx = match start_marker {
        Some(marker) => args.iter().position(|a| a == marker)? + 1,
        None => 0,
    };

    while idx < args.len() {
        let arg = args[idx].as_str();

        // `--` ends option parsing for every launcher here — the very next
        // arg (if any) is the candidate, no further flag interpretation.
        if arg == "--" {
            return args
                .get(idx + 1)
                .and_then(|candidate| validate_identifier(candidate, command_basename));
        }

        // `--flag=value` shape: one self-contained token, no following arg
        // to skip — safe to consume regardless of whether `flag` is known.
        if let Some(eq) = arg.find('=') {
            let flag = &arg[..eq];
            let value = &arg[eq + 1..];
            if naming_flags.contains(&flag) {
                return validate_identifier(value, command_basename);
            }
            idx += 1;
            continue;
        }

        if naming_flags.contains(&arg) {
            let value = args.get(idx + 1)?;
            return validate_identifier(value, command_basename);
        }

        if skip_flags.contains(&arg) {
            idx += 2; // the flag AND its value are consumed together.
            continue;
        }

        if boolean_flags.contains(&arg) || (is_docker && is_combined_docker_boolean(arg)) {
            idx += 1; // a known switch — no value to skip.
            continue;
        }

        if arg.starts_with('-') {
            // Unrecognized flag for this launcher: we don't know whether it
            // takes a value, so we cannot safely keep scanning past it —
            // fail closed rather than risk treating its value (or the next
            // unrelated positional) as the package. See the fail-closed
            // note on `derive_package_identifier`.
            return None;
        }

        // First true positional arg for this launcher.
        return validate_identifier(arg, command_basename);
    }

    None
}

/// Grammar-validate `candidate` against the ecosystem `command_basename`
/// implies, returning it unchanged only if it passes — otherwise `None`.
/// This is the second, independent layer: even a value the flag-scan
/// correctly identified as "the package position" is rejected here if it
/// isn't actually shaped like a package/image reference.
fn validate_identifier(candidate: &str, command_basename: &str) -> Option<String> {
    let valid = match command_basename {
        "npx" | "bunx" | "pnpm" => is_valid_npm_identifier(candidate),
        "uvx" | "pipx" => is_valid_pypi_identifier(candidate),
        "docker" => is_valid_docker_identifier(candidate),
        _ => false,
    };
    if valid {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// npm package identifier grammar: optional `@scope/`, a lowercase package
/// name, then an optional `@version-or-dist-tag` suffix — e.g. `linear-mcp`,
/// `@modelcontextprotocol/server-github`, `pkg@1.2.3`, `@scope/pkg@latest`.
/// Anything containing `://`, whitespace, or other punctuation outside this
/// shape (e.g. a URL's `user:pass@host`) fails.
fn is_valid_npm_identifier(candidate: &str) -> bool {
    let rest = match candidate.strip_prefix('@') {
        Some(after_at) => match after_at.split_once('/') {
            Some((scope, remainder)) if is_npm_name_segment(scope) => remainder,
            _ => return false,
        },
        None => candidate,
    };

    let (name, version) = match rest.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (rest, None),
    };

    is_npm_name_segment(name) && version.map(is_simple_version_or_tag).unwrap_or(true)
}

/// One npm scope/name segment: lowercase letters, digits, `.`, `_`, `-` only.
fn is_npm_name_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// A version/dist-tag suffix: alphanumerics, `.`, `_`, `-`, `+` only — covers
/// semver and tags like `latest`; never `://`, whitespace, or another `@`.
fn is_simple_version_or_tag(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// PyPI requirement grammar: a package name, optional `[extras]`, optional
/// version specifier (`==`/`>=`/`<=`/`~=`/`!=`/`>`/`<` followed by a version
/// string) — never a URL. E.g. `some-mcp-server`, `pkg==1.0`,
/// `pkg[extra]>=2.0`. A `git+https://...` value (as `uvx --from` sometimes
/// carries) fails at the very first character after the name run.
fn is_valid_pypi_identifier(candidate: &str) -> bool {
    let name_end = candidate
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(candidate.len());
    if name_end == 0 {
        return false;
    }
    let mut rest = &candidate[name_end..];

    if let Some(after_bracket) = rest.strip_prefix('[') {
        let Some(close) = after_bracket.find(']') else {
            return false;
        };
        let extras = &after_bracket[..close];
        if extras.is_empty()
            || !extras
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | '_' | '-'))
        {
            return false;
        }
        rest = &after_bracket[close + 1..];
    }

    if rest.is_empty() {
        return true;
    }

    const OPERATORS: &[&str] = &["==", ">=", "<=", "~=", "!=", ">", "<"];
    let Some(op) = OPERATORS.iter().find(|op| rest.starts_with(**op)) else {
        return false;
    };
    let version = &rest[op.len()..];
    !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '*' | '+'))
}

/// Docker image reference grammar: `[registry[:port]/]repo[/repo...][:tag]`
/// or `...@sha256:<64 hex chars>`. Rejects anything containing `=` or
/// `://` outright, and any `@` not immediately followed by `sha256:` (so a
/// `user:pass@host`-shaped value never validates).
fn is_valid_docker_identifier(candidate: &str) -> bool {
    if candidate.contains('=') || candidate.contains("://") {
        return false;
    }

    let name_part = match candidate.split_once('@') {
        Some((base, digest)) if is_sha256_digest(digest) => base,
        Some(_) => return false,
        None => candidate,
    };
    if name_part.is_empty() {
        return false;
    }

    let name_part = match name_part.rsplit_once(':') {
        // Only the LAST colon can be a `:tag` separator, and only when
        // nothing after it looks like a path (a real tag never contains
        // `/`) — otherwise it's a `host:port/...` colon, left alone here
        // and validated per-segment below.
        Some((base, tag)) if !tag.is_empty() && !tag.contains('/') && is_docker_word(tag) => base,
        _ => name_part,
    };

    !name_part.is_empty()
        && name_part
            .split('/')
            .enumerate()
            .all(|(i, segment)| is_docker_path_segment(segment, i == 0))
}

fn is_docker_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One `/`-separated segment of a docker image name. The first segment may
/// additionally be a `host:port` pair (digits-only port) — every other
/// segment, and a first segment with no colon, must be a plain
/// [`is_docker_word`].
fn is_docker_path_segment(segment: &str, is_first: bool) -> bool {
    if segment.is_empty() {
        return false;
    }
    if is_first {
        if let Some((host, port)) = segment.split_once(':') {
            return is_docker_word(host)
                && !port.is_empty()
                && port.chars().all(|c| c.is_ascii_digit());
        }
    }
    is_docker_word(segment)
}

fn is_sha256_digest(candidate: &str) -> bool {
    candidate
        .strip_prefix("sha256:")
        .map(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or(false)
}

/// Reduce a URL to `scheme://host[:port]` only. Returns `None` if `url`
/// doesn't parse as an absolute URL with a host (e.g. schemeless) — never
/// the raw string. Userinfo, path, query, and fragment are always dropped,
/// unconditionally, regardless of what they contain.
fn safe_url_origin(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{}://{}:{}", parsed.scheme(), host, port)),
        None => Some(format!("{}://{}", parsed.scheme(), host)),
    }
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

/// Hash the JSON-serialized (already-allowlisted) detail. Since `detail`
/// never carries a raw `args`/`env` value or a full `url`/`command` path,
/// this hash is safe to transmit alongside the item.
fn hex_sha256_of(detail: &McpDiscoveryDetail) -> String {
    let bytes = serde_json::to_vec(detail).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hex::encode(hasher.finalize())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_paths::ENV_MUTEX;
    use camino::Utf8PathBuf;
    use std::{fs, path::PathBuf};
    use vectorhawkd_mcp::setup::ConfigFormat;

    fn temp_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let state = AppState::bootstrap_in(root).unwrap();
        (state, tmp)
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vh-mcp-discovery-test-{label}-{nanos}"))
    }

    fn write_json_client(
        dir: &std::path::Path,
        name: &str,
        mcp_key: &str,
        body: serde_json::Value,
    ) -> ClientConfig {
        fs::create_dir_all(dir).unwrap();
        let config_path = dir.join("config.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&serde_json::json!({ mcp_key: body })).unwrap(),
        )
        .unwrap();
        ClientConfig {
            name: name.to_string(),
            config_path,
            mcp_key: mcp_key.to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        }
    }

    /// Serialize a whole item and assert `secret` appears nowhere in it —
    /// the standard leak-proof assertion this review round asked for.
    fn assert_never_leaks(item: &McpDiscoveryItem, secret: &str) {
        let serialized = serde_json::to_string(item).unwrap();
        assert!(
            !serialized.contains(secret),
            "secret {secret:?} must never appear in the serialized item: {serialized}"
        );
    }

    // ── 1. One foreign server in Cursor's mcp.json → one item ─────────────────

    #[test]
    fn enumerates_one_foreign_cursor_server() {
        let tmp = temp_root("cursor-one");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "linear-mcp": {"command": "npx", "args": ["-y", "linear-mcp"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "mcp");
        assert_eq!(items[0].slug, "Cursor:linear-mcp");
        assert_eq!(items[0].detail.client_name, "Cursor");
        assert_eq!(items[0].detail.server_key, "linear-mcp");
        assert_eq!(items[0].detail.transport, "stdio");
        assert_eq!(items[0].detail.command_basename.as_deref(), Some("npx"));
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("linear-mcp")
        );
        assert_eq!(items[0].detail.arg_count, 2);

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 2. env values never leak; only key names survive ──────────────────────

    #[test]
    fn env_values_are_never_present_only_key_names() {
        let tmp = temp_root("vscode-env");
        let client = write_json_client(
            &tmp,
            "VS Code",
            "servers",
            serde_json::json!({
                "github-mcp": {
                    "command": "npx",
                    "args": ["-y", "@github/mcp"],
                    "env": {"API_KEY": "sk-abc123secretvalue"}
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].detail.env_keys, vec!["API_KEY".to_string()]);
        assert_never_leaks(&items[0], "sk-abc123secretvalue");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 3. leak shape: space-separated flag/value pair (finding 1) ────────────

    #[test]
    fn space_separated_flag_value_pair_never_leaks() {
        let tmp = temp_root("space-separated-token");
        let client = write_json_client(
            &tmp,
            "Windsurf",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {"command": "custom-mcp", "args": ["--token", "OPAQUEVALUESECRET999"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].detail.arg_count, 2);
        assert_never_leaks(&items[0], "OPAQUEVALUESECRET999");
        assert_never_leaks(&items[0], "--token");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 4. leak shape: header string as a single arg (finding 2) ──────────────

    #[test]
    fn header_shaped_single_arg_never_leaks() {
        let tmp = temp_root("header-shaped-arg");
        let client = write_json_client(
            &tmp,
            "Windsurf",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "custom-mcp",
                    "args": ["--header", "Authorization: Bearer HEADERARGSECRETVALUE"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_never_leaks(&items[0], "HEADERARGSECRETVALUE");
        assert_never_leaks(&items[0], "Authorization: Bearer");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 5. leak shape: secret in a URL path segment (finding 3) ───────────────

    #[test]
    fn url_path_token_never_leaks() {
        let tmp = temp_root("url-path-token");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {"url": "https://mcp.example.com/sse/PATHTOKENSECRETVALUE"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.url.as_deref(),
            Some("https://mcp.example.com")
        );
        assert_never_leaks(&items[0], "PATHTOKENSECRETVALUE");
        assert_never_leaks(&items[0], "/sse/");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 6. leak shape: refresh_token/client_secret query params (finding 4) ───

    #[test]
    fn url_query_secrets_beyond_the_old_denylist_never_leak() {
        let tmp = temp_root("url-query-secrets");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {"url": "https://mcp.example.com/sse?refresh_token=RTOKENSECRET&client_secret=CSECRETVALUE&id_token=IDTOKENSECRET"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.url.as_deref(),
            Some("https://mcp.example.com")
        );
        assert_never_leaks(&items[0], "RTOKENSECRET");
        assert_never_leaks(&items[0], "CSECRETVALUE");
        assert_never_leaks(&items[0], "IDTOKENSECRET");
        assert_never_leaks(&items[0], "refresh_token");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 7. leak shape: URL userinfo ────────────────────────────────────────────

    #[test]
    fn url_userinfo_never_leaks() {
        let tmp = temp_root("url-userinfo");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {"url": "https://svc-user:s3cr3t-pass@mcp.example.com/sse"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.url.as_deref(),
            Some("https://mcp.example.com")
        );
        assert_never_leaks(&items[0], "svc-user");
        assert_never_leaks(&items[0], "s3cr3t-pass");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 8. leak shape: headers block with Authorization (finding 7) ───────────

    #[test]
    fn headers_block_values_never_leak_only_names_survive() {
        let tmp = temp_root("headers-block");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {
                    "url": "https://mcp.example.com/sse",
                    "headers": {"Authorization": "Bearer HEADERBLOCKSECRETVALUE", "X-Api-Version": "1"}
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        let mut header_keys = items[0].detail.header_keys.clone();
        header_keys.sort();
        assert_eq!(header_keys, vec!["Authorization", "X-Api-Version"]);
        assert_never_leaks(&items[0], "HEADERBLOCKSECRETVALUE");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 9. schemeless URL emits nothing, never the raw string ─────────────────

    #[test]
    fn schemeless_url_emits_none_not_raw_string() {
        let tmp = temp_root("schemeless-url");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {"url": "mcp.example.com:9999/weird?token=SCHEMELESSTOKENVALUE"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_never_leaks(&items[0], "SCHEMELESSTOKENVALUE");
        assert_never_leaks(&items[0], "mcp.example.com:9999/weird");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 10. JSONC file with comments containing servers (finding 5) ───────────

    #[test]
    fn jsonc_config_with_comments_is_not_a_discovery_blind_spot() {
        let tmp = temp_root("vscode-jsonc");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("mcp.json");
        fs::write(
            &config_path,
            "{\n  // VS Code mcp.json commonly carries comments\n  \"servers\": {\n    \"linear-mcp\": {\"command\": \"npx\", \"args\": [\"-y\", \"linear-mcp\"]}, // trailing comment\n  },\n}\n",
        )
        .unwrap();
        let client = ClientConfig {
            name: "VS Code".to_string(),
            config_path,
            mcp_key: "servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(
            items.len(),
            1,
            "a commented JSONC config must not silently yield zero servers"
        );
        assert_eq!(items[0].slug, "VS Code:linear-mcp");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 11. ~/.claude.json entry already in managed_path_markers → excluded ───

    #[test]
    fn skips_claude_json_entry_already_managed_by_f1() {
        let tmp = temp_root("claude-json-managed");
        let client = write_json_client(
            &tmp,
            "Claude Code",
            "mcpServers",
            serde_json::json!({
                "already-adopted": {"command": "adopted-tool", "args": []},
                "still-shadow": {"command": "shadow-tool", "args": []}
            }),
        );
        let (state, _guard) = temp_state();

        let virtual_path = format!("{}:already-adopted", client.config_path.display());
        {
            let conn = Connection::open(&state.db_path).unwrap();
            conn.execute(
                "INSERT INTO managed_path_markers \
                 (path, kind, slug, installation_id, source_sha256, migrated_at) \
                 VALUES (?1, 'mcp', 'already-adopted', NULL, 'aabbcc', '2026-01-01T000000Z')",
                rusqlite::params![virtual_path],
            )
            .unwrap();
        }

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(
            items.len(),
            1,
            "only the still-shadow entry should be reported"
        );
        assert_eq!(items[0].slug, "Claude Code:still-shadow");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 12. The vectorhawk key itself is excluded ──────────────────────────────

    #[test]
    fn excludes_the_vectorhawk_key_itself() {
        let tmp = temp_root("vectorhawk-key");
        let client = write_json_client(
            &tmp,
            "Claude Desktop",
            "mcpServers",
            serde_json::json!({
                "vectorhawk": {"command": "/opt/homebrew/bin/vectorhawk", "args": ["mcp", "serve"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert!(
            items.is_empty(),
            "the vectorhawk entry itself must never be reported as shadow AI"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 13. A per-server entry pushed by pusher::push_mcp is also excluded ────

    #[test]
    fn excludes_pusher_pushed_per_server_entry() {
        let tmp = temp_root("pusher-slug");
        // Shape pusher.rs::push_mcp writes: bare "vectorhawk" command, a
        // different key (the server's slug), args ["mcp","serve","--server",slug].
        let client = write_json_client(
            &tmp,
            "Claude Code",
            "mcpServers",
            serde_json::json!({
                "notion-mcp": {"command": "vectorhawk", "args": ["mcp", "serve", "--server", "notion-mcp"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert!(
            items.is_empty(),
            "a push_mcp-written per-server entry must be recognized as governed"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 14. A URL entry whose host matches the registry is excluded ───────────

    #[test]
    fn excludes_entry_whose_url_host_matches_registry() {
        let tmp = temp_root("gateway-url");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "brokered-mcp": {"url": "https://app.vectorhawk.ai/gateway/mcp/brokered-mcp", "type": "http"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert!(
            items.is_empty(),
            "gateway-hosted entries must be recognized as governed"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 15. Foreign http/sse entries are still reported, with correct transport

    #[test]
    fn foreign_remote_entry_is_reported_with_http_transport() {
        let tmp = temp_root("foreign-remote");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "remote-mcp": {"url": "https://other-vendor.example.com/mcp"}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].detail.transport, "http");
        assert_eq!(
            items[0].detail.url.as_deref(),
            Some("https://other-vendor.example.com")
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 16. Codex TOML entries are enumerated too (controller note) ───────────

    #[test]
    fn enumerates_codex_toml_entries() {
        let tmp = temp_root("codex-toml");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("config.toml");
        fs::write(
            &config_path,
            "model = \"gpt-5.1\"\n\n[mcp_servers.linear-mcp]\ncommand = \"npx\"\nargs = [\"-y\", \"linear-mcp\", \"--api-key=sk-supersecrettoken\"]\n",
        )
        .unwrap();
        let client = ClientConfig {
            name: "Codex".to_string(),
            config_path,
            mcp_key: "mcp_servers".to_string(),
            already_configured: false,
            format: ConfigFormat::Toml,
        };
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].slug, "Codex:linear-mcp");
        assert_eq!(items[0].detail.transport, "stdio");
        assert_eq!(items[0].detail.command_basename.as_deref(), Some("npx"));
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("linear-mcp")
        );
        assert_eq!(items[0].detail.arg_count, 3);
        assert_never_leaks(&items[0], "sk-supersecrettoken");
        assert_never_leaks(&items[0], "--api-key");

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── 17. Client whose config file doesn't exist is skipped without panic ───

    #[test]
    fn skips_client_with_missing_config_file() {
        let tmp = temp_root("missing-config");
        fs::create_dir_all(&tmp).unwrap();
        let client = ClientConfig {
            name: "Gemini CLI".to_string(),
            config_path: tmp.join("does-not-exist.json"),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
            format: ConfigFormat::Json,
        };
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert!(items.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Package-identifier derivation — one per well-known launcher ───────────

    #[test]
    fn package_identifier_npx_keeps_version() {
        let args = vec!["-y".to_string(), "@scope/pkg@1.2.3".to_string()];
        assert_eq!(
            derive_package_identifier("npx", &args).as_deref(),
            Some("@scope/pkg@1.2.3")
        );
    }

    #[test]
    fn package_identifier_bunx() {
        let args = vec!["linear-mcp".to_string()];
        assert_eq!(
            derive_package_identifier("bunx", &args).as_deref(),
            Some("linear-mcp")
        );
    }

    #[test]
    fn package_identifier_uvx() {
        let args = vec!["some-mcp-server".to_string()];
        assert_eq!(
            derive_package_identifier("uvx", &args).as_deref(),
            Some("some-mcp-server")
        );
    }

    #[test]
    fn package_identifier_pnpm_dlx() {
        let args = vec!["dlx".to_string(), "linear-mcp".to_string()];
        assert_eq!(
            derive_package_identifier("pnpm", &args).as_deref(),
            Some("linear-mcp")
        );
    }

    #[test]
    fn package_identifier_pnpm_without_dlx_is_none() {
        let args = vec!["install".to_string(), "linear-mcp".to_string()];
        assert_eq!(derive_package_identifier("pnpm", &args), None);
    }

    #[test]
    fn package_identifier_pipx_run() {
        let args = vec!["run".to_string(), "some-python-mcp".to_string()];
        assert_eq!(
            derive_package_identifier("pipx", &args).as_deref(),
            Some("some-python-mcp")
        );
    }

    #[test]
    fn package_identifier_docker_run_image_with_tag() {
        let args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "myregistry/image:1.4".to_string(),
        ];
        assert_eq!(
            derive_package_identifier("docker", &args).as_deref(),
            Some("myregistry/image:1.4")
        );
    }

    #[test]
    fn package_identifier_unrecognized_launcher_is_none() {
        let args = vec!["some-arg".to_string()];
        assert_eq!(derive_package_identifier("custom-mcp-binary", &args), None);
    }

    // ── command_basename ────────────────────────────────────────────────────

    #[test]
    fn command_basename_strips_full_path() {
        assert_eq!(
            command_basename_of("/opt/homebrew/bin/vectorhawk"),
            "vectorhawk"
        );
        assert_eq!(command_basename_of("npx"), "npx");
    }

    // ── safe_url_origin ─────────────────────────────────────────────────────

    #[test]
    fn safe_url_origin_keeps_only_scheme_host_port() {
        assert_eq!(
            safe_url_origin("https://user:pass@mcp.example.com:8443/sse?token=x#frag").as_deref(),
            Some("https://mcp.example.com:8443")
        );
        assert_eq!(
            safe_url_origin("https://mcp.example.com/sse").as_deref(),
            Some("https://mcp.example.com")
        );
        assert_eq!(safe_url_origin("not-a-url-at-all"), None);
    }

    // ── Review round 2: value-taking flags leaking into package_identifier ────
    //
    // Each test below exercises the full `collect_mcp_discoveries` path (not
    // just `derive_package_identifier` directly) so the assertion matches
    // exactly what review round 2 asked for: the secret must appear nowhere
    // in the *serialized* detail, and the identifier is either the correct
    // package or `None`.

    #[test]
    fn npx_registry_flag_value_never_leaks_and_positional_package_is_found() {
        let tmp = temp_root("npx-registry-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "npx",
                    "args": ["-y", "--registry", "https://user:tok@host", "pkg"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("pkg"),
            "the --registry flag's value must not be mistaken for the package"
        );
        assert_never_leaks(&items[0], "user:tok@host");
        assert_never_leaks(&items[0], "https://user:tok@host");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_env_flag_value_never_leaks_and_image_is_found() {
        let tmp = temp_root("docker-env-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "-e", "TOKEN=supersecret", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("image"),
            "the -e flag's value must not be mistaken for the image"
        );
        assert_never_leaks(&items[0], "supersecret");
        assert_never_leaks(&items[0], "TOKEN=supersecret");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_env_file_flag_value_never_leaks() {
        let tmp = temp_root("docker-env-file-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "--env-file", "f", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("image"),
            "the --env-file flag's value ('f') must not be mistaken for the image"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_name_flag_value_never_leaks() {
        let tmp = temp_root("docker-name-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "-v", "/host:/container", "--name", "mysecretcontainername", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].detail.package_identifier.as_deref(), Some("image"));
        assert_never_leaks(&items[0], "mysecretcontainername");
        assert_never_leaks(&items[0], "/host:/container");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn uvx_from_git_credential_url_never_leaks_and_is_rejected() {
        let tmp = temp_root("uvx-from-git-url");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "uvx",
                    "args": ["--from", "git+https://tok@github.com/x", "y"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier, None,
            "a --from value that fails the PyPI grammar must be rejected, not surfaced"
        );
        assert_never_leaks(&items[0], "tok@github.com");
        assert_never_leaks(&items[0], "git+https://");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pipx_spec_flag_names_the_correct_value_not_a_misaligned_one() {
        let tmp = temp_root("pipx-spec-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "pipx",
                    "args": ["run", "--spec", "pkg==1.0", "tool"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("pkg==1.0"),
            "--spec's own value must be used, not misaligned onto a neighboring flag"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn npx_naming_flag_p_supplies_the_package_directly() {
        let tmp = temp_root("npx-p-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "npx",
                    "args": ["-p", "left-pad", "tool-name"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("left-pad"),
            "-p names the package directly; must not fall through to 'tool-name'"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Review round 2: source_path must be $HOME-masked ───────────────────────

    #[test]
    fn source_path_masks_home_dir() {
        // `dirs::home_dir()` reads $HOME on macOS/Linux; point it at a temp
        // dir so the config path constructed "inside" it exercises the same
        // masking a real `/Users/<username>/...` path gets. Shared mutex
        // (see its doc: "$HOME") keeps this from racing other env-mutating
        // tests in the crate.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_root("home-mask");
        fs::create_dir_all(&tmp).unwrap();
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let client = write_json_client(
            &tmp.join(".cursor"),
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "linear-mcp": {"command": "npx", "args": ["-y", "linear-mcp"]}
            }),
        );
        let (state, _state_guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(items.len(), 1);
        assert!(
            items[0].source_path.starts_with('~'),
            "source_path must be $HOME-masked: {}",
            items[0].source_path
        );
        assert!(
            !items[0]
                .source_path
                .contains(&tmp.to_string_lossy().to_string()),
            "the raw temp-dir-as-$HOME path must not survive masking: {}",
            items[0].source_path
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Review round 4: source_path must be unique per server ─────────────────

    #[test]
    fn two_servers_in_one_client_config_get_distinct_source_paths() {
        // Both servers share one config file — before the fix, both got the
        // same bare masked-config-path source_path, which the backend's
        // (user_id, kind, source_path) dedupe key would collapse into one
        // row, silently dropping the second server.
        let tmp = temp_root("two-servers-one-config");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "linear-mcp": {"command": "npx", "args": ["-y", "linear-mcp"]},
                "notion-mcp": {"command": "npx", "args": ["-y", "notion-mcp"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(
            items.len(),
            2,
            "both servers in the one config must be reported"
        );
        let source_paths: std::collections::HashSet<&str> =
            items.iter().map(|i| i.source_path.as_str()).collect();
        assert_eq!(
            source_paths.len(),
            2,
            "source_path must be unique per server, not shared per config file: {source_paths:?}"
        );
        for item in &items {
            assert!(
                item.source_path
                    .ends_with(&format!("#{}", item.detail.server_key)),
                "source_path must end with #<server key>: {}",
                item.source_path
            );
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn mask_home_dir_replaces_home_prefix_only() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_root("mask-home-dir-unit");
        fs::create_dir_all(&tmp).unwrap();
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let path = format!("{}/.cursor/mcp.json", tmp.display());
        let masked = mask_home_dir(&path);

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(masked, "~/.cursor/mcp.json");
    }

    // ── Review round 3: unknown flags must fail closed, not fall through ──────

    #[test]
    fn docker_unrecognized_flag_link_fails_closed() {
        let tmp = temp_root("docker-link-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "--link", "secret-container-name:alias", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier, None,
            "--link isn't a recognized flag; its value must never be reported as the image"
        );
        assert_never_leaks(&items[0], "secret-container-name");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_invented_unrecognized_flag_fails_closed() {
        let tmp = temp_root("docker-invented-flag");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "--frobnicate", "value", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier, None,
            "a completely unknown flag must fail closed, not fall through as a boolean switch"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_combined_short_booleans_still_find_the_image() {
        let tmp = temp_root("docker-combined-booleans");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {"command": "docker", "args": ["run", "-it", "image"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("image"),
            "a combined short-flag boolean (-it) must still be recognized as a switch"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_value_taking_flag_then_combined_boolean_finds_the_image() {
        let tmp = temp_root("docker-value-then-combined-boolean");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {
                    "command": "docker",
                    "args": ["run", "-e", "K=V", "-it", "image"]
                }
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].detail.package_identifier.as_deref(), Some("image"));
        assert_never_leaks(&items[0], "K=V");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn docker_double_dash_ends_option_parsing() {
        let tmp = temp_root("docker-double-dash");
        let client = write_json_client(
            &tmp,
            "Cursor",
            "mcpServers",
            serde_json::json!({
                "custom-mcp": {"command": "docker", "args": ["run", "--", "image"]}
            }),
        );
        let (state, _guard) = temp_state();

        let items = collect_mcp_discoveries(&[client], &state, "https://app.vectorhawk.ai");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].detail.package_identifier.as_deref(),
            Some("image"),
            "-- must end option parsing; the next arg is the candidate"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
