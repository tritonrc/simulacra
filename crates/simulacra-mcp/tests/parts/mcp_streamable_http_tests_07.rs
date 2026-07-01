/// S024 Assertion: Existing SSE configuration works unchanged (backwards compatibility).
///
/// Using `connect(&url, Some("sse"))` with a server that serves legacy SSE
/// should discover tools without any streamable HTTP attempts.
#[tokio::test]
async fn existing_sse_config_works_unchanged() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_sse_tool",
                "description": "A tool served via legacy SSE transport",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("sse"))
        .await
        .expect("connect with sse transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify tools were discovered.
    assert!(
        !tools.is_empty(),
        "SSE transport should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "legacy_sse_tool");

    // Verify NO streamable HTTP POST was attempted (SSE was used directly).
    assert_eq!(
        server.post_attempts(),
        0,
        "transport='sse' should skip auto-detect entirely — no POST attempts expected; got {}",
        server.post_attempts()
    );

    // Verify SSE was used.
    assert!(
        server.sse_connections() >= 1,
        "transport='sse' should connect via SSE; got {} SSE connections",
        server.sse_connections()
    );
}

/// S024 Assertion: Tool call with application/json response is correctly parsed.
///
/// Basic streamable HTTP tool call where the server returns a JSON response
/// (not SSE streaming). Verifies the result is correctly extracted.
#[tokio::test]
async fn tool_call_json_response_works() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "json_tool",
                "description": "A tool that returns JSON directly",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "input_value": { "type": "string" }
                    }
                }
            }]
        }
    })
    .to_string();

    let tool_call_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "computed": "result-from-json-path",
            "code": 200
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(&tools_body, &tool_call_body, None);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect with http transport should succeed");

    // Trigger handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "json_tool");

    // Call the tool.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool(
            "127.0.0.1",
            "json_tool",
            json!({ "input_value": "test" }),
            &capability,
        )
        .await
        .expect("tool call with JSON response should succeed");

    // Verify the result was correctly parsed from the JSON response.
    assert_eq!(
        output["computed"],
        json!("result-from-json-path"),
        "JSON response result should be correctly extracted"
    );
    assert_eq!(
        output["code"],
        json!(200),
        "JSON response should preserve all result fields"
    );
}

// ── S043 Auth-header tests ─────────────────────────────────────────

/// S043 Assertion: connect_named_with_headers threads per-connection headers into every request.
///
/// Every HTTP request sent on a connection established with
/// `connect_named_with_headers` (initialize, notifications/initialized,
/// tools/list) must carry the supplied headers verbatim.
#[tokio::test]
async fn streamable_handshake_threads_connection_headers() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "authed_tool",
                "description": "A tool behind an auth header",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({"jsonrpc":"2.0","result":{"ok":true}}).to_string(),
        Some("sess-1"),
    );

    let mut manager = McpManager::new();
    manager
        .connect_named_with_headers(
            "github",
            &server.url("/mcp"),
            Some("http"),
            vec![
                ("Authorization".to_string(), "Bearer ghs_test".to_string()),
                ("X-MCP-Readonly".to_string(), "true".to_string()),
            ],
        )
        .await
        .expect("connect");

    let _ = list_tools_with_retry(&mut manager).await;

    let reqs = server.requests();
    for method in ["initialize", "notifications/initialized", "tools/list"] {
        let needle = format!("\"method\":\"{method}\"");
        let req = reqs
            .iter()
            .find(|r| r.contains(&needle))
            .unwrap_or_else(|| {
                panic!("no {method} request was sent; recorded requests: {reqs:#?}")
            });
        let low = req.to_lowercase();
        assert!(
            low.contains("authorization: bearer ghs_test"),
            "{method} request missing Authorization header: {req}"
        );
        assert!(
            low.contains("x-mcp-readonly: true"),
            "{method} request missing X-MCP-Readonly header: {req}"
        );
    }
}

/// S043 Assertion: connect_named (no headers) does not inject any Authorization header.
///
/// A connection made via the existing `connect_named` must not carry any
/// Authorization header — confirming header isolation between connections.
#[tokio::test]
async fn connect_named_without_headers_sends_no_auth() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "open_tool",
                "description": "A tool with no auth",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({"jsonrpc":"2.0","result":{"ok":true}}).to_string(),
        Some("sess-2"),
    );

    let mut manager = McpManager::new();
    manager
        .connect_named("github", &server.url("/mcp"), Some("http"))
        .await
        .expect("connect");

    let _ = list_tools_with_retry(&mut manager).await;

    for r in &server.requests() {
        assert!(
            !r.to_lowercase().contains("authorization:"),
            "unexpected Authorization header in request: {r}"
        );
    }
}

// ── A2 tests ───────────────────────────────────────────────────────────

/// S043-A2 Assertion: connection headers are threaded into the tools/call dispatch request.
///
/// When a connection is established via `connect_named_with_headers`, every
/// outbound HTTP request — including `tools/call` — must carry the supplied
/// headers. Today only the handshake (initialize/initialized/tools/list) is
/// threaded; tools/call is NOT yet threaded, so this test is RED.
#[tokio::test]
async fn tools_call_threads_connection_headers() {
    let _guard = test_guard().await;

    let tools_list_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "search_code",
                "description": "Search code",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let tool_call_body = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": { "content": [] }
    })
    .to_string();

    let server = spawn_streamable_http_server(&tools_list_body, &tool_call_body, Some("sess-1"));

    let mut manager = McpManager::new();
    manager
        .connect_named_with_headers(
            "github",
            &server.url("/mcp"),
            Some("http"),
            vec![("Authorization".to_string(), "Bearer ghs_secret".to_string())],
        )
        .await
        .expect("connect");

    let _ = list_tools_with_retry(&mut manager).await;

    let cap = capability_with_mcp_tools(&["mcp:github:search_code"]);
    manager
        .call_tool(
            "github",
            "search_code",
            json!({"q": "health endpoint"}),
            &cap,
        )
        .await
        .unwrap_or_else(|e| panic!("tools/call failed: {e}"));

    let call_req = server
        .requests()
        .into_iter()
        .find(|r| r.contains("\"method\":\"tools/call\""))
        .expect("a tools/call request should have been sent");
    assert!(
        call_req
            .to_lowercase()
            .contains("authorization: bearer ghs_secret"),
        "tools/call request missing Authorization header: {call_req}"
    );
}

