//! Child-process wrapper for stdio MCP backends.
//!
//! `StdioProcess` spawns and manages a child MCP server process, communicating
//! with it over line-delimited JSON-RPC on stdin/stdout.
//!
//! # Protocol
//!
//! Each message is a single UTF-8 JSON object followed by `\n`. This matches
//! the protocol used by most MCP servers (newline-delimited JSON-RPC).
//!
//! # Timeout handling
//!
//! Reading from a blocking pipe can hang forever if the backend stalls. We
//! move the stdout reader into a background thread and communicate via a
//! `std::sync::mpsc::channel`. The caller uses `recv_timeout` so a hung
//! backend is detected after `READ_TIMEOUT`.
//!
//! # Async usage
//!
//! `StdioProcess` is synchronous internally (blocking I/O). Callers in an
//! async context must wrap calls with `tokio::task::spawn_blocking`.

use crate::aggregator::ToolDefinition;
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, BufWriter, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};
use tracing::{debug, warn};

/// Timeout for a single JSON-RPC read from a backend process.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for the initial `initialize` handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(15);

// ── StdioProcess ──────────────────────────────────────────────────────────────

/// A live child MCP server process communicating over stdio JSON-RPC.
///
/// Owns the child handle, a buffered writer to stdin, and a channel receiver
/// that drains lines from a background reader thread on stdout.
#[derive(Debug)]
pub struct StdioProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    /// Lines received from the child's stdout via the reader thread.
    rx: Receiver<String>,
    /// Monotonically increasing JSON-RPC request ID.
    next_id: u64,
    /// Whether we have successfully completed the MCP initialize handshake.
    initialized: bool,
}

