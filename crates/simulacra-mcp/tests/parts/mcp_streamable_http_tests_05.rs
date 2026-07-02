/// S024 Assertion: Tool call POST includes Mcp-Session-Id header when session is active.
///
/// Verifies that after a streamable HTTP handshake that returns Mcp-Session-Id,
/// subsequent tool call requests include the session ID header.
#[tokio::test]
async fn session_id_sent_on_tool_calls() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "session_tool",
                "description": "A tool that tracks session ID",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }).to_string(),
        Some("session-for-tool-call-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);

    // Now call the tool.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed");
    assert_eq!(output["ok"], json!(true));

    // Verify the tool call request included the session ID header.
    let requests = server.requests();
    let tool_call_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"tools/call\""))
        .expect("server should have received a tools/call request");

    assert!(
        tool_call_request.contains("Mcp-Session-Id: session-for-tool-call-42")
            || tool_call_request.contains("mcp-session-id: session-for-tool-call-42"),
        "tools/call request should include Mcp-Session-Id header; got: {}",
        tool_call_request
    );
}

/// S024 Assertion: HTTP 404 with stored session ID triggers session expiry handling.
///
/// Server returns 404 on the first tool call, which should trigger re-handshake
/// (no backoff) and a retry that succeeds.
#[tokio::test]
async fn session_expiry_on_404_triggers_rehandshake() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server(1); // reject first tool call with 404

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake.
    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    // First initialize happened during list_tools.
    assert_eq!(
        server.initialize_count(),
        1,
        "one initialize should have happened during list_tools"
    );

    // Call the tool — first attempt gets 404 (session expired),
    // should re-handshake and retry.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after session expiry re-handshake");

    assert_eq!(output["session_ok"], json!(true));

    // Verify re-handshake happened: there should be 2 initialize requests total.
    assert!(
        server.initialize_count() >= 2,
        "session expiry should trigger a re-handshake (second initialize); got {} initializations",
        server.initialize_count()
    );

    // Verify the tool call was attempted at least twice.
    assert!(
        server.tool_call_attempts() >= 2,
        "should have attempted tool call at least twice (first rejected, second succeeded); got {}",
        server.tool_call_attempts()
    );
}

/// S024 Assertion: Failed re-handshake after session expiry returns an error.
///
/// A failed session-expiry re-handshake must not fall through to a raw
/// tools/call dispatch with no established transport mode.
#[tokio::test]
async fn session_expiry_failed_rehandshake_returns_error_without_retry_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server_with_failed_rehandshake();

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect_err("failed session re-handshake should return an error");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("127.0.0.1")),
        "expected ConnectionFailed mentioning the server after failed re-handshake, got {err:?}"
    );
    assert!(
        server.initialize_count() >= 2,
        "session expiry should attempt a re-handshake; got {} initializations",
        server.initialize_count()
    );
    assert_eq!(
        server.tool_call_attempts(),
        1,
        "failed re-handshake must not dispatch a retry tools/call without a transport"
    );
}

/// S024 Assertion: text/event-stream response is parsed as SSE, progress logged, final result extracted.
///
/// Server returns a tool call response with Content-Type: text/event-stream containing
/// progress notifications and a final JSON-RPC result.
#[tokio::test]
async fn sse_streaming_response_extracts_final_result() {
    let _guard = test_guard().await;

    let result_json = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "streamed_value": "hello from SSE" }
    })
    .to_string();

    let server = spawn_sse_streaming_server(SseToolResponse::WithResult(result_json));

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "streaming_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "streaming_tool", json!({}), &capability)
        .await
        .expect("SSE streaming tool call should succeed");

    assert_eq!(
        output["streamed_value"],
        json!("hello from SSE"),
        "should extract the final result from the SSE stream"
    );
}

/// S024 Assertion: SSE stream closing without a JSON-RPC response returns McpError::ProtocolError.
///
/// Server sends only progress notifications via SSE, then closes the stream
/// without a final JSON-RPC result.
#[tokio::test]
async fn sse_stream_no_result_returns_protocol_error() {
    let _guard = test_guard().await;

    let server = spawn_sse_streaming_server(SseToolResponse::NoResult);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "streaming_tool", json!({}), &capability)
        .await
        .expect_err("SSE stream without result should fail");

    assert!(
        matches!(&err, McpError::ProtocolError(msg) if msg.contains("SSE stream closed")),
        "should get ProtocolError about SSE stream closing without result; got: {err:?}"
    );
}

// ── ReconnectionServer ────────────────────────────────────────────
//
// Fake MCP server that returns a transient transport failure on the first
// tool call, then succeeds on subsequent handshakes + tool calls after
// reconnection.