/// S043-A2 Assertion: `redact_headers_for_log` masks secret values while keeping header names.
///
/// This is a compile-time RED: `redact_headers_for_log` does not yet exist in
/// `simulacra_mcp`. The test will fail to compile until the function is added.
#[test]
fn redact_headers_for_log_masks_secrets() {
    let redacted = simulacra_mcp::redact_headers_for_log(&[
        ("Authorization".to_string(), "Bearer ghs_secret".to_string()),
        ("X-MCP-Readonly".to_string(), "true".to_string()),
        (
            "Proxy-Authorization".to_string(),
            "Basic abc123".to_string(),
        ),
        ("Cookie".to_string(), "session=deadbeef".to_string()),
        ("x-api-key".to_string(), "ak_live_xyz".to_string()), // lowercase name, prefix match
        ("Content-Type".to_string(), "application/json".to_string()), // NOT secret
    ]);
    // secrets masked
    assert!(!redacted.contains("ghs_secret"), "{redacted}");
    assert!(!redacted.contains("abc123"), "{redacted}");
    assert!(!redacted.contains("deadbeef"), "{redacted}");
    assert!(!redacted.contains("ak_live_xyz"), "{redacted}");
    assert!(
        !redacted.contains("true"),
        "X-MCP-Readonly value should be masked: {redacted}"
    ); // x-mcp- prefix
    // names still visible
    assert!(redacted.contains("Authorization"), "{redacted}");
    assert!(redacted.contains("Cookie"), "{redacted}");
    // non-secret value preserved
    assert!(
        redacted.contains("application/json"),
        "Content-Type value should NOT be masked: {redacted}"
    );
}

/// S043-A2 Regression guard: headers do not leak across independent connections.
///
/// Server `a` is connected with an Authorization header; server `b` is connected
/// without any headers. None of `b`'s recorded requests should contain an
/// Authorization header.
///
/// This test may already pass (A1 already scopes headers per connection); it is
/// included as a non-regression guard for the A2 dispatch-threading change.
#[tokio::test]
async fn headers_do_not_leak_across_connections() {
    let _guard = test_guard().await;

    let tools_body_a = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "authed_tool",
                "description": "Tool on authed server",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let tools_body_b = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "open_tool",
                "description": "Tool on open server",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let noop_call_body = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": { "content": [] }
    })
    .to_string();

    let a_server = spawn_streamable_http_server(&tools_body_a, &noop_call_body, Some("sess-a"));
    let b_server = spawn_streamable_http_server(&tools_body_b, &noop_call_body, Some("sess-b"));

    let mut manager = McpManager::new();
    manager
        .connect_named_with_headers(
            "a",
            &a_server.url("/mcp"),
            Some("http"),
            vec![("Authorization".to_string(), "Bearer A".to_string())],
        )
        .await
        .expect("connect a");
    manager
        .connect_named("b", &b_server.url("/mcp"), Some("http"))
        .await
        .expect("connect b");

    let _ = list_tools_with_retry(&mut manager).await;

    assert!(
        !b_server.requests().is_empty(),
        "server b should have received handshake requests"
    );

    // Also exercise the A2 tools/call dispatch path on server b.
    let cap_b = capability_with_mcp_tools(&["mcp:b:open_tool"]);
    let _ = manager.call_tool("b", "open_tool", json!({}), &cap_b).await;

    // None of b's requests (handshake or tools/call) should carry an auth header.
    for r in &b_server.requests() {
        assert!(
            !r.to_lowercase().contains("authorization:"),
            "leaked auth to headerless connection: {r}"
        );
    }
}

// ── S043 SSE-framed handshake (RED) ───────────────────────────────────────

/// S043 Assertion: streamable-HTTP handshake accepts SSE-framed initialize + tools/list responses.
///
/// The MCP 2025-03-26 spec permits servers to respond to the initialize and
/// tools/list POST requests with `Content-Type: text/event-stream` (SSE framing:
/// `event: message\ndata: <json>\n\n`).  GitHub's hosted MCP endpoint
/// (`api.githubcopilot.com/mcp/`) does exactly this.
///
/// The current `perform_streamable_http_handshake` calls `reqwest::Response::json()`
/// which rejects any body that is not `application/json`, so the handshake fails
/// and zero tools are discovered.
///
/// This test is intentionally RED: it will fail with the current production code
/// and must turn GREEN once the handshake correctly detects `text/event-stream`
/// and extracts the JSON from the SSE `data:` line.
#[tokio::test]
async fn streamable_handshake_parses_sse_framed_responses() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "tools": [{
            "name": "search_code",
            "description": "search",
            "inputSchema": { "type": "object", "properties": {} }
        }] }
    })
    .to_string();

    let server = spawn_streamable_http_server_sse(
        &tools_body,
        &json!({"jsonrpc":"2.0","result":{"ok":true}}).to_string(),
        Some("sess-sse"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;

    assert_eq!(
        tools.len(),
        1,
        "SSE-framed initialize + tools/list must be parsed by the handshake; got {} tools",
        tools.len()
    );
    assert_eq!(tools[0].name, "search_code");
}
