struct BodyServer {
    addr: String,
    chunks_attempted: Arc<AtomicUsize>,
    hold_after: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl BodyServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn chunks_attempted(&self) -> usize {
        self.chunks_attempted.load(Ordering::SeqCst)
    }

    /// The JSON body pauses after `chunks` until [`Self::release`] or 5 s.
    fn hold_after(&self, chunks: usize) {
        self.hold_after.store(chunks, Ordering::SeqCst);
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl Drop for BodyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_body_server(mode: BodyMode) -> BodyServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("body server should bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let chunks_attempted = Arc::new(AtomicUsize::new(0));
    let hold_after = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let chunks_attempted_t = Arc::clone(&chunks_attempted);
    let hold_after_t = Arc::clone(&hold_after);
    let released_t = Arc::clone(&released);
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
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "body", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response_with_session(&body, "body-session");
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
                            match &mode {
                                BodyMode::Json { body, chunk_size } => {
                                    // No Content-Length: the body ends at close, so only the
                                    // streamed byte count can trip the cap.
                                    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                                    let _ = stream.write_all(header.as_bytes());
                                    for chunk in body.as_bytes().chunks(*chunk_size) {
                                        let sent = chunks_attempted_t.fetch_add(1, Ordering::SeqCst) + 1;
                                        if stream.write_all(chunk).is_err() {
                                            break;
                                        }
                                        let _ = stream.flush();
                                        if sent == hold_after_t.load(Ordering::SeqCst) {
                                            let until = std::time::Instant::now() + Duration::from_secs(5);
                                            while !released_t.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                                                thread::sleep(Duration::from_millis(5));
                                            }
                                        }
                                        thread::sleep(Duration::from_millis(20));
                                    }
                                }
                                BodyMode::Sse { events, final_result } => {
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

    BodyServer {
        addr,
        chunks_attempted,
        hold_after,
        released,
        stop,
        handle: Some(handle),
    }
}

#[tokio::test]
async fn json_response_over_cap_returns_response_too_large_before_full_body_is_read() {
    let _guard = test_guard().await;

    let cap = 128;
    let oversized_payload = "x".repeat(1024);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": oversized_payload }
    })
    .to_string();
    let server = spawn_body_server(BodyMode::Json {
        body,
        chunk_size: 32,
    });
    let mut manager = McpManager::new();
    connect_test_server(&mut manager, &server.url("/mcp")).await;
    manager.set_max_response_bytes(Some(cap));
    // Five 32-byte chunks cross the 128-byte cap; a client reading to the
    // end would wait at the hold for more.
    server.hold_after(5);

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("json-cap".into()),
            "127.0.0.1",
            "test_tool",
            json!({}),
            &capability,
        )
        .await
        .expect_err("oversized JSON body should fail at the configured cap");

    assert!(
        matches!(err, McpError::ResponseTooLarge { limit_bytes } if limit_bytes == cap),
        "oversized JSON response should return ResponseTooLarge with the cap, got {err:?}"
    );
    let sent = server.chunks_attempted();
    server.release();
    assert_eq!(sent, 5, "client should abort at the chunk that crosses the cap");

    manager.set_max_response_bytes(None);
    let ok = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("json-cap".into()),
            "127.0.0.1",
            "test_tool",
            json!({}),
            &capability,
        )
        .await
        .expect("the same body succeeds with no cap");
    assert!(ok.to_string().contains(&oversized_payload));
}

#[tokio::test]
async fn json_response_cap_boundary_accepts_exact_and_rejects_one_over() {
    let _guard = test_guard().await;

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": "boundary" }
    })
    .to_string();
    let exact_cap = body.len();
    let exact_server = spawn_body_server(BodyMode::Json {
        body: body.clone(),
        chunk_size: 8,
    });
    let mut exact_manager = McpManager::new();
    connect_test_server(&mut exact_manager, &exact_server.url("/mcp")).await;
    exact_manager.set_max_response_bytes(Some(exact_cap));

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = exact_manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("json-exact-cap".into()),
            "127.0.0.1",
            "test_tool",
            json!({}),
            &capability,
        )
        .await
        .expect("a JSON response exactly at the cap should succeed");
    assert_eq!(output["payload"], json!("boundary"));

    let cap = exact_cap - 1;
    let server = spawn_body_server(BodyMode::Json {
        body,
        chunk_size: 8,
    });
    let mut manager = McpManager::new();
    connect_test_server(&mut manager, &server.url("/mcp")).await;
    manager.set_max_response_bytes(Some(cap));

    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("json-one-over".into()),
            "127.0.0.1",
            "test_tool",
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
async fn sse_response_over_cap_counts_total_received_stream_bytes() {
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
    let server = spawn_body_server(BodyMode::Sse {
        events,
        final_result,
    });
    let mut manager = McpManager::new();
    connect_test_server(&mut manager, &server.url("/mcp")).await;
    manager.set_max_response_bytes(Some(cap));

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("sse-cap".into()),
            "127.0.0.1",
            "test_tool",
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
async fn unset_response_cap_preserves_large_json_success() {
    let _guard = test_guard().await;

    let large_payload = "x".repeat(2048);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "payload": large_payload }
    })
    .to_string();
    let server = spawn_body_server(BodyMode::Json {
        body,
        chunk_size: 128,
    });
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(None);
    connect_test_server(&mut manager, &server.url("/mcp")).await;

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool_for_agent(
            &simulacra_types::AgentId("json-uncapped".into()),
            "127.0.0.1",
            "test_tool",
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
fn mcp_error_kind_literals_are_stable_wire_contract() {
    assert_eq!(
        McpError::AuthFailed("token expired".into()).kind(),
        "auth_failed"
    );
    assert_eq!(
        McpError::ResponseTooLarge { limit_bytes: 64 }.kind(),
        "response_too_large"
    );
}
