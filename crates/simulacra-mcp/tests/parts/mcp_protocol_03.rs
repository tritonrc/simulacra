#[tokio::test]
async fn simulacra_mcp_connects_via_http_not_child_process() {
    let _guard = test_guard().await;

    // Start a real HTTP MCP server that responds to handshake requests.
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "net_tool",
                    "description": "A tool served over HTTP",
                    "inputSchema": { "type": "object", "properties": {} }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );

    let mut manager = McpManager::new();

    // connect_http is the only way to connect — no spawn/stdio path exists.
    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should succeed over network transport");

    // Trigger the lazy handshake and verify tools are discovered over HTTP.
    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1, "should discover tools via HTTP transport");
    assert_eq!(tools[0].name, "net_tool");
}

// S008 Assertion: Tool schema bridging produces valid ToolDefinition values from real MCP responses.
#[tokio::test]
async fn list_tools_bridges_name_description_and_input_schema_from_mcp_server() {
    let _guard = test_guard().await;
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "search_docs",
                    "description": "Searches indexed MCP documentation.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let tools = manager.list_tools().await;

    assert_eq!(
        tools.len(),
        1,
        "list_tools should bridge exactly one MCP tool into a ToolDefinition"
    );
    assert_eq!(tools[0].name, "search_docs");
    assert_eq!(tools[0].description, "Searches indexed MCP documentation.");
    assert_eq!(
        tools[0].input_schema["type"], "object",
        "bridged input_schema should preserve the JSON Schema 'type' field"
    );
    assert_eq!(
        tools[0].input_schema["properties"],
        json!({ "query": { "type": "string" } }),
        "bridged input_schema should preserve the 'properties' map with property types"
    );
    assert_eq!(
        tools[0].input_schema["required"],
        json!(["query"]),
        "bridged input_schema should preserve the 'required' array"
    );
}

// S008 Assertion: MCP capability checks use glob matching for MCP patterns, not exact tool equality only.
#[tokio::test]
async fn call_tool_with_glob_mcp_capability_pattern_is_allowed_to_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "value": { "type": "integer" }
                        },
                        "required": ["value"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "value": 1 } }
        })
        .to_string(),
    );
    let mut manager = McpManager::new();
    let granted_pattern = format!("mcp:{}:*", server.server_name());
    let capability = capability_with_mcp_tools(&[granted_pattern.as_str()]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect("a matching mcp:{server}:* capability should allow the MCP call");

    assert_eq!(output["echoed"]["value"], json!(1));
}

// S008 Behavior: Connection failures produce typed errors, not panics.
#[tokio::test]
async fn invalid_mcp_url_returns_typed_error() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let err = manager
        .connect_http("://not-a-valid-url")
        .await
        .expect_err("invalid MCP URLs should return a typed McpError");

    assert!(
        matches!(
            err,
            McpError::ConnectionFailed(_)
                | McpError::ProtocolError(_)
                | McpError::TransportError(_)
        ),
        "unexpected MCP error variant: {err:?}"
    );
}

// S008 Assertion: McpManager construction does not open an HTTP socket.
// connect_http is lazy: it registers the URL but does not perform any network I/O.
// The handshake is deferred until first use (list_tools or call_tool).
#[tokio::test]
async fn connect_http_is_lazy_and_does_not_open_a_socket_during_connect() {
    let _guard = test_guard().await;
    let probe = spawn_passive_tcp_listener_probe();
    let mut manager = McpManager::new();

    manager
        .connect_http(&probe.url("/mcp"))
        .await
        .expect("connect_http should succeed without network I/O");

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        probe.connection_count(),
        0,
        "connect_http should not open a network connection (lazy handshake)"
    );
}

// S008 Assertion: Capability proxy rejects MCP tools that are not granted.
#[tokio::test]
async fn call_tool_with_tool_outside_capability_returns_capability_denied() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["allowed_tool"]);

    let err = manager
        .call_tool(
            "docs-server",
            "forbidden_tool",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect_err("an ungranted MCP tool should be rejected before dispatch");

    assert!(
        matches!(err, McpError::CapabilityDenied(ref message) if message.contains("forbidden_tool")),
        "expected CapabilityDenied for an ungranted MCP tool, got {err:?}"
    );
}

// S008 Assertion: Calling an unconnected server returns a typed MCP error.
#[tokio::test]
async fn call_tool_to_unconnected_server_returns_typed_error() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    let err = manager
        .call_tool("missing-server", "echo", json!({ "value": 1 }), &capability)
        .await
        .expect_err("calling a tool on an unconnected server should fail");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("missing-server")),
        "expected ConnectionFailed mentioning the missing server name, got {err:?}"
    );
}

// S008 Assertion: MCP handshake implements initialize followed by tools/list.
// The handshake is triggered lazily on first list_tools() call.
#[tokio::test]
async fn connect_http_performs_initialize_then_tools_list_handshake() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(
        r#"{"jsonrpc":"2.0","result":{"protocolVersion":"2025-03-26"}}"#,
    );
    let mut manager = McpManager::new();

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the server URL");

    let _ = manager.list_tools().await;

    let deadline = Instant::now() + Duration::from_secs(2);
    let requests = loop {
        let requests = server.requests();
        if requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"tools/list\""))
        {
            break requests;
        }
        if Instant::now() >= deadline {
            break requests;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        server.request_count() > 0,
        "list_tools should trigger the MCP handshake to perform discovery"
    );

    // Find the index of the first request containing "initialize" and "tools/list".
    // The MCP protocol requires initialize to come before tools/list.
    let initialize_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"initialize\""))
        .expect("handshake should send an initialize request before exposing MCP tools");
    let tools_list_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"tools/list\""))
        .expect("handshake should request tools/list during MCP discovery");
    assert!(
        initialize_idx < tools_list_idx,
        "initialize (request index {initialize_idx}) must come before tools/list (request index {tools_list_idx}) per MCP protocol; observed requests: {requests:?}"
    );
}

// S008 Assertion: SSE transport maintains a persistent connection for server-pushed events.
// The SSE background task is started lazily on first list_tools() or call_tool().
