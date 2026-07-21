struct S067StatusServer {
    addr: String,
    initialize_status: u16,
    tool_statuses: Arc<Mutex<Vec<u16>>>,
    tool_call_attempts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl S067StatusServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }
}

impl Drop for S067StatusServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_s067_status_server(initialize_status: u16, tool_statuses: Vec<u16>) -> S067StatusServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("S067 status server should bind");
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
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        if request.contains("\"method\":\"initialize\"") {
                            if initialize_status >= 400 {
                                let response = http_response_status(initialize_status, "init rejected");
                                let _ = stream.write_all(response.as_bytes());
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "s067-status", "version": "1.0.0" },
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
                                        "name": "s067_tool",
                                        "description": "S067 red test tool",
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
                                break;
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

    S067StatusServer {
        addr,
        initialize_status,
        tool_statuses,
        tool_call_attempts,
        stop,
        handle: Some(handle),
    }
}

#[derive(Clone)]
enum S067BodyMode {
    Json { body: String, chunk_size: usize },
    Sse { events: Vec<String>, final_result: String },
}

struct S067BodyServer {
    addr: String,
    chunks_attempted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl S067BodyServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn chunks_attempted(&self) -> usize {
        self.chunks_attempted.load(Ordering::SeqCst)
    }
}

impl Drop for S067BodyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_s067_body_server(mode: S067BodyMode) -> S067BodyServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("S067 body server should bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let chunks_attempted = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let chunks_attempted_t = Arc::clone(&chunks_attempted);
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

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "s067-body", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response_with_session(&body, "s067-body-session");
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
                                        "name": "s067_tool",
                                        "description": "S067 red test tool",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            match &mode {
                                S067BodyMode::Json { body, chunk_size } => {
                                    let header = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                                        body.len()
                                    );
                                    let _ = stream.write_all(header.as_bytes());
                                    for chunk in body.as_bytes().chunks(*chunk_size) {
                                        chunks_attempted_t.fetch_add(1, Ordering::SeqCst);
                                        if stream.write_all(chunk).is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(20));
                                    }
                                    break;
                                }
                                S067BodyMode::Sse { events, final_result } => {
                                    let body = format!("{}event: message\ndata: {}\n\n", events.join(""), final_result);
                                    let header = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                                        body.len()
                                    );
                                    let _ = stream.write_all(header.as_bytes());
                                    for event in events {
                                        chunks_attempted_t.fetch_add(1, Ordering::SeqCst);
                                        if stream.write_all(event.as_bytes()).is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        thread::sleep(Duration::from_millis(20));
                                    }
                                    let _ = stream.write_all(format!("event: message\ndata: {}\n\n", final_result).as_bytes());
                                    break;
                                }
                            }
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

    S067BodyServer {
        addr,
        chunks_attempted,
        stop,
        handle: Some(handle),
    }
}

async fn connect_s067_server(manager: &mut McpManager, url: &str) {
    manager
        .connect(url, None)
        .await
        .expect("connect should register the local MCP server");
    let tools = list_tools_with_retry(manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "s067_tool");
}

