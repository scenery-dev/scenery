//! VectorHawk runner — shim library.
//!
//! Exposes [`run_shim`], the single entry point for the shim binary and for
//! the `vectorhawk mcp serve` subcommand.
//!
//! # Single-mode posture (M4)
//!
//! The shim is daemon-only. It tries the daemon socket on startup. On any
//! failure (initial connect or mid-session socket death), the shim does NOT
//! fall back to in-process execution. Instead it enters `DaemonRequired`
//! mode and answers every JSON-RPC request with a structured error
//! containing install/restart instructions. The AI client surfaces the
//! error to the user, who runs `vectorhawk daemon install` (or restarts
//! the existing agent) and retries. There is no in-process degradation
//! mode in production code.
//!
//! # State machine
//!
//! ```text
//! Relaying(SocketBackend)  --[socket I/O error]-->  DaemonRequired
//! ```
//!
//! The transition is one-way for the session: once in `DaemonRequired`,
//! the shim never reconnects mid-session. AI clients are expected to
//! handle reconnect-with-backoff per the MCP spec — they re-spawn the
//! shim, which retries the daemon socket from scratch.
//!
//! # Why not silently fall back to embedded?
//!
//! Up through M3 the shim transparently switched to an in-process
//! `EmbeddedBackend` with stub tools (list_skills, run_skill, etc.) on
//! daemon failure. The stub responses were hardcoded, not real — the AI
//! client believed VectorHawk was working when it wasn't. M4 deletes
//! that silent-degradation path: a missing daemon must be visible to the
//! user.
//!
//! `EmbeddedBackend` still exists in `vectorhawkd-mcp::backend` for tests
//! and unit-test scaffolding, but no shim production code constructs it.
//!
//! # Socket path
//!
//! The shim deliberately does NOT depend on `vectorhawkd-core` to avoid pulling
//! in `rusqlite` (bundled SQLite, ~1.5 MB of link weight). The socket path
//! function is duplicated here from `vectorhawkd-core::state`. The canonical
//! implementation lives there; any change must be mirrored here.

use anyhow::Result;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
use tracing::{debug, error, info, warn};
use vectorhawkd_mcp::{
    backend::SocketBackend,
    protocol::{JsonRpcRequest, JsonRpcResponse, PARSE_ERROR},
};

// ── JSON-RPC error code for the daemon-required state ────────────────────────

/// Server-defined JSON-RPC error code for the daemon-required state.
///
/// Per JSON-RPC 2.0, the range -32000..=-32099 is reserved for server-defined
/// errors. We use -32001 for "VectorHawk daemon unreachable".
const DAEMON_UNREACHABLE: i64 = -32001;

// ── Socket path ───────────────────────────────────────────────────────────────

/// Return the platform-appropriate path for the daemon Unix socket.
///
/// Mirrors `vectorhawkd_core::state::AppState::socket_path` exactly.
/// See that function for rationale. Do not diverge.
///
/// - macOS: `~/Library/Application Support/VectorHawk/agent.sock`
/// - Linux: `$XDG_RUNTIME_DIR/vectorhawk/agent.sock`
///   (fallback: `~/.local/share/VectorHawk/agent.sock`)
pub fn daemon_socket_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let candidate = PathBuf::from(runtime).join("vectorhawk").join("agent.sock");
            return Some(candidate);
        }
    }
    // macOS and Linux fallback: data dir
    let base = dirs::data_dir()?;
    Some(base.join("VectorHawk").join("agent.sock"))
}

// ── Session mode ──────────────────────────────────────────────────────────────

/// The shim's current dispatch mode for the session.
///
/// One-way transition: `Relaying → DaemonRequired`. Once degraded, the shim
/// stays there for the lifetime of the session. Reconnect is the AI client's
/// responsibility (re-spawn the shim).
enum SessionMode {
    /// Forwarding frames to the daemon over a Unix socket.
    #[cfg(unix)]
    Relaying(SocketBackend),
    /// Daemon unreachable. Every request gets a structured JSON-RPC error.
    DaemonRequired { reason: String },
}

// ── run_shim ──────────────────────────────────────────────────────────────────

