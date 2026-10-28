struct StatusServer {
    addr: String,
    initialize_status: u16,
    tool_statuses: Arc<Mutex<Vec<u16>>>,
    tool_call_attempts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StatusServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_status_server(initialize_status: u16, tool_statuses: Vec<u16>) -> StatusServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("status server should bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let tool_statuses = Arc::new(Mutex::new(tool_statuses));
    let tool_call_attempts = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let tool_statuses_t = Arc::clone(&tool_statuses);
    let tool_call_attempts_t = Arc::clone(&tool_call_attempts);
    let stop_t = Arc::clone(&stop);
    let ready_t = Arc::clone(&ready);

    let handle = thread::spawn(move || {
        ready_t.store(true, Ordering::SeqCst);
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    if let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            continue;
                        }

                        if request.contains("\"method\":\"initialize\"") {
                            if initialize_status >= 400 {
                                let response = http_response_status(initialize_status, "init rejected");
                                let _ = stream.write_all(response.as_bytes());
                                continue;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "status", "version": "1.0.0" },
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
                                        "name": "test_tool",
                                        "description": "test tool",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_attempts_t.fetch_add(1, Ordering::SeqCst);
                            let status = {
                                let mut statuses = tool_statuses_t.lock().expect("mutex");
                                if statuses.len() > 1 {
                                    statuses.remove(0)
                                } else {
                                    *statuses.first().expect("at least one status")
                                }
                            };
                            if status >= 400 {
                                let response = http_response_status(status, "tool rejected");
                                let _ = stream.write_all(response.as_bytes());
                                continue;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": { "ok": true }
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

    StatusServer {
        addr,
        initialize_status,
        tool_statuses,
        tool_call_attempts,
        stop,
        handle: Some(handle),
    }
}

#[derive(Clone)]
enum BodyMode {
    Json { body: String, chunk_size: usize },
    Sse { events: Vec<String>, final_result: String },
}

async fn connect_test_server(manager: &mut McpManager, url: &str) {
    manager
        .connect(url, None)
        .await
        .expect("connect should register the local MCP server");
    let tools = list_tools_with_retry(manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_tool");
}

#[tokio::test]
async fn dispatch_401_and_403_return_auth_failed_without_same_credential_retry() {
    let _guard = test_guard().await;

    for status in [401, 403] {
        let server = spawn_status_server(200, vec![status]);
        let mut manager = McpManager::new();
        manager.set_reconnect_base_delay_ms(10);
        connect_test_server(&mut manager, &server.url("/mcp")).await;

        let capability = capability_with_mcp_tools(&["mcp:*:*"]);
        let err = manager
            .call_tool_for_agent(
                &simulacra_types::AgentId(format!("agent-{status}")),
                "127.0.0.1",
                "test_tool",
                json!({}),
                &capability,
            )
            .await
            .expect_err("dispatch auth status should fail");

        assert!(
            matches!(err, McpError::AuthFailed(ref detail) if detail.contains(&status.to_string())),
            "HTTP {status} should be typed AuthFailed, got {err:?}"
        );
        assert_eq!(
            server.tool_call_attempts(),
            1,
            "HTTP {status} must return after exactly one upstream tool dispatch"
        );
    }
}

#[tokio::test]
async fn dispatch_404_and_500_remain_transport_errors_and_retryable() {
    let _guard = test_guard().await;

    for status in [404, 500] {
        let server = spawn_status_server(200, vec![status, status, status]);
        let mut manager = McpManager::new();
        manager.set_reconnect_base_delay_ms(10);
        connect_test_server(&mut manager, &server.url("/mcp")).await;

        let capability = capability_with_mcp_tools(&["mcp:*:*"]);
        let err = manager
            .call_tool_for_agent(
                &simulacra_types::AgentId(format!("agent-{status}")),
                "127.0.0.1",
                "test_tool",
                json!({}),
                &capability,
            )
            .await
            .expect_err("transport status should fail after current retry policy");

        assert!(
            matches!(err, McpError::TransportError(ref detail) if detail.contains(&status.to_string())),
            "HTTP {status} should remain TransportError, got {err:?}"
        );
        assert!(
            server.tool_call_attempts() > 1,
            "HTTP {status} should keep today's retry behavior"
        );
    }
}

#[tokio::test]
async fn handshake_401_still_returns_connection_failed() {
    let _guard = test_guard().await;

    let server = spawn_status_server(401, vec![200]);
    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect only records server config");

    let tools = manager.list_tools().await;
    assert!(tools.is_empty(), "handshake failure should yield no tools");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("handshake-agent".into()),
            "127.0.0.1",
            "test_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("handshake 401 should fail before dispatch");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref detail) if detail.contains("127.0.0.1")),
        "handshake-path 401 must stay ConnectionFailed, got {err:?}"
    );
    assert_eq!(
        server.tool_call_attempts(),
        0,
        "handshake auth rejection must not reach tools/call"
    );
}

#[tokio::test]
async fn transport_failure_then_auth_failed_stops_after_second_tool_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_status_server(200, vec![500, 401, 200]);
    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    connect_test_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("agent-500-401".into()),
            "127.0.0.1",
            "test_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("retry-dispatch auth failure should stop immediately");

    assert!(
        matches!(err, McpError::AuthFailed(ref detail) if detail.contains("401")),
        "500 followed by 401 should surface AuthFailed, got {err:?}"
    );
    assert_eq!(
        server.tool_call_attempts(),
        2,
        "500 then 401 must not burn a third same-credential dispatch"
    );
}

