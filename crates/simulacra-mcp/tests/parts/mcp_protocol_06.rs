#[test]
fn mcp_tool_calls_emit_execute_tool_span_with_mcp_source_attributes() {
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

    let ((result, server_name), spans, _events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            let result = manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await;

            (result, server_name)
        })
    });
    result.expect("call_tool should succeed before telemetry assertions are evaluated");

    let execute_tool_span = spans
        .iter()
        .find(|span| {
            span.name == "execute_tool"
                && field_matches(&span.fields, "gen_ai.operation.name", "execute_tool")
                && field_matches(&span.fields, "simulacra.tool.name", "echo")
                && field_matches(
                    &span.fields,
                    "simulacra.tool.source",
                    &format!("mcp:{server_name}"),
                )
        })
        .expect("call_tool should emit an execute_tool span with MCP source attributes");

    assert_eq!(execute_tool_span.name, "execute_tool");
}

// S008 O11y Assertion: simulacra.mcp.calls is emitted once per call with server and tool labels.
#[test]
fn mcp_tool_calls_increment_counter_with_server_and_tool_labels() {
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

    let ((result, server_name), _spans, events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            let result = manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await;

            (result, server_name)
        })
    });
    result.expect("call_tool should succeed before metric assertions are evaluated");

    let metric_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.calls", "1")
                && field_matches(&event.fields, "server", &server_name)
                && field_matches(&event.fields, "tool", "echo")
        })
        .expect("call_tool should emit simulacra.mcp.calls with server and tool labels");

    assert_eq!(metric_event.current_span.as_deref(), Some("execute_tool"));
}

// S008 O11y Assertion: MCP connection failures are logged at WARN with server and error context.
#[test]
fn mcp_connection_failures_are_logged_at_warn_with_server_and_error() {
    let ((_, failing_server), _spans, events) = capture_traces(|| {
        run_async(async {
            let listener = TcpListener::bind("127.0.0.1:0").expect("failure probe should bind");
            let addr = listener
                .local_addr()
                .expect("failure probe should have a local address");
            drop(listener);

            let mut manager = McpManager::new();
            let url = format!("http://{addr}/mcp");
            let server_name = addr.ip().to_string();

            manager
                .connect_http(&url)
                .await
                .expect("connect_http should register the MCP server before first use");

            let _ = manager.list_tools().await;
            ((), server_name)
        })
    });

    let warning = events
        .iter()
        .find(|event| {
            event.level == "WARN"
                && field_matches(&event.fields, "server", &failing_server)
                && event.fields.contains_key("error")
        })
        .expect("connection failures should emit a WARN log with server and error fields");

    assert_eq!(warning.level, "WARN");
}

// S008 O11y Assertion: MCP tool calls emit gen_ai.tool.message events for both input and output.
#[test]
fn mcp_tool_calls_emit_gen_ai_tool_message_events_for_input_and_output() {
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

    let (result, _spans, events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await
        })
    });
    result.expect("call_tool should succeed before tool message assertions are evaluated");

    let input_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && event.fields.contains_key("input")
        })
        .expect("call_tool should emit a gen_ai.tool.message event for MCP tool input");

    // Input telemetry retains only safe metadata. The echoed output above proves
    // the fake remote dispatcher still received the original arguments unchanged.
    let input_json: serde_json::Value = serde_json::from_str(
        input_event
            .fields
            .get("input")
            .expect("input event should have an 'input' field"),
    )
    .expect("input field should be valid JSON");
    assert_eq!(
        input_json,
        json!({"argument_length": json!({"query":"simulacra"}).to_string().len()}),
        "input event should contain only safe argument metadata"
    );
    assert_eq!(input_event.fields.get("server"), Some(&server.server_name().to_string()));
    assert_eq!(input_event.fields.get("tool"), Some(&"echo".to_string()));
    assert_eq!(
        input_event.fields.get("gen_ai.tool.argument_length"),
        Some(&json!({"query":"simulacra"}).to_string().len().to_string())
    );
    assert!(
        !input_event
            .fields
            .get("input")
            .expect("safe input metadata")
            .contains("simulacra")
    );

    // Output event records `gen_ai.tool.result_length` (length only — full
    // output is intentionally not logged because it may contain secrets/PII
    // returned by the MCP server).
    let output_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && event.fields.contains_key("gen_ai.tool.result_length")
        })
        .expect("call_tool should emit a gen_ai.tool.message event with result_length");

    let result_length: usize = output_event
        .fields
        .get("gen_ai.tool.result_length")
        .expect("output event should have a result_length field")
        .parse()
        .expect("result_length should parse as usize");
    let expected = json!({ "echoed": { "query": "simulacra" } })
        .to_string()
        .len();
    assert_eq!(
        result_length, expected,
        "result_length should equal the JSON-encoded output length"
    );

    assert_eq!(input_event.current_span.as_deref(), Some("execute_tool"));
    assert_eq!(output_event.current_span.as_deref(), Some("execute_tool"));
}
