/// A JSON-RPC server that can be toggled between accepting and rejecting connections.
/// When `rejecting` is true, the server closes connections immediately without responding.
struct ToggleableJsonRpcServer {
    addr: String,
    rejecting: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ToggleableJsonRpcServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
    }

    fn set_rejecting(&self, reject: bool) {
        self.rejecting.store(reject, Ordering::SeqCst);
    }
}

impl Drop for ToggleableJsonRpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_toggleable_json_rpc_server(
    tools_list_body: &str,
    tool_call_body: &str,
) -> ToggleableJsonRpcServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("toggleable server should bind");
    listener
        .set_nonblocking(true)
        .expect("toggleable server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("toggleable server should have a local address")
        .to_string();
    let rejecting = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let rejecting_for_thread = Arc::clone(&rejecting);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    if rejecting_for_thread.load(Ordering::SeqCst) {
                        // Close the connection immediately without sending a response,
                        // simulating a server that is down.
                        drop(stream);
                        continue;
                    }

                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    if let Some(request) = read_http_request(&mut stream) {
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    ToggleableJsonRpcServer {
        addr,
        rejecting,
        stop,
        handle: Some(handle),
    }
}

// S008 Assertion: Reconnection with exponential backoff on transport failure.
// A server that fails once then recovers is reconnected automatically.
#[tokio::test]
async fn reconnect_after_transient_failure_succeeds_on_retry() {
    let _guard = test_guard().await;

    let tools_list = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo a payload.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"]
                }
            }]
        }
    })
    .to_string();
    let tool_call = json!({
        "jsonrpc": "2.0",
        "result": { "echoed": { "value": 42 } }
    })
    .to_string();

    let server = spawn_toggleable_json_rpc_server(&tools_list, &tool_call);
    let mut manager = McpManager::new();
    // Use 10ms base delay so the test runs fast.
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    // First call succeeds, establishing was_connected = true.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 42 }),
            &capability,
        )
        .await
        .expect("first call_tool should succeed");
    assert_eq!(output["echoed"]["value"], json!(42));

    // Take the server down.
    server.set_rejecting(true);

    // Schedule the server to come back up after a short delay.
    let rejecting = Arc::clone(&server.rejecting);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        rejecting.store(false, Ordering::SeqCst);
    });

    // This call should fail initially, then reconnect and succeed.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 42 }),
            &capability,
        )
        .await
        .expect("call_tool should reconnect and succeed after transient failure");

    assert_eq!(output["echoed"]["value"], json!(42));
}

// S008 Assertion: After 3 reconnection failures, the error propagates.
#[tokio::test]
async fn reconnect_exhausts_retries_and_returns_error() {
    let _guard = test_guard().await;

    let tools_list = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo a payload.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"]
                }
            }]
        }
    })
    .to_string();
    let tool_call = json!({
        "jsonrpc": "2.0",
        "result": { "echoed": { "value": 1 } }
    })
    .to_string();

    let server = spawn_toggleable_json_rpc_server(&tools_list, &tool_call);
    let mut manager = McpManager::new();
    // Use 10ms base delay so the test runs fast.
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    // First call succeeds, establishing was_connected = true.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect("first call_tool should succeed");
    assert_eq!(output["echoed"]["value"], json!(1));

    // Take the server down permanently.
    server.set_rejecting(true);

    // This call should fail after exhausting all 3 retry attempts.
    let err = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 2 }),
            &capability,
        )
        .await
        .expect_err("call_tool should fail after exhausting reconnection retries");

    assert!(
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        ),
        "expected a transport or connection error after exhausted retries, got {err:?}"
    );
}

// S008 Assertion: No reconnection is attempted for servers that never connected.
// If a server has never successfully completed a handshake, transport errors
// are returned immediately without retry.
#[tokio::test]
async fn no_reconnect_for_never_connected_server() {
    let _guard = test_guard().await;

    // Bind a port then drop the listener so the port is unreachable.
    let listener = TcpListener::bind("127.0.0.1:0").expect("probe should bind");
    let addr = listener
        .local_addr()
        .expect("probe should have a local address");
    drop(listener);

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
    let url = format!("http://{addr}/mcp");

    manager
        .connect_http(&url)
        .await
        .expect("connect_http should register the server");

    // Trigger the handshake so that ensure_server_connected runs.
    // Since the server is down, the handshake will fail silently
    // (producing empty tools), but was_connected stays false.
    let _ = manager.list_tools().await;

    let start = std::time::Instant::now();
    let err = manager
        .call_tool(
            &addr.ip().to_string(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect_err("call_tool to a never-connected server should fail immediately");

    // Verify it failed fast (no reconnection backoff).
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "should fail fast without reconnection attempts, took {:?}",
        elapsed
    );

    assert!(
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        ),
        "expected a transport or connection error, got {err:?}"
    );
}
