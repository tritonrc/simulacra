struct FallbackTestServer {
    addr: String,
    post_attempts: Arc<AtomicUsize>,
    sse_connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FallbackTestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn post_attempts(&self) -> usize {
        self.post_attempts.load(Ordering::SeqCst)
    }

    fn sse_connections(&self) -> usize {
        self.sse_connections.load(Ordering::SeqCst)
    }
}

impl Drop for FallbackTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that returns `reject_status` on POST to the SSE URL,
/// but serves proper SSE endpoint discovery on GET and JSON-RPC on
/// POST /mcp-rpc.
fn spawn_fallback_test_server(
    reject_status: u16,
    reject_reason: &str,
    tools_list_body: &str,
) -> FallbackTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fallback test server should bind");
    listener
        .set_nonblocking(true)
        .expect("fallback test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("fallback test server should have a local address")
        .to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let sse_connections = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let ready = Arc::new(AtomicBool::new(false));

    let post_attempts_for_thread = Arc::clone(&post_attempts);
    let sse_connections_for_thread = Arc::clone(&sse_connections);
    let stop_for_thread = Arc::clone(&stop);
    let ready_for_thread = Arc::clone(&ready);
    let reject_reason = reject_reason.to_string();
    let tools_list_body = tools_list_body.to_string();

    let handle = thread::spawn(move || {
        ready_for_thread.store(true, Ordering::SeqCst);
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    let request = match read_http_request(&mut stream) {
                        Some(r) => r,
                        None => continue,
                    };

                    if request.starts_with("GET /sse ") {
                        sse_connections_for_thread.fetch_add(1, Ordering::SeqCst);

                        let stop_for_sse = Arc::clone(&stop_for_thread);
                        let tools_list_body_for_sse = tools_list_body.clone();

                        thread::spawn(move || {
                            // Send SSE endpoint discovery event.
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /mcp-rpc\n\n",
                            );

                            // Now we need to handle POST requests from the client on the
                            // discovered endpoint. But the SSE connection is a different TCP
                            // stream. The client will connect on a new socket for POSTs.
                            // Just keep the SSE stream alive.
                            while !stop_for_sse.load(Ordering::SeqCst) {
                                let _ = stream.write_all(b": keep-alive\n\n");
                                thread::sleep(Duration::from_millis(50));
                            }
                            let _ = tools_list_body_for_sse;
                        });
                        continue;
                    }

                    if request.starts_with("POST /sse ") {
                        // Reject streamable HTTP POST to the SSE URL.
                        post_attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                        let response = http_response_status(reject_status, &reject_reason);
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    if request.starts_with("POST /mcp-rpc ") {
                        // Handle JSON-RPC on the discovered endpoint.
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-legacy-sse", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    // Unknown request — 404.
                    let response = http_response_status(404, "Not Found");
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the server thread to be ready.
    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    FallbackTestServer {
        addr,
        post_attempts,
        sse_connections,
        stop,
        handle: Some(handle),
    }
}

// ── Tests ───────────────────────────────────────────────────────────

/// S024 Assertion: Auto-detect tries streamable HTTP POST first when `transport` is absent.
///
/// When connecting with `transport = None`, the manager should send a POST
/// with `Accept: application/json, text/event-stream` and protocol version
/// `2025-03-26` before considering any fallback.
#[tokio::test]
async fn autodetect_tries_streamable_http_post_first() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "streamable_tool",
                "description": "A tool served via streamable HTTP",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("test-session-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect with None transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;

    // Verify the server received POST requests (streamable HTTP was tried).
    assert!(
        server.post_attempts() >= 2,
        "auto-detect should POST initialize and tools/list to the server; got {} POST attempts",
        server.post_attempts()
    );

    // Verify tools were discovered.
    assert_eq!(
        tools.len(),
        1,
        "auto-detect via streamable HTTP should discover tools"
    );
    assert_eq!(tools[0].name, "streamable_tool");

    // Verify the Accept header was sent.
    let requests = server.requests();
    let init_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");
    assert!(
        init_request.contains("application/json, text/event-stream")
            || init_request.contains("application/json,text/event-stream"),
        "initialize request should include Accept: application/json, text/event-stream; got: {}",
        init_request
    );

    // Verify protocol version is 2025-03-26.
    assert!(
        init_request.contains("2025-03-26"),
        "initialize request should use protocol version 2025-03-26; got: {}",
        init_request
    );
}

#[tokio::test]
async fn transport_auto_is_treated_like_omitted_transport() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auto_tool",
                "description": "A tool served via explicit auto transport",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("auto-session-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("auto"))
        .await
        .expect("transport='auto' should use auto-detect");

    let tools = list_tools_with_retry(&mut manager).await;

    assert!(
        server.post_attempts() >= 2,
        "transport='auto' should POST initialize and tools/list to the server; got {} POST attempts",
        server.post_attempts()
    );
    assert_eq!(tools.len(), 1, "transport='auto' should discover tools");
    assert_eq!(tools[0].name, "auto_tool");
}

#[tokio::test]
async fn transport_auto_falls_back_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auto_legacy_tool",
                "description": "A tool served via explicit auto fallback",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("auto"))
        .await
        .expect("transport='auto' should register for auto-detect");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "transport='auto' should attempt streamable HTTP POST before fallback; got {} POST attempts",
        server.post_attempts()
    );
    assert!(
        server.sse_connections() >= 1,
        "transport='auto' should fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        1,
        "transport='auto' should discover tools through SSE fallback"
    );
    assert_eq!(tools[0].name, "auto_legacy_tool");
}

