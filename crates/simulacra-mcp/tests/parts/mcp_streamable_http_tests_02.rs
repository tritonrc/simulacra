/// S024 Assertion: Auto-detect falls back to legacy SSE when streamable HTTP returns 405.
///
/// Server returns 405 on POST to /sse, but serves proper SSE endpoint
/// discovery on GET /sse and JSON-RPC on POST /mcp-rpc. The manager should
/// fall back to legacy SSE and discover tools.
#[tokio::test]
async fn autodetect_falls_back_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_tool",
                "description": "A tool served via legacy SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect with None transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    // Give the SSE connection a moment to establish.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify the streamable HTTP POST was attempted first.
    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST first; got {} POST attempts",
        server.post_attempts()
    );

    // Verify the SSE fallback was used.
    assert!(
        server.sse_connections() >= 1,
        "auto-detect should fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );

    // Verify tools were discovered via SSE fallback.
    assert_eq!(
        tools.len(),
        1,
        "SSE fallback should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "legacy_tool");
}

/// S024 Assertion: `transport = "sse"` skips auto-detect and uses legacy SSE directly.
///
/// When the transport is explicitly set to "sse", no streamable HTTP POST
/// should be attempted.
#[tokio::test]
async fn transport_sse_skips_autodetect() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "sse_tool",
                "description": "A tool served via SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    // Use a fallback test server. With transport="sse", the POST should never happen.
    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("sse"))
        .await
        .expect("connect with sse transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify NO streamable HTTP POST was attempted.
    assert_eq!(
        server.post_attempts(),
        0,
        "transport='sse' should skip auto-detect — no POST attempts expected; got {}",
        server.post_attempts()
    );

    // Verify SSE was used directly.
    assert!(
        server.sse_connections() >= 1,
        "transport='sse' should connect via SSE directly; got {} SSE connections",
        server.sse_connections()
    );

    // Verify tools were discovered.
    assert_eq!(
        tools.len(),
        1,
        "SSE transport should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "sse_tool");
}

/// S024 Assertion: `transport = "http"` forces streamable HTTP.
///
/// When the transport is explicitly "http", streamable HTTP is used directly.
#[tokio::test]
async fn transport_http_forces_streamable_http() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "forced_http_tool",
                "description": "A tool forced to use streamable HTTP",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect with http transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;

    // Verify streamable HTTP was used.
    assert!(
        server.post_attempts() >= 2,
        "transport='http' should POST initialize and tools/list; got {} POST attempts",
        server.post_attempts()
    );

    // Verify tools discovered.
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "forced_http_tool");
}

/// S024 Assertion: `transport = "http"` does not fall back to legacy SSE.
///
/// Even when a server rejects streamable HTTP with a fallback-eligible 405 and
/// would serve legacy SSE, forced HTTP must not switch transports.
#[tokio::test]
async fn transport_http_does_not_fallback_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "should_not_be_discovered",
                "description": "A tool behind legacy SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("http"))
        .await
        .expect("connect with http transport should register without fallback");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "transport='http' should attempt streamable HTTP POST; got {} POST attempts",
        server.post_attempts()
    );
    assert_eq!(
        server.sse_connections(),
        0,
        "transport='http' must not fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        0,
        "transport='http' should not discover tools through SSE fallback"
    );
}

/// S024 Assertion: Both transports failing returns empty tools (graceful degradation).
///
/// When connecting to a dead port with auto-detect, both streamable HTTP
/// and SSE will fail. list_tools should return empty rather than panic.
#[tokio::test]
async fn autodetect_both_fail_returns_empty_tools() {
    let _guard = test_guard().await;
    // Bind a port and immediately drop the listener so nothing is listening.
    let port = {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("ephemeral port should be available");
        listener
            .local_addr()
            .expect("listener should have a local address")
            .port()
    };

    let url = format!("http://127.0.0.1:{}/mcp", port);

    let mut manager = McpManager::new();
    manager
        .connect(&url, None)
        .await
        .expect("connect should succeed (lazy — no network I/O)");

    let tools = list_tools_with_retry(&mut manager).await;

    assert_eq!(
        tools.len(),
        0,
        "when both transports fail, list_tools should return empty tools, not panic"
    );
}

