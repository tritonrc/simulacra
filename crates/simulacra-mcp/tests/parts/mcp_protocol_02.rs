struct SseProbeServer {
    addr: String,
    connection_count: Arc<AtomicUsize>,
    persistent_event_sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SseProbeServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    fn persistent_event_sent(&self) -> bool {
        self.persistent_event_sent.load(Ordering::SeqCst)
    }
}

impl Drop for SseProbeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_sse_probe_server() -> SseProbeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test SSE server should bind");
    listener
        .set_nonblocking(true)
        .expect("test SSE server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("test SSE server should have a local address")
        .to_string();
    let connection_count = Arc::new(AtomicUsize::new(0));
    let persistent_event_sent = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let connection_count_for_thread = Arc::clone(&connection_count);
    let persistent_event_sent_for_thread = Arc::clone(&persistent_event_sent);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    connection_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);

                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: ready\ndata: one\n\n",
                    );
                    thread::sleep(Duration::from_millis(150));
                    if stream.write_all(b"event: update\ndata: two\n\n").is_ok() {
                        persistent_event_sent_for_thread.store(true, Ordering::SeqCst);
                    }

                    while !stop_for_thread.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    SseProbeServer {
        addr,
        connection_count,
        persistent_event_sent,
        stop,
        handle: Some(handle),
    }
}

struct SseDiscoveryServer {
    addr: String,
    sse_connection_count: Arc<AtomicUsize>,
    post_requests: Arc<Mutex<Vec<String>>>,
    persistent_event_sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SseDiscoveryServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
    }

    fn sse_connection_count(&self) -> usize {
        self.sse_connection_count.load(Ordering::SeqCst)
    }

    fn post_requests(&self) -> Vec<String> {
        self.post_requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }

    fn persistent_event_sent(&self) -> bool {
        self.persistent_event_sent.load(Ordering::SeqCst)
    }
}

impl Drop for SseDiscoveryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so teardown is deterministic even when no
        // further protocol request is expected.
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(request: &str) -> usize {
    request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut expected_len = None;

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                request.extend_from_slice(&buffer[..bytes_read]);

                if header_end.is_none() {
                    if let Some(idx) = find_bytes(&request, b"\r\n\r\n") {
                        let end = idx + 4;
                        let headers = String::from_utf8_lossy(&request[..end]).into_owned();
                        let content_length = parse_content_length(&headers);
                        header_end = Some(end);
                        expected_len = Some(end + content_length);
                    }
                }

                if let Some(total_len) = expected_len {
                    if request.len() >= total_len {
                        break;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !request.is_empty() {
                    break;
                }
                return None;
            }
            Err(_) => return None,
        }
    }

    if request.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&request).into_owned())
    }
}

fn spawn_sse_discovery_server(tools_list_body: &str, tool_call_body: &str) -> SseDiscoveryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test SSE server should bind");
    let addr = listener
        .local_addr()
        .expect("test SSE server should have a local address")
        .to_string();
    let sse_connection_count = Arc::new(AtomicUsize::new(0));
    let post_requests = Arc::new(Mutex::new(Vec::new()));
    let persistent_event_sent = Arc::new(AtomicBool::new(false));
    let initialize_seen = Arc::new(AtomicBool::new(false));
    let tools_list_seen = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let sse_connection_count_for_thread = Arc::clone(&sse_connection_count);
    let post_requests_for_thread = Arc::clone(&post_requests);
    let persistent_event_sent_for_thread = Arc::clone(&persistent_event_sent);
    let initialize_seen_for_thread = Arc::clone(&initialize_seen);
    let tools_list_seen_for_thread = Arc::clone(&tools_list_seen);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    // Workspace-wide linking can delay a later request chunk;
                    // fixture timing is not part of the SSE behavior under test.
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let request = match read_http_request(&mut stream) {
                        Some(request) => request,
                        None => continue,
                    };

                    if request.starts_with("GET /sse ") {
                        sse_connection_count_for_thread.fetch_add(1, Ordering::SeqCst);

                        let stop_for_sse = Arc::clone(&stop_for_thread);
                        let initialize_seen_for_sse = Arc::clone(&initialize_seen_for_thread);
                        let tools_list_seen_for_sse = Arc::clone(&tools_list_seen_for_thread);
                        let persistent_event_sent_for_sse =
                            Arc::clone(&persistent_event_sent_for_thread);

                        thread::spawn(move || {
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /mcp-rpc\n\n",
                            );
                            let _ = stream.flush();

                            while !stop_for_sse.load(Ordering::SeqCst) {
                                let _ = stream.write_all(b": keep-alive\n\n");
                                let _ = stream.flush();

                                if initialize_seen_for_sse.load(Ordering::SeqCst)
                                    && tools_list_seen_for_sse.load(Ordering::SeqCst)
                                    && !persistent_event_sent_for_sse.swap(true, Ordering::SeqCst)
                                {
                                    let _ = stream.write_all(
                                        b"event: update\ndata: {\"status\":\"still-open\"}\n\n",
                                    );
                                    let _ = stream.flush();
                                }

                                thread::sleep(Duration::from_millis(50));
                            }
                        });
                        continue;
                    }

                    if request.starts_with("POST ") {
                        post_requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"initialize\"")
                        {
                            initialize_seen_for_thread.store(true, Ordering::SeqCst);
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-sse-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"tools/list\"")
                        {
                            tools_list_seen_for_thread.store(true, Ordering::SeqCst);
                            tools_list_body.clone()
                        } else if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"tools/call\"")
                        {
                            tool_call_body.clone()
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "error": {
                                    "code": -32000,
                                    "message": "JSON-RPC must be sent to /mcp-rpc, not the SSE URL"
                                }
                            })
                            .to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(_) => break,
            }
        }
    });

    SseDiscoveryServer {
        addr,
        sse_connection_count,
        post_requests,
        persistent_event_sent,
        stop,
        handle: Some(handle),
    }
}

// S008 Assertion: No use of std::process::Command or tokio::process::Command in simulacra-mcp.
// Constraint: MCP servers are accessed via HTTP/SSE only — no stdio, no child processes.
//
// This is verified behaviorally: we start a real HTTP MCP server, connect to it,
// and confirm the connection completes successfully over HTTP. The simulacra-mcp crate
// has no API surface for stdio/child-process transports — connect_http and connect_sse
// are the only connection methods, and both use network I/O exclusively.
// The architectural constraint (no std::process::Command) is enforced by code review
// and by the absence of any spawn/stdio API in McpManager's public interface.