/// Run the shim for one AI-client session.
///
/// When `server` is `Some(slug)` the shim acts as a single-backend adapter:
/// `tools/list` is filtered to only the tools whose namespaced name begins
/// with `<slug>__`, and the prefix is stripped before returning them to the
/// AI client. On `tools/call` the prefix is re-added before forwarding to
/// the daemon.
///
/// Reads newline-delimited JSON-RPC from stdin, writes responses to stdout.
/// Returns when stdin closes (AI client disconnects) or on an unrecoverable error.
///
/// The shim tries the daemon socket first (2 s timeout). On any failure it
/// enters `DaemonRequired` mode and serves the same JSON-RPC error to every
/// subsequent request — never an in-process degradation in production.
pub async fn run_shim(server: Option<String>) -> Result<()> {
    let socket_path = daemon_socket_path();

    #[cfg(unix)]
    let mut mode = {
        if let Some(ref path) = socket_path {
            let probe = SocketBackend::new(path);
            match probe.connect().await {
                Ok(()) => {
                    info!(socket = %path.display(), "connected to daemon socket");
                    SessionMode::Relaying(probe)
                }
                Err(e) => {
                    let reason = format!("daemon socket {} unreachable: {e}", path.display());
                    warn!(%reason, "shim entering daemon-required mode");
                    SessionMode::DaemonRequired { reason }
                }
            }
        } else {
            let reason = "daemon socket path is unavailable on this platform".to_string();
            warn!(%reason, "shim entering daemon-required mode");
            SessionMode::DaemonRequired { reason }
        }
    };

    #[cfg(not(unix))]
    let mut mode = {
        let reason = "daemon socket relay not supported on this platform".to_string();
        warn!(%reason, "shim entering daemon-required mode");
        SessionMode::DaemonRequired { reason }
    };

    let mut reader = TokioBufReader::new(tokio::io::stdin());

    // Serializing lock around stdout so the main loop's request responses
    // and the notification pump's frames can't interleave at the byte level.
    // io::stdout() has internal stdlib locking but doesn't keep it across
    // separate writeln+flush calls; this mutex makes each full frame atomic.
    let stdout_lock = Arc::new(StdMutex::new(()));

    // Spawn a background task that opens a SECOND socket connection
    // dedicated to receiving server→client notifications (e.g.
    // notifications/tools/list_changed). The daemon broadcasts these to
    // every connected session, so the pump just reads frames and forwards
    // notification frames (no id) to stdout. The original relay socket
    // keeps doing request/response on the same connection.
    if let SessionMode::Relaying(_) = &mode {
        if let Some(ref path) = socket_path {
            let path = path.clone();
            let stdout_lock = Arc::clone(&stdout_lock);
            tokio::spawn(async move {
                if let Err(e) = run_notification_pump(path, stdout_lock).await {
                    warn!(error = %e, "notification pump exited");
                }
            });
        }
    }

    info!("shim read-loop starting");

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF — AI client disconnected
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                error!(error = %e, "failed to read from stdin");
                break;
            }
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = dispatch_line(line, &mut mode, server.as_deref()).await;

        if let Some(resp) = response {
            let serialized = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to serialize response");
                    continue;
                }
            };
            let _guard = stdout_lock.lock().unwrap();
            let stdout = io::stdout();
            let mut writer = stdout.lock();
            if let Err(e) = writeln!(writer, "{serialized}") {
                error!(error = %e, "failed to write to stdout");
                break;
            }
            if let Err(e) = writer.flush() {
                error!(error = %e, "failed to flush stdout");
                break;
            }
        }
    }

    info!("shim read-loop exiting");
    Ok(())
}

// ── Notification pump ─────────────────────────────────────────────────────────

/// Background task: open a dedicated socket connection and forward any
/// JSON-RPC notification frames (those without an `id`) to stdout.
///
/// The daemon's `socket_dispatch` broadcasts `notifications/tools/list_changed`
/// to every connected session, so a second connection is the simplest way to
/// receive notifications without contending with the primary relay loop's
/// read of the same socket. Frames with an `id` (responses) on this connection
/// are ignored — they belong to no in-flight request from this pump.
#[cfg(unix)]
async fn run_notification_pump(socket_path: PathBuf, stdout_lock: Arc<StdMutex<()>>) -> Result<()> {
    use tokio::net::UnixStream;
    use vectorhawkd_mcp::backend::read_framed;

    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("notification pump connect failed: {e}"))?;
    debug!(socket = %socket_path.display(), "notification pump connected");
    let (mut reader, _writer) = stream.into_split();

    loop {
        let frame = match read_framed(&mut reader).await {
            Ok(Some(f)) => f,
            Ok(None) => {
                debug!("notification pump: daemon closed socket");
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow::anyhow!("notification pump read error: {e}"));
            }
        };

        // Only forward notifications (no `id`). Responses on this connection
        // are stray and ignored.
        let parsed: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "notification pump: dropping unparseable frame");
                continue;
            }
        };
        if parsed.get("id").is_some() || parsed.get("method").is_none() {
            continue;
        }

        let serialized = String::from_utf8_lossy(&frame);
        let _guard = stdout_lock.lock().unwrap();
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        if let Err(e) = writeln!(writer, "{serialized}") {
            return Err(anyhow::anyhow!("notification pump write failed: {e}"));
        }
        if let Err(e) = writer.flush() {
            return Err(anyhow::anyhow!("notification pump flush failed: {e}"));
        }
    }
}