impl StdioProcess {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Spawn a child process and return a `StdioProcess` connected to it.
    ///
    /// Environment variables from `env` are layered on top of the current
    /// process environment (inherited) rather than replacing it.
    pub fn spawn(
        command: &str,
        args: &[impl AsRef<str>],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        for arg in args {
            cmd.arg(arg.as_ref());
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn stdio backend '{command}'"))?;

        let child_stdin = child
            .stdin
            .take()
            .context("child process did not open a stdin pipe")?;
        let child_stdout = child
            .stdout
            .take()
            .context("child process did not open a stdout pipe")?;

        let rx = spawn_reader_thread(child_stdout);

        Ok(Self {
            child,
            stdin: BufWriter::new(child_stdin),
            rx,
            next_id: 1,
            initialized: false,
        })
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Perform the MCP `initialize` handshake.
    ///
    /// Must be called before `list_tools` or `call_tool`. Calling it a second
    /// time is a no-op (idempotent).
    pub fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let id = self.next_id();
        self.send_request(
            id,
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "vectorhawkd-mcp-aggregator",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
        .context("failed to send initialize request")?;

        let resp = self
            .recv_line(INIT_TIMEOUT)
            .context("no response to initialize request")?;
        let body: serde_json::Value =
            serde_json::from_str(&resp).context("initialize response is not valid JSON")?;

        if body.get("error").is_some() {
            bail!(
                "backend rejected initialize: {}",
                body["error"]["message"].as_str().unwrap_or("unknown error")
            );
        }

        // Send the required `notifications/initialized` notification (no response expected).
        self.send_notification("notifications/initialized")
            .context("failed to send initialized notification")?;

        self.initialized = true;
        debug!("MCP initialize handshake complete");
        Ok(())
    }

    /// Fetch the tool list from the backend via `tools/list`.
    ///
    /// Calls `initialize` automatically if not already done.
    pub fn list_tools(&mut self) -> Result<Vec<ToolDefinition>> {
        self.ensure_initialized()?;

        let id = self.next_id();
        self.send_request(id, "tools/list", serde_json::json!({}))
            .context("failed to send tools/list request")?;

        let body = self
            .recv_matching_response(id)
            .context("no response to tools/list")?;

        if let Some(err) = body.get("error") {
            bail!("backend returned JSON-RPC error on tools/list: {err}");
        }

        let tools = extract_tools_from_response(&body);
        debug!(count = tools.len(), "received tool list from backend");
        Ok(tools)
    }

    /// Call a named tool on the backend.
    ///
    /// Returns the raw `result` value from the JSON-RPC response.
    /// Calls `initialize` automatically if not already done.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.ensure_initialized()?;

        let id = self.next_id();
        self.send_request(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
        .with_context(|| format!("failed to send tools/call for '{name}'"))?;

        let body = self
            .recv_matching_response(id)
            .with_context(|| format!("no response to tools/call '{name}'"))?;

        if let Some(err) = body.get("error") {
            bail!(
                "backend returned JSON-RPC error: {}",
                err["message"].as_str().unwrap_or("unknown")
            );
        }

        Ok(body
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Returns true if the child process is still running.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,              // still running
            Ok(Some(_)) | Err(_) => false, // exited or error
        }
    }

    /// Gracefully shut down the backend: kill the child if still running.
    ///
    /// Drops the stdin pipe first so the child receives EOF, giving it a
    /// chance to exit cleanly before we send SIGKILL (on Unix).
    pub fn shutdown(&mut self) -> Result<()> {
        if !self.is_alive() {
            return Ok(());
        }
        // Flush any buffered writes so the child receives them before EOF.
        let _ = self.stdin.flush();
        // Kill forcefully — cross-platform (SIGKILL on Unix, TerminateProcess on Windows).
        self.child.kill().unwrap_or(());
        let _ = self.child.wait();
        debug!("stdio backend process terminated");
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn ensure_initialized(&mut self) -> Result<()> {
        if !self.initialized {
            self.initialize()
                .context("auto-initialize before tool operation failed")?;
        }
        Ok(())
    }

    /// Serialize and write a JSON-RPC request to the child's stdin.
    fn send_request(&mut self, id: u64, method: &str, params: serde_json::Value) -> Result<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&msg).context("failed to serialize JSON-RPC request")?;
        self.stdin
            .write_all(line.as_bytes())
            .context("failed to write to child stdin")?;
        self.stdin
            .write_all(b"\n")
            .context("failed to write newline to child stdin")?;
        self.stdin.flush().context("failed to flush child stdin")?;
        Ok(())
    }

    /// Serialize and write a JSON-RPC notification (no id, no response expected).
    fn send_notification(&mut self, method: &str) -> Result<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        let line = serde_json::to_string(&msg).context("failed to serialize notification")?;
        self.stdin
            .write_all(line.as_bytes())
            .context("failed to write notification to child stdin")?;
        self.stdin
            .write_all(b"\n")
            .context("failed to write newline to child stdin")?;
        self.stdin.flush().context("failed to flush child stdin")?;
        Ok(())
    }

    /// Read the next line from the reader-thread channel with a timeout.
    fn recv_line(&self, timeout: Duration) -> Result<String> {
        self.rx
            .recv_timeout(timeout)
            .with_context(|| format!("backend read timed out after {timeout:?}"))
    }

    /// Read frames until we get a JSON-RPC response with the requested `id`,
    /// skipping any notifications (no `id`) or stray frames that arrive in
    /// between. Some MCP servers (e.g. @modelcontextprotocol/server-everything)
    /// emit a `notifications/tools/list_changed` immediately after initialize
    /// or between request-response pairs — without this skip-loop, that
    /// notification would be consumed as the response and discovery would
    /// fail with "missing result.tools array".
    fn recv_matching_response(&self, expected_id: u64) -> Result<serde_json::Value> {
        loop {
            let line = self.recv_line(READ_TIMEOUT)?;
            let body: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("backend produced invalid JSON: {line}"))?;
            // Notifications have no `id`; skip them.
            let Some(id_val) = body.get("id") else {
                debug!(
                    method = body.get("method").and_then(|m| m.as_str()).unwrap_or("?"),
                    "skipping notification while awaiting response"
                );
                continue;
            };
            // Compare by numeric value; ids are u64 in our send_request.
            if id_val.as_u64() == Some(expected_id) {
                return Ok(body);
            }
            debug!(
                got = id_val.to_string(),
                expected = expected_id,
                "skipping stray response with non-matching id"
            );
        }
    }
}

