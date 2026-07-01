struct ReconnectionServer {
    addr: String,
    tool_call_attempts: Arc<AtomicUsize>,
    initialize_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ReconnectionServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn initialize_count(&self) -> usize {
        self.initialize_count.load(Ordering::SeqCst)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex").clone()
    }
}

impl Drop for ReconnectionServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that:
/// - Answers initialize/tools_list normally (streamable HTTP)
/// - Returns HTTP 503 on the first tool call (transport error)
/// - Succeeds on all subsequent requests (after client reconnects)
fn spawn_reconnection_server() -> ReconnectionServer {
    spawn_reconnection_server_inner(false)
}

fn spawn_reconnection_server_with_failed_rehandshake() -> ReconnectionServer {
    spawn_reconnection_server_inner(true)
}

fn spawn_reconnection_server_inner(fail_rehandshake: bool) -> ReconnectionServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let tool_call_attempts = Arc::new(AtomicUsize::new(0));
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let tool_call_attempts_t = Arc::clone(&tool_call_attempts);
    let initialize_count_t = Arc::clone(&initialize_count);
    let requests_t = Arc::clone(&requests);
    let stop_t = Arc::clone(&stop);
    let ready_t = Arc::clone(&ready);

    let handle = thread::spawn(move || {
        ready_t.store(true, Ordering::SeqCst);
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        requests_t.lock().expect("mutex").push(request.clone());

                        if request.contains("\"method\":\"initialize\"") {
                            let initialize_attempt =
                                initialize_count_t.fetch_add(1, Ordering::SeqCst);
                            if fail_rehandshake && initialize_attempt > 0 {
                                let response = http_response_status(503, "Service Unavailable");
                                let _ = stream.write_all(response.as_bytes());
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "reconnect-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "tools": [{
                                        "name": "reconnect_tool",
                                        "description": "A tool for reconnection tests",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let attempt = tool_call_attempts_t.fetch_add(1, Ordering::SeqCst);
                            if attempt == 0 {
                                let response = http_response_status(503, "Service Unavailable");
                                let _ = stream.write_all(response.as_bytes());
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": { "reconnected": true }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    ReconnectionServer {
        addr,
        tool_call_attempts,
        initialize_count,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── Task 5-7 Tests ────────────────────────────────────────────────

/// S024 Assertion: Reconnection after transport failure re-handshakes via streamable HTTP.
///
/// Server returns a transport failure on the first tool call. After reconnection,
/// the client should re-handshake using streamable HTTP (not SSE fallback)
/// and successfully complete the tool call.
#[tokio::test]
async fn reconnection_preserves_transport_mode() {
    let _guard = test_guard().await;

    let server = spawn_reconnection_server();

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake via list_tools.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "reconnect_tool");

    // First initialize happened during list_tools.
    assert_eq!(
        server.initialize_count(),
        1,
        "one initialize should have happened during list_tools"
    );

    // Call the tool. First attempt drops connection; reconnection should succeed.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "reconnect_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after reconnection");

    assert_eq!(output["reconnected"], json!(true));

    // Verify reconnection happened: multiple initialize requests.
    assert!(
        server.initialize_count() >= 2,
        "reconnection should trigger a re-handshake (second initialize); got {} initializations",
        server.initialize_count()
    );

    // Verify all initialize requests used streamable HTTP (POST method,
    // not SSE). Every request in our log is a POST (SSE would be GET).
    let requests = server.requests();
    let init_requests: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("\"method\":\"initialize\""))
        .collect();

    assert!(
        init_requests.len() >= 2,
        "server should have received at least 2 initialize requests; got {}",
        init_requests.len()
    );

    // All initialize requests should be POST (streamable HTTP), not GET (SSE).
    for (i, req) in init_requests.iter().enumerate() {
        assert!(
            req.starts_with("POST "),
            "initialize request #{} should be a POST (streamable HTTP); got: {}",
            i + 1,
            &req[..req.len().min(80)]
        );
    }
}

/// S024 Assertion: Failed reconnection handshake returns an error.
///
/// A failed reconnection handshake must not fall through to a raw tools/call
/// dispatch with `transport_mode = None`.
#[tokio::test]
async fn reconnection_failed_rehandshake_returns_error_without_retry_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_reconnection_server_with_failed_rehandshake();

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "reconnect_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "reconnect_tool", json!({}), &capability)
        .await
        .expect_err("failed reconnection handshake should return an error");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("127.0.0.1")),
        "expected ConnectionFailed mentioning the server after failed reconnection, got {err:?}"
    );
    assert!(
        server.initialize_count() >= 2,
        "reconnection should attempt a re-handshake; got {} initializations",
        server.initialize_count()
    );
    assert_eq!(
        server.tool_call_attempts(),
        1,
        "failed reconnection handshake must not dispatch a retry tools/call without a transport"
    );
}

/// S024 Assertion: Session expiry uses immediate re-handshake, not backoff.
///
/// When a tool call returns 404 (session expired), the client should
/// re-handshake immediately without waiting for the exponential backoff
/// delay. Set a huge backoff and verify the operation completes quickly.
#[tokio::test]
async fn session_expiry_has_no_backoff_delay() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server(1); // reject first tool call with 404

    let mut manager = McpManager::new();
    // Set a huge backoff — if session expiry used this, the test would time out.
    manager.set_reconnect_base_delay_ms(30_000);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    // Time the tool call. Session expiry should re-handshake immediately.
    let start = std::time::Instant::now();
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after session expiry re-handshake");

    let elapsed = start.elapsed();

    assert_eq!(output["session_ok"], json!(true));

    // The entire operation should complete well under 5 seconds.
    // If the 30s backoff were used, this would take >= 30s.
    assert!(
        elapsed < Duration::from_secs(5),
        "session expiry should use immediate re-handshake, not 30s backoff; took {:?}",
        elapsed
    );

    // Verify re-handshake actually happened.
    assert!(
        server.initialize_count() >= 2,
        "session expiry should trigger re-handshake; got {} initializations",
        server.initialize_count()
    );
}

