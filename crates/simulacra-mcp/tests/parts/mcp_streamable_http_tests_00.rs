use serde_json::json;
use simulacra_mcp::{McpError, McpManager};
use simulacra_types::CapabilityToken;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

// ── Test serialization ──────────────────────────────────────────────
//
// Test servers use non-blocking polling loops that can starve under high
// parallelism. Serialize tests within this binary to avoid flakiness.

static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

async fn list_tools_with_retry(manager: &mut McpManager) -> Vec<simulacra_mcp::ToolDefinition> {
    for _ in 0..5 {
        let tools = manager.list_tools().await;
        if !tools.is_empty() {
            return tools;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    manager.list_tools().await
}

async fn wait_for_streamable_requests(
    server: &StreamableHttpServer,
    predicate: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    for _ in 0..10 {
        let requests = server.requests();
        if predicate(&requests) {
            return requests;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    server.requests()
}

// ── Helpers ─────────────────────────────────────────────────────────

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

fn json_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    )
}

fn json_http_response_with_session(body: &str, session_id: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        session_id,
        body.len(),
        body
    )
}

/// Frame `json_body` as a single SSE message event (Content-Type: text/event-stream).
fn sse_http_response(json_body: &str) -> String {
    let sse_body = format!("event: message\ndata: {}\n\n", json_body);
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        sse_body.len(),
        sse_body
    )
}

/// Same as `sse_http_response` but also sets `Mcp-Session-Id`.
fn sse_http_response_with_session(json_body: &str, session_id: &str) -> String {
    let sse_body = format!("event: message\ndata: {}\n\n", json_body);
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nMcp-Session-Id: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        session_id,
        sse_body.len(),
        sse_body
    )
}

fn http_response_status(status: u16, reason: &str) -> String {
    let body = format!("{{\"error\":\"{reason}\"}}");
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    )
}

// ── StreamableHttpServer ────────────────────────────────────────────
//
// Fake MCP server supporting the 2025-03-26 streamable HTTP protocol.
// Accepts POST on the configured path. Returns configurable tools and
// optionally sets Mcp-Session-Id.

struct StreamableHttpServer {
    addr: String,
    post_attempts: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StreamableHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn post_attempts(&self) -> usize {
        self.post_attempts.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }
}

impl Drop for StreamableHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_streamable_http_server(
    tools_list_body: &str,
    tool_call_body: &str,
    session_id: Option<&str>,
) -> StreamableHttpServer {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("streamable HTTP test server should bind");
    listener
        .set_nonblocking(true)
        .expect("streamable HTTP test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("streamable HTTP test server should have a local address")
        .to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let post_attempts_for_thread = Arc::clone(&post_attempts);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let ready_for_thread = Arc::clone(&ready);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();
    let session_id = session_id.map(|s| s.to_string());

    let handle = thread::spawn(move || {
        ready_for_thread.store(true, Ordering::SeqCst);
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        post_attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "fake-streamable", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            // Notification — no id, no result needed. Return accepted.
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = if request.contains("\"method\":\"initialize\"") {
                            if let Some(ref sid) = session_id {
                                json_http_response_with_session(&body, sid)
                            } else {
                                json_http_response(&body)
                            }
                        } else {
                            json_http_response(&body)
                        };
                        let _ = stream.write_all(response.as_bytes());
                    }
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

    StreamableHttpServer {
        addr,
        post_attempts,
        requests,
        stop,
        handle: Some(handle),
    }
}

/// Like `spawn_streamable_http_server` but frames the `initialize` and
/// `tools/list` responses as SSE (`Content-Type: text/event-stream`,
/// `event: message\ndata: <json>\n\n`) instead of plain JSON.
///
/// This mirrors the behaviour of `api.githubcopilot.com/mcp/` and exercises
/// the handshake-path SSE parsing that the production server must support.
fn spawn_streamable_http_server_sse(
    tools_list_body: &str,
    tool_call_body: &str,
    session_id: Option<&str>,
) -> StreamableHttpServer {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("streamable HTTP SSE test server should bind");
    listener
        .set_nonblocking(true)
        .expect("streamable HTTP SSE test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("streamable HTTP SSE test server should have a local address")
        .to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let post_attempts_for_thread = Arc::clone(&post_attempts);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let ready_for_thread = Arc::clone(&ready);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();
    let session_id = session_id.map(|s| s.to_string());

    let handle = thread::spawn(move || {
        ready_for_thread.store(true, Ordering::SeqCst);
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        post_attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "fake-streamable-sse", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        // initialize and tools/list are framed as SSE; everything
                        // else (notifications/initialized, tools/call) uses plain JSON.
                        let response = if request.contains("\"method\":\"initialize\"") {
                            if let Some(ref sid) = session_id {
                                sse_http_response_with_session(&body, sid)
                            } else {
                                sse_http_response(&body)
                            }
                        } else if request.contains("\"method\":\"tools/list\"") {
                            sse_http_response(&body)
                        } else {
                            json_http_response(&body)
                        };
                        let _ = stream.write_all(response.as_bytes());
                    }
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

    StreamableHttpServer {
        addr,
        post_attempts,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── FallbackTestServer ──────────────────────────────────────────────
//
// Returns a configurable HTTP status code on POST (simulating a legacy
// SSE-only server that rejects the streamable HTTP initialize POST).
// Serves SSE endpoint discovery on GET /sse, handles JSON-RPC on POST /mcp-rpc.

