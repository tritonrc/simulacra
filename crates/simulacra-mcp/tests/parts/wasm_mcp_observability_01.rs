#[tokio::test]
async fn mcp_manager_call_tool_signature_unchanged() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("wasm MCP server should connect");

    let output = manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect("call_tool signature should still support direct await usage");

    assert_eq!(output["echoed"]["query"], json!("simulacra"));
}

#[test]
fn gen_ai_tool_message_event_for_wasm_mcp_carries_simulacra_tool_source_attribute() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && field_matches(&event.fields, "simulacra.tool.source", "mcp:github")
        }),
        "WASM MCP calls should preserve simulacra.tool.source on gen_ai.tool.message events"
    );
}

#[test]
fn simulacra_mcp_calls_counter_increments_for_wasm_mcp_with_server_and_tool_labels() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.calls", "1")
                && field_matches(&event.fields, "server", "github")
                && field_matches(&event.fields, "tool", "echo")
        }),
        "WASM MCP calls should increment simulacra.mcp.calls with server/tool labels"
    );
}

#[tokio::test]
async fn http_sse_mcp_servers_continue_to_work_unchanged() {
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": { "type": "object" }
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
    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("HTTP MCP server should register");

    let deadline = Instant::now() + Duration::from_secs(2);
    let tools = loop {
        let tools = manager.list_tools().await;
        if !tools.is_empty() || Instant::now() >= deadline {
            break tools;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(tools.len(), 1, "HTTP MCP servers should still list tools");
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:*:echo"]),
        )
        .await
        .expect("HTTP MCP servers should still handle call_tool");
    assert_eq!(output["echoed"]["query"], json!("simulacra"));

    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("adding wasm MCP should not break existing HTTP/SSE flows");
}