// ── Per-frame dispatch ────────────────────────────────────────────────────────

/// Dispatch one JSON-RPC line, potentially switching mode on socket failure.
///
/// Returns `None` for notifications (no `id`), `Some(response)` otherwise.
///
/// `server_slug` — when `Some`, the shim is in single-server mode (F2):
/// `tools/list` is filtered and de-prefixed; `tools/call` is re-prefixed.
async fn dispatch_line(
    line: &str,
    mode: &mut SessionMode,
    server_slug: Option<&str>,
) -> Option<JsonRpcResponse> {
    let request: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Some(JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                format!("invalid JSON: {e}"),
            ));
        }
    };

    debug!(method = %request.method, id = ?request.id, "received request");

    if request.id.is_none() {
        debug!(method = %request.method, "received notification, no response needed");
        return None;
    }

    Some(dispatch_request(request, mode, server_slug).await)
}

/// Dispatch a parsed request, switching to `DaemonRequired` on socket failure.
async fn dispatch_request(
    request: JsonRpcRequest,
    mode: &mut SessionMode,
    server_slug: Option<&str>,
) -> JsonRpcResponse {
    let id = request.id.clone();

    #[cfg(unix)]
    if let SessionMode::Relaying(ref backend) = *mode {
        match relay_via_socket(backend, &request, server_slug).await {
            Ok(response) => return response,
            Err(e) => {
                let reason = format!("daemon socket error: {e}");
                warn!(%reason, method = %request.method, "shim transitioning to daemon-required mode");
                *mode = SessionMode::DaemonRequired { reason };
            }
        }
    }

    match mode {
        SessionMode::DaemonRequired { reason } => daemon_required_error(id, reason),
        #[cfg(unix)]
        SessionMode::Relaying(_) => {
            // Unreachable: the relay branch above either returned a response
            // or transitioned to DaemonRequired.
            JsonRpcResponse::error(
                id,
                DAEMON_UNREACHABLE,
                "internal: relaying state should have been handled".to_string(),
            )
        }
    }
}

// ── Socket relay ──────────────────────────────────────────────────────────────

