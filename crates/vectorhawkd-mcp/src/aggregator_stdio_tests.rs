//! Integration tests for stdio backend wiring in the aggregator.
//!
//! These tests configure a `BackendRegistry` with a stdio backend and exercise
//! the full register → list_tools → dispatch pipeline without a real MCP binary.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod aggregator_stdio_tests {
    use crate::aggregator::{
        BackendEntry, BackendRegistry, BackendTransport, ToolDefinition, ToolVisibility,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // ── Python echo server (inline, no external dependency) ───────────────────

    const ECHO_SERVER_SCRIPT: &str = r#"
import sys, json

def respond(req_id, result):
    msg = {"jsonrpc": "2.0", "id": req_id, "result": result}
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

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
        respond(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "echo-mcp", "version": "0.0.1"}
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(req_id, {
            "tools": [{
                "name": "echo_tool",
                "description": "Echoes back the input",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
            }]
        })
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        respond(req_id, {
            "content": [{"type": "text", "text": json.dumps(args)}],
            "isError": False
        })
    else:
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0", "id": req_id,
            "error": {"code": -32601, "message": f"not found: {method}"}
        }) + "\n")
        sys.stdout.flush()
"#;

    /// Build a `BackendEntry` with a Stdio transport pointing at the echo server.
    fn echo_backend_entry() -> BackendEntry {
        let mut env = HashMap::new();
        env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());

        BackendEntry {
            server_id: "echo".to_string(),
            name: "Echo MCP".to_string(),
            transport: BackendTransport::Stdio {
                command: "python3".to_string(),
                args: vec!["-c".to_string(), ECHO_SERVER_SCRIPT.to_string()],
                env,
                process: Arc::new(Mutex::new(None)),
            },
            tools: vec![ToolDefinition {
                name: "echo_tool".to_string(),
                description: Some("Echoes back the input".to_string()),
                input_schema: None,
            }],
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        }
    }

    // ── register → all_tools ──────────────────────────────────────────────────

    #[test]
    fn registered_stdio_backend_appears_in_all_tools() {
        let registry = BackendRegistry::new();
        registry.register_backend(echo_backend_entry());

        let tools = registry.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"].as_str().unwrap_or(""), "echo__echo_tool");
    }

    // ── dispatch ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_routes_tool_call_to_stdio_backend() {
        let registry = BackendRegistry::new();
        registry.register_backend(echo_backend_entry());

        let args = serde_json::json!({"text": "round-trip"});
        let result = registry
            .dispatch("echo__echo_tool", &args)
            .await
            .expect("tool should be recognized by aggregator")
            .expect("dispatch should succeed");

        // Echo server wraps args in content[0].text as JSON.
        let text = result["content"][0]["text"].as_str().expect("text field");
        let echoed: serde_json::Value = serde_json::from_str(text).expect("text is JSON");
        assert_eq!(echoed["text"], "round-trip");
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_skill_tool() {
        let registry = BackendRegistry::new();
        // Non-namespaced tool — belongs to the skill layer.
        assert!(registry
            .dispatch("vectorhawk_search", &serde_json::Value::Null)
            .await
            .is_none());
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn shutdown_terminates_stdio_processes() {
        let registry = BackendRegistry::new();
        registry.register_backend(echo_backend_entry());

        // Trigger a dispatch to spawn the process.
        let _ = registry
            .dispatch("echo__echo_tool", &serde_json::json!({}))
            .await;

        // Shutdown should not hang.
        registry.shutdown();
        assert_eq!(registry.backend_count(), 0);
    }

    // ── health tracking ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_error_increments_health_counter() {
        use crate::aggregator::UNHEALTHY_THRESHOLD;

        let registry = BackendRegistry::new();
        // Register a backend that will always fail (bad command).
        let bad_entry = BackendEntry {
            server_id: "bad".to_string(),
            name: "Bad Backend".to_string(),
            transport: BackendTransport::Stdio {
                command: "nonexistent_command_xyz_abc".to_string(),
                args: vec![],
                env: HashMap::new(),
                process: Arc::new(Mutex::new(None)),
            },
            tools: vec![ToolDefinition {
                name: "some_tool".to_string(),
                description: None,
                input_schema: None,
            }],
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        };
        registry.register_backend(bad_entry);

        // Fire enough errors to hit the unhealthy threshold.
        for _ in 0..UNHEALTHY_THRESHOLD {
            let _ = registry
                .dispatch("bad__some_tool", &serde_json::json!({}))
                .await;
        }

        // Backend should now be unhealthy and excluded from all_tools.
        assert!(registry.unhealthy_backends().contains(&"bad".to_string()));
        assert!(registry.all_tools().is_empty());
    }

    #[tokio::test]
    async fn mark_healthy_re_exposes_tools() {
        use crate::aggregator::UNHEALTHY_THRESHOLD;

        let registry = BackendRegistry::new();
        let bad_entry = BackendEntry {
            server_id: "bad".to_string(),
            name: "Bad".to_string(),
            transport: BackendTransport::Stdio {
                command: "nonexistent_xyz".to_string(),
                args: vec![],
                env: HashMap::new(),
                process: Arc::new(Mutex::new(None)),
            },
            tools: vec![ToolDefinition {
                name: "t".to_string(),
                description: None,
                input_schema: None,
            }],
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        };
        registry.register_backend(bad_entry);

        for _ in 0..UNHEALTHY_THRESHOLD {
            let _ = registry.dispatch("bad__t", &serde_json::json!({})).await;
        }
        assert!(!registry.unhealthy_backends().is_empty());

        registry.mark_healthy("bad");
        assert!(registry.unhealthy_backends().is_empty());
    }

    // ── process restart ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_restarts_dead_process_and_succeeds() {
        // Server that exits after the first tools/call.
        let script = r#"
import sys, json

call_count = 0

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"flaky","version":"0"}
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/list":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "tools":[{"name":"flaky_tool","description":"flaky","inputSchema":{"type":"object","properties":{}}}]
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        call_count += 1
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "content":[{"type":"text","text":"ok"}]
        }}) + "\n")
        sys.stdout.flush()
        if call_count == 1:
            sys.exit(0)
"#;
        let mut env = HashMap::new();
        env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());

        let flaky_entry = BackendEntry {
            server_id: "flaky".to_string(),
            name: "Flaky".to_string(),
            transport: BackendTransport::Stdio {
                command: "python3".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env,
                process: Arc::new(Mutex::new(None)),
            },
            tools: vec![ToolDefinition {
                name: "flaky_tool".to_string(),
                description: None,
                input_schema: None,
            }],
            tool_visibility: ToolVisibility::All,
            priority: 50,
            consecutive_errors: 0,
            unhealthy: false,
        };

        let registry = BackendRegistry::new();
        registry.register_backend(flaky_entry);

        // First call succeeds.
        let r1 = registry
            .dispatch("flaky__flaky_tool", &serde_json::json!({}))
            .await
            .expect("tool known")
            .expect("first call ok");
        assert_eq!(r1["content"][0]["text"].as_str(), Some("ok"));

        // Give the server a moment to exit.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second call triggers restart. Should also succeed.
        let r2 = registry
            .dispatch("flaky__flaky_tool", &serde_json::json!({}))
            .await
            .expect("tool known")
            .expect("second call after restart ok");
        assert_eq!(r2["content"][0]["text"].as_str(), Some("ok"));

        registry.shutdown();
    }
}
