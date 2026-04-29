//! VectorHawk runner — shim library.
//!
//! Exposes [`run_shim`], the single entry point for the shim binary and for
//! the `vectorhawk mcp serve` subcommand.
//!
//! # Behaviour
//!
//! 1. Resolve the daemon socket path (platform-appropriate; see [`daemon_socket_path`]).
//! 2. Try to connect to the daemon socket with a 2-second timeout.
//! 3. On success: enter relay mode (`SocketBackend`). Each JSON-RPC frame from
//!    stdin is forwarded to the daemon; the daemon's response is written back to
//!    stdout.  **If the daemon dies mid-session** (socket I/O error on any
//!    frame), the shim silently switches to `EmbeddedBackend` for that frame and
//!    all subsequent frames. The AI client never sees a JSON-RPC `error` caused
//!    by the daemon dying.
//! 4. On connect failure or timeout: enter embedded mode immediately.
//!
//! # Mid-session fallback state machine
//!
//! ```text
//! Relaying(SocketBackend)  --[socket I/O error]-->  Embedded(EmbeddedBackend)
//! ```
//!
//! The transition is one-way for the session. Once fallen back, the shim stays
//! in embedded mode (no reconnect attempt to the daemon mid-session — that is
//! M1 scope).
//!
//! # Socket path
//!
//! The shim deliberately does NOT depend on `vectorhawkd-core` to avoid pulling
//! in `rusqlite` (bundled SQLite, ~1.5 MB of link weight). The socket path
//! function is duplicated here from `vectorhawkd-core::state`. The canonical
//! implementation lives there; any change must be mirrored here.
//!
//! Tracked in: TODO(M1) — extract socket path into `vectorhawkd-manifest` or a
//! zero-dep `vectorhawkd-paths` crate so neither core nor shim needs to duplicate it.

use anyhow::Result;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};
use vectorhawkd_mcp::{
    backend::{EmbeddedBackend, SocketBackend},
    protocol::{
        JsonRpcRequest, JsonRpcResponse, ToolCallParams, INTERNAL_ERROR, INVALID_PARAMS,
        METHOD_NOT_FOUND, PARSE_ERROR,
    },
};

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
/// One-way transition: `Relaying → Embedded` on socket I/O error.
enum SessionMode {
    /// Forwarding frames to the daemon over a Unix socket.
    #[cfg(unix)]
    Relaying(SocketBackend),
    /// In-process dispatch, no daemon involvement.
    Embedded(EmbeddedBackend),
}

// ── run_shim ──────────────────────────────────────────────────────────────────

/// Run the shim for one AI-client session.
///
/// Reads newline-delimited JSON-RPC from stdin, writes responses to stdout.
/// Returns when stdin closes (AI client disconnects) or on an unrecoverable error.
///
/// The shim tries the daemon socket first (2 s timeout). On failure it falls
/// back to `EmbeddedBackend` with a pre-registered stub tool set. If the daemon
/// dies mid-session, the shim transparently switches to `EmbeddedBackend` on the
/// failing frame without surfacing a JSON-RPC error to the AI client.
///
/// The fallback warning is written to stderr (WARN level via `tracing`); it does
/// not appear on stdout (which is the MCP wire).
pub async fn run_shim() -> Result<()> {
    let socket_path = daemon_socket_path();

    // Determine initial session mode.
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
                    warn!(
                        error = %e,
                        socket = %path.display(),
                        "daemon socket unreachable on startup — falling back to in-process embedded backend"
                    );
                    SessionMode::Embedded(make_embedded_backend())
                }
            }
        } else {
            warn!("daemon socket path unavailable — falling back to in-process embedded backend");
            SessionMode::Embedded(make_embedded_backend())
        }
    };

    #[cfg(not(unix))]
    let mut mode = {
        warn!("daemon socket relay not supported on this platform (M0) — using embedded backend");
        SessionMode::Embedded(make_embedded_backend())
    };

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = io::BufWriter::new(stdout.lock());

    info!("shim read-loop starting");

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — AI client disconnected
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                error!(error = %e, "failed to read from stdin");
                break;
            }
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = dispatch_line(line, &mut mode).await;

        if let Some(resp) = response {
            let serialized = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to serialize response");
                    continue;
                }
            };
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

    // Shutdown the embedded backend if we ended up there.
    if let SessionMode::Embedded(ref backend) = mode {
        use vectorhawkd_mcp::backend::Backend;
        backend.on_shutdown().await;
    }

    Ok(())
}

// ── Per-frame dispatch ────────────────────────────────────────────────────────

