fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

// ── SessionExpiryServer ────────────────────────────────────────────
//
// Fake MCP server that returns 404 on the first tool call (simulating
// session expiry), then succeeds on subsequent handshake + tool calls
// after re-initialization.

struct SessionExpiryServer {
    addr: String,
    tool_call_attempts: Arc<AtomicUsize>,
    initialize_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SessionExpiryServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }

    fn initialize_count(&self) -> usize {
        self.initialize_count.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex").clone()
    }
}

impl Drop for SessionExpiryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that:
/// - Answers initialize with a session ID
/// - Returns 404 on the first N tool calls (simulating session expiry)
/// - Succeeds on tool calls after that
fn spawn_session_expiry_server(reject_first_n_tool_calls: usize) -> SessionExpiryServer {
    spawn_session_expiry_server_inner(reject_first_n_tool_calls, false)
}

fn spawn_session_expiry_server_with_failed_rehandshake() -> SessionExpiryServer {
    spawn_session_expiry_server_inner(1, true)
}

fn spawn_session_expiry_server_inner(
    reject_first_n_tool_calls: usize,
    fail_rehandshake: bool,
) -> SessionExpiryServer {
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
                                    "serverInfo": { "name": "session-expiry-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response =
                                json_http_response_with_session(&body, "session-xyz-789");
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
                                        "name": "session_tool",
                                        "description": "A tool for session tests",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let attempt = tool_call_attempts_t.fetch_add(1, Ordering::SeqCst);
                            if attempt < reject_first_n_tool_calls {
                                // Return 404 to simulate session expiry.
                                let response = http_response_status(404, "Not Found");
                                let _ = stream.write_all(response.as_bytes());
                                break; // 404 closes connection
                            } else {
                                let body = json!({
                                    "jsonrpc": "2.0",
                                    "id": 1,
                                    "result": { "session_ok": true }
                                })
                                .to_string();
                                let response = json_http_response(&body);
                                let _ = stream.write_all(response.as_bytes());
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

    SessionExpiryServer {
        addr,
        tool_call_attempts,
        initialize_count,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── SseStreamingServer ─────────────────────────────────────────────
//
// Fake MCP server that returns SSE streaming responses for tool calls.

struct SseStreamingServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SseStreamingServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for SseStreamingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Type of SSE response for tool calls.
#[derive(Clone)]
enum SseToolResponse {
    /// SSE with progress notifications followed by a final JSON-RPC result.
    WithResult(String),
    /// SSE with only progress notifications, no final result (stream closes).
    NoResult,
}

fn spawn_sse_streaming_server(sse_tool_response: SseToolResponse) -> SseStreamingServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

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
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "sse-streaming-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response_with_session(&body, "sse-session-1");
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
                                        "name": "streaming_tool",
                                        "description": "A tool with SSE streaming responses",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            // Return an SSE streaming response.
                            let sse_response = match &sse_tool_response {
                                SseToolResponse::WithResult(result_json) => {
                                    // Progress notification + final result.
                                    let progress_event = format!(
                                        "event: message\ndata: {}\n\n",
                                        json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": { "progress": 50, "total": 100 }
                                        })
                                    );
                                    let result_event =
                                        format!("event: message\ndata: {}\n\n", result_json);
                                    format!("{}{}", progress_event, result_event)
                                }
                                SseToolResponse::NoResult => {
                                    // Only progress, then stream closes.
                                    format!(
                                        "event: message\ndata: {}\n\n",
                                        json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": { "progress": 50, "total": 100 }
                                        })
                                    )
                                }
                            };

                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                                sse_response.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(sse_response.as_bytes());
                            let _ = stream.flush();
                            break; // Close connection after SSE response.
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

    SseStreamingServer {
        addr,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── Task 4 Tests ───────────────────────────────────────────────────

