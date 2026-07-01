#[tokio::test]
async fn connect_sse_keeps_a_persistent_connection_for_server_pushed_events() {
    let _guard = test_guard().await;
    let server = spawn_sse_probe_server();
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("SSE MCP connect should register the SSE endpoint");

    let _ = manager.list_tools().await;

    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        server.connection_count() > 0,
        "list_tools should trigger the SSE connection to the server"
    );
    assert!(
        server.persistent_event_sent(),
        "SSE stream should stay open long enough to receive a later server-pushed event"
    );
}

#[tokio::test]
async fn connect_sse_discovers_post_endpoint_from_sse_events() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
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
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let mut tools = manager.list_tools().await;
    for _ in 0..4 {
        if !tools.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        tools = manager.list_tools().await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert_eq!(
        tools.len(),
        1,
        "SSE endpoint discovery should expose exactly one MCP tool after following the endpoint event"
    );
    assert_eq!(tools[0].name, "search_docs");
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/list\""),
        "list_tools should POST JSON-RPC to the endpoint discovered from SSE events; observed requests: {requests:?}"
    );
}

#[tokio::test]
async fn connect_sse_performs_handshake_via_discovered_endpoint() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "inputSchema": {
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
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let tools = manager.list_tools().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"initialize\""),
        "SSE transport should POST initialize to the discovered MCP JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/list\""),
        "SSE transport should POST tools/list to the discovered MCP JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["echo"],
        "tools/list results from the discovered endpoint should be bridged into Simulacra tool definitions"
    );
}

#[tokio::test]
async fn call_tool_via_sse_transport_uses_discovered_endpoint() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
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
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "query": "simulacra" } }
        })
        .to_string(),
    );
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect("call_tool should succeed through the JSON-RPC endpoint discovered via SSE");

    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert_eq!(output["echoed"]["query"], json!("simulacra"));
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/call\""),
        "call_tool should route tools/call through the SSE-discovered JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert!(
        !requests.contains("POST /sse "),
        "call_tool must not POST JSON-RPC to the SSE URL itself; observed requests: {requests:?}"
    );
}

#[tokio::test]
async fn connect_sse_keeps_connection_alive() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
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
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let _ = manager.list_tools().await;

    let deadline = Instant::now() + Duration::from_secs(2);
    while !server.persistent_event_sent() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        server.sse_connection_count() > 0,
        "SSE transport should establish a streaming GET connection to the SSE endpoint"
    );
    assert!(
        server.persistent_event_sent(),
        "SSE transport should keep the stream alive after the discovered-endpoint handshake so later server-pushed events can still arrive"
    );
}

