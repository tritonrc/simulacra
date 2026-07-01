/// S024 Assertion: Mcp-Session-Id from response headers is stored and sent in subsequent requests.
///
/// Verifies that when the server returns Mcp-Session-Id during initialize,
/// subsequent tools/list requests include that session ID.
#[tokio::test]
async fn session_id_stored_from_initialize_response() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "session_tool",
                "description": "A tool that requires a session",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("session-abc-123"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1, "should discover tools via streamable HTTP");

    // Verify that subsequent requests after initialize include the session ID.
    let requests = server.requests();
    let tools_list_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"tools/list\""))
        .expect("server should have received a tools/list request");

    assert!(
        tools_list_request.contains("Mcp-Session-Id: session-abc-123")
            || tools_list_request.contains("mcp-session-id: session-abc-123"),
        "tools/list request should include Mcp-Session-Id header; got: {}",
        tools_list_request
    );
}

/// S024 Assertion: Auto-detect does NOT fall back on 401 (auth error).
///
/// A 401 response should result in ConnectionFailed, not SSE fallback.
#[tokio::test]
async fn autodetect_does_not_fall_back_on_401() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auth_tool",
                "description": "Should not be discovered",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    // Server returns 401 on POST (not fallback-eligible).
    let server = spawn_fallback_test_server(401, "Unauthorized", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed (lazy)");

    let tools = list_tools_with_retry(&mut manager).await;

    // The POST was attempted.
    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST; got {} POST attempts",
        server.post_attempts()
    );

    // No SSE fallback should occur for auth errors.
    assert_eq!(
        server.sse_connections(),
        0,
        "401 should NOT trigger SSE fallback; got {} SSE connections",
        server.sse_connections()
    );

    // Tools should be empty (handshake failed).
    assert_eq!(
        tools.len(),
        0,
        "auth error should result in no tools, not SSE fallback"
    );
}

/// S024 Assertion: Auto-detect does NOT fall back on 500 (server error).
///
/// A 500 response should result in ConnectionFailed, not SSE fallback.
#[tokio::test]
async fn autodetect_does_not_fall_back_on_500() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "error_tool",
                "description": "Should not be discovered",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(500, "Internal Server Error", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed (lazy)");

    let tools = list_tools_with_retry(&mut manager).await;

    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST"
    );

    assert_eq!(
        server.sse_connections(),
        0,
        "500 should NOT trigger SSE fallback; got {} SSE connections",
        server.sse_connections()
    );

    assert_eq!(
        tools.len(),
        0,
        "server error should result in no tools, not SSE fallback"
    );
}

/// S024 Assertion: Protocol version in InitializeRequest is 2025-03-26.
///
/// Verifies the exact protocol version string sent in the initialize request.
#[tokio::test]
async fn initialize_uses_protocol_version_2025_03_26() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": []
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect should succeed");

    let _ = manager.list_tools().await;

    let requests = wait_for_streamable_requests(&server, |requests| {
        requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
    })
    .await;
    let init_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");

    assert!(
        init_request.contains("\"protocolVersion\":\"2025-03-26\""),
        "initialize should use protocolVersion 2025-03-26; got: {}",
        init_request
    );
}

/// S024 Assertion: notifications/initialized is sent after successful InitializeResult.
///
/// After the initialize response, the client must send notifications/initialized
/// before tools/list.
#[tokio::test]
async fn initialized_notification_sent_after_initialize() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "ordered_tool",
                "description": "Test ordering",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1, "handshake should discover tools");

    let requests = wait_for_streamable_requests(&server, |requests| {
        requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"notifications/initialized\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"tools/list\""))
    })
    .await;

    let init_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");

    let notif_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"notifications/initialized\""))
        .expect("server should have received a notifications/initialized request");

    let tools_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"tools/list\""))
        .expect("server should have received a tools/list request");

    assert!(
        init_idx < notif_idx,
        "initialize (idx {init_idx}) must come before notifications/initialized (idx {notif_idx})"
    );
    assert!(
        notif_idx < tools_idx,
        "notifications/initialized (idx {notif_idx}) must come before tools/list (idx {tools_idx})"
    );
}

/// S024 Assertion: Auto-detect falls back to legacy SSE on 404.
///
/// Same as the 405 test, but verifying 404 also triggers fallback.
#[tokio::test]
async fn autodetect_falls_back_to_legacy_sse_on_404() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_404_tool",
                "description": "Fallback on 404",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(404, "Not Found", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST first"
    );
    assert!(
        server.sse_connections() >= 1,
        "auto-detect should fall back to SSE after 404; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        1,
        "SSE fallback should discover tools after 404"
    );
    assert_eq!(tools[0].name, "legacy_404_tool");
}

// ── Helpers for Task 4 tests ───────────────────────────────────────