impl Drop for StdioProcess {
    fn drop(&mut self) {
        if self.is_alive() {
            warn!("StdioProcess dropped while child is still running — killing");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ── Background reader thread ───────────────────────────────────────────────────

/// Spawn a background thread that reads lines from `stdout` and sends them
/// through the returned `Receiver`. The thread exits when stdout closes (EOF).
fn spawn_reader_thread(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) if !l.trim().is_empty() => {
                    if tx.send(l).is_err() {
                        break; // receiver dropped — stop reading
                    }
                }
                Ok(_) => {} // skip blank lines
                Err(e) => {
                    debug!(error = %e, "reader thread: error reading stdout — stopping");
                    break;
                }
            }
        }
        debug!("reader thread: stdout closed");
    });
    rx
}

// ── Response parsing helpers ──────────────────────────────────────────────────

/// Extract a `Vec<ToolDefinition>` from a `tools/list` response body.
///
/// Gracefully returns an empty vec if the response does not contain the
/// expected structure — we prefer degraded service over a panic.
pub(crate) fn extract_tools_from_response(body: &serde_json::Value) -> Vec<ToolDefinition> {
    let Some(tools_arr) = body
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
    else {
        warn!("tools/list response missing result.tools array");
        return vec![];
    };

    tools_arr
        .iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let description = v
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from);
            let input_schema = v.get("inputSchema").cloned();
            Some(ToolDefinition {
                name,
                description,
                input_schema,
            })
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ECHO_SCRIPT: &str = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue

    req_id = req.get("id")
    method = req.get("method", "")

    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "echo-mcp", "version": "0.0.1"}
        }}) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {
            "tools": [{"name": "echo_tool", "description": "Echoes back input",
                       "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {
            "content": [{"type": "text", "text": json.dumps(args)}],
            "isError": False
        }}) + "\n")
        sys.stdout.flush()
"#;

    fn spawn_echo() -> StdioProcess {
        let env = {
            let mut m = HashMap::new();
            m.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
            m
        };
        StdioProcess::spawn("python3", &["-c", ECHO_SCRIPT], &env).expect("spawn echo server")
    }

    #[test]
    fn initialize_completes_handshake() {
        let mut proc = spawn_echo();
        proc.initialize().expect("initialize should succeed");
        assert!(proc.initialized);
    }

    #[test]
    fn initialize_is_idempotent() {
        let mut proc = spawn_echo();
        proc.initialize().expect("first initialize");
        proc.initialize()
            .expect("second initialize should be no-op");
    }

    #[test]
    fn list_tools_returns_expected_tools() {
        let mut proc = spawn_echo();
        let tools = proc.list_tools().expect("list_tools should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo_tool");
        assert!(tools[0].description.is_some());
    }

    #[test]
    fn call_tool_round_trips_arguments() {
        let mut proc = spawn_echo();
        let args = serde_json::json!({"text": "hello"});
        let result = proc
            .call_tool("echo_tool", &args)
            .expect("call_tool should succeed");
        let text = result["content"][0]["text"].as_str().expect("text field");
        let echoed: serde_json::Value = serde_json::from_str(text).expect("echoed JSON");
        assert_eq!(echoed["text"], "hello");
    }

    #[test]
    fn is_alive_returns_true_for_running_process() {
        let mut proc = spawn_echo();
        assert!(proc.is_alive());
    }

    #[test]
    fn shutdown_terminates_process() {
        let mut proc = spawn_echo();
        proc.initialize().expect("init");
        proc.shutdown().expect("shutdown");
        assert!(!proc.is_alive());
    }
}