#[tokio::test]
async fn s067_dispatch_401_and_403_return_auth_failed_without_same_credential_retry() {
    let _guard = test_guard().await;

    for status in [401, 403] {
        let server = spawn_s067_status_server(200, vec![status]);
        let mut manager = McpManager::new();
        manager.set_reconnect_base_delay_ms(10);
        connect_s067_server(&mut manager, &server.url("/mcp")).await;

        let capability = capability_with_mcp_tools(&["mcp:*:*"]);
        let err = manager
            .call_tool_for_agent(
                &simulacra_types::AgentId(format!("s067-agent-{status}")),
                "127.0.0.1",
                "s067_tool",
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
async fn s067_dispatch_404_and_500_remain_transport_errors_and_retryable() {
    let _guard = test_guard().await;

    for status in [404, 500] {
        let server = spawn_s067_status_server(200, vec![status, status, status]);
        let mut manager = McpManager::new();
        manager.set_reconnect_base_delay_ms(10);
        connect_s067_server(&mut manager, &server.url("/mcp")).await;

        let capability = capability_with_mcp_tools(&["mcp:*:*"]);
        let err = manager
            .call_tool_for_agent(
                &simulacra_types::AgentId(format!("s067-agent-{status}")),
                "127.0.0.1",
                "s067_tool",
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
async fn s067_handshake_401_still_returns_connection_failed() {
    let _guard = test_guard().await;

    let server = spawn_s067_status_server(401, vec![200]);
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
            &simulacra_types::AgentId("s067-handshake-agent".into()),
            "127.0.0.1",
            "s067_tool",
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
async fn s067_transport_failure_then_auth_failed_stops_after_second_tool_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_s067_status_server(200, vec![500, 401, 200]);
    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    connect_s067_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-agent-500-401".into()),
            "127.0.0.1",
            "s067_tool",
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

#[tokio::test]
async fn s067_json_response_over_cap_returns_response_too_large_before_full_body_is_read() {
    let _guard = test_guard().await;

    let cap = 128;
    let oversized_payload = "x".repeat(1024);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": oversized_payload }
    })
    .to_string();
    let server = spawn_s067_body_server(S067BodyMode::Json {
        body,
        chunk_size: 32,
    });
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(Some(cap));
    connect_s067_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-json-cap".into()),
            "127.0.0.1",
            "s067_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("oversized JSON body should fail at the configured cap");

    assert!(
        matches!(err, McpError::ResponseTooLarge { limit_bytes } if limit_bytes == cap),
        "oversized JSON response should return ResponseTooLarge with the cap, got {err:?}"
    );
    assert!(
        server.chunks_attempted() < 10,
        "client should abort while streaming instead of reading the full oversized body"
    );
}

#[tokio::test]
async fn s067_json_response_cap_boundary_accepts_exact_and_rejects_one_over() {
    let _guard = test_guard().await;

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": "boundary" }
    })
    .to_string();
    let exact_cap = body.len();
    let exact_server = spawn_s067_body_server(S067BodyMode::Json {
        body: body.clone(),
        chunk_size: 8,
    });
    let mut exact_manager = McpManager::new();
    exact_manager.set_max_response_bytes(Some(exact_cap));
    connect_s067_server(&mut exact_manager, &exact_server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = exact_manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-json-exact-cap".into()),
            "127.0.0.1",
            "s067_tool",
            json!({}),
            &capability,
        )
        .await
        .expect("a JSON response exactly at the cap should succeed");
    assert_eq!(output["payload"], json!("boundary"));

    let cap = exact_cap - 1;
    let server = spawn_s067_body_server(S067BodyMode::Json {
        body,
        chunk_size: 8,
    });
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(Some(cap));
    connect_s067_server(&mut manager, &server.url("/mcp")).await;

    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-json-one-over".into()),
            "127.0.0.1",
            "s067_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("a JSON response one byte over the cap should fail");

    assert!(
        matches!(err, McpError::ResponseTooLarge { limit_bytes } if limit_bytes == cap),
        "one-byte-over JSON response should return ResponseTooLarge, got {err:?}"
    );
}

#[tokio::test]
async fn s067_sse_response_over_cap_counts_total_received_stream_bytes() {
    let _guard = test_guard().await;

    let cap = 180;
    let events: Vec<String> = (0..8)
        .map(|index| {
            format!(
                "event: message\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": { "progress": index, "message": "small progress event" }
                })
            )
        })
        .collect();
    let final_result = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "ok": true }
    })
    .to_string();
    let server = spawn_s067_body_server(S067BodyMode::Sse {
        events,
        final_result,
    });
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(Some(cap));
    connect_s067_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-sse-cap".into()),
            "127.0.0.1",
            "s067_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("SSE stream should fail once cumulative bytes exceed cap");

    assert!(
        matches!(err, McpError::ResponseTooLarge { limit_bytes } if limit_bytes == cap),
        "oversized SSE stream should return ResponseTooLarge with the cap, got {err:?}"
    );
}

#[tokio::test]
async fn s067_unset_response_cap_preserves_large_json_success() {
    let _guard = test_guard().await;

    let large_payload = "x".repeat(2048);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": large_payload }
    })
    .to_string();
    let server = spawn_s067_body_server(S067BodyMode::Json {
        body,
        chunk_size: 128,
    });
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(None);
    connect_s067_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("s067-json-uncapped".into()),
            "127.0.0.1",
            "s067_tool",
            json!({}),
            &capability,
        )
        .await
        .expect("uncapped large JSON response should preserve default behavior");

    assert_eq!(
        output["payload"].as_str().expect("payload should be a string").len(),
        2048
    );
}

#[test]
fn s067_mcp_error_kind_literals_are_stable_wire_contract() {
    assert_eq!(
        McpError::AuthFailed("token expired".into()).kind(),
        "auth_failed"
    );
    assert_eq!(
        McpError::ResponseTooLarge { limit_bytes: 64 }.kind(),
        "response_too_large"
    );
}