/// Dispatch one JSON-RPC line, potentially switching mode on socket failure.
///
/// Returns `None` for notifications (no `id`), `Some(response)` otherwise.
async fn dispatch_line(line: &str, mode: &mut SessionMode) -> Option<JsonRpcResponse> {
    // Parse the JSON-RPC request.
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

    // Notifications (no id) get no response.
    if request.id.is_none() {
        debug!(method = %request.method, "received notification, no response needed");
        return None;
    }

    Some(dispatch_request(request, mode).await)
}

/// Dispatch a parsed request, switching to embedded mode on socket failure.
async fn dispatch_request(request: JsonRpcRequest, mode: &mut SessionMode) -> JsonRpcResponse {
    let id = request.id.clone();

    #[cfg(unix)]
    if let SessionMode::Relaying(ref backend) = *mode {
        // Try the relay. On any error, fall through to embedded fallback.
        let relay_result = relay_via_socket(backend, &request).await;
        match relay_result {
            Ok(response) => return response,
            Err(e) => {
                warn!(
                    error = %e,
                    method = %request.method,
                    "daemon socket error mid-session — falling back to in-process embedded backend"
                );
                // One-way transition: switch to embedded for this frame and all future frames.
                *mode = SessionMode::Embedded(make_embedded_backend());
            }
        }
    }

    // Embedded path (either initial fallback or post-transition).
    match mode {
        SessionMode::Embedded(ref backend) => dispatch_to_embedded(backend, request).await,
        #[cfg(unix)]
        SessionMode::Relaying(_) => {
            // Unreachable: we always transition above before reaching here,
            // but the compiler needs an exhaustive match.
            JsonRpcResponse::error(id, INTERNAL_ERROR, "unexpected relaying state")
        }
    }
}

// ── Socket relay ──────────────────────────────────────────────────────────────

/// Send the request over the socket and return the daemon's JSON-RPC response.
///
/// Returns `Err` on any I/O failure (broken pipe, closed socket, timeout).
/// The caller is responsible for switching to embedded mode on error.
#[cfg(unix)]
async fn relay_via_socket(
    backend: &SocketBackend,
    request: &JsonRpcRequest,
) -> Result<JsonRpcResponse> {
    use vectorhawkd_mcp::backend::Backend;
    use vectorhawkd_mcp::protocol::{InitializeResult, ToolCallResult, ToolsListResult};

    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            let result: InitializeResult = backend.initialize(request.params.clone()).await?;
            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        "tools/list" => {
            let result: ToolsListResult = backend.list_tools(request.params.clone()).await?;
            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        "tools/call" => {
            let params: ToolCallParams = serde_json::from_value(request.params.clone())
                .map_err(|e| anyhow::anyhow!("invalid tool call params: {e}"))?;
            let result: ToolCallResult = backend.call_tool(params).await?;
            let value = serde_json::to_value(result).unwrap_or_default();
            Ok(JsonRpcResponse::success(id, value))
        }
        other => {
            // Unknown method: return METHOD_NOT_FOUND (not a relay error).
            // Wrap in Ok so the caller does not trigger fallback for this.
            Ok(JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            ))
        }
    }
}

// ── Embedded dispatch ─────────────────────────────────────────────────────────

/// Dispatch a request to the in-process embedded backend.
async fn dispatch_to_embedded(
    backend: &EmbeddedBackend,
    request: JsonRpcRequest,
) -> JsonRpcResponse {
    use vectorhawkd_mcp::backend::Backend;

    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => match backend.initialize(request.params).await {
            Ok(result) => {
                JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
            }
            Err(e) => {
                warn!(error = %e, "embedded initialize failed");
                JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string())
            }
        },
        "tools/list" => match backend.list_tools(request.params).await {
            Ok(result) => {
                JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
            }
            Err(e) => {
                warn!(error = %e, "embedded tools/list failed");
                JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string())
            }
        },
        "tools/call" => {
            let params: ToolCallParams = match serde_json::from_value(request.params) {
                Ok(p) => p,
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        INVALID_PARAMS,
                        format!("invalid tool call params: {e}"),
                    );
                }
            };
            match backend.call_tool(params).await {
                Ok(result) => {
                    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
                }
                Err(e) => {
                    warn!(error = %e, "embedded tools/call failed");
                    JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string())
                }
            }
        }
        other => {
            debug!(method = %other, "unknown method");
            JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("unknown method: {other}"))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Construct the stub embedded backend used for the fallback path.
fn make_embedded_backend() -> EmbeddedBackend {
    EmbeddedBackend::with_stub_backend(
        "vectorhawk",
        &[
            ("list_skills", "List installed VectorHawk skills"),
            ("run_skill", "Run a VectorHawk skill"),
            ("install_skill", "Install a skill from the registry"),
            ("search_skills", "Search the skill registry"),
            ("get_status", "Get VectorHawk runner status"),
        ],
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