/// Send the request over the socket and return the daemon's JSON-RPC response.
///
/// Returns `Err` on any I/O failure (broken pipe, closed socket, timeout).
/// The caller is responsible for switching to `DaemonRequired` mode on error.
///
/// When `server_slug` is `Some(slug)`:
/// - `tools/list`: filters to tools named `<slug>__*`, strips the prefix.
/// - `tools/call`: re-adds `<slug>__` prefix before forwarding to daemon.
#[cfg(unix)]
async fn relay_via_socket(
    backend: &SocketBackend,
    request: &JsonRpcRequest,
    server_slug: Option<&str>,
) -> Result<JsonRpcResponse> {
    use vectorhawkd_mcp::backend::Backend;
    use vectorhawkd_mcp::protocol::{
        InitializeResult, ToolCallParams, ToolCallResult, ToolsListResult, METHOD_NOT_FOUND,
    };

    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            let result: InitializeResult = backend.initialize(request.params.clone()).await?;
            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        "tools/list" => {
            let mut result: ToolsListResult = backend.list_tools(request.params.clone()).await?;

            if let Some(slug) = server_slug {
                // Per-server shim: keep only this backend's tools.
                result = filter_tools_for_server(result, slug);
            } else {
                // Aggregator shim: hide backends already surfaced as
                // per-server entries in ~/.claude.json so the AI client
                // doesn't see them under both namespaces.
                let exclusions = load_aggregator_exclusions_from_claude_json();
                result = filter_tools_for_aggregator(result, &exclusions);
            }

            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        "tools/call" => {
            let mut params: ToolCallParams = serde_json::from_value(request.params.clone())
                .map_err(|e| anyhow::anyhow!("invalid tool call params: {e}"))?;

            // Re-add the slug prefix so the daemon can dispatch to the right backend.
            if let Some(slug) = server_slug {
                params.name = format!("{slug}__{}", params.name);
            }

            let result: ToolCallResult = backend.call_tool(params).await?;
            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        other => Ok(JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    }
}

/// Filter `ToolsListResult` to only the tools belonging to `slug`, stripping
/// the `<slug>__` prefix from each tool's name.
///
/// Tools whose names do not start with `<slug>__` are dropped entirely.
/// This gives the AI client a clean, prefix-free tool list as if it were
/// talking to the backend directly.
#[cfg(unix)]
fn filter_tools_for_server(
    mut result: vectorhawkd_mcp::protocol::ToolsListResult,
    slug: &str,
) -> vectorhawkd_mcp::protocol::ToolsListResult {
    let prefix = format!("{slug}__");
    let mut kept = Vec::new();

    for mut tool in result.tools {
        if let Some(bare_name) = tool.name.strip_prefix(&prefix) {
            tool.name = bare_name.to_string();
            kept.push(tool);
        }
        // Tools that don't belong to this server are silently dropped.
    }

    result.tools = kept;
    result
}

/// Filter `ToolsListResult` to drop any `<slug>__*` tools whose slug is in
/// `excluded_slugs`.
///
/// Used by the aggregator shim (`vectorhawk mcp serve` with no `--server`
/// flag) to hide backends that are already surfaced to the AI client as
/// per-server entries in `~/.claude.json`. Without this, the AI client sees
/// the same tool twice — once natively (via the per-server shim spawned for
/// that slug) and once nested under the `vectorhawk` aggregator namespace.
///
/// Unprefixed tools (`vectorhawk_*` management tools, etc.) are always kept.
#[cfg(unix)]
fn filter_tools_for_aggregator(
    mut result: vectorhawkd_mcp::protocol::ToolsListResult,
    excluded_slugs: &std::collections::HashSet<String>,
) -> vectorhawkd_mcp::protocol::ToolsListResult {
    if excluded_slugs.is_empty() {
        return result;
    }
    result.tools.retain(|tool| {
        if let Some(idx) = tool.name.find("__") {
            let slug = &tool.name[..idx];
            !excluded_slugs.contains(slug)
        } else {
            true
        }
    });
    result
}

/// Read `~/.claude.json` and return the set of `mcpServers` keys, excluding
/// the `vectorhawk` aggregator entry itself.
///
/// Each remaining key represents a backend that the AI client reaches
/// natively (via its own per-server shim), so those slugs should NOT also
/// be exposed under the aggregator's namespace.
///
/// Returns an empty set if `~/.claude.json` is missing or malformed — fail
/// open so the shim is never blocked from serving tools/list.
#[cfg(unix)]
fn load_aggregator_exclusions_from_claude_json() -> std::collections::HashSet<String> {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".claude.json"),
        None => return std::collections::HashSet::new(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return std::collections::HashSet::new(),
    };
    let root: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return std::collections::HashSet::new(),
    };
    let mut out = std::collections::HashSet::new();
    if let Some(map) = root.get("mcpServers").and_then(|v| v.as_object()) {
        for key in map.keys() {
            if key != "vectorhawk" {
                out.insert(key.clone());
            }
        }
    }
    out
}

// ── DaemonRequired error response ─────────────────────────────────────────────

/// Build the standard JSON-RPC error response served while the shim is in
/// `DaemonRequired` mode. The same shape is returned for every request so
/// AI clients can display a consistent message to the user.
fn daemon_required_error(id: Option<serde_json::Value>, reason: &str) -> JsonRpcResponse {
    let message = format!(
        "VectorHawk daemon unreachable. Run `vectorhawk daemon install` to install \
         and start it, or restart it with `launchctl kickstart gui/$(id -u)/com.vectorhawk.agent` \
         (macOS) / `systemctl --user start vectorhawk-agent` (Linux). Detail: {reason}"
    );
    JsonRpcResponse::error(id, DAEMON_UNREACHABLE, message)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
